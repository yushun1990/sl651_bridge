//! 中心站下行确认/应答帧 (表17/19/29/31/33/...)。
//!
//! 确认正文 = 上行流水号 + 中心站发报时间; 功能码与上行一致, 方向下行。
//! M2/M4: 上行 ETB -> ACK(继续); 上行 ETX -> EOT(默认)/ESC(保持在线)。
//! M3: 中间包不响应; 全部接收正确 -> EOT/ESC(序号=包总数); 坏包 -> NAK(序号=坏包序号)。
//! 2FH 链路维持报不应答。

use chrono::{Datelike, NaiveDateTime, Timelike};

use crate::bcd::time_to_bcd6;
use crate::frame::{encode, Direction, EndChar, Encoding, Frame, M3Seq, OutFrame};

/// ETX 终止后的应答策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FinalTerminator {
    /// 传输结束, 站端立即退出 (默认, 省站端功耗)
    #[default]
    Eot,
    /// 站端保持在线 10 分钟等待中心指令
    Esc,
}

/// 待构造应答帧的参数。
#[derive(Debug, Clone)]
pub struct AckSpec {
    pub encoding: Encoding,
    pub station_addr: String,
    pub center_addr: u8,
    pub password: u16,
    /// 与上行帧一致的功能码
    pub func: u8,
    /// 回显上行流水号
    pub serial: u16,
    /// 中心站发报时间
    pub now: NaiveDateTime,
    pub end: EndChar,
    /// M3 应答: EOT/ESC 时序号=包总数; NAK 时序号=坏包序号
    pub m3: Option<M3Seq>,
}

/// 构造下行确认帧 (含 CRC)。
pub fn build_ack(spec: &AckSpec) -> Vec<u8> {
    let body: Vec<u8> = match spec.encoding {
        Encoding::Ascii => {
            let mut b = format!("{:04X}", spec.serial).into_bytes();
            b.extend_from_slice(
                format!(
                    "{:02}{:02}{:02}{:02}{:02}{:02}",
                    spec.now.year2(),
                    spec.now.month(),
                    spec.now.day(),
                    spec.now.hour(),
                    spec.now.minute(),
                    spec.now.second()
                )
                .as_bytes(),
            );
            b
        }
        Encoding::Hex => {
            let mut b = spec.serial.to_be_bytes().to_vec();
            b.extend_from_slice(&time_to_bcd6(spec.now));
            b
        }
    };
    encode(&OutFrame {
        encoding: spec.encoding,
        direction: Direction::Downlink,
        center_addr: spec.center_addr,
        station_addr: &spec.station_addr,
        password: spec.password,
        func: spec.func,
        end: spec.end,
        m3: spec.m3,
        body: &body,
    })
}

/// 应答决定。
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub end: EndChar,
    pub m3: Option<M3Seq>,
}

/// 依据上行帧决定应答。
///
/// - 非上行帧 / 2FH 链路维持 / M3 中间包: 不应答
/// - M3 末包且全部正确: EOT/ESC (序号=包总数)
/// - M3 坏包 (调用方检测 CRC 失败): [nak] 产生 NAK
/// - 普通帧 ETB: ACK 继续; ETX: EOT/ESC
pub fn decide_reply(frame: &Frame, policy: FinalTerminator) -> Option<Reply> {
    if frame.direction != Direction::Uplink {
        return None;
    }
    if frame.func == 0x2F {
        // 链路维持报不应答 (§6.6.4.2)
        return None;
    }
    let final_end = match policy {
        FinalTerminator::Eot => EndChar::Eot,
        FinalTerminator::Esc => EndChar::Esc,
    };
    match frame.m3 {
        Some(m3) => {
            if m3.seq >= m3.total {
                Some(Reply { end: final_end, m3: Some(m3) })
            } else {
                None // 中间包
            }
        }
        None => match frame.end {
            EndChar::Etb => Some(Reply { end: EndChar::AckChar, m3: None }),
            EndChar::Etx => Some(Reply { end: final_end, m3: None }),
            _ => None,
        },
    }
}

