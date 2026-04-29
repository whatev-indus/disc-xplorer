use std::fs;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use crate::DiscEntry;

const SECTOR_RAW: u64 = 2352;
const SECTOR_LOGICAL: u64 = 2048;

const TAG_AVDP: u16 = 2;
const TAG_PD: u16 = 5;
const TAG_LVD: u16 = 6;
const TAG_TERM: u16 = 8;
const TAG_LVID: u16 = 9;
const TAG_FSD: u16 = 256;
const TAG_FID: u16 = 257;
const TAG_FILE_ENTRY: u16 = 261;
const TAG_EXT_FILE_ENTRY: u16 = 266;

const FILE_TYPE_VAT: u8 = 248;

pub struct UdfFs {
    file: File,
    track_offset: u64,
    user_data_offset: u64,
    partition_start: u64,
    pub volume_name: String,
    pub udf_version: String,
    root_icb_lbn: u32,
    root_icb_part_ref: u16,
    // Metadata partition (UDF 2.50+): meta_lbn_offset is the starting physical LBN
    // of the metadata file data. meta_logical(x) = phys(partition_start + meta_lbn_offset + x).
    meta_lbn_offset: u64,
    meta_part_ref: u16, // u16::MAX = no metadata partition
    // Virtual Allocation Table (UDF 1.x on CD-R): vat[virtual_lbn] = physical_lbn in partition.
    vat: Vec<u32>,
    vat_part_ref: u16, // u16::MAX = no VAT
}

fn sector_size(user_data_offset: u64) -> u64 {
    if user_data_offset == 0 { SECTOR_LOGICAL } else { SECTOR_RAW }
}

fn read_physical_sector(
    file: &mut File,
    track_offset: u64,
    user_data_offset: u64,
    lba: u64,
) -> Result<[u8; 2048], String> {
    let pos = track_offset + lba * sector_size(user_data_offset) + user_data_offset;
    file.seek(SeekFrom::Start(pos)).map_err(|e| format!("UDF seek: {e}"))?;
    let mut buf = [0u8; 2048];
    file.read_exact(&mut buf).map_err(|e| format!("UDF read: {e}"))?;
    Ok(buf)
}

fn tag_id(s: &[u8]) -> u16 {
    if s.len() < 2 { return 0; }
    u16::from_le_bytes([s[0], s[1]])
}

fn le16(b: &[u8], o: usize) -> u16 {
    if o + 2 > b.len() { return 0; }
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    if o + 4 > b.len() { return 0; }
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or([0; 4]))
}

fn le64(b: &[u8], o: usize) -> u64 {
    if o + 8 > b.len() { return 0; }
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or([0; 8]))
}

// OSTA CS0 → UTF-8. Byte 0 is the compression type (8 = Latin-1, 16 = UTF-16 BE).
fn cs0_to_string(bytes: &[u8]) -> String {
    if bytes.is_empty() { return String::new(); }
    match bytes[0] {
        16 => {
            let words: Vec<u16> = bytes[1..]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&words).to_owned()
        }
        8 => bytes[1..].iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

// ECMA-167 timestamp (12 bytes) → "YYYY-MM-DD HH:MM:SS"
fn udf_timestamp(b: &[u8]) -> String {
    if b.len() < 12 { return String::new(); }
    let year = le16(b, 2);
    let (month, day, hour, min, sec) = (b[4], b[5], b[6], b[7], b[8]);
    format!("{year}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}")
}

// BCD-encoded UDF revision (e.g. 0x0260) → "UDF 2.60"
fn format_udf_version(rev: u16) -> String {
    let major = (rev >> 8) as u8;
    let minor_bcd = (rev & 0xFF) as u8;
    let minor = (minor_bcd >> 4) as u32 * 10 + (minor_bcd & 0x0F) as u32;
    format!("UDF {}.{:02}", major, minor)
}

pub fn is_udf_disc(path: &Path, track_offset: u64, user_data_offset: u64) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    let pos = track_offset + 256 * sector_size(user_data_offset) + user_data_offset;
    if f.seek(SeekFrom::Start(pos)).is_err() { return false }
    let mut tag = [0u8; 16];
    if f.read_exact(&mut tag).is_err() { return false }
    // Tag Identifier must be 2 (AVDP).
    if u16::from_le_bytes([tag[0], tag[1]]) != TAG_AVDP { return false; }
    // Tag Checksum: sum of all tag bytes except byte 4, mod 256, must equal byte 4.
    // This is a strong discriminator (1-in-256 false-positive rate) without relying
    // on the Tag Location field, which some BD-ROM/PS4 implementations write differently.
    let checksum: u8 = tag.iter().enumerate()
        .filter(|&(i, _)| i != 4)
        .map(|(_, &b)| b)
        .fold(0u8, |acc, b| acc.wrapping_add(b));
    checksum == tag[4]
}

