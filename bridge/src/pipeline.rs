//! 归一化管道: SL651 正文 -> TB 消息。
//!
//! 全部使用协议源码, 不做任何翻译:
//! - 设备名 = 站码原样 (配置 [stations] 别名除外)
//! - 点位键 = 规约附录C 标识符原样 (Z/VT/PT/7A/FFxx...)
//! - ZT 状态位图 -> 原始值 + 逐位布尔键 (表58 中文名)
//! - DRP/DRZ 小时报块 -> 按观测时间逐 5 分钟展开为独立时间组
//! - 均匀时段 -> 按步长逐组展开
//! - 人工置数/图片元数据 -> 属性消息
//! - 链路维持 -> 空 points 心跳 (规则链旁路仅刷新 lastUploadTime)

use std::collections::BTreeMap;

use chrono::{Duration, NaiveDateTime};
use serde_json::{json, Map, Value};

use sl651_protocol::{Body, Content, Step, Value as SlValue, ZT_BITS};

use crate::config::Config;
use crate::tbmsg::{build_payload, TbMessage, Topic};

/// 设备名 (站码或别名) + 正文 -> TB 消息列表。键 = `设备名:标识符`。
pub fn process(cfg: &Config, station: &str, body: &Body) -> Vec<TbMessage> {
    let dp = cfg.payload.data_path.as_str();
    let tp = cfg.payload.time_path.as_str();
    let splitter = cfg.payload.key_splitter.as_str();
    match &body.content {
        Content::KeepAlive => vec![TbMessage {
            topic: Topic::Telemetry,
            payload: build_payload(dp, tp, Map::new(), Some(body.send_time)),
        }],
        Content::Elements(e) => {
            let base_ts = e.obs_time.unwrap_or(body.send_time);
            let mut groups: BTreeMap<(i64, NaiveDateTime), Map<String, Value>> = BTreeMap::new();
            for el in &e.elements {
                match &el.value {
                    SlValue::Missing => {}
                    SlValue::HourlyRain(vals) => expand_hourly(
                        &mut groups, station, splitter, &el.ident, vals, base_ts,
                        |v| json!(v),
                    ),
                    SlValue::HourlyLevel(vals) => expand_hourly(
                        &mut groups, station, splitter, &el.ident, vals, base_ts,
                        |v| json!(v),
                    ),
                    _ => {
                        for (k, v) in point_values(station, splitter, &el.ident, &el.value) {
                            groups.entry((0, base_ts)).or_default().insert(k, v);
                        }
                    }
                }
            }
            groups_to_messages(dp, tp, groups)
        }
        Content::Uniform(u) => {
            let base_ts = u.obs_time.unwrap_or(body.send_time);
            match u.step {
                Step::HourlyBlocks => {
                    // DRP/DRZ 固定搭配: groups[0] 内为一组块值
                    let mut groups: BTreeMap<(i64, NaiveDateTime), Map<String, Value>> = BTreeMap::new();
                    for (i, chunk) in u.groups.first().map(|g| g.as_slice()).unwrap_or(&[]).iter().enumerate() {
                        let ident = u.idents.get(i).cloned().unwrap_or_else(|| format!("E{i}"));
                        match chunk {
                            SlValue::HourlyRain(vals) => expand_hourly(
                                &mut groups, station, splitter, &ident, vals, base_ts, |v| json!(v)),
                            SlValue::HourlyLevel(vals) => expand_hourly(
                                &mut groups, station, splitter, &ident, vals, base_ts, |v| json!(v)),
                            _ => {}
                        }
                    }
                    groups_to_messages(dp, tp, groups)
                }
                _ => {
                    let step = u.step.seconds().unwrap_or(0);
                    let mut groups: BTreeMap<(i64, NaiveDateTime), Map<String, Value>> = BTreeMap::new();
                    for (gi, group) in u.groups.iter().enumerate() {
                        let ts = base_ts + Duration::seconds(step * gi as i64);
                        for (j, val) in group.iter().enumerate() {
                            if matches!(val, SlValue::Missing) {
                                continue;
                            }
                            let ident = u.idents.get(j).cloned().unwrap_or_else(|| format!("E{j}"));
                            for (k, v) in point_values(station, splitter, &ident, val) {
                                groups.entry((gi as i64, ts)).or_default().insert(k, v);
                            }
                        }
                    }
                    groups_to_messages(dp, tp, groups)
                }
            }
        }
        Content::Manual(m) => {
            let mut points = Map::new();
            points.insert(format!("{station}{splitter}RGZS"), json!(m.raw));
            vec![TbMessage {
                topic: Topic::Attributes,
                payload: build_payload(dp, tp, points, Some(body.send_time)),
            }]
        }
        Content::Image(_) => {
            // 图片由 session 层落盘后另行发布元数据
            Vec::new()
        }
    }
}

/// 单值 -> 键值对 (ZT 位图展开为多个布尔键, 表58)。
fn point_values(station: &str, splitter: &str, ident: &str, v: &SlValue) -> Vec<(String, Value)> {
    match v {
        SlValue::Int(i) => vec![(key(station, splitter, ident), json!(i))],
        SlValue::Float(f) => vec![(key(station, splitter, ident), json!(f))],
        SlValue::Str(s) => vec![(key(station, splitter, ident), json!(s))],
        SlValue::Status(bits) => {
            let mut kvs = vec![(key(station, splitter, ident), json!(bits))];
            for (bit, name) in ZT_BITS {
                let on = (bits >> bit) & 1 == 1;
                kvs.push((key(station, splitter, &format!("ZT_{name}")), json!(on)));
            }
            kvs
        }
        _ => vec![],
    }
}

