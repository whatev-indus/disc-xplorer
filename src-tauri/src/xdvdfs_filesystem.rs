// Xbox XDVDFS (Xbox DVD Filesystem) reader.
//
// Used by all Original Xbox game discs. Identified by the ASCII magic
// "MICROSOFT*XBOX*MEDIA" at the start of the volume descriptor.
//
// The XDVDFS partition begins at a fixed byte offset within the image.
// Sector numbers within the partition are 0-based and 2048 bytes each.
// The volume descriptor is always at partition sector 32 (byte +65536).
// Sectors 0-31 of the partition are reserved/unused.
//
// Common image layouts (probed automatically):
//   xiso (extracted game data):  partition at byte 0       → magic at byte 65536
//   Full DVD dump (32-sec pad):  partition at byte 65536   → magic at byte 131072
//
// Volume descriptor (sector 32 of partition, all fields little-endian):
//   bytes 0-19:     magic "MICROSOFT*XBOX*MEDIA"
//   bytes 20-23:    root directory sector (u32 LE, relative to partition)
//   bytes 24-27:    root directory table size (u32 LE, bytes)
//   bytes 28-35:    volume timestamp (FILETIME, u64 LE)
//   bytes 2028-2047: magic repeat "MICROSOFT*XBOX*MEDIA"
//
// Directory table: binary search tree (case-insensitive ASCII ordering).
// Tree root is at offset 0 within the table. Each node:
//   bytes 0-1:   left subtree DWORD offset (u16 LE; 0xFFFF = none)
//   bytes 2-3:   right subtree DWORD offset (u16 LE; 0xFFFF = none)
//   bytes 4-7:   starting sector (u32 LE, partition-relative)
//   bytes 8-11:  file size in bytes (u32 LE; for dirs: total table bytes)
//   byte  12:    attributes (0x10 = directory; 0x20 = archive/normal)
//   byte  13:    filename length N
//   bytes 14..14+N: filename (ASCII)
//   padding:     0xFF bytes to next 4-byte boundary

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use crate::DiscEntry;

const XDVDFS_MAGIC: &[u8] = b"MICROSOFT*XBOX*MEDIA";
const SECTOR_SIZE: u64 = 2048;
const VOL_DESC_SECTOR: u64 = 32;

const DE_LEFT_OFF: usize = 0;
const DE_RIGHT_OFF: usize = 2;
const DE_SECTOR_OFF: usize = 4;
const DE_SIZE_OFF: usize = 8;
const DE_ATTR_OFF: usize = 0x0C;
const DE_NAMELEN_OFF: usize = 0x0D;
const DE_NAME_OFF: usize = 0x0E;

const ATTR_DIRECTORY: u8 = 0x10;
const NO_SUBTREE: u16 = 0xFFFF;

fn le_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn le_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

// ── Detection ─────────────────────────────────────────────────────────────────

// Returns the absolute byte offset of XDVDFS partition sector 0, or None.
// Probed offsets:
//   0               — xiso (extracted game-only image)
//   32 * 2048       — some full dumps with 32-sector pad
//   64 * 2048       — full dump + 32-sector Xbox pad
//   0x30600 * 2048  — XGD1/XGD2 retail Redump dump (XDVDFS after DVD-Video zone)
//   0x02080000      — XGD3 (Xbox 360 Game Disc 3)
//   0x0FD90000      — Xbox 360 / global offset used by extract-xiso
fn probe_for_magic<F: Read + Seek>(reader: &mut F, track_offset: u64) -> Option<u64> {
    for &fs_start in &[0u64, 32 * SECTOR_SIZE, 64 * SECTOR_SIZE, 0x30600 * SECTOR_SIZE, 0x02080000, 0x0FD90000] {
        let vol_off = track_offset + fs_start + VOL_DESC_SECTOR * SECTOR_SIZE;
        if reader.seek(SeekFrom::Start(vol_off)).is_err() { continue; }
        let mut buf = [0u8; 20];
        if reader.read_exact(&mut buf).is_err() { continue; }
        if buf == *XDVDFS_MAGIC {
            return Some(track_offset + fs_start);
        }
    }
    None
}

pub fn is_xdvdfs_disc(path: &Path, track_offset: u64) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    probe_for_magic(&mut f, track_offset).is_some()
}

pub fn is_xdvdfs_reader<F: Read + Seek>(reader: &mut F, track_offset: u64) -> bool {
    probe_for_magic(reader, track_offset).is_some()
}

// ── Internal entry type ───────────────────────────────────────────────────────