impl UdfFs {
    pub fn new(file: File, track_offset: u64, user_data_offset: u64) -> Result<Self, String> {
        let mut udf = UdfFs {
            file,
            track_offset,
            user_data_offset,
            partition_start: 0,
            volume_name: String::new(),
            udf_version: "UDF".to_string(),
            root_icb_lbn: 0,
            root_icb_part_ref: 0,
            meta_lbn_offset: 0,
            meta_part_ref: u16::MAX,
            vat: Vec::new(),
            vat_part_ref: u16::MAX,
        };
        udf.init()?;
        Ok(udf)
    }

    fn phys(&mut self, lba: u64) -> Result<[u8; 2048], String> {
        read_physical_sector(&mut self.file, self.track_offset, self.user_data_offset, lba)
    }

    // Physical-partition-relative read.
    fn logical(&mut self, lbn: u64) -> Result<[u8; 2048], String> {
        read_physical_sector(&mut self.file, self.track_offset, self.user_data_offset,
            self.partition_start + lbn)
    }

    // Metadata-partition-relative read.
    fn meta_logical(&mut self, lbn: u64) -> Result<[u8; 2048], String> {
        read_physical_sector(&mut self.file, self.track_offset, self.user_data_offset,
            self.partition_start + self.meta_lbn_offset + lbn)
    }

    // Virtual-partition read: translate virtual LBN through VAT, then read physical partition.
    fn virtual_logical(&mut self, virtual_lbn: u64) -> Result<[u8; 2048], String> {
        let phys_lbn = if (virtual_lbn as usize) < self.vat.len() {
            self.vat[virtual_lbn as usize] as u64
        } else {
            virtual_lbn
        };
        self.logical(phys_lbn)
    }

    // Dispatch a sector read based on partition reference number.
    fn read_sector_for_part(&mut self, lbn: u64, part_ref: u16) -> Result<[u8; 2048], String> {
        if !self.vat.is_empty() && part_ref == self.vat_part_ref {
            self.virtual_logical(lbn)
        } else if self.meta_part_ref != u16::MAX && part_ref == self.meta_part_ref {
            self.meta_logical(lbn)
        } else {
            self.logical(lbn)
        }
    }

    // Read `length` bytes from contiguous sectors in the physical partition.
    fn logical_bytes(&mut self, start_lbn: u64, length: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(length as usize);
        let mut rem = length;
        let mut lbn = start_lbn;
        while rem > 0 {
            let sec = self.logical(lbn)?;
            let n = rem.min(2048) as usize;
            out.extend_from_slice(&sec[..n]);
            rem -= n as u64;
            lbn += 1;
        }
        Ok(out)
    }

