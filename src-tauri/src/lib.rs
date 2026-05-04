use flac_bound::FlacEncoder;
use mp3lame_encoder::{Builder as Mp3Builder, DualPcm, FlushNoGap};
use iso9660::{ISO9660, ISO9660Reader, ISODirectory, DirectoryEntry};
use serde::Serialize;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use tauri::Manager;
use chd::Chd;
use chd::read::ChdReader;

mod cdi_filesystem;
mod gcm_filesystem;
mod hfs_filesystem;
mod pce_filesystem;
mod threedo_filesystem;
mod udf_filesystem;
mod xdvdfs_filesystem;

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("Cannot create dir {:?}: {e}", dst))?;
    for entry in fs::read_dir(src).map_err(|e| format!("Cannot read dir {:?}: {e}", src))? {
        let entry = entry.map_err(|e| format!("Read error: {e}"))?;
        let child_dst = dst.join(entry.file_name());
        if entry.metadata().map(|m| m.is_dir()).unwrap_or(false) {
            copy_dir_recursive(&entry.path(), &child_dst)?;
        } else {
            fs::copy(entry.path(), &child_dst).map_err(|e| format!("Copy error: {e}"))?;
        }
    }
    Ok(())
}

fn unix_secs_to_string(secs: u64) -> String {
    // Gregorian calendar computation; accurate for dates 1970–2099.
    let s = secs % 60;
    let mins = secs / 60;
    let m = mins % 60;
    let hours = mins / 60;
    let h = hours % 24;
    let mut days = hours / 24; // days since 1970-01-01
    let mut year = 1970u64;
    loop {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let dy = if leap { 366 } else { 365 };
        if days < dy { break; }
        days -= dy;
        year += 1;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days: [u64; 12] = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1u64;
    for &md in &month_days {
        if days < md { break; }
        days -= md;
        month += 1;
    }
    format!("{year}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}", day = days + 1)
}

// ── BIN/CUE support ──────────────────────────────────────────────────────────

const RAW_SECTOR_SIZE: u64 = 2352;

struct TrackFile {
    file: File,
    track_offset: u64,
    user_data_offset: u64,
    stride: u64,       // bytes per sector (2048, 2336, or 2352)
    lba_offset: u64,   // for single-BIN legacy mode
    start_lba: u64,    // absolute disc LBA of first sector (for multi-BIN dispatch)
    sector_count: u64, // 0 = unknown / unlimited
}

pub struct MultiTrackBinReader {
    tracks: Vec<TrackFile>,
    root_idx: usize,
    multi_bin: bool,
}

impl MultiTrackBinReader {
    fn single(file: File, track_offset: u64, user_data_offset: u64, stride: u64, lba_offset: u64) -> Self {
        MultiTrackBinReader {
            tracks: vec![TrackFile {
                file, track_offset, user_data_offset, stride, lba_offset,
                start_lba: lba_offset, sector_count: 0,
            }],
            root_idx: 0,
            multi_bin: false,
        }
    }
}

impl ISO9660Reader for MultiTrackBinReader {
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> io::Result<usize> {
        if !self.multi_bin {
            // Single-BIN: use lba_offset for multisession compat (same as old BinCueReader).
            let t = &mut self.tracks[self.root_idx];
            let adjusted = if lba >= t.lba_offset { lba - t.lba_offset } else { lba };
            let pos = t.track_offset + adjusted * t.stride + t.user_data_offset;
            t.file.seek(SeekFrom::Start(pos))?;
            return t.file.read(buf);
        }
        // Multi-BIN: dispatch by absolute LBA.
        // LBA < 32 (PVD + early structures) is read track-relatively from the root track.
        let (idx, adjusted) = if lba < 32 {
            (self.root_idx, lba)
        } else {
            self.tracks.iter().enumerate()
                .find(|(_, t)| lba >= t.start_lba
                    && (t.sector_count == 0 || lba < t.start_lba + t.sector_count))
                .map(|(i, t)| (i, lba - t.start_lba))
                .unwrap_or((self.root_idx, lba))
        };
        let t = &mut self.tracks[idx];
        let pos = t.track_offset + adjusted * t.stride + t.user_data_offset;
        t.file.seek(SeekFrom::Start(pos))?;
        t.file.read(buf)
    }
}

struct DataTrack {
    bin_path: PathBuf,
    track_offset: u64,
    user_data_offset: u64,
    stride: u64,
    lba_offset: u64,
    descramble: bool,
    sector_count: u64,
}

// Read the absolute disc LBA encoded in the MODE1/MODE2 sector header at
// `byte_offset` within the file.  Returns 0 if the sync pattern is absent.
fn sector_lba_at(path: &Path, byte_offset: u64) -> u64 {
    let Ok(mut f) = File::open(path) else { return 0 };
    let mut hdr = [0u8; 15];
    if f.seek(SeekFrom::Start(byte_offset)).is_err() { return 0 }
    if f.read_exact(&mut hdr).is_err() { return 0 }
    const SYNC: [u8; 12] = [0x00,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0x00];
    if hdr[0..12] != SYNC { return 0 }
    fn bcd(b: u8) -> u64 { (b >> 4) as u64 * 10 + (b & 0x0F) as u64 }
    let abs_lba = (bcd(hdr[12]) * 60 + bcd(hdr[13])) * 75 + bcd(hdr[14]);
    // CD physical MSF is offset by 150 sectors (2-second pregap) from LBA 0
    abs_lba.saturating_sub(150)
}

fn detect_filesystems_in_bin(bin_path: &Path, track_offset: u64, user_data_offset: u64, lba_offset: u64, descramble: bool) -> Vec<String> {
    if cdi_filesystem::is_cdi_disc(bin_path, track_offset, user_data_offset, lba_offset, descramble) {
        return vec!["CD-i".to_string()];
    }
    if pce_filesystem::is_pce_disc(bin_path, track_offset, user_data_offset) {
        return vec!["PC Engine CD-ROM".to_string()];
    }
    if threedo_filesystem::is_threedo_disc(bin_path, track_offset, user_data_offset) {
        return vec!["3DO OperaFS".to_string()];
    }
    if user_data_offset == 0 {
        if let Some(kind) = gcm_filesystem::detect_gcm_disc(bin_path) {
            return vec![gcm_kind_label(kind)];
        }
    }

    let mut result: Vec<String> = Vec::new();

    // XDVDFS is added first; fall through to also detect ISO 9660 so that
    // full Xbox DVD dumps show both the game partition and the DVD-Video zone.
    if user_data_offset == 0 && xdvdfs_filesystem::is_xdvdfs_disc(bin_path, track_offset) {
        result.push("XDVDFS".to_string());
    }

    let has_hfs = hfs_filesystem::is_hfs_disc(bin_path, track_offset, user_data_offset);
    if has_hfs {
        result.push("HFS".to_string());
    }

    if udf_filesystem::is_udf_disc(bin_path, track_offset, user_data_offset) {
        let version = File::open(bin_path).ok()
            .and_then(|f| udf_filesystem::UdfFs::new(f, track_offset, user_data_offset).ok())
            .map(|u| u.udf_version.clone())
            .unwrap_or_else(|| "UDF".to_string());
        result.push(version);
        return result;
    }

    // Probe for ISO 9660 by verifying the PVD signature at LBA 16.
    // This runs even when HFS was found, to detect Mac/PC hybrid discs.
    let stride = if user_data_offset > 0 { RAW_SECTOR_SIZE } else { 2048 };
    if let Ok(mut f) = File::open(bin_path) {
        let adj16 = if 16u64 >= lba_offset { 16 - lba_offset } else { 16 };
        let pvd_pos = track_offset + adj16 * stride + user_data_offset;
        let mut buf = [0u8; 2048];
        if f.seek(SeekFrom::Start(pvd_pos)).is_ok() && f.read_exact(&mut buf).is_ok()
            && &buf[1..6] == b"CD001"
        {
            result.push("ISO 9660".to_string());
            for lba in 17u64..32 {
                let adjusted = if lba >= lba_offset { lba - lba_offset } else { lba };
                let pos = track_offset + adjusted * stride + user_data_offset;
                if f.seek(SeekFrom::Start(pos)).is_err() { break; }
                let mut buf2 = [0u8; 2048];
                if f.read_exact(&mut buf2).is_err() { break; }
                match buf2[0] {
                    0xFF => break,
                    0x02 => {
                        let esc = &buf2[88..120];
                        if esc.starts_with(b"%/@") || esc.starts_with(b"%/C") || esc.starts_with(b"%/E") {
                            result.push("Joliet".to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if result.is_empty() {
        result.push("ISO 9660".to_string());
    }
    result
}

// Returns the user_data_offset if the file uses raw 2352-byte sectors (sync
// header detected), or None for standard 2048-byte logical sector images.
fn detect_raw_sector_offset(path: &Path) -> Option<u64> {
    let Ok(mut f) = File::open(path) else { return None };
    let mut buf = [0u8; 16];
    if f.read_exact(&mut buf).is_err() { return None }
    const SYNC: [u8; 12] = [0x00,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0x00];
    if buf[0..12] != SYNC { return None }
    Some(if buf[15] == 2 { 24 } else { 16 })
}

// Probe bytes at `offset` in the file for the CD sync pattern.
// Returns (sector_size, user_data_offset): (2352, 16|24) for raw sectors,
// (2048, 0) for logical 2048-byte sectors or unrecognised data.
fn detect_sector_format_at(path: &Path, offset: u64) -> (u64, u64) {
    let Ok(mut f) = File::open(path) else { return (2048, 0) };
    if f.seek(SeekFrom::Start(offset)).is_err() { return (2048, 0) }
    let mut buf = [0u8; 16];
    if f.read_exact(&mut buf).is_err() { return (2048, 0) }
    const SYNC: [u8; 12] = [0x00,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0x00];
    if buf[..12] == SYNC { (2352, if buf[15] == 2 { 24 } else { 16 }) } else { (2048, 0) }
}

fn detect_filesystems_raw(path: &Path) -> Vec<String> {
    let user_data_offset = detect_raw_sector_offset(path).unwrap_or(0);
    let sector_size = if user_data_offset > 0 { RAW_SECTOR_SIZE } else { 2048 };

    if pce_filesystem::is_pce_disc(path, 0, user_data_offset) {
        return vec!["PC Engine CD-ROM".to_string()];
    }
    if threedo_filesystem::is_threedo_disc(path, 0, user_data_offset) {
        return vec!["3DO OperaFS".to_string()];
    }
    if user_data_offset == 0 {
        if let Some(kind) = gcm_filesystem::detect_gcm_disc(path) {
            return vec![gcm_kind_label(kind)];
        }
    }

    let mut result: Vec<String> = Vec::new();

    // XDVDFS is added first; fall through to also detect ISO 9660 so that
    // full Xbox DVD dumps show both the game partition and the DVD-Video zone.
    if user_data_offset == 0 && xdvdfs_filesystem::is_xdvdfs_disc(path, 0) {
        result.push("XDVDFS".to_string());
    }

    let has_hfs = hfs_filesystem::is_hfs_disc(path, 0, user_data_offset);
    if has_hfs {
        result.push("HFS".to_string());
    }

    if udf_filesystem::is_udf_disc(path, 0, user_data_offset) {
        let version = File::open(path).ok()
            .and_then(|f| udf_filesystem::UdfFs::new(f, 0, user_data_offset).ok())
            .map(|u| u.udf_version.clone())
            .unwrap_or_else(|| "UDF".to_string());
        result.push(version);
        return result;
    }

    // Probe for ISO 9660 by verifying the PVD signature at LBA 16.
    // This runs even when HFS was found, to detect Mac/PC hybrid discs.
    if let Ok(mut f) = File::open(path) {
        let pvd_pos = 16 * sector_size + user_data_offset;
        let mut buf = [0u8; 2048];
        if f.seek(SeekFrom::Start(pvd_pos)).is_ok() && f.read_exact(&mut buf).is_ok()
            && &buf[1..6] == b"CD001"
        {
            result.push("ISO 9660".to_string());
            for lba in 17u64..32 {
                let pos = lba * sector_size + user_data_offset;
                if f.seek(SeekFrom::Start(pos)).is_err() { break; }
                let mut buf2 = [0u8; 2048];
                if f.read_exact(&mut buf2).is_err() { break; }
                match buf2[0] {
                    0xFF => break,
                    0x02 => {
                        let esc = &buf2[88..120];
                        if esc.starts_with(b"%/@") || esc.starts_with(b"%/C") || esc.starts_with(b"%/E") {
                            result.push("Joliet".to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if result.is_empty() {
        result.push("ISO 9660".to_string());
    }
    result
}

#[tauri::command]
fn get_disc_filesystems(image_path: String) -> Result<Vec<String>, String> {
    let path = Path::new(&image_path);
    let lower = image_path.to_lowercase();
    if lower.ends_with(".cue") || lower.ends_with(".mds") || lower.ends_with(".nrg") || lower.ends_with(".ccd") || lower.ends_with(".cdi") || lower.ends_with(".gdi") {
        let track = if lower.ends_with(".cue") { parse_cue_for_data_track(path)? }
            else if lower.ends_with(".mds") { parse_mds_for_data_track(path)? }
            else if lower.ends_with(".nrg") { parse_nrg_for_data_track(path)? }
            else if lower.ends_with(".ccd") { parse_ccd_for_data_track(path)? }
            else if lower.ends_with(".gdi") { parse_gdi_for_data_track(path)? }
            else { parse_cdi_for_data_track(path)? };
        Ok(detect_filesystems_in_bin(&track.bin_path, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble))
    } else if lower.ends_with(".chd") {
        Ok(detect_filesystems_chd(path))
    } else if lower.ends_with(".mdx") {
        Ok(detect_filesystems_mdx(path))
    } else {
        Ok(detect_filesystems_raw(path))
    }
}

fn parse_cue_for_data_track(cue_path: &Path) -> Result<DataTrack, String> {
    let text = fs::read_to_string(cue_path)
        .map_err(|e| format!("Cannot read CUE: {e}"))?;
    let cue_dir = cue_path.parent().unwrap_or(Path::new("."));

    let mut cur_bin: Option<PathBuf> = None;
    let mut cur_track_type: Option<String> = None;
    let mut cur_index00: u64 = 0;
    let mut cur_index01: Option<u64> = None;
    let mut first_data: Option<DataTrack> = None;
    let mut last_data: Option<DataTrack> = None;
    let mut audio_pregaps: Vec<(PathBuf, u64, u64)> = Vec::new();

    macro_rules! flush_audio_pregap {
        () => {
            if let (Some(ref bin), Some(ref mode), Some(idx)) = (&cur_bin, &cur_track_type, cur_index01) {
                if mode == "AUDIO" && cur_index00 < idx {
                    audio_pregaps.push((bin.clone(), cur_index00, idx));
                }
            }
        };
    }

    for line in text.lines() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();

        if upper.starts_with("FILE ") {
            flush_audio_pregap!();
            if let Some(name) = extract_quoted(trimmed) {
                cur_bin = Some(cue_dir.join(name));
            }
            cur_track_type = None;
            cur_index00 = 0;
            cur_index01 = None;
        } else if upper.starts_with("TRACK ") {
            flush_audio_pregap!();
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if let Some(mode) = parts.get(2) {
                cur_track_type = Some(mode.to_uppercase());
            }
            cur_index00 = 0;
            cur_index01 = None;
        } else if let Some(rest) = upper.strip_prefix("INDEX ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.first() == Some(&"00") {
                cur_index00 = parts.get(1).and_then(|s| msf_to_sectors(s)).unwrap_or(0);
            } else if parts.first() == Some(&"01") {
                if let Some(secs) = parts.get(1).and_then(|s| msf_to_sectors(s)) {
                    cur_index01 = Some(secs);
                }
            }
        }

        if let (Some(ref bin), Some(ref mode), Some(idx)) =
            (&cur_bin, &cur_track_type, cur_index01)
        {
            let user_data_offset = if mode.starts_with("MODE1") {
                16
            } else if mode.starts_with("MODE2") || mode.starts_with("CDI") {
                24
            } else {
                continue;
            };

            let track_offset = idx * RAW_SECTOR_SIZE;
            let lba_offset = sector_lba_at(bin, track_offset);
            let sector_count = fs::metadata(bin)
                .map(|m| m.len().saturating_sub(track_offset) / RAW_SECTOR_SIZE)
                .unwrap_or(0);
            let dt = DataTrack { bin_path: bin.clone(), track_offset, user_data_offset, stride: RAW_SECTOR_SIZE, lba_offset, descramble: false, sector_count };
            if first_data.is_none() { first_data = Some(DataTrack { bin_path: dt.bin_path.clone(), track_offset: dt.track_offset, user_data_offset: dt.user_data_offset, stride: RAW_SECTOR_SIZE, lba_offset: dt.lba_offset, descramble: false, sector_count: dt.sector_count }); }
            last_data = Some(dt);
        }
    }
    flush_audio_pregap!();

    if let Some(last) = last_data {
        // Photo CD / VCD: last data track has no PVD — filesystem is in the first track.
        if let Some(first) = first_data {
            if first.bin_path != last.bin_path && !has_pvd(&last) {
                return Ok(first);
            }
        }
        return Ok(last);
    }

    // No conventional data track — check AUDIO pregaps for scrambled CD-i (CD-i Ready format).
    for (bin, pregap_start, _end) in &audio_pregaps {
        let pregap_byte_offset = pregap_start * RAW_SECTOR_SIZE;
        if cdi_filesystem::is_cdi_ready_pregap(bin, pregap_byte_offset) {
            return Ok(DataTrack {
                bin_path: bin.clone(),
                track_offset: pregap_byte_offset,
                user_data_offset: 24,
                stride: RAW_SECTOR_SIZE,
                lba_offset: 0,
                descramble: true,
                sector_count: 0,
            });
        }
    }

    Err("No data track found in CUE sheet".to_string())
}

fn has_pvd(track: &DataTrack) -> bool {
    let Ok(mut f) = File::open(&track.bin_path) else { return false };
    let pos = track.track_offset + 16 * RAW_SECTOR_SIZE + track.user_data_offset;
    if f.seek(SeekFrom::Start(pos)).is_err() { return false }
    let mut buf = [0u8; 6];
    if f.read_exact(&mut buf).is_err() { return false }
    &buf[1..6] == b"CD001"
}

// Returns all data tracks from a CUE sheet, ordered as they appear.
fn parse_cue_all_data_tracks(cue_path: &Path) -> Result<Vec<DataTrack>, String> {
    let text = fs::read_to_string(cue_path).map_err(|e| format!("Cannot read CUE: {e}"))?;
    let cue_dir = cue_path.parent().unwrap_or(Path::new("."));

    let mut cur_bin: Option<PathBuf> = None;
    let mut cur_track_type: Option<String> = None;
    let mut cur_index01: Option<u64> = None;
    let mut all_data: Vec<DataTrack> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();

        if upper.starts_with("FILE ") {
            if let Some(name) = extract_quoted(trimmed) {
                cur_bin = Some(cue_dir.join(name));
            }
            cur_track_type = None;
            cur_index01 = None;
        } else if upper.starts_with("TRACK ") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            cur_track_type = parts.get(2).map(|s| s.to_uppercase());
            cur_index01 = None;
        } else if let Some(rest) = upper.strip_prefix("INDEX 01 ") {
            cur_index01 = msf_to_sectors(rest.trim());
        }

        if let (Some(ref bin), Some(ref mode), Some(idx)) = (&cur_bin, &cur_track_type, cur_index01) {
            let user_data_offset = if mode.starts_with("MODE1") { 16 }
                else if mode.starts_with("MODE2") || mode.starts_with("CDI") { 24 }
                else { continue; };

            let track_offset = idx * RAW_SECTOR_SIZE;
            if all_data.last().map(|d: &DataTrack| d.bin_path == *bin && d.track_offset == track_offset).unwrap_or(false) {
                continue;
            }
            let lba_offset = sector_lba_at(bin, track_offset);
            let sector_count = fs::metadata(bin)
                .map(|m| m.len().saturating_sub(track_offset) / RAW_SECTOR_SIZE)
                .unwrap_or(0);
            all_data.push(DataTrack { bin_path: bin.clone(), track_offset, user_data_offset, stride: RAW_SECTOR_SIZE, lba_offset, descramble: false, sector_count });
        }
    }

    if all_data.is_empty() { return Err("No data track found in CUE sheet".to_string()); }
    Ok(all_data)
}

fn msf_to_sectors(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 { return None; }
    let m: u64 = parts[0].parse().ok()?;
    let s2: u64 = parts[1].parse().ok()?;
    let f: u64 = parts[2].parse().ok()?;
    Some((m * 60 + s2) * 75 + f)
}

fn extract_quoted(line: &str) -> Option<&str> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(&line[start..end])
}

// ── MDX support ───────────────────────────────────────────────────────────────

// MDX (Daemon Tools v2) is a single-file format: 64-byte header + raw sector
// data + an encrypted descriptor tail.  The sector data is unencrypted, so we
// can read it directly without touching the tail.

const MDX_DATA_OFFSET: u64 = 0x40; // sector data begins here in every MDX file

fn is_mdx_file(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    let mut buf = [0u8; 17];
    f.read_exact(&mut buf).is_ok()
        && &buf[..16] == b"MEDIA DESCRIPTOR"
        && buf[16] == 0x02
}

fn mdx_sector_format(path: &Path) -> (u64, u64) {
    detect_sector_format_at(path, MDX_DATA_OFFSET)
}

fn parse_mdx_as_data_track(path: &Path) -> DataTrack {
    let (sector_size, user_data_offset) = mdx_sector_format(path);
    let lba_offset = if sector_size == 2352 { sector_lba_at(path, MDX_DATA_OFFSET) } else { 0 };
    DataTrack { bin_path: path.to_path_buf(), track_offset: MDX_DATA_OFFSET, user_data_offset, stride: RAW_SECTOR_SIZE, lba_offset, descramble: false, sector_count: 0 }
}

// ISO9660Reader for 2048-byte logical MDX sectors (the common case).
struct MdxReader { file: File }

impl ISO9660Reader for MdxReader {
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> io::Result<usize> {
        self.file.seek(SeekFrom::Start(MDX_DATA_OFFSET + lba * 2048))?;
        self.file.read(buf)
    }
}

fn open_iso_fs_mdx(path: &Path) -> Result<ISO9660<MdxReader>, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open MDX: {e}"))?;
    ISO9660::new(MdxReader { file }).map_err(|e| format!("Invalid MDX disc image: {e}"))
}

fn detect_filesystems_mdx(path: &Path) -> Vec<String> {
    let (sector_size, user_data_offset) = mdx_sector_format(path);
    if sector_size == 2352 {
        return detect_filesystems_in_bin(path, MDX_DATA_OFFSET, user_data_offset, 0, false);
    }
    // 2048-byte logical sectors — scan volume descriptors directly.
    let Ok(mut f) = File::open(path) else { return vec!["ISO 9660".to_string()] };
    let mut result = vec!["ISO 9660".to_string()];
    for lba in 17u64..32 {
        let pos = MDX_DATA_OFFSET + lba * 2048;
        if f.seek(SeekFrom::Start(pos)).is_err() { break; }
        let mut buf = [0u8; 2048];
        if f.read_exact(&mut buf).is_err() { break; }
        match buf[0] {
            0xFF => break,
            0x02 => {
                let esc = &buf[88..120];
                if esc.starts_with(b"%/@") || esc.starts_with(b"%/C") || esc.starts_with(b"%/E") {
                    result.push("Joliet".to_string());
                }
            }
            _ => {}
        }
    }
    result
}

// ── NRG support ───────────────────────────────────────────────────────────────

fn parse_nrg_for_data_track(path: &Path) -> Result<DataTrack, String> {
    let data = fs::read(path).map_err(|e| format!("Cannot read NRG: {e}"))?;
    let len = data.len();
    if len < 12 { return Err("File too small for NRG".to_string()); }

    // v1 (NERO): 8-byte footer  [u32 BE chunk_offset][NERO]
    // v2 (NER5): 12-byte footer [u64 LE chunk_offset][NER5]
    let (v2, chunk_offset) = if &data[len - 4..] == b"NER5" && len >= 12 {
        let off = u64::from_le_bytes(data[len - 12..len - 4].try_into().unwrap_or([0; 8])) as usize;
        (true, off)
    } else if &data[len - 4..] == b"NERO" && len >= 8 {
        let off = u32::from_be_bytes(data[len - 8..len - 4].try_into().unwrap_or([0; 4])) as usize;
        (false, off)
    } else {
        return Err("Not a Nero (.nrg) image".to_string());
    };

    if chunk_offset >= len { return Err("Invalid NRG chunk offset".to_string()); }

    let mut pos = chunk_offset;
    while pos + 8 <= len {
        let tag = &data[pos..pos + 4];
        let chunk_len = if v2 {
            u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap_or([0; 4])) as usize
        } else {
            u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap_or([0; 4])) as usize
        };

        if tag == b"END!" || chunk_len == 0 { break; }
        let c = &data[(pos + 8).min(len)..((pos + 8 + chunk_len).min(len))];

        // DAOI (v1) / DAOX (v2): disc-at-once info.
        // Preamble: 22-byte UPC/catalog + 2-byte header = 24 bytes.
        // Per-track entry: 12 ISRC + 2 sector_size + 1 mode + 1 pad +
        //   4-byte (DAOI) or 8-byte (DAOX) index0 + same index1 + same end.
        // mode 0x02 = AUDIO in all known Nero versions.
        if (tag == b"DAOI" || tag == b"DAOX") && c.len() >= 24 {
            let entry_size: usize = if tag == b"DAOX" { 40 } else { 28 };
            let mut tp = 24usize;
            while tp + entry_size <= c.len() {
                let mode = c[tp + 14];
                if mode != 0x02 {
                    let track_off: u64 = if tag == b"DAOX" {
                        u64::from_be_bytes(c[tp + 24..tp + 32].try_into().unwrap_or([0; 8]))
                    } else {
                        u32::from_be_bytes(c[tp + 20..tp + 24].try_into().unwrap_or([0; 4])) as u64
                    };
                    if track_off < len as u64 {
                        let (_, udo) = detect_sector_format_at(path, track_off);
                        let lba_off = if udo > 0 { sector_lba_at(path, track_off) } else { 0 };
                        return Ok(DataTrack {
                            bin_path: path.to_path_buf(),
                            track_offset: track_off,
                            user_data_offset: udo,
                            stride: RAW_SECTOR_SIZE,
                            lba_offset: lba_off,
                            descramble: false,
                            sector_count: 0,
                        });
                    }
                }
                tp += entry_size;
            }
        }

        pos += 8 + chunk_len;
    }
    Err("No data track found in NRG".to_string())
}

// ── CCD/IMG support ───────────────────────────────────────────────────────────

fn parse_ccd_for_data_track(ccd_path: &Path) -> Result<DataTrack, String> {
    fn parse_int_ccd(s: &str) -> i64 {
        let s = s.trim();
        if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            i64::from_str_radix(h, 16).unwrap_or(0)
        } else {
            s.parse().unwrap_or(0)
        }
    }

    let text = fs::read_to_string(ccd_path).map_err(|e| format!("Cannot read CCD: {e}"))?;
    let img_path = ccd_path.with_extension("img");
    if !img_path.exists() {
        return Err(format!("IMG file not found: {}", img_path.display()));
    }

    struct Entry { control: u32, plba: i64 }
    let mut entries: Vec<Entry> = Vec::new();
    let mut in_entry = false;
    let mut point = -1i32;
    let mut control = 0u32;
    let mut plba = 0i64;
    let mut has = (false, false, false); // point, control, plba

    macro_rules! flush {
        () => {
            if in_entry && has.0 && has.1 && has.2 && point >= 1 && point <= 99 {
                entries.push(Entry { control, plba });
            }
        };
    }

    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            flush!();
            in_entry = t.to_ascii_lowercase().starts_with("[entry");
            point = -1; control = 0; plba = 0; has = (false, false, false);
            continue;
        }
        if !in_entry { continue; }
        if let Some(eq) = t.find('=') {
            let val = &t[eq + 1..];
            match t[..eq].trim().to_ascii_lowercase().as_str() {
                "point"   => { point   = parse_int_ccd(val) as i32; has.0 = true; }
                "control" => { control = parse_int_ccd(val) as u32; has.1 = true; }
                "plba"    => { plba    = val.trim().parse().unwrap_or(0); has.2 = true; }
                _ => {}
            }
        }
    }
    flush!();

    for e in &entries {
        if (e.control & 0x04) != 0 && e.plba >= 0 {
            let track_offset = e.plba as u64 * RAW_SECTOR_SIZE;
            let (_, udo) = detect_sector_format_at(&img_path, track_offset);
            let udo = if udo == 0 { 16 } else { udo }; // CCD .img is always raw 2352
            let lba_offset = sector_lba_at(&img_path, track_offset);
            return Ok(DataTrack {
                bin_path: img_path,
                track_offset,
                user_data_offset: udo,
                stride: RAW_SECTOR_SIZE,
                lba_offset,
                descramble: false,
                sector_count: 0,
            });
        }
    }
    Err("No data track found in CCD".to_string())
}

// ── CDI (DiscJuggler) support ─────────────────────────────────────────────────

// Scan for the ISO9660 PVD (CD001 signature) near a computed track start and
// return Some(adjusted_track_offset) such that LBA 16 maps exactly to the PVD,
// or None if no PVD was found within MAX_SCAN sectors.
// On standard CDs the PVD is at LBA 16; on Dreamcast GD-ROM discs a 150-sector
// IP.BIN bootstrap precedes the ISO9660 area (PVD at LBA 166).
fn cdi_align_to_pvd(path: &Path, base_offset: u64, stride: u64, udo: u64) -> Option<u64> {
    const PVD_LBA: u64 = 16;
    const CD001: &[u8] = b"\x01CD001\x01";
    const MAX_SCAN: u64 = 512;

    let mut f = File::open(path).ok()?;
    let mut buf = [0u8; 8];

    for delta in 0..MAX_SCAN {
        let pos = base_offset + (PVD_LBA + delta) * stride + udo;
        if f.seek(SeekFrom::Start(pos)).is_err() { break; }
        if f.read_exact(&mut buf).is_err() { break; }
        if buf.starts_with(CD001) {
            return Some(base_offset + delta * stride);
        }
    }
    None
}

fn parse_cdi_for_data_track(path: &Path) -> Result<DataTrack, String> {
    const CDI_V2:  u32 = 0x80000004;
    // const CDI_V3:  u32 = 0x80000005;
    const CDI_V35: u32 = 0x80000006;

    let mut f = File::open(path).map_err(|e| format!("Cannot open CDI: {e}"))?;
    let file_size = f.seek(SeekFrom::End(0)).map_err(|e| format!("CDI seek: {e}"))?;
    if file_size < 8 { return Err("CDI file too short".to_string()); }

    // Footer: [version u32 LE][header_offset u32 LE] at the last 8 bytes.
    f.seek(SeekFrom::Start(file_size - 8)).map_err(|e| format!("CDI seek: {e}"))?;
    let mut b4 = [0u8; 4];
    f.read_exact(&mut b4).map_err(|e| format!("CDI read: {e}"))?;
    let version = u32::from_le_bytes(b4);
    f.read_exact(&mut b4).map_err(|e| format!("CDI read: {e}"))?;
    let header_offset = u32::from_le_bytes(b4);

    // V2=0x80000004, V3=0x80000005, V3.5=0x80000006
    if version < 0x80000004 || version > 0x80000006 {
        return Err(format!("Not a CDI image (version 0x{version:08X})"));
    }
    if header_offset == 0 { return Err("Bad CDI: zero header offset".to_string()); }

    // V3.5: descriptor occupies the last header_offset bytes.
    // V2/V3: header_offset is an absolute byte position from the start.
    let desc_start: u64 = if version == CDI_V35 {
        file_size.saturating_sub(header_offset as u64)
    } else {
        header_offset as u64
    };

    f.seek(SeekFrom::Start(desc_start)).map_err(|e| format!("CDI seek: {e}"))?;

    let mut b1 = [0u8; 1];
    let mut b2 = [0u8; 2];

    macro_rules! r1 { () => {{ f.read_exact(&mut b1).map_err(|e| format!("CDI read: {e}"))?; b1[0] }} }
    macro_rules! r2 { () => {{ f.read_exact(&mut b2).map_err(|e| format!("CDI read: {e}"))?; u16::from_le_bytes(b2) }} }
    macro_rules! r4 { () => {{ f.read_exact(&mut b4).map_err(|e| format!("CDI read: {e}"))?; u32::from_le_bytes(b4) }} }
    macro_rules! sk { ($n:expr) => {{ f.seek(SeekFrom::Current($n as i64)).map_err(|e| format!("CDI seek: {e}"))?; }} }

    // Based on cdirip source (CDI_get_sessions / CDI_get_tracks / CDI_read_track).
    let num_sessions = r2!() as u32;
    let mut cur_offset: u64 = 0;
    // Two buckets: last track whose ISO9660 PVD was confirmed, and first track
    // found without a PVD (fallback). We prefer the confirmed one.
    let mut best_with_pvd:    Option<DataTrack> = None;
    let mut first_without_pvd: Option<DataTrack> = None;

    'sessions: for _ in 0..num_sessions {
        let num_tracks = r2!() as u32;  // CDI_get_tracks

        for _ in 0..num_tracks {
            // -- CDI_read_track layout (verbatim from cdirip/cdi.c) --

            // 4-byte marker; if non-zero, 8 extra bytes follow (DJ 3.00.780+)
            let marker = r4!();
            if marker != 0 { sk!(8); }

            // Two 10-byte track start marks (validated in cdirip, we skip both)
            sk!(20);

            // 4-byte skip, then 1-byte filename length, then filename bytes
            sk!(4);
            let fn_len = r1!() as i64;
            sk!(fn_len);

            // 11 + 4 + 4 = 19 bytes undeciphered
            sk!(19);

            // 4-byte DJ4 marker; if 0x80000000, 8 extra bytes follow
            let dj4 = r4!();
            if dj4 == 0x80000000 { sk!(8); }

            sk!(2);
            let pregap       = r4!() as u64;  // pregap in sectors
            let track_length = r4!() as u64;  // data sectors only (excludes pregap)
            sk!(6);
            let track_mode   = r4!();          // 0=audio, 1=Mode1, 2=Mode2
            sk!(12);
            let start_lba    = r4!() as u64;  // absolute disc LBA of first data sector
            let total_len    = r4!() as u64;  // pregap + data (used to advance cur_offset)
            sk!(16);
            let sector_size_value = r4!();     // 0→2048, 1→2336, 2→2352

            // 29-byte trailer; non-V2 adds 5 skip + 4 read (+ 78 conditional)
            sk!(29);
            if version != CDI_V2 {
                sk!(5);
                let extra = r4!();
                if extra == 0xffffffff { sk!(78); }
            }

            let stride: u64 = match sector_size_value {
                0 => 2048,
                1 => 2336,
                _ => 2352,
            };

            if track_mode != 0 {
                let base_offset = cur_offset + pregap * stride;
                let user_data_offset = match stride {
                    2048 => 0,
                    2336 => 8,
                    _ => detect_sector_format_at(path, base_offset).1,
                };
                // Probe for the actual ISO 9660 PVD. On standard CDs the PVD is at
                // LBA 16 from base_offset. On Dreamcast GD-ROM discs a 150-sector
                // IP.BIN bootstrap precedes the ISO9660 area, so the PVD is at LBA
                // 166. Adjust track_offset so that LBA 16 always hits the PVD.
                let pvd_offset = cdi_align_to_pvd(path, base_offset, stride, user_data_offset);
                let track_offset = pvd_offset.unwrap_or(base_offset);
                let dt = DataTrack {
                    bin_path: path.to_path_buf(),
                    track_offset,
                    user_data_offset,
                    stride,
                    lba_offset: start_lba,
                    descramble: false,
                    sector_count: track_length,
                };
                if pvd_offset.is_some() {
                    best_with_pvd = Some(dt);
                    break 'sessions;  // stop: later sessions may have corrupt descriptors
                } else if first_without_pvd.is_none() {
                    first_without_pvd = Some(dt);  // keep only as a fallback
                }
            }

            cur_offset += stride * total_len;
        }

        // CDI_skip_next_session: 4+8 bytes; non-V2 adds 1 more
        sk!(12);
        if version != CDI_V2 { sk!(1); }
    }

    best_with_pvd
        .or(first_without_pvd)
        .ok_or_else(|| "No data track found in CDI image".to_string())
}

// ── GDI support ───────────────────────────────────────────────────────────────
// Format: text index file; each track in its own .bin/.raw file.
// Line 1: num_tracks. Remaining lines: <num> <start_lba> <type> <sector_size> <file> <flags>
// type 0 = audio, 4 = data. sector_size typically 2352 (raw) or 2048 (cooked).

fn parse_gdi_for_data_track(gdi_path: &Path) -> Result<DataTrack, String> {
    let text = fs::read_to_string(gdi_path).map_err(|e| format!("Cannot read GDI: {e}"))?;
    let dir = gdi_path.parent().unwrap_or(Path::new("."));

    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    lines.next().ok_or("GDI: missing track count")?; // skip count line

    // Keep best data track with a confirmed PVD (prefer highest start_lba, i.e. GD-ROM area).
    let mut best_with_pvd: Option<(u64, DataTrack)> = None;
    let mut best_without_pvd: Option<(u64, DataTrack)> = None;

    for line in lines {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 { continue; }
        let start_lba: u64 = match parts[1].parse() { Ok(v) => v, Err(_) => continue };
        let track_type: u32 = match parts[2].parse() { Ok(v) => v, Err(_) => continue };
        let sector_size: u64 = match parts[3].parse() { Ok(v) => v, Err(_) => continue };
        let filename = parts[4].trim_matches('"');

        if track_type == 0 { continue; } // audio

        let bin_path = dir.join(filename);
        if !bin_path.exists() { continue; }

        let stride = sector_size;
        let user_data_offset = if stride == 2048 { 0 } else { detect_sector_format_at(&bin_path, 0).1 };
        let sector_count = fs::metadata(&bin_path).map(|m| m.len() / stride).unwrap_or(0);

        // base_offset=0: each GDI track file starts at byte 0.
        let pvd_offset = cdi_align_to_pvd(&bin_path, 0, stride, user_data_offset);
        let track_offset = pvd_offset.unwrap_or(0);

        let dt = DataTrack { bin_path, track_offset, user_data_offset, stride, lba_offset: start_lba, descramble: false, sector_count };
        if pvd_offset.is_some() {
            if best_with_pvd.as_ref().map_or(true, |(best, _)| start_lba > *best) {
                best_with_pvd = Some((start_lba, dt));
            }
        } else if best_without_pvd.as_ref().map_or(true, |(best, _)| start_lba > *best) {
            best_without_pvd = Some((start_lba, dt));
        }
    }

    best_with_pvd.map(|(_, dt)| dt)
        .or_else(|| best_without_pvd.map(|(_, dt)| dt))
        .ok_or_else(|| "No data track found in GDI image".to_string())
}

#[tauri::command]
fn get_gdi_tracks(gdi_path: String) -> Result<Vec<TrackInfo>, String> {
    let path = Path::new(&gdi_path);
    let text = fs::read_to_string(path).map_err(|e| format!("Cannot read GDI: {e}"))?;
    let dir = path.parent().unwrap_or(Path::new("."));

    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    lines.next().ok_or("GDI: missing track count")?; // skip count line

    let mut tracks: Vec<TrackInfo> = Vec::new();
    for line in lines {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 { continue; }
        let number: u32    = match parts[0].parse() { Ok(v) => v, Err(_) => continue };
        let disc_lba: u64  = match parts[1].parse() { Ok(v) => v, Err(_) => continue };
        let track_type: u32 = match parts[2].parse() { Ok(v) => v, Err(_) => continue };
        let sector_size: u64 = match parts[3].parse() { Ok(v) => v, Err(_) => continue };
        let filename = parts[4].trim_matches('"');

        let bin_path = dir.join(filename);
        let num_sectors = fs::metadata(&bin_path).map(|m| m.len() / sector_size).unwrap_or(0);
        let is_data = track_type != 0;
        let mode = if !is_data {
            "AUDIO".to_string()
        } else if sector_size == 2048 {
            "MODE1/2048".to_string()
        } else {
            let udo = detect_sector_format_at(&bin_path, 0).1;
            if udo == 24 { "MODE2/2352".to_string() } else { "MODE1/2352".to_string() }
        };
        // GD-ROM discs: tracks with disc LBA < 45000 are the CD-DA area (session 1),
        // tracks at LBA >= 45000 are the GD-ROM high-density area (session 2).
        let session = if disc_lba < 45000 { 1u32 } else { 2 };
        // start_lba=0: each GDI track is its own file starting at byte 0.
        // open_audio_src uses start_lba * RAW_SECTOR_SIZE as the seek offset within bin_path.
        tracks.push(TrackInfo {
            number, is_data, mode,
            start_lba: 0,
            num_sectors, session,
            bin_path: bin_path.to_string_lossy().into_owned(),
        });
    }

    if tracks.is_empty() { return Err("No tracks found in GDI".to_string()); }
    Ok(tracks)
}

// ── MDS/MDF support ───────────────────────────────────────────────────────────

const MDS_SIGNATURE: &[u8] = b"MEDIA DESCRIPTOR";
const MDS_TRACK_BLOCK_SIZE: usize = 80;

fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0; 4]))
}

fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap_or([0; 8]))
}

fn parse_mds_for_data_track(mds_path: &Path) -> Result<DataTrack, String> {
    let data = fs::read(mds_path).map_err(|e| format!("Cannot read MDS: {e}"))?;
    if data.len() < 0x60 || !data.starts_with(MDS_SIGNATURE) {
        return Err("Not a valid MDS file".to_string());
    }

    let mdf_path = mds_path.with_extension("mdf");
    if !mdf_path.exists() {
        return Err(format!("MDF file not found: {}", mdf_path.display()));
    }

    // DVD medium types (0x10=DVD-ROM, 0x12=DVD-R, 0x14=DVD-RW, 0x18=DVD+R…):
    // The MDF is a flat 2048-byte-per-sector image; CD session/track parsing doesn't apply.
    let medium_type = data[0x12];
    if medium_type >= 0x10 {
        let sector_count = fs::metadata(&mdf_path).map(|m| m.len() / 2048).unwrap_or(0);
        return Ok(DataTrack {
            bin_path: mdf_path,
            track_offset: 0,
            user_data_offset: 0,
            stride: 2048,
            lba_offset: 0,
            descramble: false,
            sector_count,
        });
    }

    let session_offset = read_u32_le(&data, 0x4C) as usize;
    if session_offset + 0x18 > data.len() {
        return Err("Invalid MDS session offset".to_string());
    }

    let num_blocks = data[session_offset + 0x0A] as usize;
    let track_blocks_offset = read_u32_le(&data, session_offset + 0x14) as usize;

    for i in 0..num_blocks {
        let tb = track_blocks_offset + i * MDS_TRACK_BLOCK_SIZE;
        if tb + MDS_TRACK_BLOCK_SIZE > data.len() { break; }

        let mode_byte = data[tb];
        let point = data[tb + 4];

        if point == 0 || point > 99 { continue; }
        if mode_byte == 0x00 { continue; } // AUDIO

        let user_data_offset = if mode_byte == 0x02 || mode_byte == 0x03 || mode_byte == 0x04 {
            24u64 // MODE2
        } else {
            16u64 // MODE1
        };

        let track_offset = read_u64_le(&data, tb + 0x20);
        let lba_offset = sector_lba_at(&mdf_path, track_offset);
        return Ok(DataTrack { bin_path: mdf_path, track_offset, user_data_offset, stride: RAW_SECTOR_SIZE, lba_offset, descramble: false, sector_count: 0 });
    }

    Err("No data track found in MDS".to_string())
}

