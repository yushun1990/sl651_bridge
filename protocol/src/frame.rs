//! 帧层: 表16~表23 的帧结构与流式分帧。
//!
//! ASCII 编码帧 (表16~19):
//! ```text
//! SOH(01) 中心站地址[2 ASCII-HEX] 遥测站地址[10 BCD字符] 密码[4] 功能码[2]
//! 方向及长度[4: '0'上行/'8'下行 + 3位HEX正文长度] STX(02)|SYN(16)
//! [M3: 包总数3字符+序号3字符] 正文[LEN] ETX(03)|ETB(17)|ENQ|ACK|EOT|ESC|NAK
//! CRC16[4 ASCII-HEX, 高位字节在前]
//! ```
//! 长度字段表示报文起始符之后、报文结束符之前的字节数 (M3 含 6 字符包序号字段)。
//!
//! HEX/BCD 编码帧 (表20~23): `7E7E [1B中心站][5B站址BCD][2B密码][1B功能码]
//! [2B方向+长度: 高4位方向, 低12位长度] STX|SYN [M3: 3B 包总数/序号] 正文 结束符 CRC16[2B]`。
//!
//! CRC 覆盖帧起始符至报文结束符 (含) 的全部字节。

use crate::bcd::{bcd_to_digits, digits_to_bcd};
use crate::crc16::crc16;
use crate::error::{Error, Result};

// 控制字符 (表10)
pub const SOH: u8 = 0x01;
pub const STX: u8 = 0x02;
pub const ETX: u8 = 0x03;
pub const EOT: u8 = 0x04;
pub const ENQ: u8 = 0x05;
pub const ACK: u8 = 0x06;
pub const NAK: u8 = 0x15;
pub const SYN: u8 = 0x16;
pub const ETB: u8 = 0x17;
pub const ESC: u8 = 0x1B;

/// 正文长度上限 (12 位, 0001~4095)。
pub const MAX_BODY_LEN: usize = 0x0FFF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// ASCⅡ 字符编码 (§6.4)
    Ascii,
    /// HEX/BCD 编码 (§6.5)
    Hex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// 上行 (遥测站 -> 中心站)
    Uplink,
    /// 下行 (中心站 -> 遥测站)
    Downlink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndChar {
    /// 上行结束: 后续无报文; 下行: 传输结束退出
    Etx,
    /// 上行结束: 后续还有报文
    Etb,
    /// 下行询问
    Enq,
    /// 下行肯定确认: 继续发送
    AckChar,
    /// 下行: 传输结束退出
    Eot,
    /// 下行: 结束但终端保持在线 10 分钟
    Esc,
    /// 下行否认: 反馈重发
    Nak,
}

impl EndChar {
    pub fn to_byte(self) -> u8 {
        match self {
            EndChar::Etx => ETX,
            EndChar::Etb => ETB,
            EndChar::Enq => ENQ,
            EndChar::AckChar => ACK,
            EndChar::Eot => EOT,
            EndChar::Esc => ESC,
            EndChar::Nak => NAK,
        }
    }

    pub fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            ETX => EndChar::Etx,
            ETB => EndChar::Etb,
            ENQ => EndChar::Enq,
            ACK => EndChar::AckChar,
            EOT => EndChar::Eot,
            ESC => EndChar::Esc,
            NAK => EndChar::Nak,
            _ => return None,
        })
    }
}

/// M3 多包传输的 包总数及序列号 (各 12 位, 1~4095)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct M3Seq {
    pub total: u16,
    pub seq: u16,
}

/// 解析后的完整帧。
#[derive(Clone, Debug)]
pub struct Frame {
    pub encoding: Encoding,
    pub direction: Direction,
    /// 中心站地址 (1~255)
    pub center_addr: u8,
    /// 遥测站地址, 10 位 BCD 数字字符
    pub station_addr: String,
    /// 密码
    pub password: u16,
    /// 功能码 (附录B)
    pub func: u8,
    /// 报文结束符
    pub end: EndChar,
    /// 正文 (M3 序号字段已剥离)
    pub body: Vec<u8>,
    /// M3 包信息 (SYN 起始符时存在)
    pub m3: Option<M3Seq>,
    /// 报文中的校验码
    pub crc: u16,
}

