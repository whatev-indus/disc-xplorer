// SPDX-License-Identifier: (MIT OR Apache-2.0)

use nom::bytes::complete::take;
use nom::number::complete::*;
use nom::sequence::terminated;
use nom::IResult;

// ISO 9660 uses a representation for integers with both little
// and big endian representations of the same number.

// This only reads the little endian version.
// The Linux kernel does the same, with a comment about some programs
// generating invalid ISO with incorrect big endian values.

pub fn both_endian16(i: &[u8]) -> IResult<&[u8], u16> {
    terminated(le_u16, take(2usize))(i)
}

pub fn both_endian32(i: &[u8]) -> IResult<&[u8], u32> {
    terminated(le_u32, take(4usize))(i)
}