const MDS_SESSION_BLOCK_SIZE: usize = 24;

fn get_mds_track_list(mds_path: &Path) -> Result<Vec<TrackInfo>, String> {
    let data = fs::read(mds_path).map_err(|e| format!("Cannot read MDS: {e}"))?;
    if data.len() < 0x60 || !data.starts_with(MDS_SIGNATURE) {
        return Err("Not a valid MDS file".to_string());
    }

    let mdf_path = mds_path.with_extension("mdf");
    let mdf_str = mdf_path.to_string_lossy().into_owned();

    // DVD medium types: single data track, flat 2048-byte sectors.
    let medium_type = data[0x12];
    if medium_type >= 0x10 {
        let num_sectors = fs::metadata(&mdf_path).map(|m| m.len() / 2048).unwrap_or(0);
        return Ok(vec![TrackInfo {
            number: 1, is_data: true, mode: "MODE1/2048".to_string(),
            start_lba: 0, num_sectors, session: 1,
            bin_path: mdf_str,
        }]);
    }

    // Number of sessions is at 0x14 (2 bytes LE); sessions array starts at 0x4C (4 bytes LE).
    let num_sessions = {
        let n = u16::from_le_bytes([data[0x14], data[0x15]]) as usize;
        if n == 0 { 1 } else { n }
    };
    let first_session_offset = read_u32_le(&data, 0x4C) as usize;

    let total_sectors = fs::metadata(&mdf_path)
        .map(|m| m.len() / RAW_SECTOR_SIZE)
        .unwrap_or(0);

    let mut tracks: Vec<TrackInfo> = Vec::new();

    for s in 0..num_sessions {
        let sess_off = first_session_offset + s * MDS_SESSION_BLOCK_SIZE;
        if sess_off + MDS_SESSION_BLOCK_SIZE > data.len() { break; }

        let session_number = u16::from_le_bytes([data[sess_off + 6], data[sess_off + 7]]) as u32;
        let session_num = if session_number > 0 { session_number } else { (s + 1) as u32 };

        let num_blocks = data[sess_off + 0x0A] as usize;
        let track_blocks_offset = read_u32_le(&data, sess_off + 0x14) as usize;

        for i in 0..num_blocks {
            let tb = track_blocks_offset + i * MDS_TRACK_BLOCK_SIZE;
            if tb + MDS_TRACK_BLOCK_SIZE > data.len() { break; }

            let mode_byte = data[tb];
            let point = data[tb + 4];
            if point == 0 || point > 99 { continue; }

            let is_data = mode_byte != 0x00;
            let mode = match mode_byte {
                0x00 => "AUDIO".to_string(),
                0x02 | 0x03 | 0x04 => "MODE2/2352".to_string(),
                _ => "MODE1/2352".to_string(),
            };

            let msf_m = data[tb + 8] as u64;
            let msf_s = data[tb + 9] as u64;
            let msf_f = data[tb + 10] as u64;
            let start_lba = (msf_m * 60 + msf_s) * 75 + msf_f;

            let num_sectors = {
                let n = read_u32_le(&data, tb + 0x28) as u64;
                if n > 0 { n } else { total_sectors.saturating_sub(start_lba) }
            };

            tracks.push(TrackInfo {
                number: point as u32,
                is_data,
                mode,
                start_lba,
                num_sectors,
                session: session_num,
                bin_path: mdf_str.clone(),
            });
        }
    }

    tracks.sort_by_key(|t| t.number);
    Ok(tracks)
}

