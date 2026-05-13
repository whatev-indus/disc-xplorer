use flac_bound::FlacEncoder;
use flate2::read::{DeflateDecoder, ZlibDecoder};
use zstd;
use aes::Aes128;
use cbc::Decryptor;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::NoPadding};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;
use mp3lame_encoder::{Builder as Mp3Builder, DualPcm, FlushNoGap};
use iso9660::{ISO9660, ISO9660Reader, ISODirectory, DirectoryEntry};
use serde::Serialize;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};
use chd::Chd;
use chd::read::ChdReader;

mod cdi_filesystem;
mod gcm_filesystem;
mod hfs_filesystem;
mod pce_filesystem;
mod threedo_filesystem;
mod udf_filesystem;
mod wbfs_reader;
mod wux_reader;
mod xdvdfs_filesystem;

// Spawn a system tool without the AppImage's library/Python env overrides bleeding in.
fn syscmd(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_remove("LD_LIBRARY_PATH")
       .env_remove("LD_PRELOAD")
       .env_remove("PYTHONHOME")
       .env_remove("PYTHONPATH");
    cmd
}

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
    if lower.ends_with(".cue") || lower.ends_with(".mds") || lower.ends_with(".nrg") || lower.ends_with(".ccd") || lower.ends_with(".cdi") || lower.ends_with(".gdi") || lower.ends_with(".b5t") || lower.ends_with(".b6t") || lower.ends_with(".cif") {
        let track = if lower.ends_with(".cue") { parse_cue_for_data_track(path)? }
            else if lower.ends_with(".mds") { parse_mds_for_data_track(path)? }
            else if lower.ends_with(".nrg") { parse_nrg_for_data_track(path)? }
            else if lower.ends_with(".ccd") { parse_ccd_for_data_track(path)? }
            else if lower.ends_with(".gdi") { parse_gdi_for_data_track(path)? }
            else if lower.ends_with(".b5t") || lower.ends_with(".b6t") { parse_b5t_for_data_track(path)? }
            else if lower.ends_with(".cif") { parse_cif_for_data_track(path)? }
            else { parse_cdi_for_data_track(path)? };
        Ok(detect_filesystems_in_bin(&track.bin_path, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble))
    } else if lower.ends_with(".chd") {
        Ok(detect_filesystems_chd(path))
    } else if lower.ends_with(".mdx") {
        Ok(detect_filesystems_mdx(path))
    } else if lower.ends_with(".cso") || lower.ends_with(".ciso") {
        Ok(detect_filesystems_cso(path))
    } else if lower.ends_with(".ecm") {
        Ok(detect_filesystems_ecm(path))
    } else if lower.ends_with(".uif") {
        Ok(detect_filesystems_uif(path))
    } else if lower.ends_with(".aif") {
        Ok(detect_filesystems_aif(path))
    } else if lower.ends_with(".skeleton") {
        Ok(detect_filesystems_skeleton(path))
    } else if lower.ends_with(".skeleton.zst") || lower.ends_with(".iso.zst") || lower.ends_with(".img.zst") {
        Ok(detect_filesystems_zst(path))
    } else if lower.ends_with(".wbfs") {
        Ok(detect_filesystems_wbfs(path))
    } else if lower.ends_with(".wux") || lower.ends_with(".wud") {
        Ok(detect_filesystems_wux(path))
    } else if lower.ends_with(".sdram") {
        Ok(detect_filesystems_redumper_dvd(path))
    } else if lower.ends_with(".sbram") {
        Ok(detect_filesystems_redumper_bd(path))
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

    let session_offset = read_u32_le(&data, 0x50) as usize;
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
        if mode_byte == 0x00 || mode_byte == 0xA9 { continue; } // AUDIO

        // Sector size stored at +0x10; includes +0x60 subchannel bytes when present.
        let sector_size = u16::from_le_bytes([data[tb + 0x10], data[tb + 0x11]]) as u64;
        let stride = if sector_size >= 0x800 { sector_size } else { RAW_SECTOR_SIZE };

        // Base size without subchannel bytes (always last 0x60 bytes when subchan!=0).
        let subchan_size = if data[tb + 1] != 0 { 0x60u64 } else { 0u64 };
        let base_size = stride.saturating_sub(subchan_size);

        // When base_size == 0x800 the MDF stores user data only (no raw header).
        // When base_size >= 0x930 a full raw sector is stored; skip sync+header.
        let user_data_offset = if base_size > 0x800 {
            match mode_byte {
                0xAC | 0xAD | 0x03 | 0x04 => 24u64, // MODE2 Form1/2: extra sub-header
                _ => 16u64,                           // MODE1 / MODE2 raw
            }
        } else {
            0u64
        };

        let track_offset = read_u64_le(&data, tb + 0x28);
        let lba_offset = sector_lba_at(&mdf_path, track_offset);
        return Ok(DataTrack { bin_path: mdf_path, track_offset, user_data_offset, stride, lba_offset, descramble: false, sector_count: 0 });
    }

    Err("No data track found in MDS".to_string())
}

const MDS_SESSION_BLOCK_SIZE: usize = 24;

// ── CDI track listing ─────────────────────────────────────────────────────────

fn get_cdi_track_list(path: &Path) -> Result<Vec<TrackInfo>, String> {
    const CDI_V2:  u32 = 0x80000004;
    const CDI_V35: u32 = 0x80000006;

    let mut f = File::open(path).map_err(|e| format!("Cannot open CDI: {e}"))?;
    let file_size = f.seek(SeekFrom::End(0)).map_err(|e| format!("CDI seek: {e}"))?;
    if file_size < 8 { return Err("CDI too short".to_string()); }

    f.seek(SeekFrom::Start(file_size - 8)).map_err(|e| format!("CDI seek: {e}"))?;
    let mut b4 = [0u8; 4];
    f.read_exact(&mut b4).map_err(|e| format!("CDI read: {e}"))?;
    let version = u32::from_le_bytes(b4);
    f.read_exact(&mut b4).map_err(|e| format!("CDI read: {e}"))?;
    let header_offset = u32::from_le_bytes(b4);

    if version < 0x80000004 || version > 0x80000006 {
        return Err(format!("Not a CDI image (version {version:#010X})"));
    }

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

    let num_sessions = r2!() as u32;
    let mut tracks: Vec<TrackInfo> = Vec::new();
    let mut track_num: u32 = 0;
    let mut cur_offset: u64 = 0;

    for sess in 0..num_sessions {
        let num_tracks = r2!() as u32;

        for _ in 0..num_tracks {
            let marker = r4!();
            if marker != 0 { sk!(8); }
            sk!(20);
            sk!(4);
            let fn_len = r1!() as i64;
            sk!(fn_len);
            sk!(19);
            let dj4 = r4!();
            if dj4 == 0x80000000 { sk!(8); }
            sk!(2);
            let _pregap      = r4!() as u64;
            let track_length = r4!() as u64;
            sk!(6);
            let track_mode   = r4!();
            sk!(12);
            let start_lba    = r4!() as u64;
            let total_len    = r4!() as u64;
            sk!(16);
            let sector_size_value = r4!();
            sk!(29);
            if version != CDI_V2 {
                sk!(5);
                let extra = r4!();
                if extra == 0xffffffff { sk!(78); }
            }

            let stride: u64 = match sector_size_value { 0 => 2048, 1 => 2336, _ => 2352 };
            let is_data = track_mode != 0;
            let mode = match (track_mode, stride) {
                (0, _)    => "AUDIO".to_string(),
                (_, 2048) => "MODE1/2048".to_string(),
                (_, 2336) => "MODE2/2336".to_string(),
                (1, _)    => "MODE1/2352".to_string(),
                _         => "MODE2/2352".to_string(),
            };

            track_num += 1;
            tracks.push(TrackInfo {
                number: track_num,
                is_data,
                mode,
                start_lba,
                num_sectors: track_length,
                session: sess + 1,
                bin_path: path.to_string_lossy().into_owned(),
            });

            cur_offset += stride * total_len;
            let _ = cur_offset; // suppress unused warning; kept for future use
        }

        sk!(12);
        if version != CDI_V2 { sk!(1); }
    }

    if tracks.is_empty() { return Err("No tracks found in CDI".to_string()); }
    Ok(tracks)
}

#[tauri::command]
fn get_cdi_tracks(cdi_path: String) -> Result<Vec<TrackInfo>, String> {
    get_cdi_track_list(Path::new(&cdi_path))
}

// ── CSO/CISO (Compressed ISO) support ────────────────────────────────────────
// Block-level zlib-deflate compressed ISO. Each 2048-byte sector is its own
// compressed block. Common for PSP UMD images.

const CSO_MAGIC: &[u8; 4] = b"CISO";

pub struct CsoReader {
    file:       File,
    block_size: u64,
    total_bytes: u64,
    align:      u8,
    index:      Vec<u32>,   // (num_blocks + 1) entries
    cache:      Option<(u64, Vec<u8>)>, // (block_idx, decompressed)
}

impl CsoReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut f = File::open(path).map_err(|e| format!("Cannot open CSO: {e}"))?;
        let mut hdr = [0u8; 24];
        f.read_exact(&mut hdr).map_err(|e| format!("CSO header: {e}"))?;
        if &hdr[0..4] != CSO_MAGIC {
            return Err("Not a CSO file".to_string());
        }
        let total_bytes = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let block_size  = u32::from_le_bytes(hdr[16..20].try_into().unwrap()) as u64;
        let align       = hdr[21];
        if block_size == 0 { return Err("Invalid CSO: block_size=0".to_string()); }

        let num_blocks = ((total_bytes + block_size - 1) / block_size) as usize;
        let mut index = vec![0u32; num_blocks + 1];
        let mut buf4 = [0u8; 4];
        for entry in index.iter_mut() {
            f.read_exact(&mut buf4).map_err(|e| format!("CSO index: {e}"))?;
            *entry = u32::from_le_bytes(buf4);
        }

        Ok(CsoReader { file: f, block_size, total_bytes, align, index, cache: None })
    }

    fn decompress_block(&mut self, block_idx: u64) -> io::Result<()> {
        if self.cache.as_ref().map_or(false, |(i, _)| *i == block_idx) { return Ok(()); }

        let entry      = self.index[block_idx as usize];
        let next_entry = self.index[block_idx as usize + 1];
        let is_plain   = (entry & 1) != 0;
        let offset     = ((entry      & !1) as u64) << self.align;
        let next_off   = ((next_entry & !1) as u64) << self.align;
        let comp_len   = (next_off - offset) as usize;

        self.file.seek(SeekFrom::Start(offset))?;
        let mut comp = vec![0u8; comp_len];
        self.file.read_exact(&mut comp)?;

        let block = if is_plain {
            comp
        } else {
            let mut dec = DeflateDecoder::new(&comp[..]);
            let mut out = vec![0u8; self.block_size as usize];
            dec.read_exact(&mut out).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            out
        };

        self.cache = Some((block_idx, block));
        Ok(())
    }
}

impl ISO9660Reader for CsoReader {
    // CSO stores 2048-byte logical sectors; `lba` maps 1:1 to block index when block_size==2048.
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> io::Result<usize> {
        let byte_pos   = lba * 2048;
        let block_idx  = byte_pos / self.block_size;
        let block_off  = (byte_pos % self.block_size) as usize;

        self.decompress_block(block_idx)?;

        let block = &self.cache.as_ref().unwrap().1;
        let avail  = block.len().saturating_sub(block_off);
        let to_copy = buf.len().min(avail);
        buf[..to_copy].copy_from_slice(&block[block_off..block_off + to_copy]);
        Ok(to_copy)
    }
}

fn open_cso_fs(path: &Path) -> Result<ISO9660<CsoReader>, String> {
    let reader = CsoReader::open(path)?;
    ISO9660::new(reader).map_err(|e| format!("ISO9660 (CSO): {e}"))
}

fn detect_filesystems_cso(path: &Path) -> Vec<String> {
    if open_cso_fs(path).is_ok() { vec!["ISO 9660".to_string()] } else { vec![] }
}

// ── ECM (Error Code Modeler) support ─────────────────────────────────────────
// ECM strips ECC/EDC parity bytes from CD sectors to compress them.
// Sector types stored: 0=raw bytes, 1=Mode1 (2048B user data),
// 2=Mode2 Form1 (2048B), 3=Mode2 Form2 (2336B).
// We decode by reading the stored bytes and reconstructing full 2352-byte raw sectors.

const ECM_MAGIC: &[u8; 4] = b"ECM\0";

pub struct EcmReader {
    // Fully decoded sector cache: sector_index → 2352 raw bytes.
    // Built lazily on first access; stored in a flat Vec for O(1) lookup.
    sectors: Vec<[u8; 2352]>,
}