/// DRP/DRZ 块: 第 i 组时间 = 观测时间 + i*5min (表36: 观测时间是第一组数据时间)。
fn expand_hourly(
    groups: &mut BTreeMap<(i64, NaiveDateTime), Map<String, Value>>,
    station: &str,
    splitter: &str,
    ident: &str,
    vals: &[Option<f64>],
    base: NaiveDateTime,
    conv: impl Fn(f64) -> Value,
) {
    for (i, v) in vals.iter().enumerate() {
        let Some(v) = v else { continue };
        let ts = base + Duration::seconds(300 * i as i64);
        groups
            .entry((i as i64, ts))
            .or_default()
            .insert(key(station, splitter, ident), conv(*v));
    }
}

fn groups_to_messages(
    dp: &str,
    tp: &str,
    groups: BTreeMap<(i64, NaiveDateTime), Map<String, Value>>,
) -> Vec<TbMessage> {
    groups
        .into_iter()
        .map(|((_, ts), points)| TbMessage {
            topic: Topic::Telemetry,
            payload: build_payload(dp, tp, points, Some(ts)),
        })
        .collect()
}

fn key(station: &str, splitter: &str, ident: &str) -> String {
    format!("{station}{splitter}{ident}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use sl651_protocol::{parse_body, Encoding};

    fn cfg() -> Config {
        Config::default()
    }

    fn run(station: &str, func: u8, raw: &[u8]) -> Vec<TbMessage> {
        let body = parse_body(func, raw, Encoding::Ascii).unwrap();
        process(&cfg(), station, &body)
    }

    #[test]
    fn elements_to_points() {
        let raw = b"0001260909080000ST 3301060001 H TT 2609090800 Z 6.38 VT 12.50 ";
        let msgs = run("3301060001", 0x32, raw);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].topic, Topic::Telemetry);
        let p = &msgs[0].payload;
        assert_eq!(p["points"]["3301060001:Z"], json!(6.38));
        assert_eq!(p["points"]["3301060001:VT"], json!(12.5));
    }

    #[test]
    fn obs_time_fallback_to_send_time() {
        // 发报时间 260909080500 -> 08:05:00
        let raw = b"0001260909080500Z 6.38 ";
        let msgs = run("S1", 0x33, raw);
        assert_eq!(msgs[0].payload["time"], "2026-09-09 08:05:00");
    }

    #[test]
    fn zt_bitmap_expansion() {
        let raw = b"0002260910031500TT 2609100315 ZT 00000003 ";
        let msgs = run("S1", 0x33, raw);
        let p = &msgs[0].payload["points"];
        assert_eq!(p["S1:ZT"], json!(3));
        assert_eq!(p["S1:ZT_交流停电"], json!(true));
        assert_eq!(p["S1:ZT_蓄电池电压低"], json!(true));
        assert_eq!(p["S1:ZT_水位超限报警"], json!(false));
    }

    #[test]
    fn hourly_expansion() {
        // DRP: [0.1, 0.2, ...]; 观测时间 09:00 -> 09:00, 09:05, ...
        let raw = b"0003260909090000TT 2609090900 DRP 0102030405060708090A0B0C VT 12.00 ";
        let msgs = run("S1", 0x34, raw);
        // 12 个 5 分钟时间组; VT 与 DRP 第 0 组同观测时间, 合并为同一条消息
        assert_eq!(msgs.len(), 12);
        assert_eq!(msgs[0].payload["points"]["S1:DRP"], json!(0.1));
        assert_eq!(msgs[0].payload["points"]["S1:VT"], json!(12.0));
        assert_eq!(msgs[0].payload["time"], "2026-09-09 09:00:00");
        assert_eq!(msgs[1].payload["time"], "2026-09-09 09:05:00");
        assert!(!msgs[1].payload["points"].as_object().unwrap().contains_key("S1:VT"));
        assert_eq!(msgs[11].payload["time"], "2026-09-09 09:55:00");
    }

    #[test]
    fn uniform_expansion() {
        let raw = b"0004260909080000TT 2609090800 DRN05 Z Q 6.30 12.5 6.35 12.6 ";
        let msgs = run("S1", 0x31, raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].payload["time"], "2026-09-09 08:00:00");
        assert_eq!(msgs[0].payload["points"]["S1:Z"], json!(6.3));
        assert_eq!(msgs[0].payload["points"]["S1:Q"], json!(12.5));
        assert_eq!(msgs[1].payload["time"], "2026-09-09 08:05:00");
        assert_eq!(msgs[1].payload["points"]["S1:Z"], json!(6.35));
    }

    #[test]
    fn manual_attributes() {
        let raw = b"0005260909080000RGZS MSL 5010123456 202609090800 Z 6.4 ";
        let msgs = run("S1", 0x35, raw);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].topic, Topic::Attributes);
        assert_eq!(msgs[0].payload["points"]["S1:RGZS"], json!("MSL 5010123456 202609090800 Z 6.4"));
    }

    #[test]
    fn keepalive_heartbeat() {
        let raw = b"0001260909080000";
        let msgs = run("S1", 0x2F, raw);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].payload["points"].as_object().unwrap().len(), 0);
        assert_eq!(msgs[0].payload["time"], "2026-09-09 08:00:00");
    }

    #[test]
    fn integer_untouched() {
        let raw = b"0008260909080000TT 2609090800 NS 3 ";
        let msgs = run("S1", 0x32, raw);
        assert_eq!(msgs[0].payload["points"]["S1:NS"], json!(3));
    }
}