#[tauri::command]
fn get_mds_tracks(mds_path: String) -> Result<Vec<TrackInfo>, String> {
    get_mds_track_list(Path::new(&mds_path))
}

// ── CHD (Compressed Hunks of Data) support ─────────────────────────────────

struct ChdSectorReader {
    reader: ChdReader<BufReader<File>>,
    stride: u64,
    user_data_offset: u64,
    track_byte_start: u64,
}

impl ISO9660Reader for ChdSectorReader {
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> io::Result<usize> {
        let pos = self.track_byte_start + lba * self.stride + self.user_data_offset;
        self.reader.seek(SeekFrom::Start(pos))?;
        self.reader.read(buf)
    }
}

fn chd_stride(hunk_size: u64, unit_b: u64) -> u64 {
    if unit_b == 2448 || (unit_b == 0 && hunk_size % 2448 == 0 && hunk_size >= 2448) {
        2448
    } else if unit_b == 2352 || (unit_b == 0 && hunk_size % 2352 == 0 && hunk_size >= 2352) {
        2352
    } else {
        2048
    }
}

fn open_chd(path: &Path) -> Result<ChdSectorReader, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open CHD: {e}"))?;
    let chd = Chd::open(BufReader::new(file), None)
        .map_err(|e| format!("Cannot parse CHD: {e}"))?;

    let stride = chd_stride(
        chd.header().hunk_size() as u64,
        chd.header().unit_bytes() as u64,
    );
    let mut reader = ChdReader::new(chd);

    if stride == 2048 {
        return Ok(ChdSectorReader { reader, stride: 2048, user_data_offset: 0, track_byte_start: 0 });
    }

    // CD CHD: probe the PVD to find the data track start position and sector mode.
    // Common pregap values: 0 (no stored pregap), 4 (MAME default), 150 (2-sec pregap).
    let mut track_byte_start = 0u64;
    let mut user_data_offset = 16u64;
    'probe: for pregap in [0u64, 4, 150] {
        for udo in [16u64, 24] {
            let pvd_pos = (pregap + 16) * stride + udo;
            let mut buf = [0u8; 6];
            if reader.seek(SeekFrom::Start(pvd_pos)).is_ok()
                && reader.read_exact(&mut buf).is_ok()
                && buf[0] == 1 && &buf[1..6] == b"CD001"
            {
                track_byte_start = pregap * stride;
                user_data_offset = udo;
                break 'probe;
            }
        }
    }

    Ok(ChdSectorReader { reader, stride, user_data_offset, track_byte_start })
}

fn detect_filesystems_chd(path: &Path) -> Vec<String> {
    let mut r = match open_chd(path) {
        Ok(r) => r,
        Err(_) => return vec!["ISO 9660".to_string()],
    };

    // Probe for 3DO OperaFS (LBA 0 magic, raw CD sectors) before trying ISO 9660.
    if r.stride != 2048 {
        for pregap in [0u64, 4, 150] {
            for udo in [16u64, 24] {
                if threedo_filesystem::is_threedo_reader(&mut r.reader, pregap * r.stride, udo, r.stride) {
                    return vec!["3DO OperaFS".to_string()];
                }
            }
        }
    }

    // Probe for XDVDFS (Xbox DVD, 2048-byte sectors).
    if r.stride == 2048 && xdvdfs_filesystem::is_xdvdfs_reader(&mut r.reader, 0) {
        return vec!["XDVDFS".to_string()];
    }
    // Probe for GameCube/Wii GCM (2048-byte sectors).
    if r.stride == 2048 {
        if let Some(kind) = gcm_filesystem::detect_gcm_reader(&mut r.reader) {
            return vec![gcm_kind_label(kind)];
        }
    }

    let mut result: Vec<String> = Vec::new();
    {
        // For raw-sector CHDs (stride != 2048), open_chd may have defaulted track_byte_start=0
        // and user_data_offset=16 when no ISO PVD was found, which can produce false positives
        // against non-ISO discs (e.g. 3DO). Re-probe with all pregap+udo combinations.
        let s = r.stride;
        let combos: Vec<(u64, u64)> = if s != 2048 {
            [0u64, 4, 150].iter()
                .flat_map(|&pg| [16u64, 24].iter().map(move |&udo| (pg * s, udo)))
                .collect()
        } else {
            vec![(r.track_byte_start, r.user_data_offset)]
        };
        'iso: for (tbs, udo) in combos {
            let pvd_pos = tbs + 16 * s + udo;
            let mut buf = [0u8; 2048];
            if r.reader.seek(SeekFrom::Start(pvd_pos)).is_ok()
                && r.reader.read_exact(&mut buf).is_ok()
                && buf[0] == 1 && &buf[1..6] == b"CD001"
            {
                result.push("ISO 9660".to_string());
                for lba in 17u64..32 {
                    let pos = tbs + lba * s + udo;
                    if r.reader.seek(SeekFrom::Start(pos)).is_err() { break; }
                    let mut buf2 = [0u8; 2048];
                    if r.reader.read_exact(&mut buf2).is_err() { break; }
                    match buf2[0] {
                        0xFF => break,
                        0x02 => {
                            let esc = &buf2[88..120];
                            if esc.starts_with(b"%/@") || esc.starts_with(b"%/C") || esc.starts_with(b"%/E") {
                                result.push("Joliet".to_string());
                            }
                        }
                        _ => {}
                    }
                }
                break 'iso;
            }
        }
    }

    if result.is_empty() {
        if r.stride != 2048 {
            result.push("3DO OperaFS".to_string());
        } else {
            result.push("ISO 9660".to_string());
        }
    }
    result
}

fn open_chd_iso(path: &Path) -> Result<ISO9660<ChdSectorReader>, String> {
    let r = open_chd(path)?;
    ISO9660::new(r).map_err(|e| format!("Invalid CHD: {e}"))
}

// ── Mount disc image ──────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct MountResult {
    pub mount_point: String,
    pub device: String,
}

pub struct MountedImages(pub Mutex<Vec<String>>);

