// HFS (Hierarchical File System) reader for Apple Macintosh CD-ROM discs.
// Supports classic HFS (not HFS+). Handles both the standard MDB signature
// (0xD2D7) and the macTOPiX non-standard variant (0x4244).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use crate::DiscEntry;
use encoding_rs::{SHIFT_JIS, MACINTOSH};

const RAW_SECTOR: u64 = 2352;
const UD_PER_SECTOR: u64 = 2048;
const DEV_BLOCK: u64 = 512;       // Apple partition map block size
const HFS_ROOT_CNID: u32 = 2;    // CNID of the root directory in HFS

// ── Low-level read ────────────────────────────────────────────────────────────

fn read_ud(
    file: &mut File,
    track_offset: u64,
    user_data_offset: u64,
    ud_start: u64,
    buf: &mut [u8],
) -> Result<(), String> {
    let mut done = 0usize;
    let mut offset = ud_start;
    while done < buf.len() {
        let sector = offset / UD_PER_SECTOR;
        let ud_off = offset % UD_PER_SECTOR;
        let can = ((UD_PER_SECTOR - ud_off) as usize).min(buf.len() - done);
        let raw = track_offset + sector * RAW_SECTOR + user_data_offset + ud_off;
        file.seek(SeekFrom::Start(raw)).map_err(|e| format!("Seek: {e}"))?;
        file.read_exact(&mut buf[done..done + can]).map_err(|e| format!("Read: {e}"))?;
        done += can;
        offset += can as u64;
    }
    Ok(())
}

// ── Big-endian helpers ────────────────────────────────────────────────────────

fn u16_be(b: &[u8], o: usize) -> u16 {
    ((b[o] as u16) << 8) | b[o + 1] as u16
}

fn u32_be(b: &[u8], o: usize) -> u32 {
    ((b[o] as u32) << 24)
        | ((b[o + 1] as u32) << 16)
        | ((b[o + 2] as u32) << 8)
        | b[o + 3] as u32
}

// ── Mac-specific conversions ──────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum HfsEncoding { Roman, Japanese }

fn decode_mac(bytes: &[u8], enc: HfsEncoding) -> String {
    match enc {
        HfsEncoding::Japanese => {
            let (s, _, _) = SHIFT_JIS.decode(bytes);
            s.into_owned()
        }
        HfsEncoding::Roman => {
            let (s, _, _) = MACINTOSH.decode(bytes);
            s.into_owned()
        }
    }
}

// Mac OS timestamp (seconds since 1904-01-01) → "YYYY-MM-DD HH:MM:SS"
fn mac_date(ts: u32) -> String {
    if ts == 0 {
        return String::new();
    }
    // Seconds from Mac epoch to Unix epoch
    const EPOCH_DIFF: i64 = 2_082_844_800;
    let unix = (ts as i64) - EPOCH_DIFF;
    // Rudimentary date decomposition (no external crate)
    let (neg, unix) = if unix < 0 { (true, -unix) } else { (false, unix) };
    let sec = unix % 60;
    let min = (unix / 60) % 60;
    let hour = (unix / 3600) % 24;
    let days = unix / 86400;
    // Gregorian calendar (simplified)
    let mut y = if neg { 1970 - days / 365 } else { 1970 + days / 365 } as i32;
    let mut remaining = days % 365;
    let is_leap = |yr: i32| yr % 4 == 0 && (yr % 100 != 0 || yr % 400 == 0);
    let days_in = |m: u32, yr: i32| -> u64 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => if is_leap(yr) { 29 } else { 28 },
            _ => 0,
        }
    };
    let mut month = 1u32;
    loop {
        let d = days_in(month, y);
        if remaining < d as i64 { break; }
        remaining -= d as i64;
        month += 1;
        if month > 12 { month = 1; y += 1; }
    }
    let day = remaining + 1;
    format!("{y:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}")
}

// ── B-tree helpers ────────────────────────────────────────────────────────────

// Offset table: entry i is at bytes [510-i*2 .. 512-i*2] (big-endian u16).
fn bt_entry(node: &[u8], i: usize) -> usize {
    u16_be(node, 510 - i * 2) as usize
}

