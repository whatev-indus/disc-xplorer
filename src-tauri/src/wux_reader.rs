// WUX (Wii U compressed disc image) reader.
//
// WUX deduplicates sectorSize-byte blocks across a Wii U disc image.
// Identical blocks (e.g. padding sectors) are stored once and referenced
// by multiple index table entries.
//
// File layout (all multi-byte integers little-endian):
//   Header (32 bytes):
//     [0x00] u32  magic0 = 0x30585557 ("WUX0")
//     [0x04] u32  magic1 = 0x1099D02E
//     [0x08] u32  sectorSize — block size in bytes (power-of-2, typically 32768)
//     [0x0C] u32  (reserved)
//     [0x10] u64  uncompressedSize — total disc size in bytes
//     [0x18] u32  flags
//     [0x1C] u32  (reserved)
//
//   Index table at offset 0x20:
//     numSectors = ceil(uncompressedSize / sectorSize) entries of u32 LE.
//     index_table[i] = physical block index for logical sector i.
//
//   Data section at: ALIGN(0x20 + numSectors * 4, sectorSize)
//     Physical block N is at: dataStart + N * sectorSize.
//
// Spec source: WudCompress (CEMU project, MIT)
// Attribution: Aaru (Natalia Portillo, LGPL-2.1)

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const WUX_MAGIC0: u32 = 0x30585557;
const WUX_MAGIC1: u32 = 0x1099_D02E;

pub struct WuxReader {
    file:        File,
    sector_size: u64,
    index_table: Vec<u32>,
    data_start:  u64,
    total_bytes: u64,
    pos:         u64,
}

impl WuxReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut f = File::open(path).map_err(|e| format!("Cannot open WUX: {e}"))?;

        let mut hdr = [0u8; 32];
        f.read_exact(&mut hdr).map_err(|e| format!("WUX header read: {e}"))?;

        let magic0 = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let magic1 = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        if magic0 != WUX_MAGIC0 || magic1 != WUX_MAGIC1 {
            return Err("Not a WUX file".to_string());
        }

        let sector_size = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as u64;
        if sector_size == 0 || (sector_size & (sector_size - 1)) != 0 {
            return Err(format!("WUX: invalid sector_size={sector_size}"));
        }

        let total_bytes = u64::from_le_bytes(hdr[16..24].try_into().unwrap());
        let num_sectors = (total_bytes + sector_size - 1) / sector_size;

        f.seek(SeekFrom::Start(0x20)).map_err(|e| format!("WUX index seek: {e}"))?;
        let mut raw = vec![0u8; num_sectors as usize * 4];
        f.read_exact(&mut raw).map_err(|e| format!("WUX index read: {e}"))?;

        let index_table: Vec<u32> = raw.chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();

        let index_end = 0x20u64 + num_sectors * 4;
        let data_start = (index_end + sector_size - 1) / sector_size * sector_size;

        Ok(WuxReader { file: f, sector_size, index_table, data_start, total_bytes, pos: 0 })
    }

    pub fn total_bytes(&self) -> u64 { self.total_bytes }
    pub fn sector_size(&self) -> u64 { self.sector_size }
}

impl Read for WuxReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.total_bytes {
            return Ok(0);
        }

        let sector_idx = (self.pos / self.sector_size) as usize;
        let sector_off = self.pos % self.sector_size;
        let avail = (self.sector_size - sector_off).min(self.total_bytes - self.pos) as usize;
        let to_read = buf.len().min(avail);

        let phys = self.index_table.get(sector_idx).copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "WUX: sector index OOB"))?;

        let file_off = self.data_start + phys as u64 * self.sector_size + sector_off;
        self.file.seek(SeekFrom::Start(file_off))?;
        self.file.read_exact(&mut buf[..to_read])?;

        self.pos += to_read as u64;
        Ok(to_read)
    }
}

impl Seek for WuxReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => {
                if n >= 0 { self.total_bytes.saturating_add(n as u64) }
                else      { self.total_bytes.saturating_sub((-n) as u64) }
            }
            SeekFrom::Current(n) => {
                if n >= 0 { self.pos.saturating_add(n as u64) }
                else      { self.pos.saturating_sub((-n) as u64) }
            }
        };
        Ok(self.pos)
    }
}