impl EcmReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let raw = fs::read(path).map_err(|e| format!("Cannot read ECM: {e}"))?;
        if raw.len() < 4 || &raw[0..4] != ECM_MAGIC {
            return Err("Not an ECM file".to_string());
        }

        let mut pos = 4usize;
        let mut sectors: Vec<[u8; 2352]> = Vec::new();

        // Each ECM chunk: variable-length count encoding then data.
        // Count encoding: read bytes; low 7 bits = value fragment, high bit = more.
        // The type is in the low 2 bits of the FIRST byte before shifting.
        while pos < raw.len() {
            // Check for end marker (four 0xFF bytes).
            if pos + 4 <= raw.len() && raw[pos..pos+4] == [0xFF, 0xFF, 0xFF, 0xFF] { break; }

            let b0 = raw[pos]; pos += 1;
            let typ = b0 & 3;
            let mut count = ((b0 >> 2) & 0x1F) as u64;
            let mut shift = 5u32;

            // Continuation bytes for the count.
            while raw.get(pos).copied().map_or(false, |b| b & 0x80 != 0) {
                let b = raw[pos]; pos += 1;
                count |= ((b & 0x7F) as u64) << shift;
                shift += 7;
            }
            // One final byte (high bit clear).
            if let Some(&b) = raw.get(pos) {
                count |= (b as u64) << shift;
                pos += 1;
            }

            let n = count + 1; // actual number of units

            match typ {
                0 => {
                    // Type 0: n raw bytes (not sector-aligned; pad into sectors).
                    let bytes = raw.get(pos..pos + n as usize)
                        .ok_or_else(|| "ECM: truncated type-0 data".to_string())?;
                    pos += n as usize;
                    // Type-0 data is written verbatim into the output stream (rare in practice).
                    // Pad into 2352-byte sectors; leftover bytes go into the next sector.
                    let mut off = 0usize;
                    while off < bytes.len() {
                        let mut sec = [0u8; 2352];
                        let copy = (bytes.len() - off).min(2352);
                        sec[..copy].copy_from_slice(&bytes[off..off + copy]);
                        sectors.push(sec);
                        off += copy;
                    }
                }
                1 => {
                    // Type 1: n Mode1 sectors (sync+header+2048B user data stored; ECC reconstructed).
                    for _ in 0..n {
                        let mut sec = [0u8; 2352];
                        // Sync
                        sec[0] = 0x00;
                        sec[1..12].fill(0xFF); sec[11] = 0x00;
                        // Header (MSF + mode): read 4 bytes
                        let hdr = raw.get(pos..pos+4).ok_or_else(|| "ECM: truncated type-1 header".to_string())?;
                        sec[12..16].copy_from_slice(hdr); pos += 4;
                        sec[15] = 0x01; // Mode 1
                        // User data (2048 bytes)
                        let ud = raw.get(pos..pos+2048).ok_or_else(|| "ECM: truncated type-1 data".to_string())?;
                        sec[16..2064].copy_from_slice(ud); pos += 2048;
                        // EDC/ECC reconstruction skipped — user data is at bytes 16..2064.
                        // ECC bytes (2064..2352) left as zeros; sufficient for ISO9660 reads.
                        sectors.push(sec);
                    }
                }
                2 => {
                    // Type 2: n Mode2 Form1 sectors (sub-header + 2048B user data).
                    for _ in 0..n {
                        let mut sec = [0u8; 2352];
                        sec[0] = 0x00; sec[1..12].fill(0xFF); sec[11] = 0x00;
                        sec[15] = 0x02;
                        // Sub-header (8 bytes: repeated twice in raw sector at 16..24).
                        let sub = raw.get(pos..pos+4).ok_or_else(|| "ECM: truncated type-2 sub-hdr".to_string())?;
                        sec[16..20].copy_from_slice(sub);
                        sec[20..24].copy_from_slice(sub);
                        pos += 4;
                        // User data (2048 bytes at 24..2072)
                        let ud = raw.get(pos..pos+2048).ok_or_else(|| "ECM: truncated type-2 data".to_string())?;
                        sec[24..2072].copy_from_slice(ud); pos += 2048;
                        sectors.push(sec);
                    }
                }
                3 => {
                    // Type 3: n Mode2 Form2 sectors (sub-header + 2324B user data).
                    for _ in 0..n {
                        let mut sec = [0u8; 2352];
                        sec[0] = 0x00; sec[1..12].fill(0xFF); sec[11] = 0x00;
                        sec[15] = 0x02;
                        let sub = raw.get(pos..pos+4).ok_or_else(|| "ECM: truncated type-3 sub-hdr".to_string())?;
                        sec[16..20].copy_from_slice(sub);
                        sec[20..24].copy_from_slice(sub);
                        pos += 4;
                        // Form2 data (2324 bytes at 24..2348)
                        let ud = raw.get(pos..pos+2324).ok_or_else(|| "ECM: truncated type-3 data".to_string())?;
                        sec[24..2348].copy_from_slice(ud); pos += 2324;
                        sectors.push(sec);
                    }
                }
                _ => unreachable!(),
            }
        }

        Ok(EcmReader { sectors })
    }
}

impl ISO9660Reader for EcmReader {
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> io::Result<usize> {
        let sec = self.sectors.get(lba as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "ECM: LBA out of range"))?;
        // User data is at offset 16 for Mode1, 24 for Mode2 Form1/2. Determine from mode byte.
        let mode = sec[15];
        let udo: usize = match mode {
            2 => 24,
            _ => 16,
        };
        let avail = 2352usize.saturating_sub(udo);
        let to_copy = buf.len().min(avail);
        buf[..to_copy].copy_from_slice(&sec[udo..udo + to_copy]);
        Ok(to_copy)
    }
}

fn open_ecm_fs(path: &Path) -> Result<ISO9660<EcmReader>, String> {
    let reader = EcmReader::open(path)?;
    ISO9660::new(reader).map_err(|e| format!("ISO9660 (ECM): {e}"))
}

fn detect_filesystems_ecm(path: &Path) -> Vec<String> {
    if open_ecm_fs(path).is_ok() { vec!["ISO 9660".to_string()] } else { vec![] }
}

// ── DPM (Dynamic Position Measurement) ───────────────────────────────────────
// DPM records the angular spindle position at each sector so a virtual drive
// can replay copy-protection timing checks (e.g. SafeDisc). Each u32 sample
// is a raw angular velocity reading; resolution = sectors between samples.
//
// An image may contain multiple DPM blocks (e.g. one coarse + one fine-grained).
// A disc emulator should prefer the block with the smallest resolution (finest
// granularity) that covers the queried sector.

#[derive(Serialize, Clone)]
pub struct DpmBlock {
    pub start_sector: u32,
    pub resolution: u32,   // sectors between consecutive samples
    pub data: Vec<u32>,    // raw angular velocity samples, length = num_entries
}

impl DpmBlock {
    /// Last sector covered by this block (exclusive).
    pub fn end_sector(&self) -> u32 {
        self.start_sector.saturating_add(self.resolution.saturating_mul(self.data.len() as u32))
    }

    /// True if this block contains a sample for `sector`.
    pub fn covers(&self, sector: u32) -> bool {
        sector >= self.start_sector && sector < self.end_sector()
    }

    /// Angular velocity sample for `sector`, or None if out of range.
    /// Index = (sector - start_sector) / resolution; no interpolation.
    pub fn sample_at(&self, sector: u32) -> Option<u32> {
        if !self.covers(sector) { return None; }
        let idx = ((sector - self.start_sector) / self.resolution) as usize;
        self.data.get(idx).copied()
    }
}

#[derive(Serialize, Clone)]
pub struct DpmData {
    pub blocks: Vec<DpmBlock>,
}

impl DpmData {
    /// Best available angular velocity sample for `sector`.
    /// Prefers the block with the finest resolution (smallest value) that covers the sector.
    pub fn sample_at(&self, sector: u32) -> Option<u32> {
        self.blocks.iter()
            .filter(|b| b.covers(sector))
            .min_by_key(|b| b.resolution)
            .and_then(|b| b.sample_at(sector))
    }
}

fn parse_dpm_block(raw: &[u8], block_off: usize) -> Option<DpmBlock> {
    // Block layout: block_number(u32) · start_sector(u32) · resolution(u32) · num_entries(u32)
    if block_off + 16 > raw.len() { return None; }
    let start_sector = read_u32_le(raw, block_off + 4);
    let resolution   = read_u32_le(raw, block_off + 8);
    let num_entries  = read_u32_le(raw, block_off + 12) as usize;
    if resolution == 0 || num_entries == 0 { return None; }
    let data_off = block_off + 16;
    if data_off + num_entries * 4 > raw.len() { return None; }
    let data: Vec<u32> = (0..num_entries).map(|i| read_u32_le(raw, data_off + i * 4)).collect();
    Some(DpmBlock { start_sector, resolution, data })
}

fn parse_mds_dpm(mds_path: &Path) -> Option<DpmData> {
    let raw = fs::read(mds_path).ok()?;
    // MDS header is 0x58 bytes; dpm_blocks_offset is at 0x54 (last u32 in header).
    if raw.len() < 0x58 || !raw.starts_with(MDS_SIGNATURE) { return None; }

    let table_off = read_u32_le(&raw, 0x54) as usize;
    if table_off == 0 || table_off + 8 > raw.len() { return None; }

    let num_blocks = read_u32_le(&raw, table_off) as usize;
    if num_blocks == 0 { return None; }

    let mut blocks = Vec::with_capacity(num_blocks);
    for i in 0..num_blocks {
        let ptr_off = table_off + 4 + i * 4;
        if ptr_off + 4 > raw.len() { break; }
        let block_off = read_u32_le(&raw, ptr_off) as usize;
        if let Some(block) = parse_dpm_block(&raw, block_off) {
            blocks.push(block);
        }
    }

    if blocks.is_empty() { None } else { Some(DpmData { blocks }) }
}

fn parse_bwa_dpm(bwa_path: &Path) -> Option<DpmData> {
    // BlindWrite Angular sidecar (.bwa) — 11-word header before data:
    //   [u32×4] fixed signature words
    //   [u32] block_len1  [u32] block_len2  [u32×2] reserved
    //   [u32] start_sector  [u32] resolution  [u32] num_entries
    //   [u32×num_entries] data
    let raw = fs::read(bwa_path).ok()?;
    if raw.len() < 44 { return None; }
    let start_sector = read_u32_le(&raw, 32);
    let resolution   = read_u32_le(&raw, 36);
    let num_entries  = read_u32_le(&raw, 40) as usize;
    if resolution == 0 || num_entries == 0 { return None; }
    let data_off = 44;
    if data_off + num_entries * 4 > raw.len() { return None; }
    let data: Vec<u32> = (0..num_entries).map(|i| read_u32_le(&raw, data_off + i * 4)).collect();
    Some(DpmData { blocks: vec![DpmBlock { start_sector, resolution, data }] })
}

fn parse_b6t_dpm(b6t_path: &Path) -> Option<DpmData> {
    // B6T DPM lives inside B6T_DiscBlock_2 at a variable offset requiring full
    // sequential parsing of the B6T structure. The sidecar .bwa file is equivalent
    // and simpler; prefer it when present.
    let bwa_path = b6t_path.with_extension("bwa");
    parse_bwa_dpm(&bwa_path)
}

/// Return all DPM blocks for the given image file, or null if none present.
/// For a disc emulator, call `sample_at(sector)` on the result to get the
/// angular velocity at any sector (picks finest-resolution covering block).
#[tauri::command]
fn get_dpm_data(image_path: String) -> Option<DpmData> {
    let path = Path::new(&image_path);
    let lower = image_path.to_lowercase();
    if lower.ends_with(".mds") {
        parse_mds_dpm(path)
    } else if lower.ends_with(".b6t") || lower.ends_with(".b5t") {
        parse_b6t_dpm(path)
    } else {
        None
    }
}