// Record i spans [bt_entry(i) .. bt_entry(i+1)]
fn bt_record(node: &[u8], i: usize) -> &[u8] {
    let s = bt_entry(node, i);
    let e = bt_entry(node, i + 1);
    &node[s..e.min(node.len())]
}

// ── HFS filesystem struct ─────────────────────────────────────────────────────

pub struct HfsFs {
    file: File,
    track_offset: u64,
    user_data_offset: u64,
    encoding: HfsEncoding,

    // HFS partition geometry
    part_ud: u64,       // partition start, in user-data bytes from disc start
    al_bl_st: u64,      // MDB.alBlSt: partition-relative device blocks to first alloc block
    al_blk_sz: u64,     // MDB.alBlkSiz: bytes per allocation block
    dev_per_ab: u64,    // device blocks (512 bytes) per allocation block

    // Catalog B-tree
    cat_ext: Vec<(u64, u64)>, // [(abs_alloc_block_start, count)]
    node_sz: u64,
    first_leaf: u32,

    pub volume_name: String,
}

impl HfsFs {
    pub fn new(
        mut file: File,
        track_offset: u64,
        user_data_offset: u64,
    ) -> Result<Self, String> {
        let mut buf = [0u8; 512];

        // ── 1. Apple DDR at user-data byte 0 ─────────────────────────────────
        read_ud(&mut file, track_offset, user_data_offset, 0, &mut buf)?;
        if &buf[0..2] != b"ER" {
            return Err("No Apple DDR (no 'ER' signature at sector 0)".to_string());
        }
        let dev_blk_sz = u16_be(&buf, 2) as u64;
        let dev_blk_sz = if dev_blk_sz == 0 { DEV_BLOCK } else { dev_blk_sz };

        // ── 2. Scan partition map for Apple_HFS ───────────────────────────────
        let mut part_ud: Option<u64> = None;
        let _map_entries = u32_be(&buf, 4) as usize;
        // Scan up to 16 partition map blocks starting at device block 1
        for pm_blk in 1u64..=16 {
            read_ud(
                &mut file, track_offset, user_data_offset,
                pm_blk * dev_blk_sz, &mut buf,
            )?;
            if &buf[0..2] != b"PM" {
                break;
            }
            let p_start = u32_be(&buf, 8) as u64;
            // Partition type is a C-string at offset 48 (32 bytes)
            let p_type = std::str::from_utf8(&buf[48..80])
                .unwrap_or("")
                .trim_end_matches('\0');
            if p_type == "Apple_HFS" {
                part_ud = Some(p_start * dev_blk_sz);
                break;
            }
        }
        let part_ud = part_ud.ok_or("No Apple_HFS partition found")?;

        // ── 3. HFS MDB (at partition start + 1024 = volume block 2) ──────────
        read_ud(&mut file, track_offset, user_data_offset, part_ud + 1024, &mut buf)?;
        let sig = u16_be(&buf, 0);
        if sig != 0xD2D7 && sig != 0x4244 {
            return Err(format!("Unknown HFS MDB signature: {sig:#06x}"));
        }

        let al_bl_st   = u16_be(&buf, 0x1C) as u64;
        let al_blk_sz  = u32_be(&buf, 0x14) as u64;
        let dev_per_ab = al_blk_sz / dev_blk_sz;

        // Script code: drFndrInfo[3] high byte (offset 0x5C + 12 = 0x68, then byte 0x6A = high)
        // smJapanese=1, smRoman=0 (default)
        let script_code = buf[0x6A];
        let encoding = if script_code == 1 { HfsEncoding::Japanese } else { HfsEncoding::Roman };

        let vn_len = buf[0x24] as usize;
        let volume_name = decode_mac(&buf[0x25..0x25 + vn_len.min(31)], encoding);

        // Catalog file extents at MDB offset 0x96 (3 × 4 bytes: u16 start + u16 count)
        let mut cat_ext: Vec<(u64, u64)> = Vec::new();
        for i in 0..3usize {
            let start = u16_be(&buf, 0x96 + i * 4) as u64;
            let count = u16_be(&buf, 0x98 + i * 4) as u64;
            if count > 0 {
                cat_ext.push((start, count));
            }
        }
        if cat_ext.is_empty() {
            return Err("Catalog file has no extents".to_string());
        }

        // ── 4. B-tree header node (node 0 of catalog file) ───────────────────
        let cat_node0_ud = Self::alloc_to_ud(part_ud, al_bl_st, dev_per_ab, cat_ext[0].0);
        let mut node0 = [0u8; 512];
        read_ud(&mut file, track_offset, user_data_offset, cat_node0_ud, &mut node0)?;

        let num_recs = u16_be(&node0, 10);
        if num_recs == 0 {
            return Err("B-tree header node has no records".to_string());
        }
        // Record 0 = B-tree header record
        let hdr = bt_record(&node0, 0);
        if hdr.len() < 22 {
            return Err("B-tree header record too short".to_string());
        }
        let first_leaf = u32_be(hdr, 10);
        let node_sz    = u16_be(hdr, 18) as u64;
        let node_sz    = if node_sz == 0 { 512 } else { node_sz };

        Ok(HfsFs {
            file,
            track_offset,
            user_data_offset,
            encoding,
            part_ud,
            al_bl_st,
            al_blk_sz,
            dev_per_ab,
            cat_ext,
            node_sz,
            first_leaf,
            volume_name,
        })
    }

