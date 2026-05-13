// Wii disc partition decryption.
//
// Wii game data lives inside an AES-128-CBC encrypted data partition.
// This module transparently decrypts clusters so that GcmFs can read
// the game's FST and file data normally.
//
// Cluster layout (0x8000 bytes encrypted → 0x7C00 bytes of game data):
//   [0x0000:0x0400] Hash block  (not decrypted; raw bytes [0x3D0:0x3E0] are the data IV)
//   [0x0400:0x8000] Data block  — AES-CBC decrypt, IV = raw[0x3D0:0x3E0]
//
// Key derivation:
//   1. Encrypted title key at ticket+0x01BF (16 bytes)
//   2. IV = title ID (8 bytes at ticket+0x01DC) + 8 zero bytes
//   3. Decrypt with Wii retail common key
//
// Sources: libwbfs (Wiimm, GPL-2.0), libogc, Dolphin Emulator (GPL-2.0)

use std::io::{self, Read, Seek, SeekFrom};
use aes::Aes128;
use cbc::Decryptor;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::NoPadding};

// Publicly known Wii retail common key (embedded in all Wii emulators).
const COMMON_KEY: [u8; 16] = [
    0xEB, 0xE4, 0x2A, 0x22, 0x5E, 0x85, 0x93, 0xE4,
    0x48, 0xD9, 0xC5, 0x45, 0x73, 0x81, 0xAA, 0xF7,
];

const CLUSTER_ENC:  u64   = 0x8000; // encrypted bytes per cluster
const CLUSTER_DATA: u64   = 0x7C00; // decrypted data bytes per cluster
const HASH_SIZE:    usize = 0x400;  // hash block at cluster start
const DATA_IV_OFF:  usize = 0x3D0;  // IV offset within encrypted hash block

pub struct WiiPartReader<F: Read + Seek> {
    inner:      F,
    data_start: u64,        // absolute disc offset to first encrypted cluster
    virt_size:  u64,        // total bytes in decrypted stream
    title_key:  [u8; 16],
    pos:        u64,
    cache_idx:  Option<u64>,
    cache_buf:  Vec<u8>,    // CLUSTER_DATA decrypted bytes
}

impl<F: Read + Seek> WiiPartReader<F> {
    pub fn open(mut inner: F) -> Result<Self, String> {
        let part_off = find_data_partition(&mut inner)
            .ok_or_else(|| "Wii: no data partition found".to_string())?;

        // Ticket: encrypted title key at +0x1BF, title ID at +0x1DC
        inner.seek(SeekFrom::Start(part_off + 0x1BF))
            .map_err(|e| format!("Wii ticket seek: {e}"))?;
        let mut enc_key = [0u8; 16];
        inner.read_exact(&mut enc_key)
            .map_err(|e| format!("Wii title key read: {e}"))?;

        inner.seek(SeekFrom::Start(part_off + 0x1DC))
            .map_err(|e| format!("Wii title ID seek: {e}"))?;
        let mut key_iv = [0u8; 16]; // last 8 bytes stay zero
        inner.read_exact(&mut key_iv[..8])
            .map_err(|e| format!("Wii title ID read: {e}"))?;

        let mut title_key = enc_key;
        type AesDec = Decryptor<Aes128>;
        AesDec::new(&COMMON_KEY.into(), &key_iv.into())
            .decrypt_padded_mut::<NoPadding>(&mut title_key)
            .map_err(|_| "Wii: title key decrypt failed".to_string())?;

        // Partition header at +0x2B8: data_offset (u32 ×4 relative to partition start), data_size (u32 ×4)
        inner.seek(SeekFrom::Start(part_off + 0x2B8))
            .map_err(|e| format!("Wii part header seek: {e}"))?;
        let mut buf = [0u8; 8];
        inner.read_exact(&mut buf)
            .map_err(|e| format!("Wii part header read: {e}"))?;
        let data_off_raw  = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64;
        let data_size_raw = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as u64;
        let data_start = part_off + (data_off_raw << 2);
        let data_size  = data_size_raw << 2;
        let virt_size  = (data_size / CLUSTER_ENC) * CLUSTER_DATA;

        Ok(WiiPartReader {
            inner,
            data_start,
            virt_size,
            title_key,
            pos: 0,
            cache_idx: None,
            cache_buf: vec![0u8; CLUSTER_DATA as usize],
        })
    }

    fn decrypt_cluster(&mut self, idx: u64) -> io::Result<()> {
        self.inner.seek(SeekFrom::Start(self.data_start + idx * CLUSTER_ENC))?;
        let mut raw = vec![0u8; CLUSTER_ENC as usize];
        self.inner.read_exact(&mut raw)?;

        type AesDec = Decryptor<Aes128>;

        // IV for data block comes from the *encrypted* hash block at 0x3D0 (not decrypted).
        let mut data_iv = [0u8; 16];
        data_iv.copy_from_slice(&raw[DATA_IV_OFF..DATA_IV_OFF + 16]);

        // Decrypt data block
        let mut data = raw[HASH_SIZE..].to_vec();
        AesDec::new(&self.title_key.into(), &data_iv.into())
            .decrypt_padded_mut::<NoPadding>(&mut data)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "data decrypt failed"))?;

        self.cache_buf.copy_from_slice(&data);
        self.cache_idx = Some(idx);
        Ok(())
    }
}

impl<F: Read + Seek> Read for WiiPartReader<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.virt_size { return Ok(0); }
        let cluster_idx = self.pos / CLUSTER_DATA;
        let cluster_off = (self.pos % CLUSTER_DATA) as usize;
        if self.cache_idx != Some(cluster_idx) {
            self.decrypt_cluster(cluster_idx)?;
        }
        let avail = (CLUSTER_DATA as usize - cluster_off)
            .min((self.virt_size - self.pos) as usize);
        let n = buf.len().min(avail);
        buf[..n].copy_from_slice(&self.cache_buf[cluster_off..cluster_off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<F: Read + Seek> Seek for WiiPartReader<F> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(n)   => n,
            SeekFrom::End(n)     => if n >= 0 { self.virt_size.saturating_add(n as u64) }
                                    else       { self.virt_size.saturating_sub((-n) as u64) },
            SeekFrom::Current(n) => if n >= 0 { self.pos.saturating_add(n as u64) }
                                    else       { self.pos.saturating_sub((-n) as u64) },
        };
        Ok(self.pos)
    }
}

fn find_data_partition<F: Read + Seek>(reader: &mut F) -> Option<u64> {
    reader.seek(SeekFrom::Start(0x40000)).ok()?;
    let mut hdr = [0u8; 32]; // 4 groups × 8 bytes each
    reader.read_exact(&mut hdr).ok()?;
    for g in 0..4usize {
        let count   = u32::from_be_bytes([hdr[g*8], hdr[g*8+1], hdr[g*8+2], hdr[g*8+3]]) as usize;
        let tbl_off = (u32::from_be_bytes([hdr[g*8+4], hdr[g*8+5], hdr[g*8+6], hdr[g*8+7]]) as u64) << 2;
        if count == 0 { continue; }
        reader.seek(SeekFrom::Start(tbl_off)).ok()?;
        for _ in 0..count {
            let mut e = [0u8; 8];
            reader.read_exact(&mut e).ok()?;
            let part_off  = (u32::from_be_bytes([e[0], e[1], e[2], e[3]]) as u64) << 2;
            let part_type =  u32::from_be_bytes([e[4], e[5], e[6], e[7]]);
            if part_type == 0 { return Some(part_off); } // data partition
        }
    }
    None
}