#[tauri::command]
fn mount_disc_image(
    image_path: String,
    state: tauri::State<MountedImages>,
) -> Result<MountResult, String> {
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("hdiutil")
            .args(["attach", &image_path])
            .output()
            .map_err(|e| format!("hdiutil failed: {e}"))?;

        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }

        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines().rev() {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() == 3 && !parts[2].trim().is_empty() {
                let device = parts[0].trim().to_string();
                let mount_point = parts[2].trim().to_string();
                state.0.lock().unwrap().push(device.clone());
                return Ok(MountResult { mount_point, device });
            }
        }
        Err("Could not determine mount point".to_string())
    }

    #[cfg(target_os = "windows")]
    {
        let escaped = image_path.replace('\'', "''");
        let script = format!(
            "$d = Mount-DiskImage -ImagePath '{escaped}' -PassThru; ($d | Get-Volume).DriveLetter"
        );
        let out = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .map_err(|e| format!("Mount-DiskImage failed: {e}"))?;

        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }

        let letter = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if letter.is_empty() {
            return Err("Could not determine drive letter".to_string());
        }
        let mount_point = format!("{letter}:\\");
        state.0.lock().unwrap().push(image_path.clone());
        Ok(MountResult { mount_point, device: image_path })
    }

    #[cfg(target_os = "linux")]
    {
        let lower = image_path.to_lowercase();
        let use_cdemu = lower.ends_with(".cue") || lower.ends_with(".mds") || lower.ends_with(".mdx")
            || lower.ends_with(".nrg") || lower.ends_with(".ccd")
            || lower.ends_with(".toc") || lower.ends_with(".b6t") || lower.ends_with(".bwt")
            || lower.ends_with(".c2d") || lower.ends_with(".pdi") || lower.ends_with(".gi")
            || lower.ends_with(".daa");

        if use_cdemu {
            let before = sr_devices();

            let load_out = Command::new("cdemu")
                .args(["load", "any", &image_path])
                .output()
                .map_err(|e| format!("cdemu load failed: {e}"))?;

            if !load_out.status.success() {
                return Err(String::from_utf8_lossy(&load_out.stderr).trim().to_string());
            }

            // Parse assigned slot from output: "...loaded image '...' to device N."
            let slot = String::from_utf8_lossy(&load_out.stdout)
                .split_whitespace()
                .last()
                .map(|w| w.trim_end_matches('.').to_string())
                .filter(|w| w.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or_else(|| "0".to_string());

            // Give the kernel time to expose the virtual device.
            std::thread::sleep(std::time::Duration::from_millis(1500));

            let after = sr_devices();
            let new_dev = after.into_iter()
                .find(|d| !before.contains(d))
                .ok_or_else(|| "CDemu: virtual drive did not appear as /dev/srN".to_string())?;

            // Mount via udisksctl; fall back to lsblk if auto-mounted by desktop.
            let mount_out = Command::new("udisksctl")
                .args(["mount", "-b", &new_dev])
                .output()
                .map_err(|e| format!("udisksctl mount failed: {e}"))?;

            let mount_point = if mount_out.status.success() {
                let text = String::from_utf8_lossy(&mount_out.stdout);
                text.split(" at ").nth(1).unwrap_or("").trim().trim_end_matches('.').to_string()
            } else {
                // Desktop environment may have auto-mounted it — query lsblk.
                let lsblk = Command::new("lsblk")
                    .args(["-no", "MOUNTPOINT", &new_dev])
                    .output()
                    .map_err(|e| format!("lsblk failed: {e}"))?;
                String::from_utf8_lossy(&lsblk.stdout).trim().to_string()
            };

            if mount_point.is_empty() {
                let _ = Command::new("cdemu").args(["unload", &slot]).output();
                return Err("CDemu: could not determine mount point".to_string());
            }

            let device_key = format!("cdemu:{slot}:{new_dev}");
            state.0.lock().unwrap().push(device_key.clone());
            return Ok(MountResult { mount_point, device: device_key });
        }

        let loop_out = Command::new("udisksctl")
            .args(["loop-setup", "-f", &image_path])
            .output()
            .map_err(|e| format!("udisksctl loop-setup failed: {e}"))?;

        if !loop_out.status.success() {
            return Err(String::from_utf8_lossy(&loop_out.stderr).trim().to_string());
        }

        // Output: "Mapped file /path as /dev/loop0."
        let loop_text = String::from_utf8_lossy(&loop_out.stdout);
        let loop_device = loop_text
            .split_whitespace()
            .last()
            .unwrap_or("")
            .trim_end_matches('.')
            .to_string();

        if !loop_device.starts_with("/dev/loop") {
            return Err(format!("Unexpected loop-setup output: {loop_text}"));
        }

        let mount_out = Command::new("udisksctl")
            .args(["mount", "-b", &loop_device])
            .output()
            .map_err(|e| format!("udisksctl mount failed: {e}"))?;

        if !mount_out.status.success() {
            return Err(String::from_utf8_lossy(&mount_out.stderr).trim().to_string());
        }

        // Output: "Mounted /dev/loop0 at /media/user/label."
        let mount_text = String::from_utf8_lossy(&mount_out.stdout);
        let mount_point = mount_text
            .split(" at ")
            .nth(1)
            .unwrap_or("")
            .trim()
            .trim_end_matches('.')
            .to_string();

        if mount_point.is_empty() {
            return Err("Could not determine mount point".to_string());
        }

        state.0.lock().unwrap().push(loop_device.clone());
        Ok(MountResult { mount_point, device: loop_device })
    }
}

#[cfg(target_os = "linux")]
fn sr_devices() -> Vec<String> {
    std::fs::read_dir("/dev")
        .map(|rd| {
            rd.filter_map(|e| {
                let e = e.ok()?;
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("sr") { Some(format!("/dev/{name}")) } else { None }
            }).collect()
        })
        .unwrap_or_default()
}

#[tauri::command]
fn unmount_disc_image(
    device: String,
    state: tauri::State<MountedImages>,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("hdiutil")
            .args(["detach", &device, "-quiet"])
            .output()
            .map_err(|e| format!("hdiutil detach failed: {e}"))?;

        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
    }

    #[cfg(target_os = "windows")]
    {
        let escaped = device.replace('\'', "''");
        let script = format!("Dismount-DiskImage -ImagePath '{escaped}'");
        let out = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .map_err(|e| format!("Dismount-DiskImage failed: {e}"))?;

        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
    }

    #[cfg(target_os = "linux")]
    {
        if device.starts_with("cdemu:") {
            let mut parts = device.splitn(3, ':');
            let _ = parts.next();
            let slot = parts.next().unwrap_or("0");
            let dev = parts.next().unwrap_or("");
            let _ = Command::new("udisksctl").args(["unmount", "-b", dev]).output();
            let out = Command::new("cdemu")
                .args(["unload", slot])
                .output()
                .map_err(|e| format!("cdemu unload failed: {e}"))?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
        } else {
            let _ = Command::new("udisksctl").args(["unmount", "-b", &device]).output();
            let out = Command::new("udisksctl")
                .args(["loop-delete", "-b", &device])
                .output()
                .map_err(|e| format!("udisksctl loop-delete failed: {e}"))?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
        }
    }

    state.0.lock().unwrap().retain(|d| d != &device);
    Ok(())
}

fn detach_all(devices: &[String]) {
    #[cfg(target_os = "macos")]
    for device in devices {
        let _ = Command::new("hdiutil")
            .args(["detach", device, "-quiet", "-force"])
            .output();
    }

    #[cfg(target_os = "windows")]
    for device in devices {
        let escaped = device.replace('\'', "''");
        let script = format!("Dismount-DiskImage -ImagePath '{escaped}'");
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output();
    }

    #[cfg(target_os = "linux")]
    for device in devices {
        if device.starts_with("cdemu:") {
            let parts: Vec<&str> = device.splitn(3, ':').collect();
            let slot = parts.get(1).copied().unwrap_or("0");
            let dev = parts.get(2).copied().unwrap_or("");
            let _ = Command::new("udisksctl").args(["unmount", "-b", dev]).output();
            let _ = Command::new("cdemu").args(["unload", slot]).output();
        } else {
            let _ = Command::new("udisksctl").args(["unmount", "-b", device]).output();
            let _ = Command::new("udisksctl").args(["loop-delete", "-b", device]).output();
        }
    }
}

// ── Platform ──────────────────────────────────────────────────────────────────

#[tauri::command]
fn get_platform() -> &'static str {
    if cfg!(target_os = "linux") { "linux" }
    else if cfg!(target_os = "macos") { "macos" }
    else { "windows" }
}

// ── CUE track listing ─────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct TrackInfo {
    pub number: u32,
    pub is_data: bool,
    pub mode: String,
    pub start_lba: u64,
    pub num_sectors: u64,
    pub session: u32,
    pub bin_path: String,
}

struct RawCueTrack {
    number: u32,
    mode: String,
    index00_lba: u64,
    start_lba: u64,
    bin_path: PathBuf,
    session: u32,
}

#[tauri::command]
fn get_cue_tracks(cue_path: String) -> Result<Vec<TrackInfo>, String> {
    let path = Path::new(&cue_path);
    let text = fs::read_to_string(path)
        .map_err(|e| format!("Cannot read CUE: {e}"))?;
    let cue_dir = path.parent().unwrap_or(Path::new("."));

    let mut raw: Vec<RawCueTrack> = Vec::new();
    let mut cur_session: u32 = 1;
    let mut cur_bin: Option<PathBuf> = None;
    let mut cur_number: Option<u32> = None;
    let mut cur_mode: Option<String> = None;
    let mut cur_index00: u64 = 0;
    let mut cur_lba: u64 = 0;

    // Push any pending track into `raw`, then reset state.
    macro_rules! flush {
        () => {
            if let (Some(n), Some(m), Some(b)) = (cur_number.take(), cur_mode.take(), cur_bin.as_ref()) {
                raw.push(RawCueTrack { number: n, mode: m, index00_lba: cur_index00, start_lba: cur_lba, bin_path: b.clone(), session: cur_session });
            }
            cur_index00 = 0;
            cur_lba = 0;
        };
    }

    for line in text.lines() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();

        if upper.starts_with("REM SESSION ") {
            // Flush before changing session so the pending track gets the right number.
            flush!();
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if let Some(n) = parts.get(2).and_then(|s| s.parse::<u32>().ok()) {
                cur_session = n;
            }
        } else if upper.starts_with("FILE ") {
            flush!();
            if let Some(name) = extract_quoted(trimmed) {
                cur_bin = Some(cue_dir.join(name));
            }
        } else if upper.starts_with("TRACK ") {
            flush!();
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            cur_number = parts.get(1).and_then(|s| s.parse().ok());
            cur_mode = parts.get(2).map(|s| s.to_uppercase());
        } else if let Some(rest) = upper.strip_prefix("INDEX ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.first() == Some(&"00") {
                cur_index00 = parts.get(1).and_then(|s| msf_to_sectors(s)).unwrap_or(0);
            } else if parts.first() == Some(&"01") {
                if let Some(secs) = parts.get(1).and_then(|s| msf_to_sectors(s)) {
                    cur_lba = secs;
                }
            }
        }
    }
    flush!();

    let result: Vec<TrackInfo> = raw.iter().enumerate().map(|(i, rt)| {
        // For num_sectors: if next track shares the same file, use the LBA delta.
        // Otherwise derive from the file size (handles multi-file CUEs).
        let num_sectors = if i + 1 < raw.len() && raw[i + 1].bin_path == rt.bin_path {
            raw[i + 1].start_lba.saturating_sub(rt.start_lba)
        } else {
            fs::metadata(&rt.bin_path)
                .map(|m| m.len() / RAW_SECTOR_SIZE)
                .unwrap_or(0)
                .saturating_sub(rt.start_lba)
        };
        let is_data = rt.mode.starts_with("MODE") || rt.mode.starts_with("CDI");
        TrackInfo {
            number: rt.number,
            is_data,
            mode: rt.mode.clone(),
            start_lba: rt.start_lba,
            num_sectors,
            session: rt.session,
            bin_path: rt.bin_path.to_string_lossy().into_owned(),
        }
    }).collect();

    // Detect AUDIO tracks whose pregap contains scrambled CD-i data (CD-i Ready format).
    // Insert synthetic tracks (number=0) at the front for each such pregap.
    let mut pregap_cdi: Vec<TrackInfo> = Vec::new();
    for rt in &raw {
        if rt.mode == "AUDIO" && rt.index00_lba < rt.start_lba {
            let pregap_byte_offset = rt.index00_lba * RAW_SECTOR_SIZE;
            if cdi_filesystem::is_cdi_ready_pregap(&rt.bin_path, pregap_byte_offset) {
                pregap_cdi.push(TrackInfo {
                    number: 0,
                    is_data: true,
                    mode: "CDI/PREGAP".to_string(),
                    start_lba: rt.index00_lba,
                    num_sectors: rt.start_lba - rt.index00_lba,
                    session: rt.session,
                    bin_path: rt.bin_path.to_string_lossy().into_owned(),
                });
            }
        }
    }
    pregap_cdi.extend(result);

    Ok(pregap_cdi)
}

// ── Sector View ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SectorData {
    pub bytes: Vec<u8>,
    pub sector_size: u32,
    pub user_data_offset: u32,
    pub total_sectors: u64,
    pub lba: u64,
}

#[tauri::command]
fn read_sector(image_path: String, lba: u64) -> Result<SectorData, String> {
    let path = Path::new(&image_path);
    let lower = image_path.to_lowercase();

    let (file_path, sector_size, user_data_offset, data_offset): (PathBuf, u64, u64, u64) = if lower.ends_with(".cue") {
        let track = parse_cue_for_data_track(path)?;
        (track.bin_path, RAW_SECTOR_SIZE, track.user_data_offset, 0)
    } else if lower.ends_with(".mds") {
        let track = parse_mds_for_data_track(path)?;
        (track.bin_path, RAW_SECTOR_SIZE, track.user_data_offset, 0)
    } else if lower.ends_with(".nrg") {
        let track = parse_nrg_for_data_track(path)?;
        let ss = if track.user_data_offset > 0 { RAW_SECTOR_SIZE } else { 2048 };
        (track.bin_path, ss, track.user_data_offset, track.track_offset)
    } else if lower.ends_with(".ccd") {
        let track = parse_ccd_for_data_track(path)?;
        (track.bin_path, RAW_SECTOR_SIZE, track.user_data_offset, track.track_offset)
    } else if lower.ends_with(".cdi") {
        let track = parse_cdi_for_data_track(path)?;
        (track.bin_path, track.stride, track.user_data_offset, track.track_offset)
    } else if lower.ends_with(".gdi") {
        let track = parse_gdi_for_data_track(path)?;
        (track.bin_path, track.stride, track.user_data_offset, track.track_offset)
    } else if lower.ends_with(".chd") {
        let file = File::open(path).map_err(|e| format!("Cannot open CHD: {e}"))?;
        let chd = Chd::open(BufReader::new(file), None)
            .map_err(|e| format!("Cannot parse CHD: {e}"))?;
        let stride = chd_stride(chd.header().hunk_size() as u64, chd.header().unit_bytes() as u64);
        let logical_bytes = chd.header().logical_bytes();
        let total_sectors = if stride > 0 { logical_bytes / stride } else { 0 };
        if total_sectors == 0 { return Err("CHD is empty".to_string()); }
        if lba >= total_sectors {
            return Err(format!("Sector {lba} out of range (0–{})", total_sectors - 1));
        }
        let mut reader = ChdReader::new(chd);
        reader.seek(SeekFrom::Start(lba * stride)).map_err(|e| format!("Seek error: {e}"))?;
        let mut bytes = vec![0u8; stride as usize];
        reader.read_exact(&mut bytes).map_err(|e| format!("Read error: {e}"))?;
        const SYNC: [u8; 12] = [0x00,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0x00];
        let udo = if stride > 2048 && bytes.len() >= 16 && bytes[0..12] == SYNC {
            if bytes[15] == 2 { 24u32 } else { 16u32 }
        } else { 0u32 };
        return Ok(SectorData { bytes, sector_size: stride as u32, user_data_offset: udo, total_sectors, lba });
    } else if lower.ends_with(".mdx") {
        let (ss, udo) = mdx_sector_format(path);
        (path.to_path_buf(), ss, udo, MDX_DATA_OFFSET)
    } else {
        let udo = detect_raw_sector_offset(path).unwrap_or(0);
        (path.to_path_buf(), if udo > 0 { RAW_SECTOR_SIZE } else { 2048u64 }, udo, 0)
    };

    let file_len = fs::metadata(&file_path)
        .map_err(|e| format!("Cannot stat image: {e}"))?.len();
    let total_sectors = file_len.saturating_sub(data_offset) / sector_size;

    if total_sectors == 0 { return Err("Image file is empty".to_string()); }
    if lba >= total_sectors {
        return Err(format!("Sector {lba} out of range (0–{})", total_sectors - 1));
    }

    let mut f = File::open(&file_path).map_err(|e| format!("Cannot open: {e}"))?;
    f.seek(SeekFrom::Start(data_offset + lba * sector_size)).map_err(|e| format!("Seek error: {e}"))?;
    let mut bytes = vec![0u8; sector_size as usize];
    f.read_exact(&mut bytes).map_err(|e| format!("Read error: {e}"))?;

    Ok(SectorData { bytes, sector_size: sector_size as u32, user_data_offset: user_data_offset as u32, total_sectors, lba })
}