/// Look up the angular velocity sample for a single sector.
/// Returns null if the image has no DPM or the sector is out of all block ranges.
#[tauri::command]
fn get_dpm_for_sector(image_path: String, sector: u32) -> Option<u32> {
    get_dpm_data(image_path)?.sample_at(sector)
}

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

    // Number of sessions is at 0x14 (2 bytes LE); sessions array starts at 0x50 (4 bytes LE).
    let num_sessions = {
        let n = u16::from_le_bytes([data[0x14], data[0x15]]) as usize;
        if n == 0 { 1 } else { n }
    };
    let first_session_offset = read_u32_le(&data, 0x50) as usize;

    let mut tracks: Vec<TrackInfo> = Vec::new();

    for s in 0..num_sessions {
        let sess_off = first_session_offset + s * MDS_SESSION_BLOCK_SIZE;
        if sess_off + MDS_SESSION_BLOCK_SIZE > data.len() { break; }

        // Session number field is at sess_off+8 (2 bytes LE).
        let session_number = u16::from_le_bytes([data[sess_off + 8], data[sess_off + 9]]) as u32;
        let session_num = if session_number > 0 { session_number } else { (s + 1) as u32 };

        let num_blocks = data[sess_off + 0x0A] as usize;
        let track_blocks_offset = read_u32_le(&data, sess_off + 0x14) as usize;

        for i in 0..num_blocks {
            let tb = track_blocks_offset + i * MDS_TRACK_BLOCK_SIZE;
            if tb + MDS_TRACK_BLOCK_SIZE > data.len() { break; }

            let mode_byte = data[tb];
            let point = data[tb + 4];
            if point == 0 || point > 99 { continue; }

            let is_data = mode_byte != 0x00 && mode_byte != 0xA9;

            // Sector size at +0x10; includes 0x60 subchannel bytes when subchan != 0.
            let sector_size = u16::from_le_bytes([data[tb + 0x10], data[tb + 0x11]]) as u64;
            let subchan_size = if data[tb + 1] != 0 { 0x60u64 } else { 0u64 };
            let base_size = sector_size.saturating_sub(subchan_size);
            let mode = match mode_byte {
                0x00 | 0xA9 => "AUDIO".to_string(),
                0xAB | 0x02 | 0x03 | 0x04 => "MODE2/2352".to_string(),
                0xAA | _ => if base_size <= 0x800 { "MODE1/2048".to_string() } else { "MODE1/2352".to_string() },
            };

            // PLBA: disc-absolute start LBA for this track (field at +0x24, u32 LE).
            let start_lba = read_u32_le(&data, tb + 0x24) as u64;

            // Extra/index block: pregap_sectors(u32) + track_length(u32) at absolute file offset.
            let extra_offset = read_u32_le(&data, tb + 0x0C) as usize;
            let num_sectors = if extra_offset + 8 <= data.len() {
                read_u32_le(&data, extra_offset + 4) as u64 // track length (excludes pregap)
            } else {
                0
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
            .creation_flags(CREATE_NO_WINDOW)
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
            ensure_cdemu_daemon()?;

            let load_out = syscmd("cdemu")
                .args(["load", "any", &image_path])
                .output()
                .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound {
                    "CDemu is not installed. Install the 'cdemu' package to mount this image type.".to_string()
                } else {
                    format!("cdemu load failed: {e}")
                })?;

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

            let new_dev = cdemu_device_for_slot(&slot)
                .ok_or_else(|| format!("CDemu: could not find device for slot {slot}"))?;

            // Mount via udisksctl; fall back to lsblk if auto-mounted by desktop.
            let mount_out = syscmd("udisksctl")
                .args(["mount", "-b", &new_dev])
                .output()
                .map_err(|e| format!("udisksctl mount failed: {e}"))?;

            let mount_point = if mount_out.status.success() {
                let text = String::from_utf8_lossy(&mount_out.stdout);
                text.split(" at ").nth(1).unwrap_or("").trim().trim_end_matches('.').to_string()
            } else {
                // Desktop environment may have auto-mounted it — query lsblk.
                let lsblk = syscmd("lsblk")
                    .args(["-no", "MOUNTPOINT", &new_dev])
                    .output()
                    .map_err(|e| format!("lsblk failed: {e}"))?;
                String::from_utf8_lossy(&lsblk.stdout).trim().to_string()
            };

            if mount_point.is_empty() {
                return Err("CDemu: could not determine mount point".to_string());
            }

            let device_key = format!("cdemu:{slot}:{new_dev}");
            state.0.lock().unwrap().push(device_key.clone());
            return Ok(MountResult { mount_point, device: device_key });
        }

        let loop_out = syscmd("udisksctl")
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

        let mount_out = syscmd("udisksctl")
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

// Parses `cdemu device-mapping` to find the /dev/srN path for a given slot.
fn cdemu_device_for_slot(slot: &str) -> Option<String> {
    let out = syscmd("cdemu").args(["device-mapping"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut cols = line.split_whitespace();
        if cols.next()? == slot {
            return cols.next().map(|s| s.to_string());
        }
    }
    None
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
            .creation_flags(CREATE_NO_WINDOW)
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
            let _ = syscmd("udisksctl").args(["unmount", "-b", dev]).output();
            let out = syscmd("cdemu")
                .args(["unload", slot])
                .output()
                .map_err(|e| format!("cdemu unload failed: {e}"))?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
        } else {
            let _ = syscmd("udisksctl").args(["unmount", "-b", &device]).output();
            let out = syscmd("udisksctl")
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
            .creation_flags(CREATE_NO_WINDOW)
            .args(["-NoProfile", "-Command", &script])
            .output();
    }

    #[cfg(target_os = "linux")]
    for device in devices {
        if device.starts_with("cdemu:") {
            let parts: Vec<&str> = device.splitn(3, ':').collect();
            let slot = parts.get(1).copied().unwrap_or("0");
            let dev = parts.get(2).copied().unwrap_or("");
            let _ = syscmd("udisksctl").args(["unmount", "-b", dev]).output();
        } else {
            let _ = syscmd("udisksctl").args(["unmount", "-b", device]).output();
            let _ = syscmd("udisksctl").args(["loop-delete", "-b", device]).output();
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

// ── CDemu installation helpers (Linux) ───────────────────────────────────────

#[tauri::command]
fn check_cdemu_installed() -> bool {
    #[cfg(target_os = "linux")]
    {
        syscmd("which").arg("cdemu")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    false
}

#[cfg(target_os = "linux")]
fn ensure_vhba_permissions() -> Result<(), String> {
    if !std::path::Path::new("/dev/vhba_ctl").exists() {
        return Err("The vhba kernel module is not loaded. Install the vhba-module package.".to_string());
    }

    if std::fs::OpenOptions::new().read(true).open("/dev/vhba_ctl").is_ok() {
        return Ok(());
    }

    // Grant world read/write on the device for this session.
    let chmod = syscmd("pkexec")
        .args(["chmod", "a+rw", "/dev/vhba_ctl"])
        .output()
        .map_err(|e| format!("pkexec not found: {e}"))?;

    if !chmod.status.success() {
        return Err("Administrator access is required to configure CDEmu device permissions.".to_string());
    }

    // Add the user to the cdemu group so future logins won't need this prompt.
    if let Ok(user) = std::env::var("USER") {
        let _ = syscmd("pkexec")
            .args(["usermod", "-aG", "cdemu", &user])
            .output();
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_cdemu_daemon() -> Result<(), String> {
    ensure_vhba_permissions()?;

    let is_active = syscmd("systemctl")
        .args(["--user", "is-active", "cdemu-daemon"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    if is_active == "active" {
        return Ok(());
    }

    // Clear a previous failure so systemd will let us start it again.
    if is_active == "failed" {
        let _ = syscmd("systemctl")
            .args(["--user", "reset-failed", "cdemu-daemon"])
            .output();
    }

    let started = syscmd("systemctl")
        .args(["--user", "start", "cdemu-daemon"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if started {
        std::thread::sleep(std::time::Duration::from_millis(1000));
        return Ok(());
    }

    Err("CDEmu daemon could not be started. Please check that cdemu-daemon is installed.".to_string())
}

#[cfg(target_os = "linux")]
fn cdemu_install_args() -> Option<(String, Vec<String>)> {
    let managers: &[(&str, &[&str])] = &[
        ("pacman",  &["-S", "--noconfirm", "cdemu-client"]),
        ("apt-get", &["install", "-y", "cdemu-client"]),
        ("dnf",     &["install", "-y", "cdemu-client"]),
        ("zypper",  &["install", "-y", "cdemu-client"]),
        ("eopkg",   &["install", "cdemu-client"]),
    ];
    for (bin, args) in managers {
        if syscmd("which").arg(bin).output().map(|o| o.status.success()).unwrap_or(false) {
            return Some((bin.to_string(), args.iter().map(|s| s.to_string()).collect()));
        }
    }
    None
}

#[tauri::command]
fn install_cdemu() -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        let (pm, args) = cdemu_install_args()
            .ok_or_else(|| "Could not detect a supported package manager. Install cdemu-client manually.".to_string())?;

        let out = syscmd("pkexec")
            .arg(&pm)
            .args(&args)
            .output()
            .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound {
                "pkexec not found. Install cdemu-client manually using your package manager.".to_string()
            } else {
                format!("Installation failed: {e}")
            })?;

        if out.status.success() {
            Ok("cdemu-client installed successfully.".to_string())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if stderr.is_empty() { "Installation cancelled or failed.".to_string() } else { stderr })
        }
    }
    #[cfg(not(target_os = "linux"))]
    Err("Not supported on this platform.".to_string())
}

// ── CDemu drive emulation ─────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct EmulatedDrive {
    pub slot: String,
    pub device: String,
    pub image_path: String,
}

pub struct EmulatedDrives(pub Mutex<Vec<EmulatedDrive>>);

pub struct WiiUKeyState(pub Mutex<Option<PathBuf>>);

pub struct RedumperDumpState(pub Arc<Mutex<Option<tauri_plugin_shell::process::CommandChild>>>);

fn redumper_cmd(
    source: &str,
    external_path: Option<&str>,
    app: &tauri::AppHandle,
) -> Result<tauri_plugin_shell::process::Command, String> {
    use tauri_plugin_shell::ShellExt;
    let cmd = if source == "external" {
        let p = external_path.ok_or("No external redumper path configured")?;
        app.shell().command(p)
    } else {
        app.shell().sidecar("redumper").map_err(|e| e.to_string())?
    };
    // The bundled macOS binary has @executable_path/../lib rpath for libc++.
    // Point DYLD_LIBRARY_PATH at the bundled dylibs so no binary patching is needed.
    #[cfg(target_os = "macos")]
    let cmd = {
        let lib_dir = app.path().resource_dir()
            .map(|p| p.join("lib"))
            .unwrap_or_default();
        cmd.env("DYLD_LIBRARY_PATH", lib_dir.to_string_lossy().as_ref())
    };
    Ok(cmd)
}

#[tauri::command]
async fn get_redumper_version(
    source: String,
    external_path: Option<String>,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let out = redumper_cmd(&source, external_path.as_deref(), &app)?
        .args(["--version"])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).to_string()
        + &String::from_utf8_lossy(&out.stderr);
    Ok(text.lines().find(|l| !l.trim().is_empty()).unwrap_or("unknown").to_string())
}

#[tauri::command]
async fn start_redumper_dump(
    drive: String,
    output_path: String,
    source: String,
    external_path: Option<String>,
    app: tauri::AppHandle,
    dump_state: tauri::State<'_, RedumperDumpState>,
) -> Result<(), String> {
    use tauri_plugin_shell::process::CommandEvent;
    let (mut rx, child) = redumper_cmd(&source, external_path.as_deref(), &app)?
        .args(["dump",
               &format!("--drive={drive}"),
               &format!("--image-path={output_path}"),
               "--drive-type=GENERIC", "--force-split", "--leave-unchanged"])
        .spawn()
        .map_err(|e| format!("Failed to start redumper: {e}"))?;
    let child_arc = dump_state.0.clone();
    *child_arc.lock().unwrap() = Some(child);
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line) => {
                    let _ = app2.emit("redumper-log", String::from_utf8_lossy(&line).to_string());
                }
                CommandEvent::Stderr(line) => {
                    let _ = app2.emit("redumper-log", String::from_utf8_lossy(&line).to_string());
                }
                CommandEvent::Terminated(status) => {
                    let _ = app2.emit("redumper-done", status.code.unwrap_or(-1));
                    *child_arc.lock().unwrap() = None;
                    break;
                }
                _ => {}
            }
        }
    });
    Ok(())
}

