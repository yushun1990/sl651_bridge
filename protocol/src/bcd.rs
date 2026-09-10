//! BCD 编解码与时间编解码。
//!
//! 规约时间编码: 发报时间 6 字节 BCD `YYMMDDHHmmSS`; 观测时间 5 字节 BCD `YYMMDDHHmm`。
//! 两位年份按 pivot 展开 (>=70 视为 19xx, 否则 20xx)。

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};

use crate::error::{Error, Result};

fn nibble(b: u8) -> Result<u32> {
    let n = b >> 4;
    let l = b & 0x0F;
    if n > 9 || l > 9 {
        return Err(Error::BadBcd(b as char));
    }
    Ok((n as u32) * 10 + l as u32)
}

/// BCD 字节串 -> 无符号整数 (高位在前)。
pub fn bcd_to_u64(bytes: &[u8]) -> Result<u64> {
    let mut v: u64 = 0;
    for &b in bytes {
        v = v.checked_mul(100).ok_or(Error::BadBcd(b as char))? + nibble(b)? as u64;
    }
    Ok(v)
}

/// BCD 字节串 -> 十进制数字字符串 (每个字节两位数字)。
pub fn bcd_to_digits(bytes: &[u8]) -> Result<String> {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let n = nibble(b)?;
        s.push_str(&format!("{n:02}"));
    }
    Ok(s)
}

/// 十进制数字字符串 -> BCD 字节串, 左侧补零至 `out_len` 字节。
pub fn digits_to_bcd(digits: &str, out_len: usize) -> Result<Vec<u8>> {
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::BadBcd(digits.chars().next().unwrap_or('?')));
    }
    let padded = if digits.len() % 2 == 1 {
        format!("0{digits}")
    } else {
        digits.to_string()
    };
    let mut bytes = Vec::with_capacity(padded.len() / 2);
    for pair in padded.as_bytes().chunks(2) {
        bytes.push(((pair[0] - b'0') << 4) | (pair[1] - b'0'));
    }
    if bytes.len() > out_len {
        return Err(Error::BadBcd(digits.chars().next().unwrap_or('?')));
    }
    while bytes.len() < out_len {
        bytes.insert(0, 0);
    }
    Ok(bytes)
}

fn expand_year(yy: u32) -> i32 {
    if yy >= 70 {
        1900 + yy as i32
    } else {
        2000 + yy as i32
    }
}

/// 解析 BCD 时间: 5 字节 `YYMMDDHHmm` 或 6 字节 `YYMMDDHHmmSS`。
pub fn parse_bcd_time(bytes: &[u8]) -> Option<NaiveDateTime> {
    if bytes.len() != 5 && bytes.len() != 6 {
        return None;
    }
    let digits = bcd_to_digits(bytes).ok()?;
    parse_time_digits(&digits)
}

/// 解析数字字符串时间: 10 位 `YYMMDDHHmm` 或 12 位 `YYMMDDHHmmSS` (ASCII 报文正文用)。
pub fn parse_time_digits(digits: &str) -> Option<NaiveDateTime> {
    let b = digits.as_bytes();
    if b.len() != 10 && b.len() != 12 {
        return None;
    }
    if !b.iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let two = |i: usize| -> i32 { (b[i] - b'0') as i32 * 10 + (b[i + 1] - b'0') as i32 };
    let (yy, mo, dd, hh, mi, ss) = (
        two(0),
        two(2),
        two(4),
        two(6),
        two(8),
        if b.len() == 12 { two(10) } else { 0 },
    );
    let date = NaiveDate::from_ymd_opt(expand_year(yy as u32), mo as u32, dd as u32)?;
    date.and_hms_opt(hh as u32, mi as u32, ss as u32)
}

/// 生成 6 字节 BCD 发报时间 `YYMMDDHHmmSS`。
pub fn time_to_bcd6(t: NaiveDateTime) -> [u8; 6] {
    let s = format!(
        "{:02}{:02}{:02}{:02}{:02}{:02}",
        t.year() % 100,
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    );
    let b = digits_to_bcd(&s, 6).expect("time digits valid");
    [b[0], b[1], b[2], b[3], b[4], b[5]]
}

/// 生成 5 字节 BCD 观测时间 `YYMMDDHHmm`。
pub fn time_to_bcd5(t: NaiveDateTime) -> [u8; 5] {
    let s = format!(
        "{:02}{:02}{:02}{:02}{:02}",
        t.year() % 100,
        t.month(),
        t.day(),
        t.hour(),
        t.minute()
    );
    let b = digits_to_bcd(&s, 5).expect("time digits valid");
    [b[0], b[1], b[2], b[3], b[4]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_roundtrip() {
        assert_eq!(bcd_to_u64(&[0x50, 0x10, 0x12, 0x34, 0x56]).unwrap(), 5010123456);
        assert_eq!(bcd_to_digits(&[0x01, 0x23]).unwrap(), "0123");
        let b = digits_to_bcd("5010123456", 5).unwrap();
        assert_eq!(b, vec![0x50, 0x10, 0x12, 0x34, 0x56]);
        assert_eq!(digits_to_bcd("123", 2).unwrap(), vec![0x01, 0x23]);
        assert!(digits_to_bcd("12x", 2).is_err());
    }

    #[test]
    fn time_parse() {
        // 26-09-09 08:00:00 (发报时间, 6字节 BCD)
        let t = parse_bcd_time(&[0x26, 0x09, 0x09, 0x08, 0x00, 0x00]).unwrap();
        assert_eq!(t, NaiveDate::from_ymd_opt(2026, 9, 9).unwrap().and_hms_opt(8, 0, 0).unwrap());
        // 观测时间 5 字节
        let t5 = parse_bcd_time(&[0x26, 0x09, 0x09, 0x08, 0x00]).unwrap();
        assert_eq!(t5, t);
        // ASCII 数字串
        assert_eq!(parse_time_digits("2609090800").unwrap(), t);
        assert_eq!(parse_time_digits("260909080000").unwrap(), t);
        // pivot: 99 -> 1999
        assert_eq!(
            parse_time_digits("991231235959").unwrap().year(),
            1999
        );
        assert!(parse_time_digits("26090908").is_none());
        assert!(parse_time_digits("2613320800").is_none());
    }

    #[test]
    fn time_encode() {
        let t = parse_time_digits("260909080000").unwrap();
        assert_eq!(time_to_bcd6(t), [0x26, 0x09, 0x09, 0x08, 0x00, 0x00]);
        assert_eq!(time_to_bcd5(t), [0x26, 0x09, 0x09, 0x08, 0x00]);
    }
}