// ── WAV export ────────────────────────────────────────────────────────────────

fn write_wav_header(file: &mut File, data_size: u32) -> io::Result<()> {
    file.write_all(b"RIFF")?;
    file.write_all(&(data_size + 36).to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;      // PCM
    file.write_all(&2u16.to_le_bytes())?;      // stereo
    file.write_all(&44100u32.to_le_bytes())?;
    file.write_all(&176400u32.to_le_bytes())?; // byte rate = 44100 * 2 * 2
    file.write_all(&4u16.to_le_bytes())?;      // block align
    file.write_all(&16u16.to_le_bytes())?;     // bits per sample
    file.write_all(b"data")?;
    file.write_all(&data_size.to_le_bytes())?;
    Ok(())
}

// 1 MB per chunk — divisible by 4 (stereo 16-bit frame = 4 bytes)
const AUDIO_CHUNK: usize = 1 << 20;

fn open_audio_src(track: &TrackInfo) -> Result<(File, u64), String> {
    let mut src = File::open(&track.bin_path)
        .map_err(|e| format!("Cannot open BIN: {e}"))?;
    src.seek(SeekFrom::Start(track.start_lba * RAW_SECTOR_SIZE))
        .map_err(|e| format!("Seek error: {e}"))?;
    Ok((src, track.num_sectors * RAW_SECTOR_SIZE))
}

fn save_audio_as_wav(track: &TrackInfo, dest_path: &str) -> Result<(), String> {
    let (mut src, total_bytes) = open_audio_src(track)?;
    let mut dest = File::create(dest_path)
        .map_err(|e| format!("Cannot create WAV: {e}"))?;
    write_wav_header(&mut dest, total_bytes as u32)
        .map_err(|e| format!("WAV header error: {e}"))?;
    let mut remaining = total_bytes;
    let mut buf = vec![0u8; AUDIO_CHUNK];
    while remaining > 0 {
        let to_read = remaining.min(AUDIO_CHUNK as u64) as usize;
        let n = src.read(&mut buf[..to_read])
            .map_err(|e| format!("Read error: {e}"))?;
        if n == 0 { break; }
        dest.write_all(&buf[..n])
            .map_err(|e| format!("Write error: {e}"))?;
        remaining -= n as u64;
    }
    Ok(())
}

fn save_audio_as_flac(track: &TrackInfo, dest_path: &str) -> Result<(), String> {
    let (mut src, total_bytes) = open_audio_src(track)?;
    let total_frames = total_bytes / 4; // stereo 16-bit

    let mut enc = FlacEncoder::new()
        .ok_or_else(|| "FLAC encoder allocation failed".to_string())?
        .channels(2)
        .bits_per_sample(16)
        .sample_rate(44100)
        .compression_level(8)
        .total_samples_estimate(total_frames)
        .init_file(&PathBuf::from(dest_path))
        .map_err(|e| format!("FLAC encoder init failed: {e:?}"))?;

    let mut remaining = total_bytes;
    let mut buf = vec![0u8; AUDIO_CHUNK];
    while remaining > 0 {
        let to_read = remaining.min(AUDIO_CHUNK as u64) as usize;
        src.read_exact(&mut buf[..to_read])
            .map_err(|e| format!("Read error: {e}"))?;
        let samples: Vec<i32> = buf[..to_read].chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as i32)
            .collect();
        enc.process_interleaved(&samples, (samples.len() / 2) as u32)
            .map_err(|_| "FLAC process error".to_string())?;
        remaining -= to_read as u64;
    }
    enc.finish().map_err(|_| "FLAC finish error".to_string())?;
    Ok(())
}


fn save_audio_as_mp3(track: &TrackInfo, dest_path: &str) -> Result<(), String> {
    let (mut src, total_bytes) = open_audio_src(track)?;

    let mut enc = Mp3Builder::new()
        .ok_or_else(|| "MP3 encoder allocation failed".to_string())?
        .with_num_channels(2).map_err(|e| format!("MP3 set channels: {e:?}"))?
        .with_sample_rate(44_100).map_err(|e| format!("MP3 set sample rate: {e:?}"))?
        .with_brate(mp3lame_encoder::Bitrate::Kbps320).map_err(|e| format!("MP3 set bitrate: {e:?}"))?
        .with_quality(mp3lame_encoder::Quality::Best).map_err(|e| format!("MP3 set quality: {e:?}"))?
        .build().map_err(|e| format!("MP3 encoder init failed: {e:?}"))?;

    let mut out = std::io::BufWriter::new(
        File::create(dest_path).map_err(|e| format!("Cannot create MP3: {e}"))?
    );

    let mut raw = vec![0u8; AUDIO_CHUNK];
    let mut remaining = total_bytes;
    while remaining > 0 {
        let to_read = remaining.min(AUDIO_CHUNK as u64) as usize;
        src.read_exact(&mut raw[..to_read]).map_err(|e| format!("Read error: {e}"))?;
        remaining -= to_read as u64;

        let frames = to_read / 4;
        let mut left = vec![0u16; frames];
        let mut right = vec![0u16; frames];
        for i in 0..frames {
            left[i] = u16::from_le_bytes([raw[i*4],   raw[i*4+1]]);
            right[i] = u16::from_le_bytes([raw[i*4+2], raw[i*4+3]]);
        }

        let mut chunk = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(frames));
        let n = enc.encode(DualPcm { left: &left, right: &right }, chunk.spare_capacity_mut())
            .map_err(|e| format!("MP3 encode error: {e:?}"))?;
        unsafe { chunk.set_len(n); }
        out.write_all(&chunk).map_err(|e| format!("Write error: {e}"))?;
    }

    let mut tail = Vec::with_capacity(7200);
    let n = enc.flush::<FlushNoGap>(tail.spare_capacity_mut())
        .map_err(|e| format!("MP3 flush error: {e:?}"))?;
    unsafe { tail.set_len(n); }
    out.write_all(&tail).map_err(|e| format!("Write error: {e}"))?;
    Ok(())
}

#[tauri::command]
fn save_audio_track(cue_path: String, track_number: u32, dest_path: String, format: String) -> Result<(), String> {
    let lower = cue_path.to_lowercase();
    let tracks = if lower.ends_with(".gdi") {
        get_gdi_tracks(cue_path)?
    } else if lower.ends_with(".mds") {
        get_mds_track_list(Path::new(&cue_path))?
    } else {
        get_cue_tracks(cue_path)?
    };
    let track = tracks.iter()
        .find(|t| t.number == track_number && !t.is_data)
        .ok_or_else(|| format!("Audio track {track_number} not found"))?;
    match format.as_str() {
        "flac" => save_audio_as_flac(track, &dest_path),
        "mp3"  => save_audio_as_mp3(track, &dest_path),
        _      => save_audio_as_wav(track, &dest_path),
    }
}

// ── Optical drive listing ─────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DriveInfo {
    pub name: String,
    pub device_path: String,
    pub has_disc: bool,
    pub volume_name: Option<String>,
}

fn check_disc_in_drive(device_path: &str) -> (bool, Option<String>) {
    let Ok(out) = Command::new("diskutil").args(["info", device_path]).output() else {
        return (false, None);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Volume Name:") {
            let name = rest.trim().to_string();
            if !name.is_empty() && name != "Not applicable" && name != "(null)" {
                return (true, Some(name));
            }
        }
    }
    (false, None)
}

#[tauri::command]
fn list_optical_drives() -> Result<Vec<DriveInfo>, String> {
    let out = Command::new("system_profiler")
        .args(["SPDiscBurningDataType", "-json"])
        .output()
        .map_err(|e| format!("Cannot query optical drives: {e}"))?;

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();

    let arr = json.get("SPDiscBurningDataType")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut result = Vec::new();
    for drive in &arr {
        let Some(name) = drive.get("_name").and_then(|v| v.as_str()) else { continue; };

        let node = ["spdisc_burner-devicenode", "spdisc_burning_device", "bsd_name"]
            .iter()
            .find_map(|k| drive.get(k)?.as_str());

        let Some(node) = node else { continue; };
        let device_path = format!("/dev/{node}");
        let (has_disc, volume_name) = check_disc_in_drive(&device_path);

        result.push(DriveInfo { name: name.to_string(), device_path, has_disc, volume_name });
    }

    Ok(result)
}

// ── Disc entry ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DiscEntry {
    pub name: String,
    pub is_dir: bool,
    pub lba: u32,
    pub size: u32,
    pub size_bytes: u32,
    pub modified: String,
}

// ── Generic helpers ───────────────────────────────────────────────────────────

fn collect_entries<T: ISO9660Reader>(fs: &ISO9660<T>, dir_path: &str) -> Result<Vec<DiscEntry>, String> {
    let dir = match fs.open(dir_path).map_err(|e| format!("Path error: {e}"))? {
        Some(DirectoryEntry::Directory(d)) => d,
        Some(_) => return Err(format!("{dir_path} is not a directory")),
        None => return Err(format!("Directory not found: {dir_path}")),
    };

    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for item in dir.contents() {
        let item = item.map_err(|e| format!("Read error: {e}"))?;
        let name = item.identifier().to_string();
        if matches!(name.as_str(), "\0" | "\x01" | "." | "..") { continue; }

        let header = item.header();
        let lba = header.extent_loc;
        let size_bytes = header.extent_length;

        let (is_dir, size, modified) = match &item {
            DirectoryEntry::Directory(d) => {
                let t = d.time();
                (true, 0u32, format!("{}-{:02}-{:02} {:02}:{:02}:{:02}",
                    t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second()))
            }
            DirectoryEntry::File(f) => {
                let t = f.time();
                (false, f.size(), format!("{}-{:02}-{:02} {:02}:{:02}:{:02}",
                    t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second()))
            }
        };
        if !seen.insert((name.clone(), lba)) { continue; }
        entries.push(DiscEntry { name, is_dir, lba, size, size_bytes, modified });
    }
    Ok(entries)
}

fn extract_file_from_fs<T: ISO9660Reader>(fs: &ISO9660<T>, file_path: &str, dest_path: &str) -> Result<(), String> {
    let iso_file = match fs.open(file_path).map_err(|e| format!("Path error: {e}"))? {
        Some(DirectoryEntry::File(f)) => f,
        Some(_) => return Err(format!("{file_path} is not a file")),
        None => return Err(format!("File not found: {file_path}")),
    };
    let mut reader = iso_file.read();
    let mut dest = File::create(dest_path).map_err(|e| format!("Cannot create destination: {e}"))?;
    io::copy(&mut reader, &mut dest).map_err(|e| format!("Write error: {e}"))?;
    Ok(())
}

fn extract_dir<T: ISO9660Reader>(dir: &ISODirectory<T>, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("Cannot create dir {:?}: {e}", dest))?;
    for item in dir.contents() {
        let item = item.map_err(|e| format!("Read error: {e}"))?;
        let name = item.identifier().to_string();
        if matches!(name.as_str(), "\0" | "\x01" | "." | "..") { continue; }
        let child_dest = dest.join(&name);
        match item {
            DirectoryEntry::File(f) => {
                let mut reader = f.read();
                let mut out = File::create(&child_dest)
                    .map_err(|e| format!("Cannot create {:?}: {e}", child_dest))?;
                io::copy(&mut reader, &mut out)
                    .map_err(|e| format!("Write error for {:?}: {e}", child_dest))?;
            }
            DirectoryEntry::Directory(d) => extract_dir(&d, &child_dest)?,
        }
    }
    Ok(())
}

fn extract_dir_from_fs<T: ISO9660Reader>(fs: &ISO9660<T>, dir_path: &str, dest_path: &str) -> Result<(), String> {
    let dir = match fs.open(dir_path).map_err(|e| format!("Path error: {e}"))? {
        Some(DirectoryEntry::Directory(d)) => d,
        Some(_) => return Err(format!("{dir_path} is not a directory")),
        None => return Err(format!("Directory not found: {dir_path}")),
    };
    extract_dir(&dir, Path::new(dest_path))
}

macro_rules! with_fs {
    ($image_path:expr, $fs:ident, $body:expr) => {{
        let path = $image_path.as_str();
        let lower = path.to_lowercase();
        if lower.ends_with(".cue") {
            let $fs = open_iso_fs_for_cue(Path::new(path))?;
            $body
        } else if lower.ends_with(".mds") {
            let track = parse_mds_for_data_track(Path::new(path))?;
            let $fs = open_iso_fs(&track)?;
            $body
        } else {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            let $fs = ISO9660::new(file).map_err(|e| format!("Invalid disc image: {e}"))?;
            $body
        }
    }};
}

// ── Tauri commands ────────────────────────────────────────────────────────────