#[tauri::command]
fn cancel_redumper_dump(dump_state: tauri::State<'_, RedumperDumpState>) -> Result<(), String> {
    if let Some(child) = dump_state.0.lock().unwrap().take() {
        child.kill().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn organize_dump_logs(dir: String) -> Result<(), String> {
    let dir = std::path::Path::new(&dir);
    let image_exts: &[&str] = &["iso", "bin", "cue"];
    let entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter(|e| {
            let ext = e.path().extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            !image_exts.contains(&ext.as_str())
        })
        .collect();
    if entries.is_empty() {
        return Ok(());
    }
    let logs_dir = dir.join("logs");
    std::fs::create_dir_all(&logs_dir).map_err(|e| e.to_string())?;
    for entry in entries {
        let dest = logs_dir.join(entry.file_name());
        std::fs::rename(entry.path(), &dest).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn set_wiiu_key_path(path: Option<String>, state: tauri::State<WiiUKeyState>) {
    *state.0.lock().unwrap() = path.map(PathBuf::from);
}

#[tauri::command]
fn get_wiiu_key_path(state: tauri::State<WiiUKeyState>) -> Option<String> {
    state.0.lock().unwrap().as_ref().map(|p| p.to_string_lossy().into_owned())
}

// ── Detached Sector View window ───────────────────────────────────────────────

#[derive(Serialize, Clone)]
struct SectorViewInitParams {
    image_path: String,
    lba: u64,
    compare_image_path: Option<String>,
}

struct SectorViewParamStore(Mutex<std::collections::HashMap<String, SectorViewInitParams>>);

#[tauri::command]
fn open_sector_view_window(
    app: tauri::AppHandle,
    store: tauri::State<'_, SectorViewParamStore>,
    image_path: String,
    lba: u64,
    compare_image_path: Option<String>,
) -> Result<(), String> {
    let label = format!("sv{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis());
    store.0.lock().unwrap().insert(label.clone(), SectorViewInitParams { image_path, lba, compare_image_path });
    tauri::WebviewWindowBuilder::new(&app, label, tauri::WebviewUrl::App("index.html".into()))
        .title("Sector View — Disc Xplorer")
        .inner_size(920.0, 680.0)
        .min_inner_size(600.0, 400.0)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn claim_sector_view_params(
    window: tauri::WebviewWindow,
    store: tauri::State<'_, SectorViewParamStore>,
) -> Option<SectorViewInitParams> {
    store.0.lock().unwrap().remove(window.label())
}

#[tauri::command]
fn emulate_drive(
    image_path: String,
    state: tauri::State<EmulatedDrives>,
) -> Result<EmulatedDrive, String> {
    #[cfg(not(target_os = "linux"))]
    { let _ = (image_path, state); return Err("Drive emulation via CDemu is only available on Linux.".to_string()); }

    #[cfg(target_os = "linux")]
    {
        ensure_cdemu_daemon()?;

        let try_load = || syscmd("cdemu")
            .args(["load", "any", &image_path])
            .output()
            .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound {
                "CDemu is not installed. Install the 'cdemu' package to emulate drives.".to_string()
            } else {
                format!("cdemu load failed: {e}")
            });

        let load_out = {
            let first = try_load()?;
            if !first.status.success()
                && String::from_utf8_lossy(&first.stderr).contains("No empty device found")
            {
                // All slots occupied — add one and retry.
                let _ = syscmd("cdemu").args(["add-device"]).output();
                try_load()?
            } else {
                first
            }
        };

        if !load_out.status.success() {
            return Err(String::from_utf8_lossy(&load_out.stderr).trim().to_string());
        }

        let slot = String::from_utf8_lossy(&load_out.stdout)
            .split_whitespace()
            .last()
            .map(|w| w.trim_end_matches('.').to_string())
            .filter(|w| w.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or_else(|| "0".to_string());

        let device = cdemu_device_for_slot(&slot)
            .ok_or_else(|| format!("CDemu: could not find device for slot {slot}"))?;

        let drive = EmulatedDrive { slot, device, image_path };
        state.0.lock().unwrap().push(drive.clone());
        Ok(drive)
    }
}

#[tauri::command]
fn eject_emulated_drive(
    slot: String,
    state: tauri::State<EmulatedDrives>,
) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    { let _ = (slot, state); return Err("Drive emulation via CDemu is only available on Linux.".to_string()); }

    #[cfg(target_os = "linux")]
    {
        let out = syscmd("cdemu")
            .args(["unload", &slot])
            .output()
            .map_err(|e| format!("cdemu unload failed: {e}"))?;

        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }

        state.0.lock().unwrap().retain(|d| d.slot != slot);
        Ok(())
    }
}

#[tauri::command]
fn list_emulated_drives(state: tauri::State<EmulatedDrives>) -> Vec<EmulatedDrive> {
    state.0.lock().unwrap().clone()
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
            // Use the full BIN file size so sector counts include the pregap,
            // matching what disc authoring tools report per-track.
            fs::metadata(&rt.bin_path)
                .map(|m| m.len() / RAW_SECTOR_SIZE)
                .unwrap_or(0)
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

fn read_sector_impl(image_path: &str, lba: u64) -> Result<SectorData, String> {
    let path = Path::new(image_path);
    let lower = image_path.to_lowercase();

    let (file_path, sector_size, user_data_offset, data_offset): (PathBuf, u64, u64, u64) = if lower.ends_with(".cue") {
        let tracks = parse_cue_all_data_tracks(path)?;
        let track = tracks.into_iter().next().ok_or("No data track in CUE")?;
        (track.bin_path, RAW_SECTOR_SIZE, track.user_data_offset, track.track_offset)
    } else if lower.ends_with(".mds") {
        let track = parse_mds_for_data_track(path)?;
        (track.bin_path, RAW_SECTOR_SIZE, track.user_data_offset, track.track_offset)
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
    } else if lower.ends_with(".cso") || lower.ends_with(".ciso") {
        let mut reader = CsoReader::open(path).map_err(|e| format!("CSO: {e}"))?;
        let total_sectors = reader.total_bytes / 2048;
        if total_sectors == 0 { return Err("CSO is empty".to_string()); }
        if lba >= total_sectors {
            return Err(format!("Sector {lba} out of range (0–{})", total_sectors - 1));
        }
        let mut bytes = vec![0u8; 2048];
        reader.read_at(&mut bytes, lba).map_err(|e| format!("Read error: {e}"))?;
        return Ok(SectorData { bytes, sector_size: 2048, user_data_offset: 0, total_sectors, lba });
    } else if lower.ends_with(".ecm") {
        let reader = EcmReader::open(path).map_err(|e| format!("ECM: {e}"))?;
        let total_sectors = reader.sectors.len() as u64;
        if total_sectors == 0 { return Err("ECM is empty".to_string()); }
        if lba >= total_sectors {
            return Err(format!("Sector {lba} out of range (0–{})", total_sectors - 1));
        }
        let sec = reader.sectors[lba as usize];
        const SYNC: [u8; 12] = [0x00,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0x00];
        let udo = if sec[..12] == SYNC { if sec[15] == 2 { 24u32 } else { 16u32 } } else { 0u32 };
        return Ok(SectorData { bytes: sec.to_vec(), sector_size: 2352, user_data_offset: udo, total_sectors, lba });
    } else if lower.ends_with(".b5t") || lower.ends_with(".b6t") {
        let track = parse_b5t_for_data_track(path)?;
        (track.bin_path, track.stride, track.user_data_offset, track.track_offset)
    } else if lower.ends_with(".cif") {
        let track = parse_cif_for_data_track(path)?;
        (track.bin_path, track.stride, track.user_data_offset, track.track_offset)
    } else if lower.ends_with(".uif") {
        let mut reader = UifReader::open(path).map_err(|e| format!("UIF: {e}"))?;
        let total_sectors = reader.total_sectors();
        if total_sectors == 0 { return Err("UIF is empty".to_string()); }
        if lba >= total_sectors {
            return Err(format!("Sector {lba} out of range (0–{})", total_sectors - 1));
        }
        let ss = reader.sector_size as usize;
        let mut bytes = vec![0u8; ss];
        reader.read_at(&mut bytes, lba).map_err(|e| format!("Read error: {e}"))?;
        const SYNC: [u8; 12] = [0x00,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0x00];
        let udo = if ss > 2048 && bytes.len() >= 16 && bytes[0..12] == SYNC {
            if bytes[15] == 2 { 24u32 } else { 16u32 }
        } else { 0u32 };
        return Ok(SectorData { bytes, sector_size: ss as u32, user_data_offset: udo, total_sectors, lba });
    } else if lower.ends_with(".wbfs") {
        let mut reader = wbfs_reader::WbfsReader::open(path).map_err(|e| format!("WBFS: {e}"))?;
        let total_sectors = reader.disc_size() / 2048;
        if total_sectors == 0 { return Err("WBFS is empty".to_string()); }
        if lba >= total_sectors {
            return Err(format!("Sector {lba} out of range (0–{})", total_sectors - 1));
        }
        reader.seek(SeekFrom::Start(lba * 2048)).map_err(|e| format!("Seek error: {e}"))?;
        let mut bytes = vec![0u8; 2048];
        reader.read_exact(&mut bytes).map_err(|e| format!("Read error: {e}"))?;
        return Ok(SectorData { bytes, sector_size: 2048, user_data_offset: 0, total_sectors, lba });
    } else if lower.ends_with(".skeleton.zst") || lower.ends_with(".iso.zst") || lower.ends_with(".img.zst") {
        let reader = ZstReader::open(path).map_err(|e| format!("ZST: {e}"))?;
        let ss = reader.sector_size;
        let total_sectors = reader.data.len() as u64 / ss;
        if total_sectors == 0 { return Err("ZST image is empty".to_string()); }
        if lba >= total_sectors {
            return Err(format!("Sector {lba} out of range (0–{})", total_sectors - 1));
        }
        let pos = (lba * ss) as usize;
        let bytes = reader.data[pos..pos + ss as usize].to_vec();
        let udo = reader.user_data_offset as u32;
        return Ok(SectorData { bytes, sector_size: ss as u32, user_data_offset: udo, total_sectors, lba });
    } else if lower.ends_with(".wux") {
        let mut reader = wux_reader::WuxReader::open(path).map_err(|e| format!("WUX: {e}"))?;
        let total_sectors = reader.total_bytes() / 2048;
        if total_sectors == 0 { return Err("WUX is empty".to_string()); }
        if lba >= total_sectors {
            return Err(format!("Sector {lba} out of range (0–{})", total_sectors - 1));
        }
        reader.seek(SeekFrom::Start(lba * 2048)).map_err(|e| format!("Seek error: {e}"))?;
        let mut bytes = vec![0u8; 2048];
        reader.read_exact(&mut bytes).map_err(|e| format!("Read error: {e}"))?;
        return Ok(SectorData { bytes, sector_size: 2048, user_data_offset: 0, total_sectors, lba });
    } else if lower.ends_with(".aif") || lower.ends_with(".sdram") || lower.ends_with(".sbram") {
        return Err("Sector view not supported for this format".to_string());
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

#[tauri::command]
fn read_sector(image_path: String, lba: u64) -> Result<SectorData, String> {
    read_sector_impl(&image_path, lba)
}

struct FlatInfo {
    path: PathBuf,
    sector_size: u64,
    data_offset: u64,
    total_sectors: u64,
}

fn flat_info(image_path: &str) -> Option<FlatInfo> {
    let path = Path::new(image_path);
    let lower = image_path.to_lowercase();
    // Compressed / special formats — cannot do raw bulk reads.
    if lower.ends_with(".chd") || lower.ends_with(".cso") || lower.ends_with(".ciso")
        || lower.ends_with(".ecm") || lower.ends_with(".uif") || lower.ends_with(".wbfs")
        || lower.ends_with(".wux") || lower.ends_with(".skeleton.zst")
        || lower.ends_with(".iso.zst") || lower.ends_with(".img.zst")
        || lower.ends_with(".aif") || lower.ends_with(".sdram") || lower.ends_with(".sbram")
        || lower.ends_with(".wud")
    {
        return None;
    }
    let (file_path, sector_size, data_offset): (PathBuf, u64, u64) = if lower.ends_with(".cue") {
        let track = parse_cue_all_data_tracks(path).ok()?.into_iter().next()?;
        (track.bin_path, RAW_SECTOR_SIZE, track.track_offset)
    } else if lower.ends_with(".mds") {
        let track = parse_mds_for_data_track(path).ok()?;
        (track.bin_path, RAW_SECTOR_SIZE, track.track_offset)
    } else if lower.ends_with(".nrg") {
        let track = parse_nrg_for_data_track(path).ok()?;
        let ss = if track.user_data_offset > 0 { RAW_SECTOR_SIZE } else { 2048 };
        (track.bin_path, ss, track.track_offset)
    } else if lower.ends_with(".ccd") {
        let track = parse_ccd_for_data_track(path).ok()?;
        (track.bin_path, RAW_SECTOR_SIZE, track.track_offset)
    } else if lower.ends_with(".cdi") {
        let track = parse_cdi_for_data_track(path).ok()?;
        (track.bin_path, track.stride, track.track_offset)
    } else if lower.ends_with(".gdi") {
        let track = parse_gdi_for_data_track(path).ok()?;
        (track.bin_path, track.stride, track.track_offset)
    } else if lower.ends_with(".b5t") || lower.ends_with(".b6t") {
        let track = parse_b5t_for_data_track(path).ok()?;
        (track.bin_path, track.stride, track.track_offset)
    } else if lower.ends_with(".cif") {
        let track = parse_cif_for_data_track(path).ok()?;
        (track.bin_path, track.stride, track.track_offset)
    } else if lower.ends_with(".mdx") {
        let (ss, _) = mdx_sector_format(path);
        (path.to_path_buf(), ss, MDX_DATA_OFFSET)
    } else {
        let udo = detect_raw_sector_offset(path).unwrap_or(0);
        let ss = if udo > 0 { RAW_SECTOR_SIZE } else { 2048u64 };
        (path.to_path_buf(), ss, 0)
    };
    let file_len = fs::metadata(&file_path).ok()?.len();
    let total_sectors = file_len.saturating_sub(data_offset) / sector_size;
    if total_sectors == 0 { return None; }
    Some(FlatInfo { path: file_path, sector_size, data_offset, total_sectors })
}

// Fast bulk comparison for flat (non-compressed) images with the same sector stride.
// Reads both files CHUNK_BYTES at a time; within a differing chunk, finds the exact sector.
fn find_diff_flat(
    a: &FlatInfo, b: &FlatInfo,
    scan_start: u64, scan_end: u64, forward: bool,
) -> Result<Option<u64>, String> {
    const CHUNK_BYTES: u64 = 1 << 20; // 1 MB
    let ss = a.sector_size;
    let chunk_sectors = (CHUNK_BYTES / ss).max(1);

    let mut fa = File::open(&a.path).map_err(|e| e.to_string())?;
    let mut fb = File::open(&b.path).map_err(|e| e.to_string())?;
    let mut buf_a = vec![0u8; (chunk_sectors * ss) as usize];
    let mut buf_b = vec![0u8; (chunk_sectors * ss) as usize];

    if forward {
        let mut lba = scan_start;
        while lba <= scan_end {
            let batch = chunk_sectors.min(scan_end - lba + 1);
            let n = (batch * ss) as usize;
            fa.seek(SeekFrom::Start(a.data_offset + lba * ss)).map_err(|e| e.to_string())?;
            fb.seek(SeekFrom::Start(b.data_offset + lba * ss)).map_err(|e| e.to_string())?;
            fa.read_exact(&mut buf_a[..n]).map_err(|e| e.to_string())?;
            fb.read_exact(&mut buf_b[..n]).map_err(|e| e.to_string())?;
            if buf_a[..n] != buf_b[..n] {
                for i in 0..batch {
                    let s = (i * ss) as usize;
                    let e2 = s + ss as usize;
                    if buf_a[s..e2] != buf_b[s..e2] { return Ok(Some(lba + i)); }
                }
            }
            lba += batch;
        }
    } else {
        let mut lba_end = scan_end;
        loop {
            let batch = chunk_sectors.min(lba_end - scan_start + 1);
            let batch_start = lba_end + 1 - batch;
            let n = (batch * ss) as usize;
            fa.seek(SeekFrom::Start(a.data_offset + batch_start * ss)).map_err(|e| e.to_string())?;
            fb.seek(SeekFrom::Start(b.data_offset + batch_start * ss)).map_err(|e| e.to_string())?;
            fa.read_exact(&mut buf_a[..n]).map_err(|e| e.to_string())?;
            fb.read_exact(&mut buf_b[..n]).map_err(|e| e.to_string())?;
            if buf_a[..n] != buf_b[..n] {
                for i in (0..batch).rev() {
                    let s = (i * ss) as usize;
                    let e2 = s + ss as usize;
                    if buf_a[s..e2] != buf_b[s..e2] { return Ok(Some(batch_start + i)); }
                }
            }
            if batch_start == scan_start { break; }
            lba_end = batch_start - 1;
        }
    }
    Ok(None)
}

#[tauri::command]
fn find_diff_sector(
    image_path_a: String,
    image_path_b: String,
    from_lba: u64,
    forward: bool,
    inclusive: bool,
) -> Result<Option<u64>, String> {
    let total = read_sector_impl(&image_path_a, 0)
        .map(|s| s.total_sectors)
        .unwrap_or(0);
    if total == 0 { return Ok(None); }

    let (scan_start, scan_end) = if forward {
        let start = if inclusive { from_lba } else { from_lba.saturating_add(1) };
        (start, total - 1)
    } else {
        if from_lba == 0 { return Ok(None); }
        (0, from_lba - 1)
    };
    if scan_start > scan_end { return Ok(None); }

    // Fast path: both images are flat files with the same sector stride.
    if let (Some(ia), Some(ib)) = (flat_info(&image_path_a), flat_info(&image_path_b)) {
        if ia.sector_size == ib.sector_size {
            return find_diff_flat(&ia, &ib, scan_start, scan_end, forward);
        }
    }

    // Slow fallback: per-sector reads (necessary for compressed formats).
    if forward {
        for lba in scan_start..=scan_end {
            let a = read_sector_impl(&image_path_a, lba).map(|s| s.bytes).unwrap_or_default();
            let b = read_sector_impl(&image_path_b, lba).map(|s| s.bytes).unwrap_or_default();
            let len = a.len().max(b.len());
            if (0..len).any(|i| a.get(i) != b.get(i)) { return Ok(Some(lba)); }
        }
    } else {
        for lba in (scan_start..=scan_end).rev() {
            let a = read_sector_impl(&image_path_a, lba).map(|s| s.bytes).unwrap_or_default();
            let b = read_sector_impl(&image_path_b, lba).map(|s| s.bytes).unwrap_or_default();
            let len = a.len().max(b.len());
            if (0..len).any(|i| a.get(i) != b.get(i)) { return Ok(Some(lba)); }
        }
    }
    Ok(None)
}

// ── Sector range export ───────────────────────────────────────────────────────

#[tauri::command]
async fn export_sector_range(
    image_path: String,
    lba_start: u64,
    lba_end: u64,
    dest_path: String,
) -> Result<u64, String> {
    if lba_end < lba_start {
        return Err("End LBA must be >= start LBA".to_string());
    }
    let count = lba_end - lba_start + 1;
    let path = Path::new(&image_path);
    let lower = image_path.to_lowercase();

    let mut dest = File::create(&dest_path)
        .map_err(|e| format!("Cannot create output: {e}"))?;

    // ── compressed / special-reader formats ──────────────────────────────────

    if lower.ends_with(".chd") {
        let file = File::open(path).map_err(|e| format!("Cannot open CHD: {e}"))?;
        let chd = Chd::open(BufReader::new(file), None)
            .map_err(|e| format!("Cannot parse CHD: {e}"))?;
        let stride = chd_stride(chd.header().hunk_size() as u64, chd.header().unit_bytes() as u64);
        let total = if stride > 0 { chd.header().logical_bytes() / stride } else { 0 };
        if lba_end >= total {
            return Err(format!("LBA {lba_end} out of range (0–{})", total.saturating_sub(1)));
        }
        let mut reader = ChdReader::new(chd);
        let mut buf = vec![0u8; stride as usize];
        for lba in lba_start..=lba_end {
            reader.seek(SeekFrom::Start(lba * stride))
                .map_err(|e| format!("Seek error at LBA {lba}: {e}"))?;
            reader.read_exact(&mut buf)
                .map_err(|e| format!("Read error at LBA {lba}: {e}"))?;
            dest.write_all(&buf).map_err(|e| format!("Write error: {e}"))?;
        }
        return Ok(count);
    }

    if lower.ends_with(".cso") || lower.ends_with(".ciso") {
        let mut reader = CsoReader::open(path).map_err(|e| format!("CSO: {e}"))?;
        let total = reader.total_bytes / 2048;
        if lba_end >= total {
            return Err(format!("LBA {lba_end} out of range (0–{})", total.saturating_sub(1)));
        }
        let mut buf = vec![0u8; 2048];
        for lba in lba_start..=lba_end {
            reader.read_at(&mut buf, lba).map_err(|e| format!("Read error at LBA {lba}: {e}"))?;
            dest.write_all(&buf).map_err(|e| format!("Write error: {e}"))?;
        }
        return Ok(count);
    }

    if lower.ends_with(".ecm") {
        let reader = EcmReader::open(path).map_err(|e| format!("ECM: {e}"))?;
        let total = reader.sectors.len() as u64;
        if lba_end >= total {
            return Err(format!("LBA {lba_end} out of range (0–{})", total.saturating_sub(1)));
        }
        for lba in lba_start..=lba_end {
            dest.write_all(&reader.sectors[lba as usize])
                .map_err(|e| format!("Write error: {e}"))?;
        }
        return Ok(count);
    }

    if lower.ends_with(".uif") {
        let mut reader = UifReader::open(path).map_err(|e| format!("UIF: {e}"))?;
        let total = reader.total_sectors();
        let ss = reader.sector_size as usize;
        if lba_end >= total {
            return Err(format!("LBA {lba_end} out of range (0–{})", total.saturating_sub(1)));
        }
        let mut buf = vec![0u8; ss];
        for lba in lba_start..=lba_end {
            reader.read_at(&mut buf, lba).map_err(|e| format!("Read error at LBA {lba}: {e}"))?;
            dest.write_all(&buf).map_err(|e| format!("Write error: {e}"))?;
        }
        return Ok(count);
    }

    if lower.ends_with(".wbfs") {
        let mut reader = wbfs_reader::WbfsReader::open(path)
            .map_err(|e| format!("WBFS: {e}"))?;
        let total = reader.disc_size() / 2048;
        if lba_end >= total {
            return Err(format!("LBA {lba_end} out of range (0–{})", total.saturating_sub(1)));
        }
        let mut buf = vec![0u8; 2048];
        for lba in lba_start..=lba_end {
            reader.seek(SeekFrom::Start(lba * 2048))
                .map_err(|e| format!("Seek error at LBA {lba}: {e}"))?;
            reader.read_exact(&mut buf)
                .map_err(|e| format!("Read error at LBA {lba}: {e}"))?;
            dest.write_all(&buf).map_err(|e| format!("Write error: {e}"))?;
        }
        return Ok(count);
    }

    if lower.ends_with(".wux") || lower.ends_with(".wud") {
        let (mut reader, _) = open_wiiu_disc(path)?;
        let total_bytes = if lower.ends_with(".wud") {
            fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        } else {
            wux_reader::WuxReader::open(path).map(|r| r.total_bytes()).unwrap_or(0)
        };
        let total = total_bytes / 2048;
        if lba_end >= total {
            return Err(format!("LBA {lba_end} out of range (0–{})", total.saturating_sub(1)));
        }
        let mut buf = vec![0u8; 2048];
        for lba in lba_start..=lba_end {
            reader.seek(SeekFrom::Start(lba * 2048))
                .map_err(|e| format!("Seek error at LBA {lba}: {e}"))?;
            reader.read_exact(&mut buf)
                .map_err(|e| format!("Read error at LBA {lba}: {e}"))?;
            dest.write_all(&buf).map_err(|e| format!("Write error: {e}"))?;
        }
        return Ok(count);
    }

    if lower.ends_with(".skeleton.zst") || lower.ends_with(".iso.zst") || lower.ends_with(".img.zst") {
        let mut reader = ZstReader::open(path).map_err(|e| format!("ZST: {e}"))?;
        let ss = reader.sector_size;
        let total = reader.data.len() as u64 / ss;
        if lba_end >= total {
            return Err(format!("LBA {lba_end} out of range (0–{})", total.saturating_sub(1)));
        }
        let mut buf = vec![0u8; ss as usize];
        for lba in lba_start..=lba_end {
            reader.read_at(&mut buf, lba).map_err(|e| format!("Read error at LBA {lba}: {e}"))?;
            dest.write_all(&buf).map_err(|e| format!("Write error: {e}"))?;
        }
        return Ok(count);
    }

    // ── file-based formats (seek + read loop) ────────────────────────────────

    let (file_path, sector_size, data_offset): (PathBuf, u64, u64) = if lower.ends_with(".cue") {
        let tracks = parse_cue_all_data_tracks(path)?;
        let track = tracks.into_iter().next().ok_or("No data track in CUE")?;
        (track.bin_path, RAW_SECTOR_SIZE, track.track_offset)
    } else if lower.ends_with(".mds") {
        let track = parse_mds_for_data_track(path)?;
        (track.bin_path, RAW_SECTOR_SIZE, track.track_offset)
    } else if lower.ends_with(".nrg") {
        let track = parse_nrg_for_data_track(path)?;
        let ss = if track.user_data_offset > 0 { RAW_SECTOR_SIZE } else { 2048 };
        (track.bin_path, ss, track.track_offset)
    } else if lower.ends_with(".ccd") {
        let track = parse_ccd_for_data_track(path)?;
        (track.bin_path, RAW_SECTOR_SIZE, track.track_offset)
    } else if lower.ends_with(".cdi") {
        let track = parse_cdi_for_data_track(path)?;
        (track.bin_path, track.stride, track.track_offset)
    } else if lower.ends_with(".gdi") {
        let track = parse_gdi_for_data_track(path)?;
        (track.bin_path, track.stride, track.track_offset)
    } else if lower.ends_with(".b5t") || lower.ends_with(".b6t") {
        let track = parse_b5t_for_data_track(path)?;
        (track.bin_path, RAW_SECTOR_SIZE, track.track_offset)
    } else if lower.ends_with(".cif") {
        let track = parse_cif_for_data_track(path)?;
        (track.bin_path, track.stride, track.track_offset)
    } else if lower.ends_with(".mdx") {
        let (ss, _) = mdx_sector_format(path);
        (path.to_path_buf(), ss, MDX_DATA_OFFSET)
    } else {
        let udo = detect_raw_sector_offset(path).unwrap_or(0);
        (path.to_path_buf(), if udo > 0 { RAW_SECTOR_SIZE } else { 2048u64 }, 0)
    };

    let file_len = fs::metadata(&file_path)
        .map_err(|e| format!("Cannot stat image: {e}"))?.len();
    let total = file_len.saturating_sub(data_offset) / sector_size;
    if lba_end >= total {
        return Err(format!("LBA {lba_end} out of range (0–{})", total.saturating_sub(1)));
    }

    let mut src = File::open(&file_path).map_err(|e| format!("Cannot open: {e}"))?;
    let mut buf = vec![0u8; sector_size as usize];
    for lba in lba_start..=lba_end {
        src.seek(SeekFrom::Start(data_offset + lba * sector_size))
            .map_err(|e| format!("Seek error at LBA {lba}: {e}"))?;
        src.read_exact(&mut buf)
            .map_err(|e| format!("Read error at LBA {lba}: {e}"))?;
        dest.write_all(&buf).map_err(|e| format!("Write error: {e}"))?;
    }

    Ok(count)
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
    // num_sectors is the full BIN size; subtract the pregap to get playable audio length.
    Ok((src, track.num_sectors.saturating_sub(track.start_lba) * RAW_SECTOR_SIZE))
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
    pub raw_device_path: String,
    pub has_disc: bool,
    pub volume_name: Option<String>,
    pub mount_point: Option<String>,
}

// ── macOS ─────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn check_disc_in_drive(device_path: &str) -> (bool, Option<String>, Option<String>) {
    let Ok(out) = Command::new("diskutil").args(["info", device_path]).output() else {
        return (false, None, None);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut volume_name: Option<String> = None;
    let mut mount_point: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Volume Name:") {
            let n = rest.trim().to_string();
            if !n.is_empty() && n != "Not applicable" && n != "(null)" {
                volume_name = Some(n);
            }
        }
        if let Some(rest) = t.strip_prefix("Mount Point:") {
            let mp = rest.trim().to_string();
            if !mp.is_empty() && mp != "Not applicable" {
                mount_point = Some(mp);
            }
        }
    }
    if volume_name.is_some() {
        (true, volume_name, mount_point)
    } else {
        (false, None, None)
    }
}

// Fallback: scan `diskutil list` for whole-disk nodes that look like optical
// media (no partition-table type on entry 0), then confirm via `diskutil info`.
// Returns a map of "Device / Media Name" → BSD node name (e.g. "disk11").
#[cfg(target_os = "macos")]
fn scan_optical_nodes() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(out) = Command::new("diskutil").args(["list"]).output() else { return map };
    let text = String::from_utf8_lossy(&out.stdout);

    let mut cur_node: Option<String> = None;
    let mut skip_header = false;

    for line in text.lines() {
        if line.starts_with("/dev/disk") {
            cur_node = line.split_whitespace().next()
                .map(|s| s.trim_start_matches("/dev/").to_string());
            skip_header = true;
            continue;
        }
        if skip_header {
            skip_header = false;
            continue;
        }
        let Some(node) = cur_node.take() else { continue };
        let trimmed = line.trim_start();
        if !trimmed.starts_with("0:") { continue; }

        let rest = trimmed[2..].trim_start();
        let first_word = rest.split_whitespace().next().unwrap_or("");
        let has_partition_type = first_word.contains('_')
            || matches!(first_word, "Apple" | "EFI" | "FAT" | "Microsoft" | "Linux" | "FreeBSD");
        if has_partition_type { continue; }

        let Ok(info) = Command::new("diskutil").args(["info", &format!("/dev/{node}")]).output() else { continue };
        let info_text = String::from_utf8_lossy(&info.stdout);
        let mut is_optical = false;
        let mut media_name = String::new();
        for l in info_text.lines() {
            let t = l.trim();
            if t.starts_with("Optical Drive Type:") { is_optical = true; }
            if let Some(r) = t.strip_prefix("Device / Media Name:") {
                media_name = r.trim().to_string();
            }
        }
        if is_optical && !media_name.is_empty() {
            map.insert(media_name, node);
        }
    }
    map
}

#[cfg(target_os = "macos")]
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

    let needs_fallback = arr.iter().any(|d| {
        ["spdisc_burner-devicenode", "spdisc_burning_device", "bsd_name"]
            .iter()
            .all(|k| d.get(k).is_none())
    });
    let fallback = if needs_fallback { scan_optical_nodes() } else { std::collections::HashMap::new() };

    let mut result = Vec::new();
    for drive in &arr {
        let Some(name) = drive.get("_name").and_then(|v| v.as_str()) else { continue; };

        let node = ["spdisc_burner-devicenode", "spdisc_burning_device", "bsd_name"]
            .iter()
            .find_map(|k| drive.get(k)?.as_str().map(|s| s.to_string()))
            .or_else(|| fallback.get(name).cloned());

        let Some(node) = node else { continue; };
        let device_path = if node.starts_with("/dev/") { node } else { format!("/dev/{node}") };
        let (has_disc, volume_name, mount_point) = check_disc_in_drive(&device_path);
        let access_path = mount_point.clone().unwrap_or_else(|| device_path.clone());
        // redumper expects just the BSD name (e.g. "disk11") for --drive on macOS
        let raw_device_path = device_path.trim_start_matches("/dev/").to_string();

        result.push(DriveInfo {
            name: name.to_string(),
            device_path: access_path,
            raw_device_path,
            has_disc,
            volume_name,
            mount_point,
        });
    }

    Ok(result)
}

// ── Windows ───────────────────────────────────────────────────────────────────

// Query optical drives via PowerShell Get-CimInstance Win32_CDROMDrive.
// Each drive exposes: Name, Drive (letter e.g. "D:"), VolumeName, MediaLoaded.
#[cfg(target_os = "windows")]
#[tauri::command]
fn list_optical_drives() -> Result<Vec<DriveInfo>, String> {
    // Force UTF-8 output so non-ASCII volume names survive the pipe.
    let script = r#"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8;
$drives = Get-CimInstance Win32_CDROMDrive | Select-Object Name, Drive, VolumeName, MediaLoaded;
if ($drives -eq $null) { '[]' } else { $drives | ConvertTo-Json -Compress }
"#;
    let out = Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(|e| format!("PowerShell failed: {e}"))?;

    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if text.is_empty() || text == "[]" { return Ok(vec![]); }

    // PowerShell returns an object (not array) when there's only one drive.
    let json: serde_json::Value = serde_json::from_str(text)
        .unwrap_or(serde_json::Value::Array(vec![]));
    let arr: Vec<serde_json::Value> = match json {
        serde_json::Value::Array(a) => a,
        obj @ serde_json::Value::Object(_) => vec![obj],
        _ => vec![],
    };

    let mut result = Vec::new();
    for drive in arr {
        let name = drive.get("Name").and_then(|v| v.as_str()).unwrap_or("Optical Drive").to_string();
        let raw_letter = drive.get("Drive").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if raw_letter.is_empty() { continue; }
        // Normalise to "D:" regardless of whether Windows included the colon.
        let letter = if raw_letter.ends_with(':') {
            raw_letter.clone()
        } else {
            format!("{raw_letter}:")
        };

        let media_loaded = drive.get("MediaLoaded").and_then(|v| v.as_bool()).unwrap_or(false);
        let volume_name = drive.get("VolumeName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        // "D:\" is a valid directory path that list_disc_contents handles via is_dir().
        let mount_path = format!("{}\\", letter);
        let (device_path, mount_point) = if media_loaded {
            (mount_path.clone(), Some(mount_path))
        } else {
            (letter.clone(), None)
        };

        result.push(DriveInfo {
            name,
            raw_device_path: device_path.clone(),
            device_path,
            has_disc: media_loaded,
            volume_name,
            mount_point,
        });
    }
    Ok(result)
}

// ── Linux ─────────────────────────────────────────────────────────────────────

// Query optical drives via lsblk JSON output filtered to rom type.
// Uses the raw /dev/srN device path for sector-level access; mount point is
// kept for informational use. SIZE field is used for reliable disc detection.
#[cfg(target_os = "linux")]
#[tauri::command]
fn list_optical_drives() -> Result<Vec<DriveInfo>, String> {
    // -d: list devices without children (avoids partition sub-entries).
    // SIZE is non-zero when media is present; 0B/empty when drive is empty.
    let out = match syscmd("lsblk")
        .args(["-J", "-d", "-o", "NAME,TYPE,LABEL,MOUNTPOINT,MODEL,SIZE"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Ok(vec![]),  // lsblk not available on this system
    };

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let devices = json.get("blockdevices")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut result = Vec::new();
    for dev in devices {
        let dev_type = dev.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if dev_type != "rom" { continue; }

        let dev_name = dev.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if dev_name.is_empty() { continue; }

        let model = dev.get("model").and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(dev_name)
            .trim()
            .to_string();

        let label = dev.get("label").and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        // lsblk ≥2.37 emits "mountpoints" (array); older versions emit "mountpoint" (scalar).
        let mount = dev.get("mountpoints")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.iter().find_map(|m| m.as_str().filter(|s| !s.is_empty())))
            .map(|s| s.to_string())
            .or_else(|| {
                dev.get("mountpoint").and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            });

        // SIZE is "0B" or empty when no media; a real capacity like "700M" when loaded.
        let size_str = dev.get("size").and_then(|v| v.as_str()).unwrap_or("").trim();
        let has_disc = (!size_str.is_empty() && size_str != "0" && size_str != "0B")
            || label.is_some() || mount.is_some();

        let device_node = format!("/dev/{dev_name}");

        result.push(DriveInfo {
            name: model,
            raw_device_path: device_node.clone(),
            device_path: device_node,
            has_disc,
            volume_name: label,
            mount_point: mount,
        });
    }
    Ok(result)
}

// ── Disc ejection ─────────────────────────────────────────────────────────────

#[tauri::command]
fn eject_disc(path: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("diskutil")
            .args(["eject", &path])
            .output()
            .map_err(|e| format!("diskutil eject failed: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        // path is like "D:\" — trim to "D:" for the Shell.Application eject verb
        let drive = path.trim_end_matches(['\\', '/']);
        let script = format!(
            r#"(New-Object -ComObject Shell.Application).NameSpace(17).ParseName('{drive}').InvokeVerb('Eject')"#
        );
        Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .map_err(|e| format!("PowerShell eject failed: {e}"))?;
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        // Try `eject` first (util-linux), fall back to `udisksctl`
        match Command::new("eject").arg(&path).output() {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                let eject_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                // Try udisksctl as fallback
                match syscmd("udisksctl")
                    .args(["eject", "-b", &path])
                    .output()
                {
                    Ok(u) if u.status.success() => return Ok(()),
                    Ok(u) => {
                        let ud_err = String::from_utf8_lossy(&u.stderr).trim().to_string();
                        return Err(format!("eject: {eject_err}; udisksctl: {ud_err}"));
                    }
                    Err(_) => return Err(eject_err),
                }
            }
            Err(_) => {
                // `eject` not installed — try udisksctl
                match syscmd("udisksctl")
                    .args(["eject", "-b", &path])
                    .output()
                {
                    Ok(u) if u.status.success() => return Ok(()),
                    Ok(u) => {
                        return Err(String::from_utf8_lossy(&u.stderr).trim().to_string());
                    }
                    Err(_) => {
                        return Err(
                            "Neither 'eject' nor 'udisksctl' is available on this system"
                                .to_string(),
                        );
                    }
                }
            }
        }
    }

    #[allow(unreachable_code)]
    Err("Eject not supported on this platform".to_string())
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
        } else if lower.ends_with(".cso") || lower.ends_with(".ciso") {
            let $fs = open_cso_fs(Path::new(path))?;
            $body
        } else if lower.ends_with(".ecm") {
            let $fs = open_ecm_fs(Path::new(path))?;
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

fn detect_filesystems_wbfs(path: &Path) -> Vec<String> {
    let Ok(mut reader) = wbfs_reader::WbfsReader::open(path) else { return vec![] };
    match gcm_filesystem::detect_gcm_reader(&mut reader) {
        Some(kind) => vec![gcm_kind_label(kind)],
        None => vec![],
    }
}

fn open_gcm_wbfs(path: &Path) -> Result<gcm_filesystem::GcmFs<wbfs_reader::WbfsReader>, String> {
    let reader = wbfs_reader::WbfsReader::open(path)?;
    gcm_filesystem::GcmFs::new(reader, 0)
}

// ── WUX (Wii U compressed disc image) support ────────────────────────────────
// Wii U game discs (Blu-ray based, 25 GB).  The WuxReader deduplication layer
// maps the WUD logical layout.  The cleartext SI (System Information) partition
// is at logical disc offset 0x28000 and contains title.cert, title.tik,
// title.tmd inside an FST under the "01/" content directory.
//
// FST layout (all multi-byte integers big-endian):
//   Header 0x20 bytes: magic "FST\0", offsetFactor (u32), numSections (u32)
//   Section headers (numSections × 0x20): [0x00] size_sectors, [0x04] base_sectors
//   File entries (entry_count × 0x10), root entry first:
//     [0x00] u8 flags (bit 0 = directory)
//     [0x01-0x03] u24 name_off in string table
//     [0x04-0x07] u32 data_off (file: in offsetFactor units; dir: parent entry idx)
//     [0x08-0x0B] u32 size (file: bytes; dir: next-entry idx = subtree end)
//     [0x0C-0x0D] u16 flags2
//     [0x0E-0x0F] u16 content_idx (section index)
//   String table immediately follows the last file entry.
//
// File disc byte offset = SecHdr[content_idx].base_sectors * sector_size
//                       + data_off * offsetFactor

const WIIU_SI_FST_OFFSET: u64 = 0x28000;

trait WiiUDisc: Read + Seek + Send {}
impl WiiUDisc for wux_reader::WuxReader {}
impl WiiUDisc for File {}  // .wud is a plain raw disc image

struct WiiUFstEntry {
    flags:       u8,
    name_off:    u32,
    data_off:    u32,
    size:        u32,
    content_idx: u16,
}

struct WiiUFst {
    entries:       Vec<WiiUFstEntry>,
    string_table:  Vec<u8>,
    sec_hdrs:      Vec<(u64, u64)>,  // (base_sectors, size_sectors)
    sector_size:   u64,
    offset_factor: u64,
}

impl WiiUFst {
    fn name(&self, idx: usize) -> &str {
        let off = self.entries[idx].name_off as usize;
        let end = self.string_table[off..].iter().position(|&b| b == 0)
            .map(|p| off + p).unwrap_or(self.string_table.len());
        std::str::from_utf8(&self.string_table[off..end]).unwrap_or("?")
    }

    fn is_dir(&self, idx: usize) -> bool {
        self.entries[idx].flags & 1 != 0
    }

    fn disc_offset(&self, idx: usize) -> u64 {
        let e = &self.entries[idx];
        let (base, _) = self.sec_hdrs.get(e.content_idx as usize).copied().unwrap_or((0, 0));
        base * self.sector_size + e.data_off as u64 * self.offset_factor
    }

    // Direct children of the directory at dir_idx.
    fn list_children(&self, dir_idx: usize) -> Vec<usize> {
        let end = self.entries[dir_idx].size as usize;
        let mut result = Vec::new();
        let mut i = dir_idx + 1;
        while i < end && i < self.entries.len() {
            result.push(i);
            if self.is_dir(i) { i = self.entries[i].size as usize; }
            else               { i += 1; }
        }
        result
    }

    fn find_entry(&self, path: &str) -> Option<usize> {
        let mut current = 0usize;
        for part in path.split('/').filter(|s| !s.is_empty()) {
            if !self.is_dir(current) { return None; }
            let children = self.list_children(current);
            current = *children.iter().find(|&&i| self.name(i) == part)?;
        }
        Some(current)
    }
}

fn parse_wiiu_fst(buf: &[u8], sector_size: u64) -> Result<WiiUFst, String> {
    if buf.len() < 0x20 { return Err("WUX FST: buffer too small".to_string()); }
    if &buf[0..4] != b"FST\0" {
        return Err(format!("WUX FST: bad magic {:08X}",
            u32::from_be_bytes(buf[0..4].try_into().unwrap())));
    }
    let offset_factor = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as u64;
    let num_sec_hdrs  = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;

    let sec_start = 0x20usize;
    let sec_end   = sec_start + num_sec_hdrs * 0x20;
    if buf.len() < sec_end + 0x10 {
        return Err("WUX FST: buffer too small for section headers".to_string());
    }

    let mut sec_hdrs = Vec::with_capacity(num_sec_hdrs);
    for i in 0..num_sec_hdrs {
        let b = sec_start + i * 0x20;
        let size   = u32::from_be_bytes(buf[b..b+4].try_into().unwrap()) as u64;
        let base   = u32::from_be_bytes(buf[b+4..b+8].try_into().unwrap()) as u64;
        sec_hdrs.push((base, size));
    }

    // Root entry at sec_end; its size field = total entry count.
    let root_base   = sec_end;
    let entry_count = u32::from_be_bytes(buf[root_base+8..root_base+12].try_into().unwrap()) as usize;
    let entries_end = root_base + entry_count * 0x10;
    if buf.len() < entries_end {
        return Err(format!("WUX FST: buffer too small for {entry_count} entries"));
    }

    let mut entries = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let b = root_base + i * 0x10;
        entries.push(WiiUFstEntry {
            flags:       buf[b],
            name_off:    u32::from_be_bytes([0, buf[b+1], buf[b+2], buf[b+3]]),
            data_off:    u32::from_be_bytes(buf[b+4..b+8].try_into().unwrap()),
            size:        u32::from_be_bytes(buf[b+8..b+12].try_into().unwrap()),
            content_idx: u16::from_be_bytes(buf[b+14..b+16].try_into().unwrap()),
        });
    }

    let string_table = buf[entries_end..].to_vec();
    Ok(WiiUFst { entries, string_table, sec_hdrs, sector_size, offset_factor })
}

// Load a Wii U title key from a same-named .key file alongside the disc image.
// Title key files contain exactly 16 raw bytes (the AES-128 key).
fn load_title_key(disc_path: &Path) -> Option<[u8; 16]> {
    let key_path = disc_path.with_extension("key");
    let data = fs::read(key_path).ok()?;
    if data.len() < 16 { return None; }
    let mut key = [0u8; 16];
    key.copy_from_slice(&data[..16]);
    Some(key)
}

// AES-128-CBC decrypt with IV = 0 (Wii U per-sector scheme).
// Each 0x8000-byte disc sector is an independent CBC stream starting with IV=0.
fn wiiu_decrypt_sector(key: &[u8; 16], data: &mut [u8]) {
    let iv = [0u8; 16];
    if let Ok(dec) = Decryptor::<Aes128>::new_from_slices(key, &iv) {
        let _ = dec.decrypt_padded_mut::<NoPadding>(data);
    }
}

fn open_wiiu_disc(path: &Path) -> Result<(Box<dyn WiiUDisc>, u64), String> {
    let lower = path.to_string_lossy().to_lowercase();
    if lower.ends_with(".wud") {
        // .wud is a plain raw disc image — read directly from the file.
        let f = File::open(path).map_err(|e| format!("Cannot open WUD: {e}"))?;
        Ok((Box::new(f), 0x8000))
    } else {
        let r = wux_reader::WuxReader::open(path)?;
        let ss = r.sector_size();
        Ok((Box::new(r), ss))
    }
}

fn open_wiiu_si_fst(path: &Path) -> Result<(WiiUFst, Box<dyn WiiUDisc>, Option<[u8; 16]>), String> {
    let (mut reader, sector_size) = open_wiiu_disc(path)?;
    reader.seek(SeekFrom::Start(WIIU_SI_FST_OFFSET))
        .map_err(|e| format!("WiiU SI seek: {e}"))?;
    // One 0x8000-byte sector is ample for the SI FST (typically < 1 KB).
    let mut buf = vec![0u8; 0x8000];
    let n = reader.read(&mut buf).map_err(|e| format!("WiiU SI read: {e}"))?;
    buf.truncate(n);

    // Try cleartext (CAT-R / dev discs).
    if buf.starts_with(b"FST\0") {
        let fst = parse_wiiu_fst(&buf, sector_size)?;
        return Ok((fst, reader, None));
    }

    // Encrypted (retail disc) — look for a same-named .key file with the title key.
    let title_key = load_title_key(path)
        .ok_or_else(|| "Wii U disc is encrypted; place a matching .key file alongside it".to_string())?;

    let mut dec_buf = buf.clone();
    wiiu_decrypt_sector(&title_key, &mut dec_buf);

    if !dec_buf.starts_with(b"FST\0") {
        return Err("Wii U SI FST decryption failed — wrong title key?".to_string());
    }

    let fst = parse_wiiu_fst(&dec_buf, sector_size)?;
    Ok((fst, reader, Some(title_key)))
}

// Read `size` bytes from `disc_off`, decrypting each 0x8000-byte sector with IV=0
// if a title key is provided.
fn wiiu_read_at<R: Read + Seek>(
    reader: &mut R,
    disc_off: u64,
    size: u64,
    title_key: Option<&[u8; 16]>,
) -> io::Result<Vec<u8>> {
    const WSECTOR: u64 = 0x8000;
    let mut result = Vec::with_capacity(size as usize);
    let mut remaining = size;
    let mut cur = disc_off;
    while remaining > 0 {
        let sec_base = (cur / WSECTOR) * WSECTOR;
        let sec_off  = (cur - sec_base) as usize;
        let chunk    = ((WSECTOR - sec_off as u64).min(remaining)) as usize;
        if let Some(key) = title_key {
            let mut sector = vec![0u8; WSECTOR as usize];
            reader.seek(SeekFrom::Start(sec_base))?;
            reader.read_exact(&mut sector)?;
            wiiu_decrypt_sector(key, &mut sector);
            result.extend_from_slice(&sector[sec_off..sec_off + chunk]);
        } else {
            let mut raw = vec![0u8; chunk];
            reader.seek(SeekFrom::Start(cur))?;
            reader.read_exact(&mut raw)?;
            result.extend_from_slice(&raw);
        }
        remaining -= chunk as u64;
        cur       += chunk as u64;
    }
    Ok(result)
}

fn wiiu_fst_list_dir(fst: &WiiUFst, dir_path: &str) -> Result<Vec<DiscEntry>, String> {
    let dir_idx = fst.find_entry(dir_path)
        .ok_or_else(|| format!("WUX: directory not found: {dir_path}"))?;
    if !fst.is_dir(dir_idx) {
        return Err(format!("WUX: not a directory: {dir_path}"));
    }
    let mut entries = Vec::new();
    for idx in fst.list_children(dir_idx) {
        let is_dir = fst.is_dir(idx);
        let e = &fst.entries[idx];
        let size_bytes = if is_dir { 0 } else { e.size };
        let lba = if is_dir { 0 } else { (fst.disc_offset(idx) / 2048) as u32 };
        entries.push(DiscEntry {
            name:       fst.name(idx).to_string(),
            is_dir,
            lba,
            size:       size_bytes,
            size_bytes,
            modified:   String::new(),
        });
    }
    Ok(entries)
}

fn wiiu_fst_extract_file<R: Read + Seek>(
    fst: &WiiUFst, reader: &mut R, file_path: &str, dest_path: &str,
    title_key: Option<&[u8; 16]>,
) -> Result<(), String> {
    let idx = fst.find_entry(file_path)
        .ok_or_else(|| format!("WiiU: file not found: {file_path}"))?;
    if fst.is_dir(idx) {
        return Err(format!("WiiU: {file_path} is a directory"));
    }
    let disc_off = fst.disc_offset(idx);
    let size     = fst.entries[idx].size as u64;
    let data = wiiu_read_at(reader, disc_off, size, title_key)
        .map_err(|e| format!("WiiU file read: {e}"))?;
    let mut out = File::create(dest_path)
        .map_err(|e| format!("Create dest file: {e}"))?;
    out.write_all(&data).map_err(|e| format!("Write: {e}"))?;
    Ok(())
}

fn wiiu_fst_extract_dir<R: Read + Seek>(
    fst: &WiiUFst, reader: &mut R, dir_path: &str, dest_path: &str,
    title_key: Option<&[u8; 16]>,
) -> Result<(), String> {
    let dir_idx = fst.find_entry(dir_path)
        .ok_or_else(|| format!("WiiU: directory not found: {dir_path}"))?;
    if !fst.is_dir(dir_idx) {
        return Err(format!("WiiU: {dir_path} is not a directory"));
    }
    fs::create_dir_all(dest_path)
        .map_err(|e| format!("Create dir {dest_path}: {e}"))?;
    for idx in fst.list_children(dir_idx) {
        let name      = fst.name(idx).to_string();
        let child_dest = format!("{dest_path}/{name}");
        let child_src  = if dir_path == "/" || dir_path.is_empty() {
            format!("/{name}")
        } else {
            format!("{dir_path}/{name}")
        };
        if fst.is_dir(idx) {
            wiiu_fst_extract_dir(fst, reader, &child_src, &child_dest, title_key)?;
        } else {
            wiiu_fst_extract_file(fst, reader, &child_src, &child_dest, title_key)?;
        }
    }
    Ok(())
}

fn wux_disc_label(path: &Path) -> Option<String> {
    let (mut reader, _) = open_wiiu_disc(path).ok()?;
    // Wii U disc header at offset 0: bytes 0x20–0x5F contain the title string.
    let mut hdr = [0u8; 0x60];
    reader.seek(SeekFrom::Start(0)).ok()?;
    reader.read_exact(&mut hdr).ok()?;
    let title_raw = &hdr[0x20..0x60];
    let title_end = title_raw.iter().position(|&b| b == 0).unwrap_or(title_raw.len());
    let title = String::from_utf8_lossy(&title_raw[..title_end]).trim().to_string();
    if title.is_empty() {
        Some("Wii U disc".to_string())
    } else {
        Some(format!("Wii U — {title}"))
    }
}

fn detect_filesystems_wux(path: &Path) -> Vec<String> {
    match wux_disc_label(path) {
        Some(label) => vec![label],
        None => vec![],
    }
}

// ── Redumper raw dump support ─────────────────────────────────────────────────
// .sdram — Redumper scrambled DVD dump (raw EFM/ECC encoded sectors)
// .sbram — Redumper Blu-ray dump (2052-byte sectors: 2048 data + 4 EDC)
// Neither format maps directly to 2048-byte logical ISO sectors without full
// EFM decoding / EDC stripping; filesystem browsing is not yet supported.

fn detect_filesystems_redumper_dvd(_path: &Path) -> Vec<String> {
    vec!["Redumper DVD dump".to_string()]
}

fn detect_filesystems_redumper_bd(_path: &Path) -> Vec<String> {
    vec!["Redumper BD dump".to_string()]
}

// ── BlindWrite 5/6 (.b5t/.b6t) support ──────────────────────────────────────
// BWT5 single-file metadata format + companion .b5i/.b6i data file.
// Header layout (260 bytes, all LE):
//   [0x00]  u8[16]  signature "BWT5 STREAM SIGN"
//   [0x10]  u32[8]  unknown1
//   [0x30]  u16     profile  (0–8 = CD types; >=9 = DVD → use dvdInfoLen)
//   [0x32]  u16     sessions
//   [0x34]  u32[3]  unknown2
//   [0x40]  bool[3] mcnIsValid
//   [0x43]  u8[13]  mcn
//   [0x50]  u16     unknown3
//   [0x52]  u32[4]  unknown4
//   [0x62]  u16     pmaLen
//   [0x64]  u16     atipLen
//   [0x66]  u16     cdtLen
//   [0x68]  u16     cdInfoLen
//   [0x6A]  u32     bcaLen
//   [0x6E]  u32[3]  unknown5
//   [0x7A]  u32     dvdStrLen
//   [0x7E]  u32     dvdInfoLen
//   [0x82]  u8[32]  unknown6
//   [0xA2]  u8[8]   manufacturer
//   [0xAA]  u8[16]  product
//   [0xBA]  u8[4]   revision
//   [0xBE]  u8[20]  vendor
//   [0xD2]  u8[32]  volumeId
//   [0xF2]  u32     mode2ALen
//   [0xF6]  u32     unkBlkLen
//   [0xFA]  u32     dataLen
//   [0xFE]  u32     sessionsLen
//   [0x102] u32     dpmLen
//
// After header: blobs in order: mode2A, unkBlk, pma, atip, cdt, bca, dvdStr, discInfo
// Then: u32 pathCharCount + pathCharCount*2 UTF-16LE bytes
// Then: u32 dataBlockCount + dataBlockCount × DataFile records
// DataFile fixed block (52 bytes):
//   [0x00] u32 type, [0x04] u32 length,
//   [0x08] u32[4] unknown1, [0x18] u32 offset (into .b5i/.b6i),
//   [0x1C] u32[3] unknown2, [0x28] i32 startLba, [0x2C] i32 sectors,
//   [0x30] u32 filenameLen
// Followed by filenameLen UTF-16LE bytes + 4 bytes unknown3.
// TrackType: 0=NotData, 1=Audio, 2=Mode1, 3=Mode2, 4=Mode2F1, 5=Mode2F2, 6=Dvd

const B5T_SIGNATURE: &[u8; 16] = b"BWT5 STREAM SIGN";

fn parse_b5t_for_data_track(path: &Path) -> Result<DataTrack, String> {
    use encoding_rs::UTF_16LE;
    let data = fs::read(path).map_err(|e| format!("Cannot read BlindWrite: {e}"))?;
    if data.len() < 260 || &data[0..16] != B5T_SIGNATURE {
        return Err("Not a BlindWrite 5/6 file".to_string());
    }

    let r16 = |off: usize| u16::from_le_bytes([data[off], data[off+1]]) as usize;
    let r32 = |off: usize| u32::from_le_bytes([data[off], data[off+1], data[off+2], data[off+3]]) as usize;

    let profile      = r16(0x30) as u16;
    let pma_len      = r16(0x62);
    let atip_len     = r16(0x64);
    let cdt_len      = r16(0x66);
    let cd_info_len  = r16(0x68);
    let bca_len      = r32(0x6A);
    let dvd_str_len  = r32(0x7A);
    let dvd_info_len = r32(0x7E);
    let mode2a_len   = r32(0xF2);
    let unk_blk_len  = r32(0xF6);

    let disc_info_len = if profile <= 8 { cd_info_len } else { dvd_info_len };
    let blob_skip = mode2a_len + unk_blk_len + pma_len + atip_len + cdt_len + bca_len + dvd_str_len + disc_info_len;

    let mut pos = 260 + blob_skip;
    if pos + 4 > data.len() { return Err("BlindWrite: truncated after blobs".to_string()); }

    let path_char_count = r32(pos);
    pos += 4 + path_char_count * 2;
    if pos + 4 > data.len() { return Err("BlindWrite: truncated before dataBlockCount".to_string()); }

    let data_block_count = r32(pos);
    pos += 4;

    let companion_ext = if path.extension().map_or(false, |e| e.eq_ignore_ascii_case("b5t")) { "b5i" } else { "b6i" };
    let parent_dir = path.parent().unwrap_or(Path::new("."));

    for _ in 0..data_block_count {
        if pos + 52 > data.len() { break; }
        let df_type      = u32::from_le_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]);
        let df_offset    = u32::from_le_bytes([data[pos+0x18], data[pos+0x19], data[pos+0x1A], data[pos+0x1B]]) as u64;
        let filename_len = u32::from_le_bytes([data[pos+0x30], data[pos+0x31], data[pos+0x32], data[pos+0x33]]) as usize;
        pos += 52;
        if pos + filename_len > data.len() { break; }
        let fname_bytes = &data[pos..pos+filename_len];
        pos += filename_len + 4; // skip filename + Unknown3

        // Skip NotData (0) and Audio (1)
        if df_type == 0 || df_type == 1 { continue; }

        let (stride, user_data_offset) = match df_type {
            6 => (2048u64, 0u64),       // DVD
            2 | 4 => (2352u64, 16u64),  // Mode1, Mode2Form1
            _ => (2352u64, 24u64),      // Mode2, Mode2Form2
        };

        // Resolve companion file: try decoded filename, then extension swap
        let (decoded, _, _) = UTF_16LE.decode(fname_bytes);
        let decoded_name = decoded.trim_end_matches('\0');
        let bin_path = if !decoded_name.is_empty() {
            let candidate = parent_dir.join(Path::new(decoded_name).file_name().unwrap_or_default());
            if candidate.exists() { candidate } else { path.with_extension(companion_ext) }
        } else {
            path.with_extension(companion_ext)
        };

        let lba_offset = if user_data_offset > 0 { sector_lba_at(&bin_path, df_offset) } else { 0 };

        return Ok(DataTrack {
            bin_path, track_offset: df_offset, user_data_offset, stride,
            lba_offset, descramble: false, sector_count: 0,
        });
    }

    Err("BlindWrite: no data track found".to_string())
}

fn detect_filesystems_b5t(path: &Path) -> Vec<String> {
    let Ok(track) = parse_b5t_for_data_track(path) else { return vec![] };
    detect_filesystems_in_bin(&track.bin_path, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble)
}

// ── UIF (MagicISO compressed image) support ───────────────────────────────────
// BBIS footer (56 bytes at file_end - 56) points to BLHR block.
// BLHR contains a zlib-compressed table of sector-block descriptors.
// Each block covers `size` sectors, compressed with zlib or stored raw.

const UIF_BBIS_SIGN: u32 = 0x73696262; // "bbis" LE
const UIF_BLHR_SIGN: u32 = 0x72686C62; // "blhr" LE

struct BlhrEntry {
    offset: u64,
    zsize:  u32,
    sector: u32,
    size:   u32,
    typ:    u32,
}

pub struct UifReader {
    file:        File,
    entries:     Vec<BlhrEntry>,
    sector_size: u32,
    cache:       Option<(usize, Vec<u8>)>, // (entry_idx, decompressed block)
}

impl UifReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut f = File::open(path).map_err(|e| format!("Cannot open UIF: {e}"))?;
        let file_len = fs::metadata(path).map_err(|e| format!("UIF stat: {e}"))?.len();
        if file_len < 56 { return Err("UIF: file too small".to_string()); }

        // Read BBIS footer
        f.seek(SeekFrom::Start(file_len - 56)).map_err(|e| format!("UIF BBIS seek: {e}"))?;
        let mut footer = [0u8; 56];
        f.read_exact(&mut footer).map_err(|e| format!("UIF BBIS read: {e}"))?;

        if u32::from_le_bytes([footer[0], footer[1], footer[2], footer[3]]) != UIF_BBIS_SIGN {
            return Err("Not a UIF file".to_string());
        }
        let sector_size = u32::from_le_bytes([footer[0x14], footer[0x15], footer[0x16], footer[0x17]]);
        let blhr_off = u64::from_le_bytes(footer[0x1C..0x24].try_into().unwrap());
        if sector_size == 0 { return Err("UIF: sector_size=0".to_string()); }

        // Read BLHR header
        f.seek(SeekFrom::Start(blhr_off)).map_err(|e| format!("UIF BLHR seek: {e}"))?;
        let mut blhr_hdr = [0u8; 16];
        f.read_exact(&mut blhr_hdr).map_err(|e| format!("UIF BLHR header: {e}"))?;
        if u32::from_le_bytes([blhr_hdr[0], blhr_hdr[1], blhr_hdr[2], blhr_hdr[3]]) != UIF_BLHR_SIGN {
            return Err("UIF: invalid BLHR signature".to_string());
        }
        let blhr_size    = u32::from_le_bytes([blhr_hdr[4], blhr_hdr[5], blhr_hdr[6], blhr_hdr[7]]) as usize;
        let num_entries  = u32::from_le_bytes([blhr_hdr[12], blhr_hdr[13], blhr_hdr[14], blhr_hdr[15]]) as usize;

        // Compressed entry table follows; size field includes the ver+num fields (8 bytes).
        let comp_size = blhr_size.saturating_sub(8);
        let mut comp_data = vec![0u8; comp_size];
        f.read_exact(&mut comp_data).map_err(|e| format!("UIF BLHR data: {e}"))?;

        let expected_raw = num_entries * 24;
        let mut raw = vec![0u8; expected_raw];
        ZlibDecoder::new(&comp_data[..])
            .read_exact(&mut raw)
            .map_err(|e| format!("UIF BLHR decompress: {e}"))?;

        let entries: Vec<BlhrEntry> = raw.chunks_exact(24).map(|e| BlhrEntry {
            offset: u64::from_le_bytes(e[0..8].try_into().unwrap()),
            zsize:  u32::from_le_bytes([e[8],  e[9],  e[10], e[11]]),
            sector: u32::from_le_bytes([e[12], e[13], e[14], e[15]]),
            size:   u32::from_le_bytes([e[16], e[17], e[18], e[19]]),
            typ:    u32::from_le_bytes([e[20], e[21], e[22], e[23]]),
        }).collect();

        Ok(UifReader { file: f, entries, sector_size, cache: None })
    }

    pub fn total_sectors(&self) -> u64 {
        self.entries.last().map(|e| (e.sector + e.size) as u64).unwrap_or(0)
    }

    fn find_entry(&self, lba: u64) -> Option<usize> {
        let lba32 = lba as u32;
        self.entries.iter().position(|e| lba32 >= e.sector && lba32 < e.sector + e.size)
    }

    fn load_entry(&mut self, idx: usize) -> io::Result<()> {
        if self.cache.as_ref().map_or(false, |(i, _)| *i == idx) { return Ok(()); }
        let e = &self.entries[idx];
        self.file.seek(SeekFrom::Start(e.offset))?;
        let mut comp = vec![0u8; e.zsize as usize];
        self.file.read_exact(&mut comp)?;
        let block = match e.typ {
            1 => comp, // raw/uncompressed
            3 => vec![0u8; (e.size * self.sector_size) as usize], // zero-fill
            _ => {
                let out_size = (e.size * self.sector_size) as usize;
                let mut out = vec![0u8; out_size];
                ZlibDecoder::new(&comp[..])
                    .read_exact(&mut out)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                out
            }
        };
        self.cache = Some((idx, block));
        Ok(())
    }
}