    // ── Geometry helpers ──────────────────────────────────────────────────────

    fn alloc_to_ud(part_ud: u64, al_bl_st: u64, dev_per_ab: u64, abs_ab: u64) -> u64 {
        part_ud + (al_bl_st + abs_ab * dev_per_ab) * DEV_BLOCK
    }

    fn ab_to_ud(&self, abs_ab: u64) -> u64 {
        Self::alloc_to_ud(self.part_ud, self.al_bl_st, self.dev_per_ab, abs_ab)
    }

    // Map byte offset within the catalog file to user-data byte position.
    fn catalog_ud(&self, file_offset: u64) -> u64 {
        let ab_idx = file_offset / self.al_blk_sz;
        let within = file_offset % self.al_blk_sz;
        let mut local = 0u64;
        for &(start, count) in &self.cat_ext {
            if ab_idx < local + count {
                return self.ab_to_ud(start + ab_idx - local) + within;
            }
            local += count;
        }
        0 // should not reach
    }

    // ── B-tree node read ──────────────────────────────────────────────────────

    fn read_node(&mut self, node_num: u32) -> Result<Vec<u8>, String> {
        let file_off = node_num as u64 * self.node_sz;
        let ud = self.catalog_ud(file_off);
        let mut buf = vec![0u8; self.node_sz as usize];
        read_ud(&mut self.file, self.track_offset, self.user_data_offset, ud, &mut buf)?;
        Ok(buf)
    }

    // ── Catalog scanning ──────────────────────────────────────────────────────

    // Walk leaf node chain looking for records with parentID == dir_cnid.
    fn scan_dir(&mut self, dir_cnid: u32) -> Result<Vec<DiscEntry>, String> {
        let mut result: Vec<DiscEntry> = Vec::new();
        let mut node_num = self.first_leaf;

        loop {
            if node_num == 0 {
                break;
            }
            let node = self.read_node(node_num)?;
            let flink = u32_be(&node, 0);
            let num_recs = u16_be(&node, 10) as usize;

            let mut stop = false;
            for i in 0..num_recs {
                let rec = bt_record(&node, i);
                if rec.len() < 8 {
                    continue;
                }
                // Catalog key: [0]=keyLen [1]=reserved [2-5]=parentID [6]=nameLen [7+]=name
                let key_len   = rec[0] as usize;
                let parent_id = u32_be(rec, 2);
                if parent_id < dir_cnid {
                    continue;
                }
                if parent_id > dir_cnid {
                    stop = true;
                    break;
                }
                let name_len = rec[6] as usize;
                if rec.len() < 7 + name_len {
                    continue;
                }
                let name = decode_mac(&rec[7..7 + name_len], self.encoding);

                // Data starts after key, padded to even offset
                let key_total = 1 + key_len + (1 + key_len) % 2;
                if rec.len() <= key_total + 2 {
                    continue;
                }
                let data = &rec[key_total..];
                if data.is_empty() { continue; }
                let rec_type = data[0];

                let entry = match rec_type {
                    // Directory record
                    1 if data.len() >= 18 => {
                        let cnid     = u32_be(data, 6);
                        let mod_date = u32_be(data, 14);
                        DiscEntry {
                            name,
                            is_dir: true,
                            lba: cnid,
                            size: 0,
                            size_bytes: 0,
                            modified: mac_date(mod_date),
                        }
                    }
                    // File record
                    2 if data.len() >= 52 => {
                        let cnid     = u32_be(data, 20);
                        let lgl_len  = u32_be(data, 26);
                        let mod_date = u32_be(data, 48);
                        DiscEntry {
                            name,
                            is_dir: false,
                            lba: cnid,
                            size: lgl_len,
                            size_bytes: lgl_len,
                            modified: mac_date(mod_date),
                        }
                    }
                    _ => continue,
                };
                result.push(entry);
            }

            if stop {
                break;
            }
            node_num = flink;
        }

        Ok(result)
    }

