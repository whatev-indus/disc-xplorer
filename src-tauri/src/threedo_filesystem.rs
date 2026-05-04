// 3DO Interactive Multiplayer (OperaFS) filesystem reader.
//
// The 3DO CD-ROM uses the Opera filesystem identified by a 7-byte magic at
// the first data sector: {0x01, 0x5A×5, 0x01}.
//
// Volume header (LBA 0, all fields big-endian) — layout confirmed by real disc probe:
//   bytes 0-6:    magic {0x01, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x01}
//   byte  7:      flags
//   bytes 8-39:   comment (32 bytes, null-padded)
//   bytes 40-71:  volume label (32 bytes, null-padded)
//   bytes 72-75:  unique id (BE)
//   bytes 76-79:  block_size (BE, always 2048)
//   bytes 80-83:  block_count (BE)
//   bytes 84-87:  root directory flags/id (BE)
//   bytes 88-91:  root directory first LBA (BE)
//   bytes 92-95:  root directory byte count (BE)
//   bytes 96-99:  root directory block count (BE)
//
// Directory block (2048 bytes):
//   bytes 0-3:   next block LBA (BE, 0xFFFFFFFF = last)
//   bytes 4-7:   prev block LBA (BE)
//   bytes 8-11:  flags (BE)
//   bytes 12-15: first free byte offset (BE)
//   bytes 16-19: entry count (BE)
//   bytes 20+:   72-byte directory entries
//
// Directory entry (72 bytes):
//   bytes 0-3:   flags (BE)
//   bytes 4-7:   unique id (BE)
//   bytes 8-11:  type tag (BE) — TYPE_DIR='Cat ', TYPE_FILE='Lvl '
//   bytes 12-15: block size (BE)
//   bytes 16-19: byte count (BE) — exact file size
//   bytes 20-23: block count (BE) — size in 2048-byte blocks
//   bytes 24-27: burst (BE)
//   bytes 28-31: gap (BE)
//   bytes 32-63: name (32 bytes, null-terminated ASCII)
//   bytes 64-67: last avatar index (BE) — always 0 for single-extent files
//   bytes 68-71: avatar[0] (BE) — starting LBA of file/directory data

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use crate::DiscEntry;

const OPERA_MAGIC: [u8; 7] = [0x01, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x01];

const VOL_LABEL_OFF: usize = 40;
const VOL_LABEL_LEN: usize = 32;
// Root dir first LBA is at volume header offset 100 (confirmed by disc probe).
// Offset 88 holds the root dir unique ID, not the LBA.
const VOL_ROOT_LBA_OFF: usize = 100;

const DIR_HDR_SIZE: usize = 20;
const DIR_ENTRY_SIZE: usize = 72;
const ENTRY_TYPE_OFF: usize = 8;
const ENTRY_BYTECOUNT_OFF: usize = 16;
const ENTRY_NAME_OFF: usize = 32;
const ENTRY_NAME_LEN: usize = 32;
const ENTRY_AVATAR0_OFF: usize = 68;

// Type tags vary by disc mastering tool:
//   'Cat ' (0x43617420) and '*dir' (0x2A646972) both mark directories
//   'Lvl ' (0x4C766C20) and '    ' (0x20202020) both mark files
// Flags bit 2 (0x4) is set for directories; bit 1 (0x2) for files — used as fallback.

fn be_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

pub fn default_stride(user_data_offset: u64) -> u64 {
    if user_data_offset > 0 { 2352 } else { 2048 }
}