struct XEntry {
    name: String,
    is_dir: bool,
    sector: u32,
    size: u32,
}

// In-order (alphabetical) traversal of the BST stored in `table`.
// `seen` tracks visited offsets to prevent exponential blowup from corrupt
// entries that point back to ancestor nodes (two recursive calls per node
// means 2^depth total calls without cycle detection).
fn traverse_inner(table: &[u8], offset: usize, results: &mut Vec<XEntry>, depth: u32, seen: &mut std::collections::HashSet<usize>) {
    if depth > 64 || offset + DE_NAME_OFF + 1 > table.len() { return; }
    if !seen.insert(offset) { return; }

    let left = le_u16(table, offset + DE_LEFT_OFF);
    let right = le_u16(table, offset + DE_RIGHT_OFF);

    if left != NO_SUBTREE {
        let lo = left as usize * 4;
        if lo < table.len() { traverse_inner(table, lo, results, depth + 1, seen); }
    }

    let name_len = table[offset + DE_NAMELEN_OFF] as usize;
    if name_len > 0 && offset + DE_NAME_OFF + name_len <= table.len() {
        let name = String::from_utf8_lossy(
            &table[offset + DE_NAME_OFF..offset + DE_NAME_OFF + name_len],
        ).into_owned();
        let sector = le_u32(table, offset + DE_SECTOR_OFF);
        let size = le_u32(table, offset + DE_SIZE_OFF);
        let is_dir = table[offset + DE_ATTR_OFF] & ATTR_DIRECTORY != 0;
        results.push(XEntry { name, is_dir, sector, size });
    }

    if right != NO_SUBTREE {
        let ro = right as usize * 4;
        if ro < table.len() { traverse_inner(table, ro, results, depth + 1, seen); }
    }
}

fn list_dir_table(table: &[u8]) -> Vec<XEntry> {
    let mut v = Vec::new();
    traverse_inner(table, 0, &mut v, 0, &mut std::collections::HashSet::new());
    v
}

// BST search (case-insensitive); falls back to searching both sides on mismatch.
fn find_node(table: &[u8], offset: usize, target: &str, depth: u32) -> Option<XEntry> {
    if depth > 64 || offset + DE_NAME_OFF + 1 > table.len() { return None; }

    let name_len = table[offset + DE_NAMELEN_OFF] as usize;
    if name_len == 0 || offset + DE_NAME_OFF + name_len > table.len() { return None; }

    let name_bytes = &table[offset + DE_NAME_OFF..offset + DE_NAME_OFF + name_len];
    let name = String::from_utf8_lossy(name_bytes);
    let left = le_u16(table, offset + DE_LEFT_OFF);
    let right = le_u16(table, offset + DE_RIGHT_OFF);

    use std::cmp::Ordering;
    match target.to_ascii_uppercase().cmp(&name.to_ascii_uppercase()) {
        Ordering::Equal => {
            let sector = le_u32(table, offset + DE_SECTOR_OFF);
            let size = le_u32(table, offset + DE_SIZE_OFF);
            let is_dir = table[offset + DE_ATTR_OFF] & ATTR_DIRECTORY != 0;
            Some(XEntry { name: name.into_owned(), is_dir, sector, size })
        }
        Ordering::Less => {
            if left != NO_SUBTREE { find_node(table, left as usize * 4, target, depth + 1) } else { None }
        }
        Ordering::Greater => {
            if right != NO_SUBTREE { find_node(table, right as usize * 4, target, depth + 1) } else { None }
        }
    }
}

// ── Filesystem ────────────────────────────────────────────────────────────────

pub struct XDVDFSFs<F: Read + Seek> {
    file: F,
    fs_start: u64,   // absolute byte offset of XDVDFS partition sector 0
    root_sector: u32,
    root_size: u32,
}

impl<F: Read + Seek> XDVDFSFs<F> {
    pub fn new(mut file: F, track_offset: u64) -> Result<Self, String> {
        let fs_start = probe_for_magic(&mut file, track_offset)
            .ok_or_else(|| "Not an XDVDFS disc".to_string())?;
        let vol_off = fs_start + VOL_DESC_SECTOR * SECTOR_SIZE;
        file.seek(SeekFrom::Start(vol_off))
            .map_err(|e| format!("Seek error: {e}"))?;
        let mut vol = [0u8; 2048];
        file.read_exact(&mut vol)
            .map_err(|e| format!("Read error: {e}"))?;
        let root_sector = le_u32(&vol, 0x14);
        let root_size = le_u32(&vol, 0x18);
        Ok(XDVDFSFs { file, fs_start, root_sector, root_size })
    }

