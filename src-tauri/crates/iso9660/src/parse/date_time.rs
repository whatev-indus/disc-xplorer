// SPDX-License-Identifier: (MIT OR Apache-2.0)

use nom::bytes::complete::take;
use nom::number::complete::le_u8;
use nom::sequence::tuple;
use nom::IResult;
use std::convert::TryFrom;
use std::str;
use time::{Date, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

pub fn date_time(i: &[u8]) -> IResult<&[u8], OffsetDateTime> {
    let (i, (year, month, day, hour, minute, second, gmt_offset)) =
        tuple((le_u8, le_u8, le_u8, le_u8, le_u8, le_u8, le_u8))(i)?;

    // Create Date and Time from parsed values. Since those values can be 0,
    // creating Date and Time struct can fail, in this case assume default
    // values.
    let date = Date::from_calendar_date(
        1900 + year as i32,
        time::Month::try_from(month).unwrap_or(time::Month::January),
        day,
    )
    .unwrap_or_else(|_| Date::from_calendar_date(0, time::Month::January, 1).unwrap());

    let time =
        Time::from_hms(hour, minute, second).unwrap_or_else(|_| Time::from_hms(0, 0, 0).unwrap());

    // gmt_offset represents 15 minutes intervals from GMT.
    let offset =
        UtcOffset::from_whole_seconds((gmt_offset as i32) * 15 * 60).unwrap_or(UtcOffset::UTC);

    Ok((i, PrimitiveDateTime::new(date, time).assume_offset(offset)))
}

fn ascii_i32(n: usize) -> impl Fn(&[u8]) -> IResult<&[u8], i32> {
    move |i: &[u8]| {
        let (i, bytes) = take(n)(i)?;
        let s = str::from_utf8(bytes).unwrap_or("0");
        let v = s.trim().parse::<i32>().unwrap_or(0);
        Ok((i, v))
    }
}

pub fn date_time_ascii(i: &[u8]) -> IResult<&[u8], OffsetDateTime> {
    let (i, (tm_year, tm_mon, tm_mday, tm_hour, tm_min, tm_sec, centisecond, gmt_offset)) =
        tuple((
            ascii_i32(4),
            ascii_i32(2),
            ascii_i32(2),
            ascii_i32(2),
            ascii_i32(2),
            ascii_i32(2),
            ascii_i32(2),
            le_u8,
        ))(i)?;

    let date = Date::from_calendar_date(
        1900 + tm_year,
        time::Month::try_from(tm_mon as u8).unwrap_or(time::Month::January),
        tm_mday as u8,
    )
    .unwrap_or_else(|_| Date::from_calendar_date(0, time::Month::January, 1).unwrap());

    let time = Time::from_hms_milli(
        tm_hour as u8,
        tm_min as u8,
        tm_sec as u8,
        centisecond as u16 * 10,
    )
    .unwrap_or_else(|_| Time::from_hms(0, 0, 0).unwrap());

    let offset =
        UtcOffset::from_whole_seconds((gmt_offset as i32) * 15 * 60).unwrap_or(UtcOffset::UTC);

    Ok((i, PrimitiveDateTime::new(date, time).assume_offset(offset)))
}
