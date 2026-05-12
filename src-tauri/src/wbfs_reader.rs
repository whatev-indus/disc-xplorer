// WBFS (Wii Backup File System) container reader.
//
// WBFS stores one or more Wii disc images by dividing each disc into chunks
// ("WBFS sectors") and mapping them via a per-disc lookup table (wlba_table).
// Sparse chunks (wlba entry == 0) read as zeros, so unused disc regions don't
// take up space on disk.
//
// File layout (all multi-byte integers big-endian):
//   HD sector 0 (bytes 0..hd_sec_sz):
//     [0x00] u32  magic = "WBFS"
//     [0x04] u32  n_hd_sec — total HD sectors in the WBFS partition
//     [0x08] u8   hd_sec_sz_s — log2(HD sector size), commonly 9 (512 B)
//     [0x09] u8   wbfs_sec_sz_s — log2(WBFS sector size), commonly 18–21
//     [0x0A] u8   wbfs_ver
//     [0x0B] u8   padding
//     [0x0C] u8[] disc_table — one byte per disc slot (0=empty, 1=present)
//
//   HD sector 1..N: disc info blocks, one per slot, packed at HD-sector
//     granularity (not WBFS-sector granularity). Each block:
//     [0x000..0x0FF] dhead — copy of first 256 bytes of the Wii disc header
//     [0x100..]      wlba_table — be16 entries, one per WBFS-sector-sized
//                    disc chunk; 0 = sparse (return zeros on read)
//
//   Remaining WBFS sectors: game data, addressed by wlba_table values.
//
// Spec source: Wiimm's ISO Tools (GPL-2.0), libwbfs — Wiimm/wiimms-iso-tools
// Attribution: Aaru format documentation (Natalia Portillo, LGPL-2.1)

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const WBFS_MAGIC: &[u8; 4] = b"WBFS";

// Wii disc geometry — always sized for double-layer discs so the wlba_table
// covers the full disc range (upper half is all-sparse for single-layer games).
const WII_MAX_SECTORS: u64 = 286_864; // sectors in a double-layer Wii disc
const WII_SEC_SZ_S: u32 = 15;         // log2(32768) — Wii physical sector size

pub struct WbfsReader {
    file: File,
    wbfs_sec_sz: u64,
    wlba_table: Vec<u16>,
    disc_size: u64,
    pos: u64,
}

impl WbfsReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut f = File::open(path).map_err(|e| format!("Cannot open WBFS: {e}"))?;

        let mut hdr = [0u8; 12];
        f.read_exact(&mut hdr).map_err(|e| format!("WBFS header read: {e}"))?;

        if &hdr[0..4] != WBFS_MAGIC {
            return Err("Not a WBFS file".to_string());
        }

        let hd_sec_sz_s   = hdr[8] as u32;
        let wbfs_sec_sz_s = hdr[9] as u32;

        if wbfs_sec_sz_s < WII_SEC_SZ_S || wbfs_sec_sz_s > 30 {
            return Err(format!(
                "WBFS: unsupported wbfs_sec_sz_s={wbfs_sec_sz_s} (expected {WII_SEC_SZ_S}–30)"
            ));
        }
        let hd_sec_sz   = 1u64 << hd_sec_sz_s.min(30);
        let wbfs_sec_sz = 1u64 << wbfs_sec_sz_s;

        if hd_sec_sz < 12 {
            return Err(format!("WBFS: hd_sec_sz={hd_sec_sz} is too small"));
        }

        // disc_table occupies bytes [12, hd_sec_sz).  Cap read to 512 slots to
        // avoid reading the entire sector when hd_sec_sz == wbfs_sec_sz.
        let table_bytes = (hd_sec_sz - 12).min(512) as usize;
        let mut disc_table = vec![0u8; table_bytes];
        f.read_exact(&mut disc_table).map_err(|e| format!("WBFS disc_table read: {e}"))?;

        let slot = disc_table.iter().position(|&b| b != 0)
            .ok_or_else(|| "WBFS: no disc found in file".to_string())?;

        // Number of WBFS-sector chunks per disc, sized for a double-layer disc.
        // Formula from libwbfs: n_wii_sec_per_disc >> (wbfs_sec_sz_s - wii_sec_sz_s)
        let n_wlba = (WII_MAX_SECTORS >> (wbfs_sec_sz_s - WII_SEC_SZ_S)) as usize;

        // Disc info blocks start at HD sector 1, packed at HD-sector granularity.
        // disc_info_sz = ceil((256 + n_wlba * 2) / hd_sec_sz) * hd_sec_sz
        let disc_info_sz_raw = 256u64 + n_wlba as u64 * 2;
        let disc_info_sz = (disc_info_sz_raw + hd_sec_sz - 1) / hd_sec_sz * hd_sec_sz;
        let disc_info_off = hd_sec_sz + slot as u64 * disc_info_sz;

        f.seek(SeekFrom::Start(disc_info_off + 0x100))
            .map_err(|e| format!("WBFS wlba seek: {e}"))?;

        let mut raw = vec![0u8; n_wlba * 2];
        f.read_exact(&mut raw).map_err(|e| format!("WBFS wlba read: {e}"))?;
        let wlba_table: Vec<u16> = raw.chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();

        let disc_size = n_wlba as u64 * wbfs_sec_sz;

        Ok(WbfsReader { file: f, wbfs_sec_sz, wlba_table, disc_size, pos: 0 })
    }
}

impl WbfsReader {
    pub fn disc_size(&self) -> u64 { self.disc_size }
}

impl Read for WbfsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.disc_size {
            return Ok(0);
        }

        let chunk_idx = (self.pos / self.wbfs_sec_sz) as usize;
        let chunk_off = self.pos % self.wbfs_sec_sz;
        let avail = (self.wbfs_sec_sz - chunk_off)
            .min(self.disc_size - self.pos) as usize;
        let to_read = buf.len().min(avail);

        let wbfs_sector = self.wlba_table.get(chunk_idx).copied().unwrap_or(0);

        if wbfs_sector == 0 {
            buf[..to_read].fill(0);
        } else {
            let file_off = (wbfs_sector as u64) * self.wbfs_sec_sz + chunk_off;
            self.file.seek(SeekFrom::Start(file_off))
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            self.file.read_exact(&mut buf[..to_read])
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        }

        self.pos += to_read as u64;
        Ok(to_read)
    }
}

impl Seek for WbfsReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => {
                if n >= 0 { self.disc_size.saturating_add(n as u64) }
                else { self.disc_size.saturating_sub((-n) as u64) }
            }
            SeekFrom::Current(n) => {
                if n >= 0 { self.pos.saturating_add(n as u64) }
                else { self.pos.saturating_sub((-n) as u64) }
            }
        };
        Ok(self.pos)
    }
}
