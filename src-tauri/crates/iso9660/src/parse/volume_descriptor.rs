// SPDX-License-Identifier: (MIT OR Apache-2.0)

use nom::branch::alt;
use nom::bytes::complete::{tag, take};
use nom::combinator::{map, map_res};
use nom::number::complete::*;
use nom::sequence::tuple;
use nom::IResult;
use std::str;
use time::OffsetDateTime;

use super::both_endian::{both_endian16, both_endian32};
use super::date_time::date_time_ascii;
use super::directory_entry::{directory_entry, DirectoryEntryHeader};
use crate::ISOError;

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) enum VolumeDescriptor {
    Primary {
        system_identifier: String,
        volume_identifier: String,
        volume_space_size: u32,
        volume_set_size: u16,
        volume_sequence_number: u16,
        logical_block_size: u16,

        path_table_size: u32,
        path_table_loc: u32,
        optional_path_table_loc: u32,

        root_directory_entry: DirectoryEntryHeader,
        root_directory_entry_identifier: String,

        volume_set_identifier: String,
        publisher_identifier: String,
        data_preparer_identifier: String,
        application_identifier: String,
        copyright_file_identifier: String,
        abstract_file_identifier: String,
        bibliographic_file_identifier: String,

        creation_time: OffsetDateTime,
        modification_time: OffsetDateTime,
        expiration_time: OffsetDateTime,
        effective_time: OffsetDateTime,

        file_structure_version: u8,
    },
    BootRecord {
        boot_system_identifier: String,
        boot_identifier: String,
        data: Vec<u8>,
    },
    VolumeDescriptorSetTerminator,
}

impl VolumeDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Option<VolumeDescriptor>, ISOError> {
        Ok(volume_descriptor(bytes)?.1)
    }
}

fn take_string_trim(count: usize) -> impl Fn(&[u8]) -> IResult<&[u8], String> {
    move |i: &[u8]| {
        map(
            map(map_res(take(count), str::from_utf8), str::trim_end),
            str::to_string,
        )(i)
    }
}

fn boot_record(i: &[u8]) -> IResult<&[u8], VolumeDescriptor> {
    let (i, (boot_system_identifier, boot_identifier, data)) = tuple((
        take_string_trim(32usize),
        take_string_trim(32usize),
        take(1977usize),
    ))(i)?;
    Ok((
        i,
        VolumeDescriptor::BootRecord {
            boot_system_identifier,
            boot_identifier,
            data: data.to_vec(),
        },
    ))
}

fn volume_descriptor(i: &[u8]) -> IResult<&[u8], Option<VolumeDescriptor>> {
    let (i, type_code) = le_u8(i)?;
    let (i, _) = alt((tag("CD001\u{1}"), tag("CD-I \u{1}")))(i)?;
    match type_code {
        0 => map(boot_record, Some)(i),
        1 => map(primary_descriptor, Some)(i),
        //2 => map(supplementary_volume_descriptor, Some)(i),
        //3 => map!(volume_partition_descriptor, Some)(i),
        255 => Ok((i, Some(VolumeDescriptor::VolumeDescriptorSetTerminator))),
        _ => Ok((i, None)),
    }
}

fn primary_descriptor(i: &[u8]) -> IResult<&[u8], VolumeDescriptor> {
    let (i, _) = take(1usize)(i)?; // padding
    let (i, system_identifier) = take_string_trim(32usize)(i)?;
    let (i, volume_identifier) = take_string_trim(32usize)(i)?;
    let (i, _) = take(8usize)(i)?; // padding
    let (i, volume_space_size) = both_endian32(i)?;
    let (i, _) = take(32usize)(i)?; // padding
    let (i, volume_set_size) = both_endian16(i)?;
    let (i, volume_sequence_number) = both_endian16(i)?;
    let (i, logical_block_size) = both_endian16(i)?;

    let (i, path_table_size) = both_endian32(i)?;
    let (i, path_table_loc) = le_u32(i)?;
    let (i, optional_path_table_loc) = le_u32(i)?;
    let (i, _) = take(4usize)(i)?; // path_table_loc_be
    let (i, _) = take(4usize)(i)?; // optional_path_table_loc_be

    let (i, root_directory_entry) = directory_entry(i)?;

    let (i, volume_set_identifier) = take_string_trim(128)(i)?;
    let (i, publisher_identifier) = take_string_trim(128)(i)?;
    let (i, data_preparer_identifier) = take_string_trim(128)(i)?;
    let (i, application_identifier) = take_string_trim(128)(i)?;
    let (i, copyright_file_identifier) = take_string_trim(38)(i)?;
    let (i, abstract_file_identifier) = take_string_trim(36)(i)?;
    let (i, bibliographic_file_identifier) = take_string_trim(37)(i)?;

    let (i, creation_time) = date_time_ascii(i)?;
    let (i, modification_time) = date_time_ascii(i)?;
    let (i, expiration_time) = date_time_ascii(i)?;
    let (i, effective_time) = date_time_ascii(i)?;

    let (i, file_structure_version) = le_u8(i)?;

    Ok((
        i,
        VolumeDescriptor::Primary {
            system_identifier,
            volume_identifier,
            volume_space_size,
            volume_set_size,
            volume_sequence_number,
            logical_block_size,

            path_table_size,
            path_table_loc,
            optional_path_table_loc,

            root_directory_entry: root_directory_entry.0,
            root_directory_entry_identifier: root_directory_entry.1,

            volume_set_identifier,
            publisher_identifier,
            data_preparer_identifier,
            application_identifier,
            copyright_file_identifier,
            abstract_file_identifier,
            bibliographic_file_identifier,

            creation_time,
            modification_time,
            expiration_time,
            effective_time,

            file_structure_version,
        },
    ))
}
