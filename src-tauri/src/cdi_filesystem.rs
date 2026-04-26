// CD-i Green Book filesystem reader (Chapter III of Full Functional Specification)
// All multi-byte fields are big-endian only (unlike ISO 9660 which uses both-endian).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::OnceLock;
use crate::DiscEntry;

const SECTOR_SIZE: u64 = 2352;
const BLOCK_SIZE: usize = 2048;

// ECMA-130 scramble table: 15-bit LFSR (X^15 + X + 1), initial state 1, LSB-first per byte.
// Covers bytes 12-2351 of each raw sector (2340 bytes). Sectors are individually scrambled
// (LFSR resets to 1 at the start of each sector's scrambled region).
static SCRAMBLE_TABLE: OnceLock<[u8; 2340]> = OnceLock::new();

fn scramble_table() -> &'static [u8; 2340] {
    SCRAMBLE_TABLE.get_or_init(|| {
        let mut state: u16 = 1;
        let mut table = [0u8; 2340];
        for byte in table.iter_mut() {
            let mut val = 0u8;
            for bit in 0..8 {
                let out = (state & 1) as u8;
                val |= out << bit;
                let feedback = (state & 1) ^ ((state >> 1) & 1);
                state = ((state >> 1) | (feedback << 14)) & 0x7FFF;
            }
            *byte = val;
        }
        table
    })
}

pub struct CdiFs {
    file: File,
    track_offset: u64,
    user_data_offset: u64,
    lba_offset: u64,
    descramble: bool,
    pub root_lba: u32,
    pub volume_id: String,
}

impl CdiFs {
    pub fn new(
        mut file: File,
        track_offset: u64,
        user_data_offset: u64,
        lba_offset: u64,
        descramble: bool,
    ) -> Result<Self, String> {
        let buf = read_block(&mut file, track_offset, user_data_offset, lba_offset, 16, descramble)?;

        // Disc Label Record Type must be 1 (standard) and Standard ID must be "CD-I "
        if buf[0] != 1 || &buf[1..6] != b"CD-I " {
            return Err(format!(
                "Not a CD-i volume descriptor (type={}, id={})",
                buf[0],
                std::str::from_utf8(&buf[1..6]).unwrap_or("?")
            ));
        }

        // Volume identifier: bytes 40-71 (BP 41-72, 32 bytes), space-padded
        let volume_id = std::str::from_utf8(&buf[40..72])
            .unwrap_or("")
            .trim_end()
            .to_string();

        // Address of Path Table: bytes 148-151 (BP 149-152, 4 bytes BE)
        let path_table_lba = u32::from_be_bytes([buf[148], buf[149], buf[150], buf[151]]);

        // Read the path table; first entry describes the root directory.
        // Path table entry: name_size(1) + ext_attr(1) + dir_lba(4 BE) + parent(2 BE) + name(n)
        let pt = read_block(&mut file, track_offset, user_data_offset, lba_offset, path_table_lba as u64, descramble)?;
        // bytes 2-5 of first entry = root directory LBN
        let root_lba = u32::from_be_bytes([pt[2], pt[3], pt[4], pt[5]]);

        Ok(Self {
            file,
            track_offset,
            user_data_offset,
            lba_offset,
            descramble,
            root_lba,
            volume_id,
        })
    }

    pub fn list_directory(&mut self, dir_path: &str) -> Result<Vec<DiscEntry>, String> {
        let lba = self.resolve_path(dir_path)?;
        let buf = read_block(
            &mut self.file,
            self.track_offset,
            self.user_data_offset,
            self.lba_offset,
            lba as u64,
            self.descramble,
        )?;
        parse_dir_records(&buf)
    }