fn read_sector<F: Read + Seek>(
    file: &mut F,
    track_offset: u64,
    stride: u64,
    user_data_offset: u64,
    lba: u64,
) -> Option<[u8; 2048]> {
    let pos = track_offset + lba * stride + user_data_offset;
    file.seek(SeekFrom::Start(pos)).ok()?;
    let mut buf = [0u8; 2048];
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn trim_null(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ── Detection ─────────────────────────────────────────────────────────────────

pub fn is_threedo_disc(path: &Path, track_offset: u64, user_data_offset: u64) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    let stride = default_stride(user_data_offset);
    let Some(s) = read_sector(&mut f, track_offset, stride, user_data_offset, 0) else { return false };
    s[0..7] == OPERA_MAGIC
}

pub fn is_threedo_reader<F: Read + Seek>(
    reader: &mut F,
    track_byte_start: u64,
    user_data_offset: u64,
    stride: u64,
) -> bool {
    let Some(s) = read_sector(reader, track_byte_start, stride, user_data_offset, 0) else { return false };
    s[0..7] == OPERA_MAGIC
}

// ── Internal entry type ───────────────────────────────────────────────────────

#[derive(PartialEq)]
enum EntryKind { File, Directory }

struct DirEntry {
    name: String,
    kind: EntryKind,
    lba: u32,
    byte_count: u32,
    #[allow(dead_code)]
    block_count: u32,
}

fn parse_entry(buf: &[u8]) -> Option<DirEntry> {
    if buf.len() < DIR_ENTRY_SIZE { return None; }
    let type_tag = be_u32(buf, ENTRY_TYPE_OFF);
    let flags = be_u32(buf, 0);
    let kind = match type_tag {
        0x43617420 | 0x2A646972 => EntryKind::Directory, // 'Cat ' or '*dir'
        0x4C766C20 | 0x20202020 => EntryKind::File,      // 'Lvl ' or '    '
        _ => {
            if flags & 4 != 0 { EntryKind::Directory }
            else if flags & 2 != 0 { EntryKind::File }
            else { return None; }
        }
    };
    let name = trim_null(&buf[ENTRY_NAME_OFF..ENTRY_NAME_OFF + ENTRY_NAME_LEN]);
    if name.is_empty() { return None; }
    let byte_count = be_u32(buf, ENTRY_BYTECOUNT_OFF);
    let block_count = be_u32(buf, 20);
    let lba = be_u32(buf, ENTRY_AVATAR0_OFF);
    Some(DirEntry { name, kind, lba, byte_count, block_count })
}

// ── Filesystem ────────────────────────────────────────────────────────────────

pub struct ThreeDOFs<F: Read + Seek> {
    file: F,
    track_offset: u64,
    user_data_offset: u64,
    stride: u64,
    root_lba: u64,
}

impl<F: Read + Seek> ThreeDOFs<F> {
    pub fn new(mut file: F, track_offset: u64, user_data_offset: u64, stride: u64) -> Result<Self, String> {
        let s = read_sector(&mut file, track_offset, stride, user_data_offset, 0)
            .ok_or_else(|| "Cannot read 3DO volume header".to_string())?;
        if s[0..7] != OPERA_MAGIC {
            return Err("Not a 3DO OperaFS disc".to_string());
        }
        let root_lba = be_u32(&s, VOL_ROOT_LBA_OFF) as u64;
        Ok(ThreeDOFs { file, track_offset, user_data_offset, stride, root_lba })
    }

    fn read_sector(&mut self, lba: u64) -> Option<[u8; 2048]> {
        read_sector(&mut self.file, self.track_offset, self.stride, self.user_data_offset, lba)
    }

    fn read_dir_at(&mut self, start_lba: u64) -> Vec<DirEntry> {
        let mut entries = Vec::new();
        let mut lba = start_lba;
        for _ in 0..256 { // safety cap to avoid infinite loops on corrupt images
            let block = match self.read_sector(lba) {
                Some(b) => b,
                None => break,
            };
            let next = be_u32(&block, 0);
            // first_free (offset 12) marks the end of valid entry data in this block.
            // Clamping iteration to it prevents reading garbage past the used portion.
            let first_free = (be_u32(&block, 12) as usize).min(2048);
            let count = be_u32(&block, 16) as usize;
            let mut off = DIR_HDR_SIZE;
            for _ in 0..count {
                if off + DIR_ENTRY_SIZE > first_free { break; }
                if let Some(e) = parse_entry(&block[off..off + DIR_ENTRY_SIZE]) {
                    entries.push(e);
                }
                off += DIR_ENTRY_SIZE;
            }
            if next == 0xFFFF_FFFF || next == 0 { break; }
            lba = next as u64;
        }
        entries
    }

    fn resolve_dir(&mut self, path: &str) -> Result<u64, String> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut lba = self.root_lba;
        for part in parts {
            let entries = self.read_dir_at(lba);
            let dir = entries.into_iter()
                .find(|e| e.kind == EntryKind::Directory && e.name == part)
                .ok_or_else(|| format!("Directory not found: {part}"))?;
            lba = dir.lba as u64;
        }
        Ok(lba)
    }

    pub fn list_directory(&mut self, dir_path: &str) -> Result<Vec<DiscEntry>, String> {
        let lba = self.resolve_dir(dir_path)?;
        let entries = self.read_dir_at(lba);
        Ok(entries.into_iter().map(|e| {
            let is_dir = e.kind == EntryKind::Directory;
            DiscEntry {
                name: e.name,
                is_dir,
                lba: e.lba,
                size: if is_dir { 0 } else { e.byte_count },
                size_bytes: e.byte_count,
                modified: String::new(),
            }
        }).collect())
    }

    pub fn extract_file(&mut self, file_path: &str, dest_path: &str) -> Result<(), String> {
        let (dir, name) = split_path(file_path);
        let dir_lba = self.resolve_dir(dir)?;
        let entries = self.read_dir_at(dir_lba);
        let entry = entries.into_iter()
            .find(|e| e.kind == EntryKind::File && e.name == name)
            .ok_or_else(|| format!("File not found: {file_path}"))?;
        let mut out = File::create(dest_path)
            .map_err(|e| format!("Cannot create output: {e}"))?;
        self.write_blocks(entry.lba as u64, entry.byte_count as u64, &mut out)
    }

    pub fn extract_directory(&mut self, dir_path: &str, dest_path: &str) -> Result<(), String> {
        let lba = self.resolve_dir(dir_path)?;
        std::fs::create_dir_all(dest_path)
            .map_err(|e| format!("Cannot create directory: {e}"))?;
        self.extract_dir_recursive(lba, Path::new(dest_path))
    }

    fn extract_dir_recursive(&mut self, dir_lba: u64, dest: &Path) -> Result<(), String> {
        let entries = self.read_dir_at(dir_lba);
        for entry in entries {
            let child = dest.join(&entry.name);
            match entry.kind {
                EntryKind::File => {
                    let mut out = File::create(&child)
                        .map_err(|e| format!("Cannot create {:?}: {e}", child))?;
                    self.write_blocks(entry.lba as u64, entry.byte_count as u64, &mut out)?;
                }
                EntryKind::Directory => {
                    std::fs::create_dir_all(&child)
                        .map_err(|e| format!("Cannot create {:?}: {e}", child))?;
                    self.extract_dir_recursive(entry.lba as u64, &child)?;
                }
            }
        }
        Ok(())
    }

    fn write_blocks(&mut self, start_lba: u64, byte_count: u64, out: &mut File) -> Result<(), String> {
        let mut remaining = byte_count;
        let mut lba = start_lba;
        while remaining > 0 {
            let block = self.read_sector(lba)
                .ok_or_else(|| format!("Cannot read block {lba}"))?;
            let n = remaining.min(2048) as usize;
            out.write_all(&block[..n])
                .map_err(|e| format!("Write error: {e}"))?;
            remaining = remaining.saturating_sub(2048);
            lba += 1;
        }
        Ok(())
    }
}

fn split_path(path: &str) -> (&str, &str) {
    let path = path.trim_start_matches('/');
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

// ── Volume label helper (used by lib.rs for display) ─────────────────────────

pub fn volume_label(path: &Path, track_offset: u64, user_data_offset: u64) -> String {
    let Ok(mut f) = File::open(path) else { return String::new() };
    let stride = default_stride(user_data_offset);
    let Some(s) = read_sector(&mut f, track_offset, stride, user_data_offset, 0) else { return String::new() };
    trim_null(&s[VOL_LABEL_OFF..VOL_LABEL_OFF + VOL_LABEL_LEN])
}