fn open_udf_fs(track: &DataTrack) -> Result<udf_filesystem::UdfFs, String> {
    let bin = File::open(&track.bin_path).map_err(|e| format!("Cannot open: {e}"))?;
    udf_filesystem::UdfFs::new(bin, track.track_offset, track.user_data_offset)
}

fn open_hfs_fs(track: &DataTrack) -> Result<hfs_filesystem::HfsFs, String> {
    let bin = File::open(&track.bin_path).map_err(|e| format!("Cannot open: {e}"))?;
    hfs_filesystem::HfsFs::new(bin, track.track_offset, track.user_data_offset)
}

fn open_cdi_fs(track: &DataTrack) -> Result<cdi_filesystem::CdiFs, String> {
    let bin = File::open(&track.bin_path).map_err(|e| format!("Cannot open: {e}"))?;
    cdi_filesystem::CdiFs::new(bin, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble)
}

fn open_pce_fs(track: &DataTrack) -> Result<pce_filesystem::PceFs, String> {
    let bin = File::open(&track.bin_path).map_err(|e| format!("Cannot open: {e}"))?;
    pce_filesystem::PceFs::new(bin, track.track_offset, track.user_data_offset)
}

fn open_threedo_fs(track: &DataTrack) -> Result<threedo_filesystem::ThreeDOFs<File>, String> {
    let bin = File::open(&track.bin_path).map_err(|e| format!("Cannot open: {e}"))?;
    let stride = threedo_filesystem::default_stride(track.user_data_offset);
    threedo_filesystem::ThreeDOFs::new(bin, track.track_offset, track.user_data_offset, stride)
}

fn open_threedo_chd(path: &Path) -> Result<threedo_filesystem::ThreeDOFs<ChdReader<BufReader<File>>>, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open CHD: {e}"))?;
    let chd = Chd::open(BufReader::new(file), None)
        .map_err(|e| format!("Cannot parse CHD: {e}"))?;
    let stride = chd_stride(chd.header().hunk_size() as u64, chd.header().unit_bytes() as u64);
    let mut reader = ChdReader::new(chd);

    let mut track_byte_start = 0u64;
    let mut udo = if stride == 2048 { 0u64 } else { 16u64 };
    if stride != 2048 {
        'probe: for pregap in [0u64, 4, 150] {
            for ud in [16u64, 24] {
                if threedo_filesystem::is_threedo_reader(&mut reader, pregap * stride, ud, stride) {
                    track_byte_start = pregap * stride;
                    udo = ud;
                    break 'probe;
                }
            }
        }
    }

    threedo_filesystem::ThreeDOFs::new(reader, track_byte_start, udo, stride)
}

fn gcm_kind_label(kind: gcm_filesystem::DiscKind) -> String {
    match kind {
        gcm_filesystem::DiscKind::GameCube => "GameCube GCM".to_string(),
        gcm_filesystem::DiscKind::Wii => "Wii GCM".to_string(),
    }
}

fn open_gcm_fs(track: &DataTrack) -> Result<gcm_filesystem::GcmFs<File>, String> {
    let bin = File::open(&track.bin_path).map_err(|e| format!("Cannot open: {e}"))?;
    gcm_filesystem::GcmFs::new(bin, track.track_offset)
}

fn open_gcm_chd(path: &Path) -> Result<gcm_filesystem::GcmFs<ChdReader<BufReader<File>>>, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open CHD: {e}"))?;
    let chd = Chd::open(BufReader::new(file), None)
        .map_err(|e| format!("Cannot parse CHD: {e}"))?;
    let reader = ChdReader::new(chd);
    gcm_filesystem::GcmFs::new(reader, 0)
}

fn open_xdvdfs_fs(track: &DataTrack) -> Result<xdvdfs_filesystem::XDVDFSFs<File>, String> {
    let bin = File::open(&track.bin_path).map_err(|e| format!("Cannot open: {e}"))?;
    xdvdfs_filesystem::XDVDFSFs::new(bin, track.track_offset)
}

fn open_xdvdfs_chd(path: &Path) -> Result<xdvdfs_filesystem::XDVDFSFs<ChdReader<BufReader<File>>>, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open CHD: {e}"))?;
    let chd = Chd::open(BufReader::new(file), None)
        .map_err(|e| format!("Cannot parse CHD: {e}"))?;
    let reader = ChdReader::new(chd);
    xdvdfs_filesystem::XDVDFSFs::new(reader, 0)
}

fn open_iso_fs(track: &DataTrack) -> Result<ISO9660<MultiTrackBinReader>, String> {
    let bin = File::open(&track.bin_path).map_err(|e| format!("Cannot open: {e}"))?;
    let reader = MultiTrackBinReader::single(bin, track.track_offset, track.user_data_offset, track.stride, track.lba_offset);
    ISO9660::new(reader).map_err(|e| format!("Invalid disc image: {e}"))
}

// Builds an ISO 9660 reader for a CUE sheet, using a multi-BIN reader when the
// disc has separate data tracks in different BIN files (Photo CD, VCD, etc.).
fn open_iso_fs_for_cue(cue_path: &Path) -> Result<ISO9660<MultiTrackBinReader>, String> {
    let all_tracks = parse_cue_all_data_tracks(cue_path)?;

    let use_multi_bin = all_tracks.len() > 1
        && all_tracks.last().map(|t| !has_pvd(t)).unwrap_or(false)
        && all_tracks.windows(2).any(|w| w[0].bin_path != w[1].bin_path);

    if use_multi_bin {
        let mut track_files: Vec<TrackFile> = Vec::with_capacity(all_tracks.len());
        for dt in all_tracks {
            let file = File::open(&dt.bin_path).map_err(|e| format!("Cannot open BIN: {e}"))?;
            track_files.push(TrackFile {
                file,
                track_offset: dt.track_offset,
                user_data_offset: dt.user_data_offset,
                stride: dt.stride,
                lba_offset: dt.lba_offset,
                start_lba: dt.lba_offset,
                sector_count: dt.sector_count,
            });
        }
        let reader = MultiTrackBinReader { tracks: track_files, root_idx: 0, multi_bin: true };
        ISO9660::new(reader).map_err(|e| format!("Invalid disc image: {e}"))
    } else {
        let dt = all_tracks.into_iter().last().unwrap();
        let bin = File::open(&dt.bin_path).map_err(|e| format!("Cannot open BIN: {e}"))?;
        let reader = MultiTrackBinReader::single(bin, dt.track_offset, dt.user_data_offset, dt.stride, dt.lba_offset);
        ISO9660::new(reader).map_err(|e| format!("Invalid disc image: {e}"))
    }
}

// Returns true when filesystem is None (auto-detect) OR explicitly matches target.
fn fs_matches(fs: &Option<String>, target: &str) -> bool {
    fs.as_deref().map_or(true, |s| s == target)
}

fn fs_matches_udf(fs: &Option<String>) -> bool {
    fs.as_deref().map_or(true, |s| s.starts_with("UDF"))
}

fn fs_matches_gcm(fs: &Option<String>) -> bool {
    fs.as_deref().map_or(true, |s| s == "GameCube GCM" || s == "Wii GCM")
}



#[tauri::command]
fn list_disc_contents(image_path: String, dir_path: String, filesystem: Option<String>) -> Result<Vec<DiscEntry>, String> {
    let path = image_path.as_str();

    // If image_path is a real directory (e.g. a mounted disc volume), list it directly.
    if Path::new(path).is_dir() {
        let target = if dir_path == "/" {
            PathBuf::from(path)
        } else {
            Path::new(path).join(dir_path.trim_start_matches('/'))
        };
        let rd = fs::read_dir(&target).map_err(|e| format!("Cannot read directory: {e}"))?;
        let mut entries = Vec::new();
        for entry in rd {
            let entry = entry.map_err(|e| format!("Read error: {e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().map_err(|e| format!("Metadata error: {e}"))?;
            let is_dir = meta.is_dir();
            let size_bytes = if is_dir { 0 } else { meta.len() as u32 };
            let modified = meta.modified().ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| unix_secs_to_string(d.as_secs()))
                .unwrap_or_default();
            entries.push(DiscEntry {
                name, is_dir, lba: 0, size: size_bytes, size_bytes, modified,
            });
        }
        entries.sort_by(|a, b| {
            if a.is_dir != b.is_dir { return if a.is_dir { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater }; }
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        });
        return Ok(entries);
    }

    let lower = path.to_lowercase();

    if lower.ends_with(".cue") || lower.ends_with(".mds") || lower.ends_with(".nrg") || lower.ends_with(".ccd") || lower.ends_with(".cdi") || lower.ends_with(".gdi") {
        let track = if lower.ends_with(".cue") { parse_cue_for_data_track(Path::new(path))? }
            else if lower.ends_with(".mds") { parse_mds_for_data_track(Path::new(path))? }
            else if lower.ends_with(".nrg") { parse_nrg_for_data_track(Path::new(path))? }
            else if lower.ends_with(".ccd") { parse_ccd_for_data_track(Path::new(path))? }
            else if lower.ends_with(".gdi") { parse_gdi_for_data_track(Path::new(path))? }
            else { parse_cdi_for_data_track(Path::new(path))? };
        if cdi_filesystem::is_cdi_disc(&track.bin_path, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble) {
            open_cdi_fs(&track)?.list_directory(&dir_path)
        } else if pce_filesystem::is_pce_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_pce_fs(&track)?.list_directory(&dir_path)
        } else if threedo_filesystem::is_threedo_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_threedo_fs(&track)?.list_directory(&dir_path)
        } else if fs_matches(&filesystem, "XDVDFS") && track.user_data_offset == 0 && xdvdfs_filesystem::is_xdvdfs_disc(&track.bin_path, track.track_offset) {
            open_xdvdfs_fs(&track)?.list_directory(&dir_path)
        } else if fs_matches_gcm(&filesystem) && track.user_data_offset == 0 && gcm_filesystem::detect_gcm_disc(&track.bin_path).is_some() {
            open_gcm_fs(&track)?.list_directory(&dir_path)
        } else if fs_matches(&filesystem, "HFS") && hfs_filesystem::is_hfs_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_hfs_fs(&track)?.list_directory(&dir_path)
        } else if fs_matches_udf(&filesystem) && udf_filesystem::is_udf_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_udf_fs(&track)?.list_directory(&dir_path)
        } else if lower.ends_with(".cue") {
            collect_entries(&open_iso_fs_for_cue(Path::new(path))?, &dir_path)
        } else {
            collect_entries(&open_iso_fs(&track)?, &dir_path)
        }
    } else if lower.ends_with(".chd") {
        if filesystem.as_deref() == Some("3DO OperaFS") {
            open_threedo_chd(Path::new(path))?.list_directory(&dir_path)
        } else if filesystem.as_deref() == Some("XDVDFS") {
            open_xdvdfs_chd(Path::new(path))?.list_directory(&dir_path)
        } else if filesystem.as_deref() == Some("GameCube GCM") || filesystem.as_deref() == Some("Wii GCM") {
            open_gcm_chd(Path::new(path))?.list_directory(&dir_path)
        } else {
            collect_entries(&open_chd_iso(Path::new(path))?, &dir_path)
        }
    } else if lower.ends_with(".mdx") {
        let path_obj = Path::new(path);
        let track = parse_mdx_as_data_track(path_obj);
        if track.user_data_offset > 0 {
            // 2352-byte raw sectors: reuse existing filesystem openers.
            if cdi_filesystem::is_cdi_disc(&track.bin_path, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble) {
                open_cdi_fs(&track)?.list_directory(&dir_path)
            } else if pce_filesystem::is_pce_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_pce_fs(&track)?.list_directory(&dir_path)
            } else if threedo_filesystem::is_threedo_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_threedo_fs(&track)?.list_directory(&dir_path)
            } else if hfs_filesystem::is_hfs_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_hfs_fs(&track)?.list_directory(&dir_path)
            } else if udf_filesystem::is_udf_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_udf_fs(&track)?.list_directory(&dir_path)
            } else {
                collect_entries(&open_iso_fs(&track)?, &dir_path)
            }
        } else {
            // 2048-byte logical sectors: use MdxReader.
            collect_entries(&open_iso_fs_mdx(path_obj)?, &dir_path)
        }
    } else {
        let path_obj = Path::new(path);
        let user_data_offset = detect_raw_sector_offset(path_obj).unwrap_or(0);
        if pce_filesystem::is_pce_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return pce_filesystem::PceFs::new(file, 0, user_data_offset)?.list_directory(&dir_path);
        }
        if threedo_filesystem::is_threedo_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            let stride = threedo_filesystem::default_stride(user_data_offset);
            return threedo_filesystem::ThreeDOFs::new(file, 0, user_data_offset, stride)?.list_directory(&dir_path);
        }
        if fs_matches(&filesystem, "XDVDFS") && user_data_offset == 0 && xdvdfs_filesystem::is_xdvdfs_disc(path_obj, 0) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return xdvdfs_filesystem::XDVDFSFs::new(file, 0)?.list_directory(&dir_path);
        }
        if fs_matches_gcm(&filesystem) && user_data_offset == 0 && gcm_filesystem::detect_gcm_disc(path_obj).is_some() {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return gcm_filesystem::GcmFs::new(file, 0)?.list_directory(&dir_path);
        }
        if fs_matches_udf(&filesystem) && udf_filesystem::is_udf_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            if let Ok(mut udf) = udf_filesystem::UdfFs::new(file, 0, user_data_offset) {
                return udf.list_directory(&dir_path);
            }
        }
        if fs_matches(&filesystem, "HFS") && hfs_filesystem::is_hfs_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return hfs_filesystem::HfsFs::new(file, 0, user_data_offset)?.list_directory(&dir_path);
        }
        let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
        if user_data_offset > 0 {
            let reader = MultiTrackBinReader::single(file, 0, user_data_offset, RAW_SECTOR_SIZE, 0);
            let fs = ISO9660::new(reader).map_err(|e| format!("Invalid disc image: {e}"))?;
            collect_entries(&fs, &dir_path)
        } else {
            let fs = ISO9660::new(file).map_err(|e| format!("Invalid disc image: {e}"))?;
            collect_entries(&fs, &dir_path)
        }
    }
}

