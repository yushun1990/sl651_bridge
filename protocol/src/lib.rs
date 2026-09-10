//! SL651-2014 水文监测数据通信规约 编解码库。
//!
//! 帧层与正文层相互独立: `frame` 负责 SOH/7E7E 帧(ASCII 与 HEX/BCD 两种编码)的
//! 解析/构造/流式分帧, `body` 负责报文正文(流水号/发报时间/要素组)按功能码解析,
//! `ack` 负责中心站下行确认帧的构造。

pub mod ack;
pub mod bcd;
pub mod body;
pub mod crc16;
pub mod error;
pub mod frame;
pub mod ident;

pub use ack::{build_ack, decide_reply, nak_reply, AckSpec, FinalTerminator, Reply};
pub use body::{
    parse_body, Body, Content, Element, ElementsBody, ImageBody, ManualBody, Step, UniformBody,
    Value,
};
pub use error::{Error, Result};
pub use frame::{
    encode, scan, station_to_bytes, station_to_string, Direction, EndChar, Encoding, Frame,
    M3Seq, OutFrame, Scanned, ACK, ENQ, EOT, ESC, ETB, ETX, NAK, SOH, STX, SYN,
};
pub use ident::{IdentDef, ZT_BITS};

/// 遥测站分类码 ASCII 字符 (附录A) -> 引导符 HEX
pub fn class_char_to_hex(c: char) -> Option<u8> {
    ident::CLASS_CODES.iter().find(|(ch, _)| *ch == c).map(|(_, h)| *h)
}

/// 遥测站分类码引导符 HEX -> ASCII 字符 (附录A)
pub fn class_hex_to_char(h: u8) -> Option<char> {
    ident::CLASS_CODES.iter().find(|(_, hh)| *hh == h).map(|(c, _)| *c)
}