impl ISO9660Reader for UifReader {
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> io::Result<usize> {
        let idx = self.find_entry(lba)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "UIF: LBA not found"))?;
        self.load_entry(idx)?;
        let e = &self.entries[idx];
        let block_off = ((lba as u32 - e.sector) * self.sector_size) as usize;
        let block = &self.cache.as_ref().unwrap().1;
        let avail = block.len().saturating_sub(block_off);
        let to_copy = buf.len().min(avail);
        buf[..to_copy].copy_from_slice(&block[block_off..block_off + to_copy]);
        Ok(to_copy)
    }
}

fn open_uif_fs(path: &Path) -> Result<ISO9660<UifReader>, String> {
    let reader = UifReader::open(path)?;
    ISO9660::new(reader).map_err(|e| format!("ISO9660 (UIF): {e}"))
}

fn detect_filesystems_uif(path: &Path) -> Vec<String> {
    if open_uif_fs(path).is_ok() { vec!["ISO 9660".to_string()] } else { vec![] }
}

// ── CIF (Easy CD Creator) support ─────────────────────────────────────────────
// Pure RIFF file: form type "imag". Data tracks are embedded in the .cif itself.
// The "ofs " chunk holds an offset table mapping tracks to in-file data positions.
// Each CifOffsetEntry (22 bytes): signature(4) + length(4) + type(4) + offset(4) + dummy(6).
// Type "info" (0x6F666E69) = data track; add 12 to entry.offset to skip RIFF block header.