    pub fn extract_file(&mut self, file_path: &str, dest_path: &str) -> Result<(), String> {
        let (lba, size) = self.find_file(file_path)?;
        let mut dest = File::create(dest_path)
            .map_err(|e| format!("Cannot create destination: {e}"))?;

        let num_blocks = (size as u64 + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;
        let mut remaining = size as u64;

        for i in 0..num_blocks {
            let buf = read_block(
                &mut self.file,
                self.track_offset,
                self.user_data_offset,
                self.lba_offset,
                lba as u64 + i,
                self.descramble,
            )?;
            let to_write = remaining.min(BLOCK_SIZE as u64) as usize;
            dest.write_all(&buf[..to_write])
                .map_err(|e| format!("Write error: {e}"))?;
            remaining -= to_write as u64;
        }

        Ok(())
    }

    pub fn extract_directory(&mut self, dir_path: &str, dest_path: &str) -> Result<(), String> {
        let entries = self.list_directory(dir_path)?;
        std::fs::create_dir_all(dest_path)
            .map_err(|e| format!("Cannot create directory: {e}"))?;

        for entry in entries {
            let src = if dir_path == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{}/{}", dir_path.trim_end_matches('/'), entry.name)
            };
            let dst = std::path::Path::new(dest_path).join(&entry.name);
            let dst_str = dst.to_string_lossy().into_owned();

            if entry.is_dir {
                self.extract_directory(&src, &dst_str)?;
            } else {
                self.extract_file(&src, &dst_str)?;
            }
        }

        Ok(())
    }

    fn resolve_path(&mut self, path: &str) -> Result<u32, String> {
        let mut lba = self.root_lba;
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            let buf = read_block(
                &mut self.file,
                self.track_offset,
                self.user_data_offset,
                self.lba_offset,
                lba as u64,
                self.descramble,
            )?;
            let entries = parse_dir_records(&buf)?;
            lba = entries
                .iter()
                .find(|e| e.is_dir && e.name.eq_ignore_ascii_case(segment))
                .map(|e| e.lba)
                .ok_or_else(|| format!("Directory not found: '{segment}'"))?;
        }
        Ok(lba)
    }

    fn find_file(&mut self, path: &str) -> Result<(u32, u32), String> {
        let (dir_path, file_name) = match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        };
        let dir_lba = if dir_path.is_empty() || dir_path == "/" {
            self.root_lba
        } else {
            self.resolve_path(dir_path)?
        };
        let buf = read_block(
            &mut self.file,
            self.track_offset,
            self.user_data_offset,
            self.lba_offset,
            dir_lba as u64,
            self.descramble,
        )?;
        parse_dir_records(&buf)?
            .iter()
            .find(|e| !e.is_dir && e.name.eq_ignore_ascii_case(file_name))
            .map(|e| (e.lba, e.size_bytes))
            .ok_or_else(|| format!("File not found: '{file_name}'"))
    }
}

fn read_block(
    file: &mut File,
    track_offset: u64,
    user_data_offset: u64,
    lba_offset: u64,
    lba: u64,
    descramble: bool,
) -> Result<[u8; BLOCK_SIZE], String> {
    let adjusted = if lba >= lba_offset { lba - lba_offset } else { lba };
    if descramble {
        // Read the full 2352-byte raw sector, descramble bytes 12-2351, then extract user data.
        let pos = track_offset + adjusted * SECTOR_SIZE;
        file.seek(SeekFrom::Start(pos))
            .map_err(|e| format!("Seek error at LBA {lba}: {e}"))?;
        let mut sector = [0u8; 2352];
        file.read_exact(&mut sector)
            .map_err(|e| format!("Read error at LBA {lba}: {e}"))?;
        let table = scramble_table();
        for i in 12..2352usize {
            sector[i] ^= table[i - 12];
        }
        let start = user_data_offset as usize;
        let mut buf = [0u8; BLOCK_SIZE];
        buf.copy_from_slice(&sector[start..start + BLOCK_SIZE]);
        Ok(buf)
    } else {
        let pos = track_offset + adjusted * SECTOR_SIZE + user_data_offset;
        file.seek(SeekFrom::Start(pos))
            .map_err(|e| format!("Seek error at LBA {lba}: {e}"))?;
        let mut buf = [0u8; BLOCK_SIZE];
        file.read_exact(&mut buf)
            .map_err(|e| format!("Read error at LBA {lba}: {e}"))?;
        Ok(buf)
    }
}