    // Read `length` bytes from contiguous sectors, dispatching per part_ref.
    fn logical_bytes_for_part(&mut self, start_lbn: u64, length: u64, part_ref: u16) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(length as usize);
        let mut rem = length;
        let mut lbn = start_lbn;
        while rem > 0 {
            let sec = self.read_sector_for_part(lbn, part_ref)?;
            let n = rem.min(2048) as usize;
            out.extend_from_slice(&sec[..n]);
            rem -= n as u64;
            lbn += 1;
        }
        Ok(out)
    }

    // Returns (icb_flags, info_len, l_ea_offset, l_ad_offset, alloc_base)
    // ECMA-167 Table 9 (FE):  L_EA@168, L_AD@172, data@176
    // ECMA-167 Table 16 (EFE): Reserved between Checkpoint and EA-ICB is 12 bytes (not 8),
    // placing L_EA@208, L_AD@212, data@216.
    fn fe_layout(fe: &[u8; 2048]) -> (u16, u64, usize, usize, usize) {
        if tag_id(fe) == TAG_EXT_FILE_ENTRY {
            (le16(fe, 34), le64(fe, 56), 208, 212, 216)
        } else {
            (le16(fe, 34), le64(fe, 56), 168, 172, 176)
        }
    }

    // Read all file data via a FE's allocation descriptors, dispatching by part_ref.
    // For short_ad (alloc_type 0): extents are in the same partition as the FE (fe_part_ref).
    // For long_ad (alloc_type 1): extents carry their own explicit partition reference.
    fn read_alloc_bytes(&mut self, fe: &[u8; 2048], fe_part_ref: u16) -> Result<Vec<u8>, String> {
        let (icb_flags, info_len, l_ea_off, l_ad_off, alloc_base) = Self::fe_layout(fe);
        let alloc_type = icb_flags & 0x0007;
        let l_ea = le32(fe, l_ea_off) as usize;
        let l_ad = le32(fe, l_ad_off) as usize;
        let ad_start = alloc_base + l_ea;

        if alloc_type == 3 {
            let end = (ad_start + l_ad).min(2048);
            return Ok(fe[ad_start..end].to_vec());
        }

        let ad_size: usize = if alloc_type == 0 { 8 } else { 16 };
        let mut out = Vec::new();
        let mut off = ad_start;
        let ad_end = (ad_start + l_ad).min(2048);

        while off + ad_size <= ad_end && (out.len() as u64) < info_len {
            let len_raw = le32(fe, off);
            let ext_len = (len_raw & 0x3FFF_FFFF) as u64;
            let ext_lbn = le32(fe, off + 4) as u64;
            let ext_type = (len_raw >> 30) & 0x3;
            if ext_type == 0 && ext_len > 0 {
                let to_read = ext_len.min(info_len - out.len() as u64);
                let part_ref = if alloc_type == 0 { fe_part_ref } else { le16(fe, off + 8) };
                let data = self.logical_bytes_for_part(ext_lbn, to_read, part_ref)?;
                out.extend_from_slice(&data);
            }
            off += ad_size;
        }
        Ok(out)
    }

    // Like read_alloc_bytes but always uses the physical partition (logical()).
    // Used during VAT loading before VAT translation is available.
    fn read_alloc_bytes_logical(&mut self, fe: &[u8; 2048]) -> Result<Vec<u8>, String> {
        let (icb_flags, info_len, l_ea_off, l_ad_off, alloc_base) = Self::fe_layout(fe);
        let alloc_type = icb_flags & 0x0007;
        let l_ea = le32(fe, l_ea_off) as usize;
        let l_ad = le32(fe, l_ad_off) as usize;
        let ad_start = alloc_base + l_ea;

        if alloc_type == 3 {
            let end = (ad_start + l_ad).min(2048);
            return Ok(fe[ad_start..end].to_vec());
        }

        let ad_size: usize = if alloc_type == 0 { 8 } else { 16 };
        let mut out = Vec::new();
        let mut off = ad_start;
        let ad_end = (ad_start + l_ad).min(2048);

        while off + ad_size <= ad_end && (out.len() as u64) < info_len {
            let len_raw = le32(fe, off);
            let ext_len = (len_raw & 0x3FFF_FFFF) as u64;
            let ext_lbn = le32(fe, off + 4) as u64;
            let ext_type = (len_raw >> 30) & 0x3;
            if ext_type == 0 && ext_len > 0 {
                let to_read = ext_len.min(info_len - out.len() as u64);
                let data = self.logical_bytes(ext_lbn, to_read)?;
                out.extend_from_slice(&data);
            }
            off += ad_size;
        }
        Ok(out)
    }

    // Stream a FE's file data to dest, dispatching sector reads by part_ref.
    fn copy_alloc_to_file(&mut self, fe: &[u8; 2048], fe_part_ref: u16, dest: &mut File) -> Result<(), String> {
        let (icb_flags, info_len, l_ea_off, l_ad_off, alloc_base) = Self::fe_layout(fe);
        let alloc_type = icb_flags & 0x0007;
        let l_ea = le32(fe, l_ea_off) as usize;
        let l_ad = le32(fe, l_ad_off) as usize;
        let ad_start = alloc_base + l_ea;

        if alloc_type == 3 {
            let end = (ad_start + l_ad).min(2048);
            dest.write_all(&fe[ad_start..end]).map_err(|e| format!("Write: {e}"))?;
            return Ok(());
        }

        let ad_size: usize = if alloc_type == 0 { 8 } else { 16 };
        let mut off = ad_start;
        let ad_end = (ad_start + l_ad).min(2048);
        let mut written: u64 = 0;

        while off + ad_size <= ad_end && written < info_len {
            let len_raw = le32(fe, off);
            let ext_len = (len_raw & 0x3FFF_FFFF) as u64;
            let ext_lbn = le32(fe, off + 4) as u64;
            let ext_type = (len_raw >> 30) & 0x3;
            if ext_type == 0 && ext_len > 0 {
                let to_write = ext_len.min(info_len - written);
                let part_ref = if alloc_type == 0 { fe_part_ref } else { le16(fe, off + 8) };
                let mut rem = to_write;
                let mut lbn = ext_lbn;
                while rem > 0 {
                    let sec = self.read_sector_for_part(lbn, part_ref)?;
                    let n = rem.min(2048) as usize;
                    dest.write_all(&sec[..n]).map_err(|e| format!("Write: {e}"))?;
                    rem -= n as u64;
                    lbn += 1;
                }
                written += to_write;
            }
            off += ad_size;
        }
        Ok(())
    }

    // Scan backward from end of disc for a File Entry with file type 248 (VAT ICB).
    // Returns the track-relative physical LBA if found.
    fn find_vat_icb_lba(&mut self) -> Option<u64> {
        let file_size = self.file.metadata().ok()?.len();
        let ss = sector_size(self.user_data_offset);
        if file_size <= self.track_offset { return None; }
        let track_sectors = (file_size - self.track_offset) / ss;
        let scan_start = track_sectors.saturating_sub(256);
        for i in (scan_start..track_sectors).rev() {
            if let Ok(sec) = read_physical_sector(&mut self.file, self.track_offset, self.user_data_offset, i) {
                let t = tag_id(&sec);
                // FileType is at ICBTag offset 11, which is FE offset 27.
                if (t == TAG_FILE_ENTRY || t == TAG_EXT_FILE_ENTRY) && sec[27] == FILE_TYPE_VAT {
                    return Some(i);
                }
            }
        }
        None
    }

    // use_header: true for UDF 2.01+ (VAT has L_HD/L_IU header), false for 1.02/1.50 (flat array).
    fn load_vat(&mut self, use_header: bool) -> Result<(), String> {
        let vat_lba = match self.find_vat_icb_lba() {
            Some(lba) => lba,
            None => return Ok(()),
        };

        let vat_fe = self.phys(vat_lba)?;
        let t = tag_id(&vat_fe);
        if t != TAG_FILE_ENTRY && t != TAG_EXT_FILE_ENTRY { return Ok(()); }

        // VAT FE lives at a physical sector; its short_ad extents are partition-relative.
        let vat_data = self.read_alloc_bytes_logical(&vat_fe)?;
        if vat_data.len() < 4 { return Ok(()); }

        if use_header {
            // UDF 2.01+: VAT starts with L_HD (2) + L_IU (2) + header fields.
            let l_hd = le16(&vat_data, 0) as usize;
            if l_hd < 4 || l_hd > vat_data.len() { return Ok(()); }

            let entry_count = (vat_data.len() - l_hd) / 4;
            self.vat = (0..entry_count).map(|i| le32(&vat_data, l_hd + i * 4)).collect();

            // LV identifier dstring at offset 4 (128 bytes); MinimumUDFReadRevision at 144.
            if l_hd >= 132 {
                let cs0_len = vat_data[4 + 127] as usize;
                if cs0_len > 0 && cs0_len <= 127 {
                    let name = cs0_to_string(&vat_data[4..4 + cs0_len]);
                    if !name.is_empty() { self.volume_name = name; }
                }
            }
            if l_hd >= 146 {
                let min_rev = le16(&vat_data, 144);
                if min_rev != 0 { self.udf_version = format_udf_version(min_rev); }
            }
        } else {
            // UDF 1.02/1.50: VAT is a flat sequence of Uint32 physical LBNs, no header.
            let entry_count = vat_data.len() / 4;
            self.vat = (0..entry_count).map(|i| le32(&vat_data, i * 4)).collect();
        }

        Ok(())
    }

    fn init(&mut self) -> Result<(), String> {
        // Anchor Volume Descriptor Pointer is always at LBA 256.
        let avdp = self.phys(256)?;
        if tag_id(&avdp) != TAG_AVDP {
            return Err("No UDF AVDP at LBA 256".to_string());
        }
        let vds_len = le32(&avdp, 16) as u64;
        let vds_lba = le32(&avdp, 20) as u64;
        let vds_sectors = (vds_len + 2047) / 2048;

        let mut part_start: Option<u64> = None;
        let mut fsd_lbn: Option<u32> = None;
        let mut fsd_part_ref: u16 = 0;
        let mut vol_name = String::new();
        let mut lvd_sec: Option<[u8; 2048]> = None;
        let mut lvid_lba: u64 = 0;

        for i in 0..vds_sectors {
            let sec = self.phys(vds_lba + i)?;
            match tag_id(&sec) {
                TAG_TERM => break,
                TAG_PD => {
                    // PartitionStartingLocation at offset 188 (ECMA-167 3/10.5)
                    part_start = Some(le32(&sec, 188) as u64);
                }
                TAG_LVD => {
                    // LogicalVolumeIdentifier: 128-byte dstring at offset 84
                    let cs0_len = sec[84 + 127] as usize;
                    if cs0_len > 0 && cs0_len <= 127 {
                        vol_name = cs0_to_string(&sec[84..84 + cs0_len]);
                    }
                    // LogicalVolumeContentsUse long_ad at offset 248: LBN at 252, PartRef at 256
                    fsd_lbn = Some(le32(&sec, 252));
                    fsd_part_ref = le16(&sec, 256);
                    // IntegritySequenceExtent at offset 432: length at 432, location at 436
                    lvid_lba = le32(&sec, 436) as u64;
                    lvd_sec = Some(sec);
                }
                _ => {}
            }
        }

        self.partition_start = part_start.ok_or("UDF: no Partition Descriptor found")?;
        self.volume_name = vol_name;
        let fsd_lbn = fsd_lbn.ok_or("UDF: no Logical Volume Descriptor found")?;

        // ── Partition Maps ────────────────────────────────────────────────────────
        // Scan for Type 2 partition maps: "*UDF Metadata Partition" and "*UDF Virtual Partition".
        if let Some(ref lvd) = lvd_sec {
            let map_table_len = le32(lvd, 264) as usize;
            let num_pms = le32(lvd, 268) as usize;
            let pm_area_end = (440 + map_table_len).min(2048);
            let mut pm_off = 440usize;
            let mut pm_idx: u16 = 0;

            while pm_off + 2 <= pm_area_end && (pm_idx as usize) < num_pms {
                let pm_type = lvd[pm_off];
                let pm_len = lvd[pm_off + 1] as usize;
                if pm_len < 2 { break; }

                if pm_type == 2 && pm_off + 36 <= pm_area_end {
                    // EntityID identifier string starts at pm_off + 5 (after type(1)+len(1)+reserved(2)+flags(1))
                    let id_end = (pm_off + 5 + 23).min(pm_area_end);
                    let ident = &lvd[pm_off + 5..id_end];

                    if ident.starts_with(b"*UDF Metadata Partition") {
                        // MetadataFileLocation is at pm_off + 40 (after type+len+reserved+EntityID+VSN+PN)
                        if pm_off + 44 <= pm_area_end {
                            let meta_file_icb = le32(lvd, pm_off + 40);
                            if let Ok(meta_fe) = self.logical(meta_file_icb as u64) {
                                let ftag = tag_id(&meta_fe);
                                if ftag == TAG_FILE_ENTRY || ftag == TAG_EXT_FILE_ENTRY {
                                    let (icb_flags, info_len, l_ea_off, l_ad_off, alloc_base) =
                                        Self::fe_layout(&meta_fe);
                                    let alloc_type = icb_flags & 0x0007;
                                    let l_ea = le32(&meta_fe, l_ea_off) as usize;
                                    let l_ad = le32(&meta_fe, l_ad_off) as usize;
                                    if alloc_type == 0 || alloc_type == 1 {
                                        let ad_start = alloc_base + l_ea;
                                        let ad_end = (ad_start + l_ad).min(2048);
                                        let ad_size = if alloc_type == 0 { 8usize } else { 16 };
                                        if ad_start + ad_size <= ad_end {
                                            let lbn_off = ad_start + 4;
                                            self.meta_lbn_offset = le32(&meta_fe, lbn_off) as u64;
                                            self.meta_part_ref = pm_idx;
                                        } else if info_len > 0 {
                                            // l_ad=0 but file has data: scan entire disc for FSD.
                                            // The metadata file area start = physical FSD lba - partition_start - fsd_lbn.
                                            let fsd_lbn_hint = fsd_lbn as u64;
                                            for probe in 0u64..131072 {
                                                if let Ok(sec) = self.phys(probe) {
                                                    if tag_id(&sec) == TAG_FSD {
                                                        let probe_part_rel = probe.saturating_sub(self.partition_start);
                                                        self.meta_lbn_offset = probe_part_rel.saturating_sub(fsd_lbn_hint);
                                                        self.meta_part_ref = pm_idx;
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if ident.starts_with(b"*UDF Virtual Partition") {
                        self.vat_part_ref = pm_idx;
                    }
                }

                pm_off += pm_len;
                pm_idx += 1;
            }
        }

        // ── LVID version detection ────────────────────────────────────────────────
        // Read LVID before loading VAT so we know which VAT format to expect.
        if lvid_lba > 0 {
            if let Ok(lvid) = self.phys(lvid_lba) {
                if tag_id(&lvid) == TAG_LVID {
                    // LVID ImplementationUse starts at offset 80 + 8 * NumberOfPartitions.
                    let n_part = le32(&lvid, 72) as usize;
                    let l_iu = le32(&lvid, 76) as usize;
                    let iu_start = 80 + 8 * n_part;
                    // MinimumUDFReadRevision is at ImplementationUse + 40.
                    if iu_start + 46 <= 2048 && l_iu >= 46 {
                        let min_rev = le16(&lvid, iu_start + 40);
                        if min_rev != 0 {
                            self.udf_version = format_udf_version(min_rev);
                        }
                    }
                }
            }
        }

        // ── Virtual Allocation Table ──────────────────────────────────────────────
        // UDF 2.01+ VAT has a header (L_HD, LV identifier, revision).
        // UDF 1.02/1.50 VAT is a flat Uint32 array with no header.
        if self.vat_part_ref != u16::MAX {
            let use_header = !matches!(
                self.udf_version.as_str(),
                "UDF 1.02" | "UDF 1.50" | "UDF"
            );
            let _ = self.load_vat(use_header); // best-effort; proceed even without VAT
        }

        // ── File Set Descriptor ───────────────────────────────────────────────────
        let fsd_primary = self.read_sector_for_part(fsd_lbn as u64, fsd_part_ref)?;
        let fsd = if tag_id(&fsd_primary) == TAG_FSD {
            fsd_primary
        } else if fsd_lbn != 0 {
            let fsd_lbn0 = self.read_sector_for_part(0, fsd_part_ref)?;
            if tag_id(&fsd_lbn0) == TAG_FSD {
                fsd_lbn0
            } else {
                return Err("UDF: File Set Descriptor not found".to_string());
            }
        } else {
            return Err("UDF: File Set Descriptor not found".to_string());
        };

        // RootDirectoryICB long_ad at offset 400: LBN at 404, PartRef at 408.
        self.root_icb_lbn = le32(&fsd, 404);
        self.root_icb_part_ref = le16(&fsd, 408);
        Ok(())
    }

    fn list_dir_at(&mut self, icb_lbn: u32, part_ref: u16) -> Result<Vec<DiscEntry>, String> {
        let fe = self.read_sector_for_part(icb_lbn as u64, part_ref)?;
        let tag = tag_id(&fe);
        if tag != TAG_FILE_ENTRY && tag != TAG_EXT_FILE_ENTRY {
            return Err(format!("UDF: LBN {icb_lbn} has tag {tag}, expected file entry"));
        }
        let dir_data = self.read_alloc_bytes(&fe, part_ref)?;
        let mut entries = Vec::new();
        let mut pos = 0usize;

        while pos + 38 <= dir_data.len() {
            if le16(&dir_data, pos) != TAG_FID { break; }
            // FID layout per UDF spec (ECMA-167 Table 14.4 + UDF File Version Number):
            // [0-15] tag, [16-17] File Version Number (always 1),
            // [18] File Characteristics, [19] L_FI,
            // [20-35] ICB long_ad (ExtLen[4], LBN[4], PartRef[2], ImpUse[6]),
            // [36-37] L_IU, [38+L_IU .. 38+L_IU+L_FI] file identifier
            // Size = ((38 + L_IU + L_FI) + 3) & !3
            let file_chars = dir_data[pos + 18];
            let l_fi = dir_data[pos + 19] as usize;
            let icb_lbn_entry = le32(&dir_data, pos + 24);
            let icb_part_ref_entry = le16(&dir_data, pos + 28);
            let l_iu = le16(&dir_data, pos + 36) as usize;
            let name_start = pos + 38 + l_iu;
            let name_end = name_start + l_fi;
            let fid_len = ((38 + l_iu + l_fi) + 3) & !3;

            let is_parent = (file_chars & 0x08) != 0;
            let is_dir = (file_chars & 0x02) != 0;

            if !is_parent && l_fi > 0 && name_end <= dir_data.len() {
                let name = cs0_to_string(&dir_data[name_start..name_end]);
                let (size_bytes, modified) = self
                    .read_entry_info(icb_lbn_entry, icb_part_ref_entry)
                    .unwrap_or((0, String::new()));
                entries.push(DiscEntry {
                    name,
                    is_dir,
                    lba: icb_lbn_entry,
                    size: if is_dir { 0 } else { size_bytes as u32 },
                    size_bytes: size_bytes as u32,
                    modified,
                });
            }

            pos += fid_len.max(38);
        }
        Ok(entries)
    }

    fn read_entry_info(&mut self, icb_lbn: u32, part_ref: u16) -> Result<(u64, String), String> {
        let fe = self.read_sector_for_part(icb_lbn as u64, part_ref)?;
        let size = le64(&fe, 56);
        let modified = udf_timestamp(&fe[84..96]);
        Ok((size, modified))
    }

    fn resolve(&mut self, path: &str) -> Result<(u32, u16), String> {
        let mut lbn = self.root_icb_lbn;
        let mut part_ref = self.root_icb_part_ref;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            let result = self.find_in_dir(lbn, part_ref, seg)?;
            lbn = result.0;
            part_ref = result.1;
        }
        Ok((lbn, part_ref))
    }

    fn find_in_dir(&mut self, dir_lbn: u32, dir_part_ref: u16, name: &str) -> Result<(u32, u16), String> {
        let fe = self.read_sector_for_part(dir_lbn as u64, dir_part_ref)?;
        let tag = tag_id(&fe);
        if tag != TAG_FILE_ENTRY && tag != TAG_EXT_FILE_ENTRY {
            return Err(format!("UDF: '{name}' not found (not a directory)"));
        }
        let dir_data = self.read_alloc_bytes(&fe, dir_part_ref)?;
        let mut pos = 0usize;

        while pos + 38 <= dir_data.len() {
            if le16(&dir_data, pos) != TAG_FID { break; }
            let file_chars = dir_data[pos + 18];
            let l_fi = dir_data[pos + 19] as usize;
            let icb_lbn_entry = le32(&dir_data, pos + 24);
            let icb_part_ref_entry = le16(&dir_data, pos + 28);
            let l_iu = le16(&dir_data, pos + 36) as usize;
            let name_start = pos + 38 + l_iu;
            let name_end = name_start + l_fi;
            let fid_len = ((38 + l_iu + l_fi) + 3) & !3;
            let is_parent = (file_chars & 0x08) != 0;

            if !is_parent && l_fi > 0 && name_end <= dir_data.len() {
                let entry_name = cs0_to_string(&dir_data[name_start..name_end]);
                if entry_name.eq_ignore_ascii_case(name) {
                    return Ok((icb_lbn_entry, icb_part_ref_entry));
                }
            }
            pos += fid_len.max(38);
        }
        Err(format!("UDF: '{name}' not found"))
    }

    pub fn list_directory(&mut self, dir_path: &str) -> Result<Vec<DiscEntry>, String> {
        let (lbn, part_ref) = self.resolve(dir_path)?;
        self.list_dir_at(lbn, part_ref)
    }

    pub fn extract_file(&mut self, file_path: &str, dest_path: &str) -> Result<(), String> {
        let (lbn, part_ref) = self.resolve(file_path)?;
        let fe = self.read_sector_for_part(lbn as u64, part_ref)?;
        let tag = tag_id(&fe);
        if tag != TAG_FILE_ENTRY && tag != TAG_EXT_FILE_ENTRY {
            return Err(format!("UDF: '{file_path}' is not a file entry"));
        }
        let mut dest = File::create(dest_path).map_err(|e| format!("Cannot create: {e}"))?;
        self.copy_alloc_to_file(&fe, part_ref, &mut dest)
    }

    pub fn extract_directory(&mut self, dir_path: &str, dest_path: &str) -> Result<(), String> {
        let (lbn, part_ref) = self.resolve(dir_path)?;
        self.extract_dir_at(lbn, part_ref, Path::new(dest_path))
    }

    fn extract_dir_at(&mut self, icb_lbn: u32, part_ref: u16, dest: &Path) -> Result<(), String> {
        fs::create_dir_all(dest).map_err(|e| format!("Cannot create dir: {e}"))?;

        let fe = self.read_sector_for_part(icb_lbn as u64, part_ref)?;
        let tag = tag_id(&fe);
        if tag != TAG_FILE_ENTRY && tag != TAG_EXT_FILE_ENTRY {
            return Err(format!("UDF: LBN {icb_lbn} has tag {tag}, expected file entry"));
        }
        let dir_data = self.read_alloc_bytes(&fe, part_ref)?;

        // Collect entries before processing, to avoid borrow conflicts.
        let mut children: Vec<(String, u32, u16, bool)> = Vec::new();
        let mut pos = 0usize;

        while pos + 38 <= dir_data.len() {
            if le16(&dir_data, pos) != TAG_FID { break; }
            let file_chars = dir_data[pos + 18];
            let l_fi = dir_data[pos + 19] as usize;
            let icb_lbn_entry = le32(&dir_data, pos + 24);
            let icb_part_ref_entry = le16(&dir_data, pos + 28);
            let l_iu = le16(&dir_data, pos + 36) as usize;
            let name_start = pos + 38 + l_iu;
            let name_end = name_start + l_fi;
            let fid_len = ((38 + l_iu + l_fi) + 3) & !3;
            let is_parent = (file_chars & 0x08) != 0;
            let is_dir = (file_chars & 0x02) != 0;

            if !is_parent && l_fi > 0 && name_end <= dir_data.len() {
                let name = cs0_to_string(&dir_data[name_start..name_end]);
                children.push((name, icb_lbn_entry, icb_part_ref_entry, is_dir));
            }
            pos += fid_len.max(38);
        }

        for (name, child_lbn, child_part_ref, is_dir) in children {
            let name = name.trim_end_matches(|c| c == '/' || c == '\\');
            let child_path = dest.join(name);
            if is_dir {
                self.extract_dir_at(child_lbn, child_part_ref, &child_path)?;
            } else {
                let child_fe = self.read_sector_for_part(child_lbn as u64, child_part_ref)?;
                let mut f = File::create(&child_path)
                    .map_err(|e| format!("Cannot create '{}': {e}", child_path.display()))?;
                self.copy_alloc_to_file(&child_fe, child_part_ref, &mut f)?;
            }
        }
        Ok(())
    }
}
