// Nintendo GameCube / Wii GCM disc filesystem reader.
//
// Both consoles share the same disc header format. Wii discs additionally
// carry a second magic word and store game data inside an AES-encrypted
// partition, so file extraction is only supported for GameCube discs (and
// decrypted Wii images). The detection routine identifies both disc types.
//
// Disc header (big-endian throughout, offsets from disc start):
//   0x000-0x003: Game ID (ASCII, e.g. "GALE")
//   0x004-0x005: Maker code (ASCII)
//   0x006:       Disc number
//   0x007:       Revision
//   0x018-0x01B: Wii magic (0x5D1C9EA3; 0x00000000 on GameCube)
//   0x01C-0x01F: DVD magic (0xC2339F3D; present on both)
//   0x020-0x3FF: Game title (null-terminated ASCII, up to 992 bytes)
//   0x420-0x423: Main DOL offset (u32 BE, direct byte offset)
//   0x424-0x427: FST offset (u32 BE, direct byte offset from disc start)
//   0x428-0x42B: FST size (u32 BE, bytes)
//
// File System Table (FST):
//   Entry 0: root directory (is_dir=1, data=0, size=total_entry_count)
//   Each entry is 12 bytes:
//     bytes 0-3:  flag (bit 24) | name_offset (bits 0-23)
//                  flag=0 → file, flag=1 → directory
//                  name_offset → byte offset into string table after entries
//     bytes 4-7:  file: absolute byte offset on disc | dir: parent entry index
//     bytes 8-11: file: byte size | dir: next entry index after this subtree
//   String table: immediately follows the last entry; null-terminated names.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use crate::DiscEntry;

const DVD_MAGIC: u32 = 0xC233_9F3D;
const WII_MAGIC: u32 = 0x5D1C_9EA3;

const HDR_WII_MAGIC_OFF: usize = 0x18;
const HDR_DVD_MAGIC_OFF: usize = 0x1C;
const HDR_TITLE_OFF: usize = 0x20;
const HDR_TITLE_LEN: usize = 0x3E0;
const HDR_FST_OFF: usize = 0x424;
const HDR_FST_SIZE: usize = 0x428;

const FST_ENTRY_SIZE: usize = 12;
const ATTR_DIRECTORY: u32 = 0x0100_0000;

fn be_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn trim_null(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ── Detection ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum DiscKind { GameCube, Wii }

fn probe_header<F: Read + Seek>(reader: &mut F, disc_offset: u64) -> Option<DiscKind> {
    reader.seek(SeekFrom::Start(disc_offset)).ok()?;
    let mut hdr = [0u8; 0x440];
    reader.read_exact(&mut hdr).ok()?;
    // Wii partition data headers have Wii magic at 0x18 but no DVD magic at 0x1C
    // (DVD magic only exists in the outer unencrypted disc header).
    // Accept Wii magic alone, or DVD magic + optional Wii magic for GameCube/Wii.
    let wii_magic = be_u32(&hdr, HDR_WII_MAGIC_OFF) == WII_MAGIC;
    let dvd_magic = be_u32(&hdr, HDR_DVD_MAGIC_OFF) == DVD_MAGIC;
    if wii_magic {
        Some(DiscKind::Wii)
    } else if dvd_magic {
        Some(DiscKind::GameCube)
    } else {
        None
    }
}

pub fn detect_gcm_disc(path: &Path) -> Option<DiscKind> {
    let Ok(mut f) = File::open(path) else { return None };
    // Try direct offset 0, then common stripped-header offsets.
    for &off in &[0u64, 0x8000] {
        if let Some(k) = probe_header(&mut f, off) { return Some(k); }
    }
    None
}

pub fn detect_gcm_reader<F: Read + Seek>(reader: &mut F) -> Option<DiscKind> {
    for &off in &[0u64, 0x8000] {
        if let Some(k) = probe_header(reader, off) { return Some(k); }
    }
    None
}

// ── Internal FST types ────────────────────────────────────────────────────────

#[derive(Clone)]
struct FstEntry {
    is_dir: bool,
    name_offset: u32,
    data: u32,   // file: abs byte offset on disc | dir: parent index
    size: u32,   // file: byte size | dir: next entry index (after subtree)
}

fn parse_fst(buf: &[u8]) -> Vec<FstEntry> {
    if buf.len() < FST_ENTRY_SIZE { return Vec::new(); }
    let root_size = be_u32(buf, 8) as usize;
    let num = root_size.min(buf.len() / FST_ENTRY_SIZE);
    let mut entries = Vec::with_capacity(num);
    for i in 0..num {
        let off = i * FST_ENTRY_SIZE;
        let word0 = be_u32(buf, off);
        entries.push(FstEntry {
            is_dir: (word0 & ATTR_DIRECTORY) != 0,
            name_offset: word0 & 0x00FF_FFFF,
            data: be_u32(buf, off + 4),
            size: be_u32(buf, off + 8),
        });
    }
    entries
}

fn entry_name<'a>(entries: &[FstEntry], idx: usize, str_table: &'a [u8]) -> &'a str {
    if idx == 0 { return "/"; }
    let off = entries[idx].name_offset as usize;
    if off >= str_table.len() { return ""; }
    let end = str_table[off..].iter().position(|&b| b == 0).map(|p| off + p).unwrap_or(str_table.len());
    std::str::from_utf8(&str_table[off..end]).unwrap_or("")
}

