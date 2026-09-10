//! ThingsBoard 上行消息模型与载荷组装。
//!
//! 载荷与 onboard-tool `PLC虚拟网关` 规则链兼容:
//! `{"points": {"<站>:<要素>": 值}, "time": "yyyy-MM-dd HH:mm:ss"}`
//! 规则链按 Asia/Shanghai 解析 time -> metadata.ts, 断网补传时间不失真。

use chrono::{Datelike, NaiveDateTime, Timelike};
use serde_json::{json, Map, Value};

/// TB 上行主题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    Telemetry,
    Attributes,
}

impl Topic {
    pub fn as_str(self) -> &'static str {
        match self {
            Topic::Telemetry => "v1/devices/me/telemetry",
            Topic::Attributes => "v1/devices/me/attributes",
        }
    }
}

/// 一条待发布到 TB 的消息。
#[derive(Debug, Clone)]
pub struct TbMessage {
    pub topic: Topic,
    pub payload: Value,
}

impl TbMessage {
    pub fn payload_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.payload).unwrap_or_default()
    }
}

/// 时间格式化: 规则链守卫要求 `^[0-9]{4}.*` 且含 `:`。
pub fn fmt_time(t: NaiveDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// 组装单站点单时间组载荷。
pub fn build_payload(
    data_path: &str,
    time_path: &str,
    points: Map<String, Value>,
    ts: Option<NaiveDateTime>,
) -> Value {
    let mut payload = Map::new();
    payload.insert(data_path.to_string(), Value::Object(points));
    if let Some(t) = ts {
        payload.insert(time_path.to_string(), json!(fmt_time(t)));
    }
    Value::Object(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_shape() {
        let mut points = Map::new();
        points.insert("寨上站:Z".into(), json!(6.38));
        let p = build_payload("points", "time", points, Some(
            NaiveDateTime::parse_from_str("2026-09-09 08:00:00", "%Y-%m-%d %H:%M:%S").unwrap()));
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(s, r#"{"points":{"寨上站:Z":6.38},"time":"2026-09-09 08:00:00"}"#);
        // 规则链时间守卫
        let t = p.get("time").unwrap().as_str().unwrap();
        assert!(t.contains(':') && t.starts_with(|c: char| c.is_ascii_digit()) && t.len() >= 10);
    }
}