fn parse_dir_records(buf: &[u8; BLOCK_SIZE]) -> Result<Vec<DiscEntry>, String> {
    let mut entries = Vec::new();
    let mut offset = 0usize;

    while offset < BLOCK_SIZE {
        let record_len = buf[offset] as usize;
        if record_len == 0 {
            break;
        }
        if offset + record_len > BLOCK_SIZE {
            break;
        }

        let r = &buf[offset..offset + record_len];
        offset += record_len;

        // Need at least 33 bytes for fixed-size fields
        if r.len() < 33 {
            continue;
        }

        // File beginning LBN: bytes 6-9 (BP 7-10, 4 bytes BE)
        let file_lba = u32::from_be_bytes([r[6], r[7], r[8], r[9]]);

        // File size: bytes 14-17 (BP 15-18, 4 bytes BE)
        let file_size = u32::from_be_bytes([r[14], r[15], r[16], r[17]]);

        // Creation date: bytes 18-23 (6 bytes: year-1900, month, day, hour, min, sec)
        let year = r[18] as u32 + 1900;
        let month = r[19];
        let day = r[20];
        let hour = r[21];
        let min = r[22];
        let sec = r[23];

        // File name size: byte 32 (BP 33)
        let name_size = r[32] as usize;
        let name_end = 33 + name_size;

        if name_end > r.len() {
            continue;
        }

        let name_bytes = &r[33..name_end];

        // Skip the "." entry (null byte name) and ".." entry (0x01 byte name)
        if name_size == 0
            || (name_size == 1 && (name_bytes[0] == 0x00 || name_bytes[0] == 0x01))
        {
            continue;
        }

        let name = std::str::from_utf8(name_bytes)
            .unwrap_or("???")
            .to_string();

        // Determine if directory from Attributes field (bit 15 = directory).
        // Layout after filename: if name_size is even, 1 padding byte; then 4 bytes Owner ID;
        // then 2 bytes Attributes (BP 38+n or 39+n depending on padding).
        let padding = if name_size % 2 == 0 { 1usize } else { 0usize };
        let attr_start = 33 + name_size + padding + 4; // skip Owner ID (4 bytes)

        let is_dir = if attr_start + 2 <= r.len() {
            let attr = u16::from_be_bytes([r[attr_start], r[attr_start + 1]]);
            attr & 0x8000 != 0
        } else {
            false
        };

        let modified = format!(
            "{}-{:02}-{:02} {:02}:{:02}:{:02}",
            year, month, day, hour, min, sec
        );

        entries.push(DiscEntry {
            name,
            is_dir,
            lba: file_lba,
            size: if is_dir { 0 } else { file_size },
            size_bytes: file_size,
            modified,
        });
    }

    Ok(entries)
}

/// Peek at the volume descriptor at LBA 16 and return true if it uses the CD-i
/// Green Book filesystem. Pass `descramble: true` for sectors stored scrambled
/// (ECMA-130 LFSR), e.g. the pregap of a CD-i Ready disc.
pub fn is_cdi_disc(
    bin_path: &std::path::Path,
    track_offset: u64,
    user_data_offset: u64,
    lba_offset: u64,
    descramble: bool,
) -> bool {
    let Ok(mut f) = File::open(bin_path) else { return false };
    let adjusted = if 16u64 >= lba_offset { 16 - lba_offset } else { 16 };
    if descramble {
        let pos = track_offset + adjusted * SECTOR_SIZE;
        if f.seek(SeekFrom::Start(pos)).is_err() { return false; }
        let mut sector = [0u8; 2352];
        if f.read_exact(&mut sector).is_err() { return false; }
        let table = scramble_table();
        for i in 12..2352usize { sector[i] ^= table[i - 12]; }
        // After descrambling, user data starts at user_data_offset; VD identifier is bytes 1-5.
        let start = user_data_offset as usize;
        &sector[start + 1..start + 6] == b"CD-I "
    } else {
        let pos = track_offset + adjusted * SECTOR_SIZE + user_data_offset;
        if f.seek(SeekFrom::Start(pos)).is_err() { return false; }
        let mut buf = [0u8; 6];
        if f.read_exact(&mut buf).is_err() { return false; }
        &buf[1..6] == b"CD-I "
    }
}

/// Returns true when the pregap starting at `pregap_byte_offset` in the BIN
/// file contains ECMA-130-scrambled CD-i Mode 2 sectors (CD-i Ready format).
pub fn is_cdi_ready_pregap(bin_path: &std::path::Path, pregap_byte_offset: u64) -> bool {
    // Treat the pregap as a track starting at byte pregap_byte_offset, lba_offset=0.
    is_cdi_disc(bin_path, pregap_byte_offset, 24, 0, true)
}