const CIF_RIFF:   u32 = 0x46464952; // "RIFF" LE
const CIF_IMAG:   u32 = 0x67616D69; // "imag" LE
const CIF_OFS_ID: u32 = 0x2073666F; // "ofs " LE (6F 66 73 20)
const CIF_INFO:   u32 = 0x6F666E69; // "info" LE

fn parse_cif_for_data_track(path: &Path) -> Result<DataTrack, String> {
    let data = fs::read(path).map_err(|e| format!("Cannot read CIF: {e}"))?;
    if data.len() < 12 { return Err("Not a CIF file".to_string()); }
    let riff_id  = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let form_type = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    if riff_id != CIF_RIFF || form_type != CIF_IMAG {
        return Err("Not a CIF (Easy CD Creator) file".to_string());
    }

    // Scan for "ofs " chunk by iterating RIFF blocks starting at byte 12.
    let mut scan = 12usize;
    while scan + 8 <= data.len() {
        let chunk_id  = u32::from_le_bytes([data[scan], data[scan+1], data[scan+2], data[scan+3]]);
        let chunk_len = u32::from_le_bytes([data[scan+4], data[scan+5], data[scan+6], data[scan+7]]) as usize;
        let chunk_data_start = scan + 8;
        let next_chunk = chunk_data_start + chunk_len;

        if chunk_id == CIF_OFS_ID && chunk_len >= 14 {
            // "ofs " chunk: 8 dummy bytes + u16 numEntries + entries
            let entries_start = chunk_data_start + 8;
            if entries_start + 2 > data.len() { break; }
            let num_entries = u16::from_le_bytes([data[entries_start], data[entries_start+1]]) as usize;
            let mut ep = entries_start + 2;
            for _ in 0..num_entries {
                if ep + 22 > data.len() { break; }
                let entry_type   = u32::from_le_bytes([data[ep+8], data[ep+9], data[ep+10], data[ep+11]]);
                let entry_offset = u32::from_le_bytes([data[ep+12], data[ep+13], data[ep+14], data[ep+15]]) as u64;
                ep += 22;
                if entry_type != CIF_INFO { continue; }
                // Data sector data starts 12 bytes after the raw RIFF block header offset
                let data_start = entry_offset + 12;
                let (stride, user_data_offset) = detect_sector_format_at(path, data_start);
                return Ok(DataTrack {
                    bin_path: path.to_path_buf(),
                    track_offset: data_start,
                    user_data_offset,
                    stride,
                    lba_offset: 0,
                    descramble: false,
                    sector_count: 0,
                });
            }
            break;
        }

        // Each RIFF chunk is padded to even length
        let padded = chunk_len + (chunk_len & 1);
        scan = chunk_data_start + padded;
    }

    Err("CIF: no data track found".to_string())
}