impl Frame {
    /// 按报文校验码验证 CRC。
    pub fn crc_ok(&self) -> bool {
        self.compute_crc() == self.crc
    }

    /// 对当前帧内容重算 CRC。
    pub fn compute_crc(&self) -> u16 {
        let head = build_header(self.encoding, self.direction, &self.station_addr, self.center_addr, self.password, self.func, self.body.len() + m3_field_len(self.encoding, self.m3));
        let mut data = head;
        push_m3_and_body(&mut data, self.encoding, self.m3, &self.body);
        data.push(self.end.to_byte());
        crc16(&data)
    }
}

/// 待构造帧的参数。
#[derive(Clone, Debug)]
pub struct OutFrame<'a> {
    pub encoding: Encoding,
    pub direction: Direction,
    pub center_addr: u8,
    /// 10 位 BCD 数字字符串
    pub station_addr: &'a str,
    pub password: u16,
    pub func: u8,
    pub end: EndChar,
    pub m3: Option<M3Seq>,
    pub body: &'a [u8],
}

/// 构造完整帧 (含 CRC)。
pub fn encode(f: &OutFrame) -> Vec<u8> {
    let len = f.body.len() + m3_field_len(f.encoding, f.m3);
    let mut data = build_header(f.encoding, f.direction, f.station_addr, f.center_addr, f.password, f.func, len);
    push_m3_and_body(&mut data, f.encoding, f.m3, f.body);
    data.push(f.end.to_byte());
    let crc = crc16(&data);
    match f.encoding {
        Encoding::Ascii => push_hex_u16(&mut data, crc),
        Encoding::Hex => data.extend_from_slice(&crc.to_be_bytes()),
    }
    data
}

fn m3_field_len(encoding: Encoding, m3: Option<M3Seq>) -> usize {
    match (encoding, m3) {
        (_, None) => 0,
        (Encoding::Ascii, Some(_)) => 6, // 3+3 个 ASCII-HEX 字符
        (Encoding::Hex, Some(_)) => 3,   // 3 字节: 高12位总数, 低12位序号
    }
}

fn build_header(
    encoding: Encoding,
    direction: Direction,
    station_addr: &str,
    center_addr: u8,
    password: u16,
    func: u8,
    len: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + len);
    match encoding {
        Encoding::Ascii => {
            out.push(SOH);
            // 上行: 中心站地址在前; 下行: 遥测站地址在前 (表16/17)
            if direction == Direction::Uplink {
                push_hex_u8(&mut out, center_addr);
                out.extend_from_slice(station_addr.as_bytes());
            } else {
                out.extend_from_slice(station_addr.as_bytes());
                push_hex_u8(&mut out, center_addr);
            }
            push_hex_u16(&mut out, password);
            push_hex_u8(&mut out, func);
            let dir_char = if direction == Direction::Uplink { b'0' } else { b'8' };
            out.push(dir_char);
            push_hex_len3(&mut out, len);
            // 报文起始符由调用方继续追加
        }
        Encoding::Hex => {
            out.extend_from_slice(&[0x7E, 0x7E]);
            let station = station_to_bytes(station_addr).unwrap_or([0; 5]).to_vec();
            if direction == Direction::Uplink {
                out.push(center_addr);
                out.extend_from_slice(&station);
            } else {
                out.extend_from_slice(&station);
                out.push(center_addr);
            }
            out.extend_from_slice(&password.to_be_bytes());
            out.push(func);
            let dir_nibble: u16 = if direction == Direction::Uplink { 0 } else { 8 << 12 };
            out.extend_from_slice(&(dir_nibble | len as u16).to_be_bytes());
        }
    }
    out
}

fn push_m3_and_body(data: &mut Vec<u8>, encoding: Encoding, m3: Option<M3Seq>, body: &[u8]) {
    if let Some(m3) = m3 {
        data.push(SYN);
        match encoding {
            Encoding::Ascii => {
                push_hex_u12(data, m3.total);
                push_hex_u12(data, m3.seq);
            }
            Encoding::Hex => {
                // 高12位包总数, 低12位序列号, 3字节
                let v: u32 = ((m3.total as u32) << 12) | m3.seq as u32;
                data.extend_from_slice(&v.to_be_bytes()[1..]);
            }
        }
    } else {
        data.push(STX);
    }
    data.extend_from_slice(body);
}

// ---------- ASCII-HEX 辅助 ----------

fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Error::BadHex(c as char)),
    }
}

fn parse_hex(slice: &[u8]) -> Result<u64> {
    // 长度字段(3字符)与 M3 序号字段(3字符)为奇数位, 不能强制偶数
    if slice.is_empty() || slice.len() > 8 {
        return Err(Error::BadHex(slice.first().map(|&c| c as char).unwrap_or('?')));
    }
    let mut v: u64 = 0;
    for &c in slice {
        v = (v << 4) | hex_val(c)? as u64;
    }
    Ok(v)
}

fn push_hex_u8(out: &mut Vec<u8>, v: u8) {
    out.extend_from_slice(format!("{v:02X}").as_bytes());
}

fn push_hex_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(format!("{v:04X}").as_bytes());
}

fn push_hex_u12(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(format!("{v:03X}").as_bytes());
}

fn push_hex_len3(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(format!("{len:03X}").as_bytes());
}

// ---------- 解析 ----------

/// 流式分帧结果。
#[derive(Debug)]
pub enum Scanned {
    /// 成功取出一帧; `consumed` 含帧前被跳过的垃圾字节
    Frame { frame: Frame, consumed: usize },
    /// 数据不足, 保留缓冲等待更多字节
    NeedMore,
    /// 跳过 `skip` 字节 (垃圾或损坏帧) 后重扫; CRC 失败时 `partial` 携带
    /// 已解析的帧信息 (用于 M3 坏包 NAK)
    Skip { skip: usize, reason: Error, partial: Option<Frame> },
}

/// 从缓冲区头部扫描一帧 (自动跳过帧前垃圾)。TCP 流式与 UDP 数据报通用。
pub fn scan(buf: &[u8]) -> Scanned {
    let mut idx = 0;
    while idx < buf.len() {
        let b = buf[idx];
        if b == SOH {
            return scan_from(buf, idx, Encoding::Ascii);
        }
        if b == 0x7E {
            if idx + 1 < buf.len() {
                if buf[idx + 1] == 0x7E {
                    return scan_from(buf, idx, Encoding::Hex);
                }
                // 7E 后不是 7E, 当作垃圾跳过
                idx += 1;
                continue;
            }
            // 末尾孤立 7E, 可能是 7E7E 前半
            return if idx == 0 {
                Scanned::NeedMore
            } else {
                Scanned::Skip { skip: idx, reason: Error::BadStart(b), partial: None }
            };
        }
        idx += 1;
    }
    // 无起始符: 全部丢弃但保留末尾 1 字节以防半截起始符 (此处不可能是 SOH/7E7E 开头)
    let keep = buf.len().saturating_sub(1);
    if keep == 0 {
        Scanned::NeedMore
    } else {
        Scanned::Skip { skip: keep, reason: Error::BadStart(*buf.last().unwrap()), partial: None }
    }
}

fn scan_from(buf: &[u8], start: usize, encoding: Encoding) -> Scanned {
    match parse_frame(&buf[start..], encoding) {
        Ok((frame, len)) => {
            if frame.crc_ok() {
                Scanned::Frame { frame, consumed: start + len }
            } else {
                Scanned::Skip {
                    skip: start + 1,
                    reason: Error::CrcMismatch { got: frame.crc, expect: frame.compute_crc() },
                    partial: Some(frame),
                }
            }
        }
        Err(Error::TooShort { need, .. }) => {
            let _ = need;
            Scanned::NeedMore
        }
        Err(e) => Scanned::Skip { skip: start + 1, reason: e, partial: None },
    }
}

fn parse_frame(buf: &[u8], encoding: Encoding) -> Result<(Frame, usize)> {
    match encoding {
        Encoding::Ascii => parse_ascii(buf),
        Encoding::Hex => parse_hex_frame(buf),
    }
}

/// ASCII 帧最小长度: 头 24 + 结束符 1 + CRC 4 (正文可为空)
const ASCII_MIN: usize = 29;

/// 遥测站地址 5 字节 -> 10 字符: 前 3 字节 BCD (6位行政区划) + 后 2 字节 HEX (自定义段)。
pub fn station_to_string(bytes: &[u8]) -> Result<String> {
    if bytes.len() < 5 {
        return Err(Error::TooShort { need: 5, have: bytes.len() });
    }
    let mut s = bcd_to_digits(&bytes[..3])?;
    s.push_str(&format!("{:02X}{:02X}", bytes[3], bytes[4]));
    Ok(s)
}