    // Find CNID of a direct child of parent_cnid with the given name.
    fn find_cnid(&mut self, parent_cnid: u32, child_name: &str) -> Result<u32, String> {
        let mut node_num = self.first_leaf;
        loop {
            if node_num == 0 {
                break;
            }
            let node = self.read_node(node_num)?;
            let flink = u32_be(&node, 0);
            let num_recs = u16_be(&node, 10) as usize;

            let mut stop = false;
            for i in 0..num_recs {
                let rec = bt_record(&node, i);
                if rec.len() < 8 {
                    continue;
                }
                let key_len   = rec[0] as usize;
                let parent_id = u32_be(rec, 2);
                if parent_id < parent_cnid { continue; }
                if parent_id > parent_cnid { stop = true; break; }
                let name_len = rec[6] as usize;
                if rec.len() < 7 + name_len { continue; }
                let name = decode_mac(&rec[7..7 + name_len], self.encoding);
                if !name.eq_ignore_ascii_case(child_name) { continue; }

                let key_total = 1 + key_len + (1 + key_len) % 2;
                if rec.len() <= key_total { continue; }
                let data = &rec[key_total..];
                if data.is_empty() { continue; }
                match data[0] {
                    1 if data.len() >= 10 => return Ok(u32_be(data, 6)),
                    2 if data.len() >= 24 => return Ok(u32_be(data, 20)),
                    _ => {}
                }
            }
            if stop { break; }
            node_num = flink;
        }
        Err(format!("Not found: '{child_name}'"))
    }