fn detect_filesystems_cif(path: &Path) -> Vec<String> {
    let Ok(track) = parse_cif_for_data_track(path) else { return vec![] };
    detect_filesystems_in_bin(&track.bin_path, track.track_offset, track.user_data_offset, track.lba_offset, track.descramble)
}

// ── AaruFormat (.aif) support ─────────────────────────────────────────────────
// Complex multi-compression format (LZMA, zstd, zlib, raw).  We detect the
// magic and report the format name; full filesystem browsing is not yet
// implemented.

const AARU_MAGIC: &[u8; 8] = b"AARUFRMT";
const DICM_MAGIC: &[u8; 8] = b"DICMFRMT";

fn detect_filesystems_aif(path: &Path) -> Vec<String> {
    let Ok(mut f) = File::open(path) else { return vec![] };
    let mut magic = [0u8; 8];
    if f.read_exact(&mut magic).is_err() { return vec![]; }
    if &magic == AARU_MAGIC || &magic == DICM_MAGIC {
        vec!["AaruFormat image".to_string()]
    } else {
        vec![]
    }
}

// ── Skeleton disc images (.skeleton) ─────────────────────────────────────────
// Disc images with zeroed file data; structurally identical to a raw ISO/BIN.
// Sector size is auto-detected (2048 logical or 2352 raw with sync bytes).