#[tauri::command]
fn save_file(image_path: String, file_path: String, dest_path: String, filesystem: Option<String>) -> Result<(), String> {
    let path = image_path.as_str();

    if Path::new(path).is_dir() {
        let src = Path::new(path).join(file_path.trim_start_matches('/'));
        fs::copy(&src, &dest_path).map_err(|e| format!("Copy error: {e}"))?;
        return Ok(());
    }

    let lower = path.to_lowercase();

    if lower.ends_with(".cue") || lower.ends_with(".mds") || lower.ends_with(".nrg") || lower.ends_with(".ccd") || lower.ends_with(".cdi") || lower.ends_with(".gdi") {
        let track = if lower.ends_with(".cue") { parse_cue_for_data_track(Path::new(path))? }
            else if lower.ends_with(".mds") { parse_mds_for_data_track(Path::new(path))? }
            else if lower.ends_with(".nrg") { parse_nrg_for_data_track(Path::new(path))? }
            else if lower.ends_with(".ccd") { parse_ccd_for_data_track(Path::new(path))? }
            else if lower.ends_with(".gdi") { parse_gdi_for_data_track(Path::new(path))? }
            else { parse_cdi_for_data_track(Path::new(path))? };
        if cdi_filesystem::is_cdi_disc(&track.bin_path, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble) {
            open_cdi_fs(&track)?.extract_file(&file_path, &dest_path)
        } else if pce_filesystem::is_pce_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_pce_fs(&track)?.extract_file(&file_path, &dest_path)
        } else if threedo_filesystem::is_threedo_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_threedo_fs(&track)?.extract_file(&file_path, &dest_path)
        } else if fs_matches(&filesystem, "XDVDFS") && track.user_data_offset == 0 && xdvdfs_filesystem::is_xdvdfs_disc(&track.bin_path, track.track_offset) {
            open_xdvdfs_fs(&track)?.extract_file(&file_path, &dest_path)
        } else if fs_matches_gcm(&filesystem) && track.user_data_offset == 0 && gcm_filesystem::detect_gcm_disc(&track.bin_path).is_some() {
            open_gcm_fs(&track)?.extract_file(&file_path, &dest_path)
        } else if fs_matches(&filesystem, "HFS") && hfs_filesystem::is_hfs_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_hfs_fs(&track)?.extract_file(&file_path, &dest_path)
        } else if fs_matches_udf(&filesystem) && udf_filesystem::is_udf_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_udf_fs(&track)?.extract_file(&file_path, &dest_path)
        } else if lower.ends_with(".cue") {
            extract_file_from_fs(&open_iso_fs_for_cue(Path::new(path))?, &file_path, &dest_path)
        } else {
            extract_file_from_fs(&open_iso_fs(&track)?, &file_path, &dest_path)
        }
    } else if lower.ends_with(".chd") {
        if filesystem.as_deref() == Some("3DO OperaFS") {
            open_threedo_chd(Path::new(path))?.extract_file(&file_path, &dest_path)
        } else if filesystem.as_deref() == Some("XDVDFS") {
            open_xdvdfs_chd(Path::new(path))?.extract_file(&file_path, &dest_path)
        } else if filesystem.as_deref() == Some("GameCube GCM") || filesystem.as_deref() == Some("Wii GCM") {
            open_gcm_chd(Path::new(path))?.extract_file(&file_path, &dest_path)
        } else {
            extract_file_from_fs(&open_chd_iso(Path::new(path))?, &file_path, &dest_path)
        }
    } else if lower.ends_with(".mdx") {
        let path_obj = Path::new(path);
        let track = parse_mdx_as_data_track(path_obj);
        if track.user_data_offset > 0 {
            if cdi_filesystem::is_cdi_disc(&track.bin_path, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble) {
                open_cdi_fs(&track)?.extract_file(&file_path, &dest_path)
            } else if pce_filesystem::is_pce_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_pce_fs(&track)?.extract_file(&file_path, &dest_path)
            } else if threedo_filesystem::is_threedo_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_threedo_fs(&track)?.extract_file(&file_path, &dest_path)
            } else if hfs_filesystem::is_hfs_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_hfs_fs(&track)?.extract_file(&file_path, &dest_path)
            } else if udf_filesystem::is_udf_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_udf_fs(&track)?.extract_file(&file_path, &dest_path)
            } else {
                extract_file_from_fs(&open_iso_fs(&track)?, &file_path, &dest_path)
            }
        } else {
            extract_file_from_fs(&open_iso_fs_mdx(path_obj)?, &file_path, &dest_path)
        }
    } else {
        let path_obj = Path::new(path);
        let user_data_offset = detect_raw_sector_offset(path_obj).unwrap_or(0);
        if pce_filesystem::is_pce_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return pce_filesystem::PceFs::new(file, 0, user_data_offset)?.extract_file(&file_path, &dest_path);
        }
        if threedo_filesystem::is_threedo_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            let stride = threedo_filesystem::default_stride(user_data_offset);
            return threedo_filesystem::ThreeDOFs::new(file, 0, user_data_offset, stride)?.extract_file(&file_path, &dest_path);
        }
        if fs_matches(&filesystem, "XDVDFS") && user_data_offset == 0 && xdvdfs_filesystem::is_xdvdfs_disc(path_obj, 0) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return xdvdfs_filesystem::XDVDFSFs::new(file, 0)?.extract_file(&file_path, &dest_path);
        }
        if fs_matches_gcm(&filesystem) && user_data_offset == 0 && gcm_filesystem::detect_gcm_disc(path_obj).is_some() {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return gcm_filesystem::GcmFs::new(file, 0)?.extract_file(&file_path, &dest_path);
        }
        if fs_matches_udf(&filesystem) && udf_filesystem::is_udf_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            if let Ok(mut udf) = udf_filesystem::UdfFs::new(file, 0, user_data_offset) {
                return udf.extract_file(&file_path, &dest_path);
            }
        }
        if fs_matches(&filesystem, "HFS") && hfs_filesystem::is_hfs_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return hfs_filesystem::HfsFs::new(file, 0, user_data_offset)?.extract_file(&file_path, &dest_path);
        }
        if user_data_offset > 0 {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            let reader = MultiTrackBinReader::single(file, 0, user_data_offset, RAW_SECTOR_SIZE, 0);
            let fs = ISO9660::new(reader).map_err(|e| format!("Invalid disc image: {e}"))?;
            extract_file_from_fs(&fs, &file_path, &dest_path)
        } else {
            with_fs!(image_path, fs, extract_file_from_fs(&fs, &file_path, &dest_path))
        }
    }
}

#[tauri::command]
fn save_directory(image_path: String, dir_path: String, dest_path: String, filesystem: Option<String>) -> Result<(), String> {
    let path = image_path.as_str();

    if Path::new(path).is_dir() {
        let src = if dir_path == "/" {
            PathBuf::from(path)
        } else {
            Path::new(path).join(dir_path.trim_start_matches('/'))
        };
        copy_dir_recursive(&src, Path::new(&dest_path))?;
        return Ok(());
    }

    let lower = path.to_lowercase();

    if lower.ends_with(".cue") || lower.ends_with(".mds") || lower.ends_with(".nrg") || lower.ends_with(".ccd") || lower.ends_with(".cdi") || lower.ends_with(".gdi") {
        let track = if lower.ends_with(".cue") { parse_cue_for_data_track(Path::new(path))? }
            else if lower.ends_with(".mds") { parse_mds_for_data_track(Path::new(path))? }
            else if lower.ends_with(".nrg") { parse_nrg_for_data_track(Path::new(path))? }
            else if lower.ends_with(".ccd") { parse_ccd_for_data_track(Path::new(path))? }
            else if lower.ends_with(".gdi") { parse_gdi_for_data_track(Path::new(path))? }
            else { parse_cdi_for_data_track(Path::new(path))? };
        if cdi_filesystem::is_cdi_disc(&track.bin_path, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble) {
            open_cdi_fs(&track)?.extract_directory(&dir_path, &dest_path)
        } else if pce_filesystem::is_pce_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_pce_fs(&track)?.extract_directory(&dir_path, &dest_path)
        } else if threedo_filesystem::is_threedo_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_threedo_fs(&track)?.extract_directory(&dir_path, &dest_path)
        } else if fs_matches(&filesystem, "XDVDFS") && track.user_data_offset == 0 && xdvdfs_filesystem::is_xdvdfs_disc(&track.bin_path, track.track_offset) {
            open_xdvdfs_fs(&track)?.extract_directory(&dir_path, &dest_path)
        } else if fs_matches_gcm(&filesystem) && track.user_data_offset == 0 && gcm_filesystem::detect_gcm_disc(&track.bin_path).is_some() {
            open_gcm_fs(&track)?.extract_directory(&dir_path, &dest_path)
        } else if fs_matches(&filesystem, "HFS") && hfs_filesystem::is_hfs_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_hfs_fs(&track)?.extract_directory(&dir_path, &dest_path)
        } else if fs_matches_udf(&filesystem) && udf_filesystem::is_udf_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
            open_udf_fs(&track)?.extract_directory(&dir_path, &dest_path)
        } else if lower.ends_with(".cue") {
            extract_dir_from_fs(&open_iso_fs_for_cue(Path::new(path))?, &dir_path, &dest_path)
        } else {
            extract_dir_from_fs(&open_iso_fs(&track)?, &dir_path, &dest_path)
        }
    } else if lower.ends_with(".chd") {
        if filesystem.as_deref() == Some("3DO OperaFS") {
            open_threedo_chd(Path::new(path))?.extract_directory(&dir_path, &dest_path)
        } else if filesystem.as_deref() == Some("XDVDFS") {
            open_xdvdfs_chd(Path::new(path))?.extract_directory(&dir_path, &dest_path)
        } else if filesystem.as_deref() == Some("GameCube GCM") || filesystem.as_deref() == Some("Wii GCM") {
            open_gcm_chd(Path::new(path))?.extract_directory(&dir_path, &dest_path)
        } else {
            extract_dir_from_fs(&open_chd_iso(Path::new(path))?, &dir_path, &dest_path)
        }
    } else if lower.ends_with(".mdx") {
        let path_obj = Path::new(path);
        let track = parse_mdx_as_data_track(path_obj);
        if track.user_data_offset > 0 {
            if cdi_filesystem::is_cdi_disc(&track.bin_path, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble) {
                open_cdi_fs(&track)?.extract_directory(&dir_path, &dest_path)
            } else if pce_filesystem::is_pce_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_pce_fs(&track)?.extract_directory(&dir_path, &dest_path)
            } else if threedo_filesystem::is_threedo_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_threedo_fs(&track)?.extract_directory(&dir_path, &dest_path)
            } else if hfs_filesystem::is_hfs_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_hfs_fs(&track)?.extract_directory(&dir_path, &dest_path)
            } else if udf_filesystem::is_udf_disc(&track.bin_path, track.track_offset, track.user_data_offset) {
                open_udf_fs(&track)?.extract_directory(&dir_path, &dest_path)
            } else {
                extract_dir_from_fs(&open_iso_fs(&track)?, &dir_path, &dest_path)
            }
        } else {
            extract_dir_from_fs(&open_iso_fs_mdx(path_obj)?, &dir_path, &dest_path)
        }
    } else {
        let path_obj = Path::new(path);
        let user_data_offset = detect_raw_sector_offset(path_obj).unwrap_or(0);
        if pce_filesystem::is_pce_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return pce_filesystem::PceFs::new(file, 0, user_data_offset)?.extract_directory(&dir_path, &dest_path);
        }
        if threedo_filesystem::is_threedo_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            let stride = threedo_filesystem::default_stride(user_data_offset);
            return threedo_filesystem::ThreeDOFs::new(file, 0, user_data_offset, stride)?.extract_directory(&dir_path, &dest_path);
        }
        if fs_matches(&filesystem, "XDVDFS") && user_data_offset == 0 && xdvdfs_filesystem::is_xdvdfs_disc(path_obj, 0) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return xdvdfs_filesystem::XDVDFSFs::new(file, 0)?.extract_directory(&dir_path, &dest_path);
        }
        if fs_matches_gcm(&filesystem) && user_data_offset == 0 && gcm_filesystem::detect_gcm_disc(path_obj).is_some() {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return gcm_filesystem::GcmFs::new(file, 0)?.extract_directory(&dir_path, &dest_path);
        }
        if fs_matches_udf(&filesystem) && udf_filesystem::is_udf_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            if let Ok(mut udf) = udf_filesystem::UdfFs::new(file, 0, user_data_offset) {
                return udf.extract_directory(&dir_path, &dest_path);
            }
        }
        if fs_matches(&filesystem, "HFS") && hfs_filesystem::is_hfs_disc(path_obj, 0, user_data_offset) {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            return hfs_filesystem::HfsFs::new(file, 0, user_data_offset)?.extract_directory(&dir_path, &dest_path);
        }
        if user_data_offset > 0 {
            let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
            let reader = MultiTrackBinReader::single(file, 0, user_data_offset, RAW_SECTOR_SIZE, 0);
            let fs = ISO9660::new(reader).map_err(|e| format!("Invalid disc image: {e}"))?;
            extract_dir_from_fs(&fs, &dir_path, &dest_path)
        } else {
            with_fs!(image_path, fs, extract_dir_from_fs(&fs, &dir_path, &dest_path))
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(MountedImages(Mutex::new(Vec::new())))
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                let state = window.app_handle().state::<MountedImages>();
                let devices = state.0.lock().unwrap().clone();
                detach_all(&devices);
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_disc_contents, save_file, save_directory,
            get_cue_tracks, get_gdi_tracks, save_audio_track, list_optical_drives,
            get_mds_tracks, mount_disc_image, unmount_disc_image,
            get_disc_filesystems, read_sector, get_platform
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