/// M3 坏包 NAK: 序号为坏包序号。
pub fn nak_reply(frame: &Frame) -> Option<Reply> {
    let m3 = frame.m3?;
    Some(Reply { end: EndChar::Nak, m3: Some(M3Seq { total: m3.total, seq: m3.seq }) })
}

// AckSpec.now 格式化辅助 (ASCII 正文用数字串)
trait Year2 {
    fn year2(&self) -> i32;
}

impl Year2 for NaiveDateTime {
    fn year2(&self) -> i32 {
        self.year() % 100
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{scan, Scanned, STX, ETX, ETB, SYN};
    use chrono::NaiveDate;

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 9, 9).unwrap().and_hms_opt(8, 0, 5).unwrap()
    }

    #[test]
    fn ack_ascii_roundtrip() {
        let bytes = build_ack(&AckSpec {
            encoding: Encoding::Ascii,
            station_addr: "5010123456".into(),
            center_addr: 1,
            password: 0,
            func: 0x32,
            serial: 7,
            now: now(),
            end: EndChar::Eot,
            m3: None,
        });
        // 下行: 站址在前
        match scan(&bytes) {
            Scanned::Frame { frame, .. } => {
                assert_eq!(frame.direction, Direction::Downlink);
                assert_eq!(frame.station_addr, "5010123456");
                assert_eq!(frame.center_addr, 1);
                assert_eq!(frame.func, 0x32);
                assert_eq!(frame.end, EndChar::Eot);
                assert_eq!(frame.body, b"0007260909080005".to_vec());
                assert!(frame.crc_ok());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ack_m3() {
        let bytes = build_ack(&AckSpec {
            encoding: Encoding::Ascii,
            station_addr: "5010123456".into(),
            center_addr: 1,
            password: 0,
            func: 0x31,
            serial: 9,
            now: now(),
            end: EndChar::Eot,
            m3: Some(M3Seq { total: 3, seq: 3 }),
        });
        match scan(&bytes) {
            Scanned::Frame { frame, .. } => {
                assert_eq!(frame.m3, Some(M3Seq { total: 3, seq: 3 }));
                assert_eq!(frame.end, EndChar::Eot);
                assert!(frame.crc_ok());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reply_policy() {
        let mk = |end: u8, m3: Option<M3Seq>| Frame {
            encoding: Encoding::Ascii,
            direction: Direction::Uplink,
            center_addr: 1,
            station_addr: "5010123456".into(),
            password: 0,
            func: 0x32,
            end: EndChar::from_byte(end).unwrap(),
            body: vec![],
            m3,
            crc: 0,
        };
        // ETX -> EOT
        assert_eq!(
            decide_reply(&mk(ETX, None), FinalTerminator::Eot),
            Some(Reply { end: EndChar::Eot, m3: None })
        );
        // ETX -> ESC
        assert_eq!(
            decide_reply(&mk(ETX, None), FinalTerminator::Esc),
            Some(Reply { end: EndChar::Esc, m3: None })
        );
        // ETB -> ACK
        assert_eq!(
            decide_reply(&mk(ETB, None), FinalTerminator::Eot),
            Some(Reply { end: EndChar::AckChar, m3: None })
        );
        // 2F 不应答
        let mut f = mk(ETX, None);
        f.func = 0x2F;
        assert_eq!(decide_reply(&f, FinalTerminator::Eot), None);
        // M3 中间包不应答, 末包 EOT
        assert_eq!(decide_reply(&mk(ETB, Some(M3Seq { total: 3, seq: 1 })), FinalTerminator::Eot), None);
        assert_eq!(
            decide_reply(&mk(ETX, Some(M3Seq { total: 3, seq: 3 })), FinalTerminator::Eot),
            Some(Reply { end: EndChar::Eot, m3: Some(M3Seq { total: 3, seq: 3 }) })
        );
        // NAK
        assert_eq!(
            nak_reply(&mk(ETB, Some(M3Seq { total: 3, seq: 2 }))),
            Some(Reply { end: EndChar::Nak, m3: Some(M3Seq { total: 3, seq: 2 }) })
        );
        let _ = (STX, SYN);
    }
}