pub struct SkeletonReader {
    file:             File,
    file_len:         u64,
    sector_size:      u64,
    user_data_offset: u64,
}

impl SkeletonReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut file = File::open(path).map_err(|e| format!("Cannot open Skeleton: {e}"))?;
        let file_len = file.seek(SeekFrom::End(0)).map_err(|e| format!("Seek error: {e}"))?;
        let user_data_offset = detect_raw_sector_offset(path).unwrap_or(0);
        let sector_size = if user_data_offset > 0 { RAW_SECTOR_SIZE } else { 2048 };
        Ok(SkeletonReader { file, file_len, sector_size, user_data_offset })
    }
}

impl ISO9660Reader for SkeletonReader {
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> io::Result<usize> {
        let pos = lba * self.sector_size + self.user_data_offset;
        if pos >= self.file_len { return Ok(0); }
        self.file.seek(SeekFrom::Start(pos))?;
        let avail = ((self.file_len - pos) as usize).min(buf.len());
        self.file.read(&mut buf[..avail])
    }
}

fn open_skeleton_fs(path: &Path) -> Result<ISO9660<SkeletonReader>, String> {
    let reader = SkeletonReader::open(path)?;
    ISO9660::new(reader).map_err(|e| format!("ISO9660 (Skeleton): {e}"))
}

fn detect_filesystems_skeleton(path: &Path) -> Vec<String> {
    if open_skeleton_fs(path).is_ok() { vec!["ISO 9660".to_string()] } else { vec![] }
}

// ── Zstandard-compressed disc images (.skeleton.zst, etc.) ───────────────────
// Decompress the entire file into memory, then detect + serve sectors normally.
// Skeleton images compress to near-nothing (all-zero file data), so even a
// DVD-sized skeleton fits comfortably in RAM after decompression.

pub struct ZstReader {
    data:             Vec<u8>,
    sector_size:      u64,
    user_data_offset: u64,
}

impl ZstReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let f = File::open(path).map_err(|e| format!("Cannot open ZST: {e}"))?;
        let data = zstd::decode_all(f).map_err(|e| format!("ZST decompress: {e}"))?;
        const SYNC: [u8; 12] = [0x00,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0x00];
        let (sector_size, user_data_offset) = if data.len() >= 16 && data[0..12] == SYNC {
            (2352u64, if data[15] == 2 { 24u64 } else { 16u64 })
        } else {
            (2048u64, 0u64)
        };
        Ok(ZstReader { data, sector_size, user_data_offset })
    }
}

impl ISO9660Reader for ZstReader {
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> io::Result<usize> {
        let pos = (lba * self.sector_size + self.user_data_offset) as usize;
        if pos >= self.data.len() { return Ok(0); }
        let avail = (self.data.len() - pos).min(buf.len());
        buf[..avail].copy_from_slice(&self.data[pos..pos + avail]);
        Ok(avail)
    }
}

fn open_zst_fs(path: &Path) -> Result<ISO9660<ZstReader>, String> {
    let reader = ZstReader::open(path)?;
    ISO9660::new(reader).map_err(|e| format!("ISO9660 (ZST): {e}"))
}

fn detect_filesystems_zst(path: &Path) -> Vec<String> {
    if open_zst_fs(path).is_ok() { vec!["ISO 9660".to_string()] } else { vec![] }
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

    if lower.ends_with(".cue") || lower.ends_with(".mds") || lower.ends_with(".nrg") || lower.ends_with(".ccd") || lower.ends_with(".cdi") || lower.ends_with(".gdi") || lower.ends_with(".b5t") || lower.ends_with(".b6t") || lower.ends_with(".cif") {
        let track = if lower.ends_with(".cue") { parse_cue_for_data_track(Path::new(path))? }
            else if lower.ends_with(".mds") { parse_mds_for_data_track(Path::new(path))? }
            else if lower.ends_with(".nrg") { parse_nrg_for_data_track(Path::new(path))? }
            else if lower.ends_with(".ccd") { parse_ccd_for_data_track(Path::new(path))? }
            else if lower.ends_with(".gdi") { parse_gdi_for_data_track(Path::new(path))? }
            else if lower.ends_with(".b5t") || lower.ends_with(".b6t") { parse_b5t_for_data_track(Path::new(path))? }
            else if lower.ends_with(".cif") { parse_cif_for_data_track(Path::new(path))? }
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
    } else if lower.ends_with(".cso") || lower.ends_with(".ciso") {
        collect_entries(&open_cso_fs(Path::new(path))?, &dir_path)
    } else if lower.ends_with(".ecm") {
        collect_entries(&open_ecm_fs(Path::new(path))?, &dir_path)
    } else if lower.ends_with(".uif") {
        collect_entries(&open_uif_fs(Path::new(path))?, &dir_path)
    } else if lower.ends_with(".aif") {
        Err("AaruFormat full browsing is not yet supported".to_string())
    } else if lower.ends_with(".skeleton") {
        collect_entries(&open_skeleton_fs(Path::new(path))?, &dir_path)
    } else if lower.ends_with(".skeleton.zst") || lower.ends_with(".iso.zst") || lower.ends_with(".img.zst") {
        collect_entries(&open_zst_fs(Path::new(path))?, &dir_path)
    } else if lower.ends_with(".wbfs") {
        open_gcm_wbfs(Path::new(path))?.list_directory(&dir_path)
    } else if lower.ends_with(".wux") || lower.ends_with(".wud") {
        let (fst, _, _) = open_wiiu_si_fst(Path::new(path))?;
        wiiu_fst_list_dir(&fst, &dir_path)
    } else if lower.ends_with(".sdram") || lower.ends_with(".sbram") {
        Err("Redumper raw dumps require EFM/EDC decoding; browsing not yet supported".to_string())
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

    if lower.ends_with(".cue") || lower.ends_with(".mds") || lower.ends_with(".nrg") || lower.ends_with(".ccd") || lower.ends_with(".cdi") || lower.ends_with(".gdi") || lower.ends_with(".b5t") || lower.ends_with(".b6t") || lower.ends_with(".cif") {
        let track = if lower.ends_with(".cue") { parse_cue_for_data_track(Path::new(path))? }
            else if lower.ends_with(".mds") { parse_mds_for_data_track(Path::new(path))? }
            else if lower.ends_with(".nrg") { parse_nrg_for_data_track(Path::new(path))? }
            else if lower.ends_with(".ccd") { parse_ccd_for_data_track(Path::new(path))? }
            else if lower.ends_with(".gdi") { parse_gdi_for_data_track(Path::new(path))? }
            else if lower.ends_with(".b5t") || lower.ends_with(".b6t") { parse_b5t_for_data_track(Path::new(path))? }
            else if lower.ends_with(".cif") { parse_cif_for_data_track(Path::new(path))? }
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
    } else if lower.ends_with(".cso") || lower.ends_with(".ciso") {
        extract_file_from_fs(&open_cso_fs(Path::new(path))?, &file_path, &dest_path)
    } else if lower.ends_with(".ecm") {
        extract_file_from_fs(&open_ecm_fs(Path::new(path))?, &file_path, &dest_path)
    } else if lower.ends_with(".uif") {
        extract_file_from_fs(&open_uif_fs(Path::new(path))?, &file_path, &dest_path)
    } else if lower.ends_with(".aif") {
        Err("AaruFormat full browsing is not yet supported".to_string())
    } else if lower.ends_with(".skeleton") {
        extract_file_from_fs(&open_skeleton_fs(Path::new(path))?, &file_path, &dest_path)
    } else if lower.ends_with(".skeleton.zst") || lower.ends_with(".iso.zst") || lower.ends_with(".img.zst") {
        extract_file_from_fs(&open_zst_fs(Path::new(path))?, &file_path, &dest_path)
    } else if lower.ends_with(".wbfs") {
        open_gcm_wbfs(Path::new(path))?.extract_file(&file_path, &dest_path)
    } else if lower.ends_with(".wux") || lower.ends_with(".wud") {
        let (fst, mut reader, key) = open_wiiu_si_fst(Path::new(path))?;
        wiiu_fst_extract_file(&fst, &mut reader, &file_path, &dest_path, key.as_ref())
    } else if lower.ends_with(".sdram") || lower.ends_with(".sbram") {
        Err("Redumper raw dumps require EFM/EDC decoding; browsing not yet supported".to_string())
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

    if lower.ends_with(".cue") || lower.ends_with(".mds") || lower.ends_with(".nrg") || lower.ends_with(".ccd") || lower.ends_with(".cdi") || lower.ends_with(".gdi") || lower.ends_with(".b5t") || lower.ends_with(".b6t") || lower.ends_with(".cif") {
        let track = if lower.ends_with(".cue") { parse_cue_for_data_track(Path::new(path))? }
            else if lower.ends_with(".mds") { parse_mds_for_data_track(Path::new(path))? }
            else if lower.ends_with(".nrg") { parse_nrg_for_data_track(Path::new(path))? }
            else if lower.ends_with(".ccd") { parse_ccd_for_data_track(Path::new(path))? }
            else if lower.ends_with(".gdi") { parse_gdi_for_data_track(Path::new(path))? }
            else if lower.ends_with(".b5t") || lower.ends_with(".b6t") { parse_b5t_for_data_track(Path::new(path))? }
            else if lower.ends_with(".cif") { parse_cif_for_data_track(Path::new(path))? }
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
    } else if lower.ends_with(".cso") || lower.ends_with(".ciso") {
        extract_dir_from_fs(&open_cso_fs(Path::new(path))?, &dir_path, &dest_path)
    } else if lower.ends_with(".ecm") {
        extract_dir_from_fs(&open_ecm_fs(Path::new(path))?, &dir_path, &dest_path)
    } else if lower.ends_with(".uif") {
        extract_dir_from_fs(&open_uif_fs(Path::new(path))?, &dir_path, &dest_path)
    } else if lower.ends_with(".aif") {
        Err("AaruFormat full browsing is not yet supported".to_string())
    } else if lower.ends_with(".skeleton") {
        extract_dir_from_fs(&open_skeleton_fs(Path::new(path))?, &dir_path, &dest_path)
    } else if lower.ends_with(".skeleton.zst") || lower.ends_with(".iso.zst") || lower.ends_with(".img.zst") {
        extract_dir_from_fs(&open_zst_fs(Path::new(path))?, &dir_path, &dest_path)
    } else if lower.ends_with(".wbfs") {
        open_gcm_wbfs(Path::new(path))?.extract_directory(&dir_path, &dest_path)
    } else if lower.ends_with(".wux") || lower.ends_with(".wud") {
        let (fst, mut reader, key) = open_wiiu_si_fst(Path::new(path))?;
        wiiu_fst_extract_dir(&fst, &mut reader, &dir_path, &dest_path, key.as_ref())
    } else if lower.ends_with(".sdram") || lower.ends_with(".sbram") {
        Err("Redumper raw dumps require EFM/EDC decoding; browsing not yet supported".to_string())
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

#[tauri::command]
async fn check_for_update(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_updater::UpdaterExt;
    match app.updater() {
        Ok(updater) => match updater.check().await {
            Ok(Some(update)) => Ok(Some(update.version)),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("{e}")),
        },
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .manage(MountedImages(Mutex::new(Vec::new())))
        .manage(EmulatedDrives(Mutex::new(Vec::new())))
        .manage(SectorViewParamStore(Mutex::new(std::collections::HashMap::new())))
        .manage(WiiUKeyState(Mutex::new(None)))
        .manage(RedumperDumpState(Arc::new(Mutex::new(None))))
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                let state = window.app_handle().state::<MountedImages>();
                let devices = state.0.lock().unwrap().clone();
                detach_all(&devices);
                #[cfg(target_os = "linux")]
                {
                    let edrives = window.app_handle().state::<EmulatedDrives>();
                    for drive in edrives.0.lock().unwrap().iter() {
                        let _ = syscmd("cdemu").args(["unload", &drive.slot]).output();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_disc_contents, save_file, save_directory,
            get_cue_tracks, get_gdi_tracks, save_audio_track, list_optical_drives,
            get_mds_tracks, mount_disc_image, unmount_disc_image,
            get_disc_filesystems, read_sector, find_diff_sector, get_platform, eject_disc,
            check_cdemu_installed, install_cdemu,
            get_dpm_data, get_dpm_for_sector,
            get_cdi_tracks,
            emulate_drive, eject_emulated_drive, list_emulated_drives,
            open_sector_view_window, claim_sector_view_params,
            export_sector_range,
            check_for_update,
            set_wiiu_key_path, get_wiiu_key_path,
            get_redumper_version, start_redumper_dump, cancel_redumper_dump,
            organize_dump_logs
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
