// SPDX-License-Identifier: (MIT OR Apache-2.0)

use time::OffsetDateTime;

use super::both_endian::{both_endian16, both_endian32};
use super::date_time::date_time;
use crate::Result;
use nom::combinator::{map, map_res};
use nom::multi::length_data;
use nom::number::complete::le_u8;
use nom::IResult;
use std::str;

bitflags! {
    #[derive(Clone, Debug)]
    pub struct FileFlags: u8 {
        const EXISTANCE = 1 << 0;
        const DIRECTORY = 1 << 1;
        const ASSOCIATEDFILE = 1 << 2;
        const RECORD = 1 << 3;
        const PROTECTION = 1 << 4;
        // Bits 5 and 6 are reserved; should be zero
        const MULTIEXTENT = 1 << 7;
    }
}

#[derive(Clone, Debug)]
pub struct DirectoryEntryHeader {
    pub length: u8,
    pub extended_attribute_record_length: u8,
    pub extent_loc: u32,
    pub extent_length: u32,
    pub time: OffsetDateTime,
    pub file_flags: FileFlags,
    pub file_unit_size: u8,
    pub interleave_gap_size: u8,
    pub volume_sequence_number: u16,
}

impl DirectoryEntryHeader {
    pub fn parse(input: &[u8]) -> Result<(DirectoryEntryHeader, String)> {
        Ok(directory_entry(input)?.1)
    }
}

pub fn directory_entry(i: &[u8]) -> IResult<&[u8], (DirectoryEntryHeader, String)> {
    let (i, length) = le_u8(i)?;
    let (i, extended_attribute_record_length) = le_u8(i)?;
    let (i, extent_loc) = both_endian32(i)?;
    let (i, extent_length) = both_endian32(i)?;
    let (i, time) = date_time(i)?;
    let (i, file_flags) = le_u8(i)?;
    let (i, file_unit_size) = le_u8(i)?;
    let (i, interleave_gap_size) = le_u8(i)?;
    let (i, volume_sequence_number) = both_endian16(i)?;
    let (i, identifier) = map(map_res(length_data(le_u8), str::from_utf8), str::to_string)(i)?;
    // After the file identifier, ISO 9660 allows addition space for
    // system use. Ignore that for now.

    Ok((
        i,
        (
            DirectoryEntryHeader {
                length,
                extended_attribute_record_length,
                extent_loc,
                extent_length,
                time,
                file_flags: FileFlags::from_bits_truncate(file_flags),
                file_unit_size,
                interleave_gap_size,
                volume_sequence_number,
            },
            identifier,
        ),
    ))
}