// ── Filesystem ────────────────────────────────────────────────────────────────

pub struct GcmFs<F: Read + Seek> {
    file: F,
    disc_offset: u64,
    entries: Vec<FstEntry>,
    str_table: Vec<u8>,
    #[allow(dead_code)]
    pub disc_kind: DiscKind,
    #[allow(dead_code)]
    pub game_title: String,
}

impl<F: Read + Seek> GcmFs<F> {
    pub fn new(mut file: F, disc_offset: u64) -> Result<Self, String> {
        let disc_kind = probe_header(&mut file, disc_offset)
            .ok_or_else(|| "Not a GameCube/Wii disc".to_string())?;

        let mut hdr = [0u8; 0x440];
        file.seek(SeekFrom::Start(disc_offset))
            .map_err(|e| format!("Seek error: {e}"))?;
        file.read_exact(&mut hdr)
            .map_err(|e| format!("Read error: {e}"))?;

        let game_title = trim_null(&hdr[HDR_TITLE_OFF..HDR_TITLE_OFF + HDR_TITLE_LEN]);
        let fst_byte_off_raw = be_u32(&hdr, HDR_FST_OFF) as u64;
        let fst_size_raw = be_u32(&hdr, HDR_FST_SIZE) as usize;
        // Wii partition data headers store offsets and sizes as value÷4 (>>2 convention).
        // GameCube disc headers use direct byte values.
        let (fst_byte_off, fst_size) = match disc_kind {
            DiscKind::Wii       => (fst_byte_off_raw << 2, fst_size_raw << 2),
            DiscKind::GameCube  => (fst_byte_off_raw,      fst_size_raw),
        };

        if fst_size == 0 || fst_size > 64 * 1024 * 1024 {
            return Err(format!("FST size {fst_size} out of range"));
        }

        file.seek(SeekFrom::Start(disc_offset + fst_byte_off))
            .map_err(|e| format!("FST seek error: {e}"))?;
        let mut fst_buf = vec![0u8; fst_size];
        file.read_exact(&mut fst_buf)
            .map_err(|e| format!("FST read error: {e}"))?;

        let entries = parse_fst(&fst_buf);
        if entries.is_empty() {
            return Err("Empty FST".to_string());
        }
        let str_table_start = entries.len() * FST_ENTRY_SIZE;
        let str_table = if str_table_start < fst_buf.len() {
            fst_buf[str_table_start..].to_vec()
        } else {
            Vec::new()
        };

        Ok(GcmFs { file, disc_offset, entries, str_table, disc_kind, game_title })
    }

