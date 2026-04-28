// NEC PC Engine / TurboGrafx-CD filesystem reader.
//
// PC Engine CD-ROM² does not use ISO 9660. The data track begins with an IPL
// (Initial Program Loader) block whose layout is documented in the Hu7 CD
// System BIOS Manual:
//
//   Bytes 0x00–0x1F: boot parameters (record number, load/exec addresses,
//                    MPR bank map, opening-animation flags)
//   Byte  0x20+:     "PC Engine CD-ROM SYSTEM\0" ID string
//   After ID:        "Copyright HUDSON SOFT / NEC Home Electronics, Ltd.\0"
//   After copyright: 16-byte + 6-byte program name (space-padded)
//
// Files are addressed by absolute 2048-byte record numbers; no on-disc
// directory exists on shipped games (it lived only on the developer's hard
// disk as CD_DOC.DIR).  The disc listing shows the boot program name and
// record location extracted from the IPL block.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use crate::DiscEntry;

const PCE_SIGNATURE: &[u8] = b"PC Engine CD-ROM SYSTEM";
const SIG_OFFSET: usize = 0x20;

// ── Low-level I/O ─────────────────────────────────────────────────────────────

fn sector_stride(user_data_offset: u64) -> u64 {
    if user_data_offset > 0 { 2352 } else { 2048 }
}

fn read_sector(
    file: &mut File,
    track_offset: u64,
    user_data_offset: u64,
    lba: u64,
) -> Option<[u8; 2048]> {
    let stride = sector_stride(user_data_offset);
    let pos = track_offset + lba * stride + user_data_offset;
    file.seek(SeekFrom::Start(pos)).ok()?;
    let mut buf = [0u8; 2048];
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

// ── String helpers ────────────────────────────────────────────────────────────

fn space_padded(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(|c: char| c == ' ' || c == '\0')
        .trim_start_matches(|c: char| c == ' ' || c == '\0')
        .to_string()
}

// ── Detection ─────────────────────────────────────────────────────────────────

pub fn is_pce_disc(bin_path: &Path, track_offset: u64, user_data_offset: u64) -> bool {
    let Ok(mut f) = File::open(bin_path) else { return false };
    for lba in [0u64, 1] {
        if let Some(s) = read_sector(&mut f, track_offset, user_data_offset, lba) {
            if s[SIG_OFFSET..].starts_with(PCE_SIGNATURE) {
                return true;
            }
        }
    }
    false
}

// ── Filesystem ────────────────────────────────────────────────────────────────

pub struct PceFs {
    file: File,
    track_offset: u64,
    user_data_offset: u64,
    ipl_lba: u64,
}

impl PceFs {
    pub fn new(
        mut file: File,
        track_offset: u64,
        user_data_offset: u64,
    ) -> Result<Self, String> {
        for lba in [0u64, 1] {
            if let Some(s) = read_sector(&mut file, track_offset, user_data_offset, lba) {
                if s[SIG_OFFSET..].starts_with(PCE_SIGNATURE) {
                    return Ok(PceFs { file, track_offset, user_data_offset, ipl_lba: lba });
                }
            }
        }
        Err("Not a PC Engine CD-ROM disc".to_string())
    }

    pub fn list_directory(&mut self, _dir_path: &str) -> Result<Vec<DiscEntry>, String> {
        let s = read_sector(&mut self.file, self.track_offset, self.user_data_offset, self.ipl_lba)
            .ok_or_else(|| "Failed to read IPL sector".to_string())?;
        Ok(parse_ipl_as_entries(&s))
    }

    pub fn extract_file(&mut self, _file_path: &str, dest_path: &str) -> Result<(), String> {
        let ipl = read_sector(&mut self.file, self.track_offset, self.user_data_offset, self.ipl_lba)
            .ok_or_else(|| "Failed to read IPL sector".to_string())?;
        let entries = parse_ipl_as_entries(&ipl);
        let entry = entries.first().ok_or_else(|| "No boot entry in IPL".to_string())?;
        let start_lba = entry.lba as u64;
        let block_count = entry.size as u64;
        let mut out = std::fs::File::create(dest_path)
            .map_err(|e| format!("Cannot create output: {e}"))?;
        for lba in start_lba..start_lba + block_count {
            let s = read_sector(&mut self.file, self.track_offset, self.user_data_offset, lba)
                .ok_or_else(|| format!("Failed to read sector {lba}"))?;
            out.write_all(&s).map_err(|e| format!("Write error: {e}"))?;
        }
        Ok(())
    }

    pub fn extract_directory(&mut self, _dir_path: &str, _dest_path: &str) -> Result<(), String> {
        Err("Directory extraction not supported for PC Engine discs".to_string())
    }
}

// ── IPL block → DiscEntry (no directory on disc) ──────────────────────────────

fn parse_ipl_as_entries(sector: &[u8]) -> Vec<DiscEntry> {
    // Bytes 0x00–0x02: IPLBLK H/M/L (boot record number, high byte first)
    let boot_record = ((sector[0] as u32) << 16)
        | ((sector[1] as u32) << 8)
        | sector[2] as u32;
    // Byte 0x03: IPLBLW (number of 2048-byte records to load)
    let boot_length = sector[3] as u32;

    // Copyright string follows the signature + null terminator at 0x20.
    let sig_end = SIG_OFFSET + PCE_SIGNATURE.len() + 1; // 0x38
    let copyright_len = sector[sig_end..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| p + 1)
        .unwrap_or(60);
    let name_start = sig_end + copyright_len;

    // Program name: 16-byte part + 6-byte part, space-padded.
    let game_name = if name_start + 22 <= 2048 {
        let p1 = space_padded(&sector[name_start..name_start + 16]);
        let p2 = space_padded(&sector[name_start + 16..name_start + 22]);
        if p2.is_empty() { p1 } else { format!("{p1} {p2}") }
    } else {
        String::new()
    };

    let name = if game_name.is_empty() {
        "PC Engine Program".to_string()
    } else {
        game_name
    };

    vec![DiscEntry {
        name,
        is_dir: false,
        lba: boot_record,
        size: boot_length,
        size_bytes: boot_length * 2048,
        modified: String::new(),
    }]
}