    // Resolve a slash-separated path to the CNID of the target directory.
    fn resolve(&mut self, path: &str) -> Result<u32, String> {
        let mut cnid = HFS_ROOT_CNID;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cnid = self.find_cnid(cnid, seg)?;
        }
        Ok(cnid)
    }

    // ── Public API ────────────────────────────────────────────────────────────

    pub fn list_directory(&mut self, dir_path: &str) -> Result<Vec<DiscEntry>, String> {
        let cnid = self.resolve(dir_path)?;
        self.scan_dir(cnid)
    }

    pub fn extract_file(&mut self, file_path: &str, dest_path: &str) -> Result<(), String> {
        let (dir, fname) = match file_path.rfind('/') {
            Some(i) => (&file_path[..i], &file_path[i + 1..]),
            None    => ("", file_path),
        };
        let dir_cnid = if dir.is_empty() || dir == "/" {
            HFS_ROOT_CNID
        } else {
            self.resolve(dir)?
        };

        // Find the file record to get data fork extents
        let extents = self.find_file_extents(dir_cnid, fname)?;

        let mut dest = File::create(dest_path)
            .map_err(|e| format!("Cannot create {dest_path}: {e}"))?;
        let (lgl_len, exts) = extents;
        let mut remaining = lgl_len as u64;

        for (start_ab, count) in exts {
            for ab in 0..count {
                if remaining == 0 { break; }
                let ud = self.ab_to_ud(start_ab + ab);
                let to_write = remaining.min(self.al_blk_sz) as usize;
                let mut buf = vec![0u8; self.al_blk_sz as usize];
                read_ud(&mut self.file, self.track_offset, self.user_data_offset, ud, &mut buf)?;
                dest.write_all(&buf[..to_write])
                    .map_err(|e| format!("Write error: {e}"))?;
                remaining -= to_write as u64;
            }
            if remaining == 0 { break; }
        }
        Ok(())
    }

    pub fn extract_directory(&mut self, dir_path: &str, dest_path: &str) -> Result<(), String> {
        let entries = self.list_directory(dir_path)?;
        std::fs::create_dir_all(dest_path)
            .map_err(|e| format!("Cannot create dir: {e}"))?;
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

    // ── Internal: find file extents ───────────────────────────────────────────

    // Returns (logical_length, vec of (abs_alloc_block_start, count))
    fn find_file_extents(
        &mut self,
        parent_cnid: u32,
        name: &str,
    ) -> Result<(u32, Vec<(u64, u64)>), String> {
        let mut node_num = self.first_leaf;
        loop {
            if node_num == 0 { break; }
            let node = self.read_node(node_num)?;
            let flink = u32_be(&node, 0);
            let num_recs = u16_be(&node, 10) as usize;
            let mut stop = false;
            for i in 0..num_recs {
                let rec = bt_record(&node, i);
                if rec.len() < 8 { continue; }
                let key_len   = rec[0] as usize;
                let parent_id = u32_be(rec, 2);
                if parent_id < parent_cnid { continue; }
                if parent_id > parent_cnid { stop = true; break; }
                let name_len = rec[6] as usize;
                if rec.len() < 7 + name_len { continue; }
                if !decode_mac(&rec[7..7 + name_len], self.encoding).eq_ignore_ascii_case(name) { continue; }

                let key_total = 1 + key_len + (1 + key_len) % 2;
                if rec.len() <= key_total { continue; }
                let data = &rec[key_total..];
                if data.is_empty() || data[0] != 2 || data.len() < 86 { continue; }

                let lgl_len = u32_be(data, 26);
                // Data fork extents: 3 × (u16 start, u16 count) at data offset 74
                let mut exts: Vec<(u64, u64)> = Vec::new();
                for e in 0..3usize {
                    let s = u16_be(data, 74 + e * 4) as u64;
                    let c = u16_be(data, 76 + e * 4) as u64;
                    if c > 0 { exts.push((s, c)); }
                }
                return Ok((lgl_len, exts));
            }
            if stop { break; }
            node_num = flink;
        }
        Err(format!("File not found: '{name}'"))
    }
}

// ── Detection ─────────────────────────────────────────────────────────────────

/// Returns true when the disc image at the given track parameters contains an
/// Apple Macintosh HFS filesystem (detected via DDR → APM → MDB chain).
pub fn is_hfs_disc(
    bin_path: &std::path::Path,
    track_offset: u64,
    user_data_offset: u64,
) -> bool {
    let Ok(mut f) = File::open(bin_path) else { return false };
    let mut buf = [0u8; 512];
    // 1. Apple DDR at user-data byte 0
    if read_ud(&mut f, track_offset, user_data_offset, 0, &mut buf).is_err() { return false; }
    if &buf[0..2] != b"ER" { return false; }
    let dev_blk_sz = { let d = u16_be(&buf, 2) as u64; if d == 0 { DEV_BLOCK } else { d } };
    // 2. Scan partition map for Apple_HFS
    let mut part_ud: Option<u64> = None;
    for pm_blk in 1u64..=16 {
        if read_ud(&mut f, track_offset, user_data_offset, pm_blk * dev_blk_sz, &mut buf).is_err() { break; }
        if &buf[0..2] != b"PM" { break; }
        let p_start = u32_be(&buf, 8) as u64;
        let p_type = std::str::from_utf8(&buf[48..80]).unwrap_or("").trim_end_matches('\0');
        if p_type == "Apple_HFS" { part_ud = Some(p_start * dev_blk_sz); break; }
    }
    let Some(part_ud) = part_ud else { return false };
    // 3. Verify HFS MDB signature at partition start + 1024
    if read_ud(&mut f, track_offset, user_data_offset, part_ud + 1024, &mut buf).is_err() { return false; }
    let sig = u16_be(&buf, 0);
    sig == 0xD2D7 || sig == 0x4244
}