    fn read_dir_table(&mut self, sector: u32, size: u32) -> Result<Vec<u8>, String> {
        const MAX_DIR_TABLE: u32 = 4 * 1024 * 1024;
        if size > MAX_DIR_TABLE {
            return Err(format!("Directory table size {size} exceeds 4 MiB limit"));
        }
        let read_size = ((size as u64 + SECTOR_SIZE - 1) / SECTOR_SIZE * SECTOR_SIZE) as usize;
        let offset = self.fs_start + sector as u64 * SECTOR_SIZE;
        self.file.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("Seek error: {e}"))?;
        let mut buf = vec![0u8; read_size];
        self.file.read_exact(&mut buf)
            .map_err(|e| format!("Read error: {e}"))?;
        buf.truncate(size as usize);
        Ok(buf)
    }

    fn resolve_dir(&mut self, path: &str) -> Result<(u32, u32), String> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut sector = self.root_sector;
        let mut size = self.root_size;
        for part in parts {
            let table = self.read_dir_table(sector, size)?;
            let e = find_node(&table, 0, part, 0)
                .ok_or_else(|| format!("Directory not found: {part}"))?;
            if !e.is_dir {
                return Err(format!("Not a directory: {part}"));
            }
            sector = e.sector;
            size = e.size;
        }
        Ok((sector, size))
    }

    pub fn list_directory(&mut self, dir_path: &str) -> Result<Vec<DiscEntry>, String> {
        let (sector, size) = self.resolve_dir(dir_path)?;
        let table = self.read_dir_table(sector, size)?;
        Ok(list_dir_table(&table).into_iter().map(|e| DiscEntry {
            name: e.name,
            is_dir: e.is_dir,
            lba: e.sector,
            size: if e.is_dir { 0 } else { e.size },
            size_bytes: e.size,
            modified: String::new(),
        }).collect())
    }

    pub fn extract_file(&mut self, file_path: &str, dest_path: &str) -> Result<(), String> {
        let (dir, name) = split_path(file_path);
        let (dir_sector, dir_size) = self.resolve_dir(dir)?;
        let table = self.read_dir_table(dir_sector, dir_size)?;
        let e = find_node(&table, 0, name, 0)
            .ok_or_else(|| format!("File not found: {file_path}"))?;
        if e.is_dir { return Err(format!("Not a file: {file_path}")); }
        let mut out = File::create(dest_path)
            .map_err(|e| format!("Cannot create output: {e}"))?;
        self.write_file_data(e.sector, e.size, &mut out)
    }

    pub fn extract_directory(&mut self, dir_path: &str, dest_path: &str) -> Result<(), String> {
        std::fs::create_dir_all(dest_path)
            .map_err(|e| format!("Cannot create directory: {e}"))?;
        let (sector, size) = self.resolve_dir(dir_path)?;
        self.extract_dir_recursive(sector, size, Path::new(dest_path))
    }

    fn extract_dir_recursive(&mut self, dir_sector: u32, dir_size: u32, dest: &Path) -> Result<(), String> {
        let table = self.read_dir_table(dir_sector, dir_size)?;
        for e in list_dir_table(&table) {
            let child = dest.join(&e.name);
            if e.is_dir {
                std::fs::create_dir_all(&child)
                    .map_err(|err| format!("Cannot create {:?}: {err}", child))?;
                self.extract_dir_recursive(e.sector, e.size, &child)?;
            } else {
                let mut out = File::create(&child)
                    .map_err(|err| format!("Cannot create {:?}: {err}", child))?;
                self.write_file_data(e.sector, e.size, &mut out)?;
            }
        }
        Ok(())
    }

    fn write_file_data(&mut self, sector: u32, size: u32, out: &mut File) -> Result<(), String> {
        let mut remaining = size as u64;
        let mut s = sector as u64;
        while remaining > 0 {
            let offset = self.fs_start + s * SECTOR_SIZE;
            self.file.seek(SeekFrom::Start(offset))
                .map_err(|e| format!("Seek error: {e}"))?;
            let mut buf = [0u8; 2048];
            self.file.read_exact(&mut buf)
                .map_err(|e| format!("Read error at sector {s}: {e}"))?;
            let n = remaining.min(2048) as usize;
            out.write_all(&buf[..n])
                .map_err(|e| format!("Write error: {e}"))?;
            remaining = remaining.saturating_sub(2048);
            s += 1;
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