/// 10 字符站址 -> 5 字节 (前 6 位 BCD + 后 4 位 HEX)。
pub fn station_to_bytes(addr: &str) -> Result<[u8; 5]> {
    let b = addr.as_bytes();
    if b.len() != 10
        || !b[..6].iter().all(|c| c.is_ascii_digit())
        || !b[6..].iter().all(|c| c.is_ascii_hexdigit())
    {
        return Err(Error::BadStationAddr(addr.to_string()));
    }
    let bcd = digits_to_bcd(&addr[..6], 3)?;
    let tail = u16::from_str_radix(&addr[6..], 16).map_err(|_| Error::BadStationAddr(addr.to_string()))?;
    Ok([bcd[0], bcd[1], bcd[2], (tail >> 8) as u8, (tail & 0xFF) as u8])
}

fn parse_ascii(buf: &[u8]) -> Result<(Frame, usize)> {
    if buf.len() < ASCII_MIN {
        return Err(Error::TooShort { need: ASCII_MIN, have: buf.len() });
    }
    // 方向字段位置固定; 地址顺序随方向: 上行 中心站/遥测站, 下行 遥测站/中心站 (表16/17)
    let dir_char = buf[19];
    let direction = match dir_char {
        b'0' => Direction::Uplink,
        b'8' => Direction::Downlink,
        _ => return Err(Error::BadDirection(dir_char)),
    };
    let digits_field = |s: &[u8]| -> Result<String> {
        let t = std::str::from_utf8(s)
            .map_err(|_| Error::BadStationAddr(String::from_utf8_lossy(s).into_owned()))?;
        // 表13/14: 前 6 位 BCD 数字(行政区划/水文站前缀) + 后 4 位 HEX 字符(自定义段)
        let b = t.as_bytes();
        let valid = b.len() == 10
            && b[..6].iter().all(|c| c.is_ascii_digit())
            && b[6..].iter().all(|c| c.is_ascii_hexdigit());
        if !valid {
            return Err(Error::BadStationAddr(t.to_string()));
        }
        Ok(t.to_ascii_uppercase())
    };
    let (center_addr, station_addr) = match direction {
        Direction::Uplink => (parse_hex(&buf[1..3])? as u8, digits_field(&buf[3..13])?),
        Direction::Downlink => (parse_hex(&buf[11..13])? as u8, digits_field(&buf[1..11])?),
    };
    let password = parse_hex(&buf[13..17])? as u16;
    let func = parse_hex(&buf[17..19])? as u8;
    let len = parse_hex(&buf[20..23])? as usize;
    if !(1..=MAX_BODY_LEN).contains(&len) {
        return Err(Error::BadLength(len));
    }
    let total_len = ASCII_MIN + len;
    if buf.len() < total_len {
        return Err(Error::TooShort { need: total_len, have: buf.len() });
    }
    let start_marker = buf[23];
    let (m3, body) = match start_marker {
        STX => (None, &buf[24..24 + len]),
        SYN => {
            if len < 6 {
                return Err(Error::BadLength(len));
            }
            let total = parse_hex(&buf[24..27])? as u16;
            let seq = parse_hex(&buf[27..30])? as u16;
            (Some(M3Seq { total, seq }), &buf[30..24 + len])
        }
        other => return Err(Error::BadStart(other)),
    };
    let end = EndChar::from_byte(buf[24 + len]).ok_or(Error::BadEnd(buf[24 + len]))?;
    let crc = parse_hex(&buf[25 + len..29 + len])? as u16;

    let frame = Frame {
        encoding: Encoding::Ascii,
        direction,
        center_addr,
        station_addr,
        password,
        func,
        end,
        body: body.to_vec(),
        m3,
        crc,
    };
    Ok((frame, total_len))
}

/// HEX 帧最小长度: 2+1+5+2+1+2+1+1+2 = 17
const HEX_MIN: usize = 17;