    // Resolve a slash-separated path to an entry index. Empty/root → index 0.
    fn resolve_path(&self, path: &str) -> Result<usize, String> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut dir_idx = 0usize;
        for part in parts {
            let dir_next = self.entries[dir_idx].size as usize;
            let mut k = dir_idx + 1;
            let mut found = false;
            while k < dir_next && k < self.entries.len() {
                let name = entry_name(&self.entries, k, &self.str_table);
                if name.eq_ignore_ascii_case(part) {
                    dir_idx = k;
                    found = true;
                    break;
                }
                if self.entries[k].is_dir {
                    k = self.entries[k].size as usize;
                } else {
                    k += 1;
                }
            }
            if !found {
                return Err(format!("Not found: {part}"));
            }
        }
        Ok(dir_idx)
    }

    pub fn list_directory(&self, dir_path: &str) -> Result<Vec<DiscEntry>, String> {
        let dir_idx = self.resolve_path(dir_path)?;
        if !self.entries[dir_idx].is_dir {
            return Err(format!("Not a directory: {dir_path}"));
        }
        let dir_next = self.entries[dir_idx].size as usize;
        let mut results = Vec::new();
        let mut k = dir_idx + 1;
        while k < dir_next && k < self.entries.len() {
            let e = &self.entries[k];
            let name = entry_name(&self.entries, k, &self.str_table).to_string();
            if e.is_dir {
                results.push(DiscEntry {
                    name, is_dir: true, lba: 0, size: 0, size_bytes: 0,
                    modified: String::new(),
                });
                k = e.size as usize;
            } else {
                let lba = e.data / 2048;
                results.push(DiscEntry {
                    name, is_dir: false,
                    lba,
                    size: e.size,
                    size_bytes: e.size,
                    modified: String::new(),
                });
                k += 1;
            }
        }
        Ok(results)
    }

    pub fn extract_file(&mut self, file_path: &str, dest_path: &str) -> Result<(), String> {
        let (dir, name) = split_path(file_path);
        let dir_idx = self.resolve_path(dir)?;
        let dir_next = self.entries[dir_idx].size as usize;
        let mut k = dir_idx + 1;
        let entry = loop {
            if k >= dir_next || k >= self.entries.len() {
                return Err(format!("File not found: {file_path}"));
            }
            let e = &self.entries[k];
            if !e.is_dir && entry_name(&self.entries, k, &self.str_table).eq_ignore_ascii_case(name) {
                break e.clone();
            }
            if e.is_dir { k = e.size as usize; } else { k += 1; }
        };
        let mut out = File::create(dest_path)
            .map_err(|e| format!("Cannot create output: {e}"))?;
        self.write_file_data(entry.data as u64, entry.size as u64, &mut out)
    }

    pub fn extract_directory(&mut self, dir_path: &str, dest_path: &str) -> Result<(), String> {
        let dir_idx = self.resolve_path(dir_path)?;
        std::fs::create_dir_all(dest_path)
            .map_err(|e| format!("Cannot create directory: {e}"))?;
        self.extract_dir_recursive(dir_idx, Path::new(dest_path))
    }

    fn extract_dir_recursive(&mut self, dir_idx: usize, dest: &Path) -> Result<(), String> {
        let dir_next = self.entries[dir_idx].size as usize;
        let mut k = dir_idx + 1;
        while k < dir_next && k < self.entries.len() {
            let e = self.entries[k].clone();
            let name = entry_name(&self.entries, k, &self.str_table).to_string();
            let child = dest.join(&name);
            if e.is_dir {
                std::fs::create_dir_all(&child)
                    .map_err(|err| format!("Cannot create {:?}: {err}", child))?;
                self.extract_dir_recursive(k, &child)?;
                k = e.size as usize;
            } else {
                let mut out = File::create(&child)
                    .map_err(|err| format!("Cannot create {:?}: {err}", child))?;
                self.write_file_data(e.data as u64, e.size as u64, &mut out)?;
                k += 1;
            }
        }
        Ok(())
    }

    fn write_file_data(&mut self, disc_byte_off: u64, size: u64, out: &mut File) -> Result<(), String> {
        self.file.seek(SeekFrom::Start(self.disc_offset + disc_byte_off))
            .map_err(|e| format!("Seek error: {e}"))?;
        let mut remaining = size;
        let mut buf = [0u8; 65536];
        while remaining > 0 {
            let n = remaining.min(buf.len() as u64) as usize;
            self.file.read_exact(&mut buf[..n])
                .map_err(|e| format!("Read error: {e}"))?;
            out.write_all(&buf[..n])
                .map_err(|e| format!("Write error: {e}"))?;
            remaining -= n as u64;
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