fn parse_hex_frame(buf: &[u8]) -> Result<(Frame, usize)> {
    if buf.len() < HEX_MIN {
        return Err(Error::TooShort { need: HEX_MIN, have: buf.len() });
    }
    let center_first = buf[2];
    let password = u16::from_be_bytes([buf[8], buf[9]]);
    let func = buf[10];
    let dir_len = u16::from_be_bytes([buf[11], buf[12]]);
    let direction = match dir_len >> 12 {
        0 => Direction::Uplink,
        8 => Direction::Downlink,
        n => return Err(Error::BadDirection(n as u8)),
    };
    // 地址顺序: 上行 中心站/遥测站, 下行 遥测站/中心站 (表20/21)
    let (center_addr, station_addr) = match direction {
        Direction::Uplink => (center_first, station_to_string(&buf[3..8])?),
        Direction::Downlink => (buf[7], station_to_string(&buf[2..7])?),
    };
    let len = (dir_len & 0x0FFF) as usize;
    if !(1..=MAX_BODY_LEN).contains(&len) {
        return Err(Error::BadLength(len));
    }
    let total_len = HEX_MIN + len;
    if buf.len() < total_len {
        return Err(Error::TooShort { need: total_len, have: buf.len() });
    }
    let start_marker = buf[13];
    let (m3, body) = match start_marker {
        STX => (None, &buf[14..14 + len]),
        SYN => {
            if len < 3 {
                return Err(Error::BadLength(len));
            }
            let v = u32::from_be_bytes([0, buf[14], buf[15], buf[16]]);
            (Some(M3Seq { total: (v >> 12) as u16, seq: (v & 0xFFF) as u16 }), &buf[17..14 + len])
        }
        other => return Err(Error::BadStart(other)),
    };
    let end = EndChar::from_byte(buf[14 + len]).ok_or(Error::BadEnd(buf[14 + len]))?;
    let crc = u16::from_be_bytes([buf[15 + len], buf[16 + len]]);

    let frame = Frame {
        encoding: Encoding::Hex,
        direction,
        center_addr,
        station_addr,
        password,
        func,
        end,
        body: body.to_vec(),
        m3,
        crc,
    };
    Ok((frame, total_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_body() -> Vec<u8> {
        b"0001 260909080000 ST 5010123456 H TT 2609090800 Z 6.38 VT 12.50 "
            .to_vec()
    }

    #[test]
    fn ascii_roundtrip() {
        let body = sample_body();
        let out = OutFrame {
            encoding: Encoding::Ascii,
            direction: Direction::Uplink,
            center_addr: 1,
            station_addr: "5010123456",
            password: 0,
            func: 0x32,
            end: EndChar::Etx,
            m3: None,
            body: &body,
        };
        let bytes = encode(&out);
        match scan(&bytes) {
            Scanned::Frame { frame, consumed } => {
                assert_eq!(consumed, bytes.len());
                assert_eq!(frame.encoding, Encoding::Ascii);
                assert_eq!(frame.direction, Direction::Uplink);
                assert_eq!(frame.center_addr, 1);
                assert_eq!(frame.station_addr, "5010123456");
                assert_eq!(frame.func, 0x32);
                assert_eq!(frame.end, EndChar::Etx);
                assert_eq!(frame.body, body);
                assert!(frame.m3.is_none());
                assert!(frame.crc_ok());
            }
            other => panic!("scan failed: {other:?}"),
        }
    }

    #[test]
    fn ascii_downlink_order() {
        let body = b"0001260909080000".to_vec();
        let out = OutFrame {
            encoding: Encoding::Ascii,
            direction: Direction::Downlink,
            center_addr: 7,
            station_addr: "5010123456",
            password: 0x1234,
            func: 0x32,
            end: EndChar::Eot,
            m3: None,
            body: &body,
        };
        let bytes = encode(&out);
        // 下行: 站址在前
        assert_eq!(&bytes[1..11], b"5010123456");
        assert_eq!(&bytes[11..13], b"07");
        match scan(&bytes) {
            Scanned::Frame { frame, .. } => {
                assert_eq!(frame.direction, Direction::Downlink);
                assert_eq!(frame.station_addr, "5010123456");
                assert_eq!(frame.center_addr, 7);
                assert_eq!(frame.password, 0x1234);
                assert!(frame.crc_ok());
            }
            other => panic!("scan failed: {other:?}"),
        }
    }

    #[test]
    fn hex_roundtrip() {
        let body = sample_body();
        let out = OutFrame {
            encoding: Encoding::Hex,
            direction: Direction::Uplink,
            center_addr: 1,
            station_addr: "5010123456",
            password: 0xABCD,
            func: 0x33,
            end: EndChar::Etb,
            m3: None,
            body: &body,
        };
        let bytes = encode(&out);
        assert_eq!(&bytes[0..2], &[0x7E, 0x7E]);
        match scan(&bytes) {
            Scanned::Frame { frame, consumed } => {
                assert_eq!(consumed, bytes.len());
                assert_eq!(frame.encoding, Encoding::Hex);
                assert_eq!(frame.station_addr, "5010123456");
                assert_eq!(frame.password, 0xABCD);
                assert_eq!(frame.func, 0x33);
                assert_eq!(frame.end, EndChar::Etb);
                assert_eq!(frame.body, body);
                assert!(frame.crc_ok());
            }
            other => panic!("scan failed: {other:?}"),
        }
    }

    #[test]
    fn m3_ascii() {
        let body = b"chunk-data".to_vec();
        let out = OutFrame {
            encoding: Encoding::Ascii,
            direction: Direction::Uplink,
            center_addr: 1,
            station_addr: "5010123456",
            password: 0,
            func: 0x31,
            end: EndChar::Etb,
            m3: Some(M3Seq { total: 5, seq: 2 }),
            body: &body,
        };
        let bytes = encode(&out);
        // SYN + 015 002
        assert_eq!(bytes[23], SYN);
        assert_eq!(&bytes[24..27], b"005");
        assert_eq!(&bytes[27..30], b"002");
        match scan(&bytes) {
            Scanned::Frame { frame, .. } => {
                assert_eq!(frame.m3, Some(M3Seq { total: 5, seq: 2 }));
                assert_eq!(frame.body, body);
                assert!(frame.crc_ok());
            }
            other => panic!("scan failed: {other:?}"),
        }
    }

    #[test]
    fn m3_hex() {
        let body = b"x".to_vec();
        let out = OutFrame {
            encoding: Encoding::Hex,
            direction: Direction::Uplink,
            center_addr: 1,
            station_addr: "5010123456",
            password: 0,
            func: 0x36,
            end: EndChar::Etx,
            m3: Some(M3Seq { total: 3, seq: 3 }),
            body: &body,
        };
        let bytes = encode(&out);
        match scan(&bytes) {
            Scanned::Frame { frame, .. } => {
                assert_eq!(frame.m3, Some(M3Seq { total: 3, seq: 3 }));
                assert!(frame.crc_ok());
            }
            other => panic!("scan failed: {other:?}"),
        }
    }

    #[test]
    fn scan_skips_garbage_and_bad_crc() {
        let body = sample_body();
        let out = OutFrame {
            encoding: Encoding::Ascii,
            direction: Direction::Uplink,
            center_addr: 1,
            station_addr: "5010123456",
            password: 0,
            func: 0x32,
            end: EndChar::Etx,
            m3: None,
            body: &body,
        };
        let mut bytes = encode(&out);
        // 前置垃圾 + 破坏 CRC
        let mut buf = b"garbage!!".to_vec();
        let mut corrupted = bytes.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xFF;
        buf.extend_from_slice(&corrupted);
        buf.extend_from_slice(&bytes);
        bytes.clear();

        let mut got_frame = 0;
        let mut pos = 0;
        loop {
            match scan(&buf[pos..]) {
                Scanned::Frame { consumed, .. } => {
                    got_frame += 1;
                    pos += consumed;
                }
                Scanned::NeedMore => break,
                Scanned::Skip { skip, .. } => pos += skip,
            }
            if pos >= buf.len() {
                break;
            }
        }
        assert_eq!(got_frame, 1, "应从损坏帧后恢复并解析出好帧");
    }

    #[test]
    fn incomplete_returns_need_more() {
        let body = sample_body();
        let out = OutFrame {
            encoding: Encoding::Ascii,
            direction: Direction::Uplink,
            center_addr: 1,
            station_addr: "5010123456",
            password: 0,
            func: 0x32,
            end: EndChar::Etx,
            m3: None,
            body: &body,
        };
        let bytes = encode(&out);
        assert!(matches!(scan(&bytes[..bytes.len() - 3]), Scanned::NeedMore));
        // 半截 HEX 起始符
        assert!(matches!(scan(&[0x7E]), Scanned::NeedMore));
    }
}
