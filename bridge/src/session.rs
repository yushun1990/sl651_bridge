//! 会话层: 站点注册表、流水号去重、M3 分包重组、应答决策、图片落盘。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Local;
use dashmap::DashMap;
use serde_json::json;
use tracing::{debug, info, warn};

use sl651_protocol::{
    build_ack, decide_reply, parse_body, AckSpec, Body, Content, Frame, M3Seq,
};

use crate::config::Config;
use crate::images::ImageStore;
use crate::mqtt::TbSink;
use crate::pipeline;
use crate::tbmsg::{build_payload, TbMessage, Topic};

/// 站点运行状态。
pub struct StationState {
    pub last_seen: Instant,
    pub last_serial: Option<u16>,
    pub remote: String,
    pub frames: AtomicU64,
}

/// M3 重组中的报文。
struct M3Partial {
    total: u16,
    parts: HashMap<u16, Vec<u8>>,
    deadline: Instant,
}

/// 去重键: (站址, 功能码, 流水号) -> (正文哈希, 记录时间)
type DedupMap = HashMap<(String, u8, u16), (u64, Instant)>;

/// 全局会话管理。
pub struct Sessions {
    cfg: Arc<Config>,
    stations: DashMap<String, StationState>,
    dedup: Mutex<DedupMap>,
    /// M3 重组缓存: 站址 -> 分包
    m3: Mutex<HashMap<String, M3Partial>>,
    images: ImageStore,
}

impl Sessions {
    pub fn new(cfg: Arc<Config>) -> Self {
        let images = ImageStore::new(cfg.images.dir.clone(), cfg.images.enabled);
        Self {
            cfg,
            stations: DashMap::new(),
            dedup: Mutex::new(HashMap::new()),
            m3: Mutex::new(HashMap::new()),
            images,
        }
    }

    /// 设备显示名: 配置别名 > 站码原样 (不翻译)。
    pub fn station_display(&self, addr: &str) -> String {
        self.cfg
            .stations
            .get(addr)
            .and_then(|s| s.name.clone())
            .unwrap_or_else(|| addr.to_string())
    }

    /// TCP 连接空闲超时 (秒)。
    pub fn idle_timeout_secs(&self) -> u64 {
        self.cfg.listen.idle_timeout_secs
    }

    /// 单连接累积缓冲上限 (字节)。
    pub fn max_buffer(&self) -> usize {
        self.cfg.listen.max_buffer
    }

    /// 处理一帧 (已通过 CRC 校验), 返回应答字节。
    pub fn handle_frame(&self, frame: Frame, remote: &str, sink: &Arc<dyn TbSink>) -> Option<Vec<u8>> {
        self.touch(&frame, remote);

        if frame.direction != sl651_protocol::Direction::Uplink {
            debug!(station = %frame.station_addr, "忽略非上行帧");
            return None;
        }

        if self.cfg.center.check_address && frame.center_addr != self.cfg.center.address {
            warn!(station = %frame.station_addr, center = frame.center_addr, "中心站地址不符, 丢弃");
            return None;
        }
        if self.cfg.center.check_password {
            let expect = u16::from_str_radix(&self.cfg.center.password, 16).unwrap_or(0);
            if frame.password != expect {
                warn!(station = %frame.station_addr, "密码不符, 丢弃");
                return None;
            }
        }

        // M3 分包: 末包到齐后重组解析
        if let Some(m3) = frame.m3 {
            if m3.total > 1 {
                return self.handle_m3_packet(frame, m3, remote, sink);
            }
        }

        self.process_complete_frame(frame, remote, sink)
    }

    /// CRC 失败帧: M3 坏包回 NAK, 其余仅记录。
    /// NAK 正文流水号无法从损坏正文获取, 填 0 (站端按终止符+坏包序号处理)。
    pub fn handle_corrupt(&self, partial: Option<Frame>, reason: &str) -> Option<Vec<u8>> {
        warn!("帧校验失败: {reason}");
        let frame = partial?;
        let m3 = frame.m3?;
        // NAK: 序号=坏包序号
        Some(build_ack(&AckSpec {
            encoding: frame.encoding,
            station_addr: frame.station_addr.clone(),
            center_addr: self.cfg.center.address,
            password: frame.password,
            func: frame.func,
            serial: 0,
            now: Local::now().naive_local(),
            end: sl651_protocol::EndChar::Nak,
            m3: Some(M3Seq { total: m3.total, seq: m3.seq }),
        }))
    }

    fn process_complete_frame(&self, frame: Frame, remote: &str, sink: &Arc<dyn TbSink>) -> Option<Vec<u8>> {
        let station_addr = frame.station_addr.clone();
        let station_name = self.station_display(&station_addr);
        let func = frame.func;

        let body = match parse_body(func, &frame.body, frame.encoding) {
            Ok(b) => b,
            Err(e) => {
                warn!(station = %station_name, func = format!("{func:02X}"), "正文解析失败: {e}");
                return None;
            }
        };

        // 应答 (2FH 不应答由 decide_reply 处理)
        let ack = self.build_reply(&frame, &body);

        // 去重: 同 (站, 功能码, 流水号) 且正文一致 -> 仅应答不重发
        if self.cfg.dedup.enabled && self.is_duplicate(&station_addr, func, body.serial, &frame.body) {
            debug!(station = %station_name, serial = body.serial, "重复帧, 跳过发布");
            return ack;
        }

        // 图片报: 落盘 + 元数据属性
        if let Content::Image(img) = &body.content {
            if let Some(msg) = self.save_image(&station_name, body.serial, &body.send_time, &img.data) {
                if let Err(e) = sink.publish(msg) {
                    warn!("图片元数据发布失败: {e}");
                }
            }
        }

        for msg in pipeline::process(&self.cfg, &station_name, &body) {
            if let Err(e) = sink.publish(msg) {
                warn!(station = %station_name, "TB 发布失败: {e}");
            }
        }
        info!(
            station = %station_name,
            func = format!("{func:02X}"),
            serial = body.serial,
            transport = remote,
            "报文处理完成"
        );
        ack
    }

    fn build_reply(&self, frame: &Frame, body: &Body) -> Option<Vec<u8>> {
        if !self.cfg.ack.enabled {
            return None;
        }
        let reply = decide_reply(frame, self.cfg.ack.terminator())?;
        Some(build_ack(&AckSpec {
            encoding: frame.encoding,
            station_addr: frame.station_addr.clone(),
            center_addr: self.cfg.center.address,
            password: frame.password,
            func: frame.func,
            serial: body.serial,
            now: Local::now().naive_local(),
            end: reply.end,
            m3: reply.m3,
        }))
    }

    // ---------------- M3 ----------------

    /// M3 分包: 以站址为键缓存分包 (单站同时只有一条 M3 传输);
    /// 首包内容或总包数变化视为新一轮传输并重置; 集齐后重组解析。
    /// 完成的缓存在超时前保留, 站端未收到确认而重发时可直接再应答 (去重层防重复入库)。
    fn handle_m3_packet(&self, frame: Frame, m3: M3Seq, remote: &str, sink: &Arc<dyn TbSink>) -> Option<Vec<u8>> {
        let M3Seq { total, seq } = m3;
        let timeout = Duration::from_secs(self.cfg.m3.reassembly_timeout_secs);
        let payload = frame.body;
        let station_addr = frame.station_addr.clone();

        let completed: Option<Vec<u8>> = {
            let mut guard = self.m3.lock().unwrap();
            guard.retain(|_, v| v.deadline > Instant::now());
            let entry = guard
                .entry(station_addr.clone())
                .or_insert_with(|| M3Partial { total, parts: HashMap::new(), deadline: Instant::now() + timeout });
            let first_changed =
                seq == 1 && entry.parts.get(&1).map(|p| p.as_slice()) != Some(payload.as_slice());
            if entry.total != total || first_changed {
                entry.total = total;
                entry.parts.clear();
                entry.deadline = Instant::now() + timeout;
            }
            entry.parts.insert(seq, payload);
            if (1..=entry.total).all(|s| entry.parts.contains_key(&s)) {
                let mut full = Vec::new();
                for s in 1..=entry.total {
                    if let Some(p) = entry.parts.get(&s) {
                        full.extend_from_slice(p);
                    }
                }
                Some(full)
            } else {
                None
            }
        };

        match completed {
            Some(full_body) => {
                let merged = Frame {
                    encoding: frame.encoding,
                    direction: sl651_protocol::Direction::Uplink,
                    center_addr: frame.center_addr,
                    station_addr,
                    password: frame.password,
                    func: frame.func,
                    end: frame.end,
                    body: full_body,
                    m3: Some(M3Seq { total, seq: total }),
                    crc: 0,
                };
                // 以末包身份走完整处理 (解析/发布/末包 EOT 应答)
                self.process_complete_frame(merged, remote, sink)
            }
            None => {
                debug!(station = %station_addr, seq, total, "M3 分包缓存");
                None // 中间包不应答
            }
        }
    }

    // ---------------- 去重 ----------------

    fn is_duplicate(&self, station: &str, func: u8, serial: u16, body: &[u8]) -> bool {
        let mut hash: u64 = 1469598103934665603;
        for &b in body {
            hash ^= b as u64;
            hash = hash.wrapping_mul(1099511628211);
        }
        let key = (station.to_string(), func, serial);
        let window = Duration::from_secs(self.cfg.dedup.window_secs);
        let mut guard = self.dedup.lock().unwrap();
        guard.retain(|_, (_, t)| t.elapsed() < window);
        match guard.get(&key) {
            Some((h, _)) if *h == hash => true,
            _ => {
                guard.insert(key, (hash, Instant::now()));
                false
            }
        }
    }

    // ---------------- 图片 ----------------

    fn save_image(&self, station: &str, serial: u16, ts: &chrono::NaiveDateTime, data: &[u8]) -> Option<TbMessage> {
        if !self.cfg.images.enabled {
            debug!(station = %station, "图片落盘已禁用, 丢弃 {} 字节", data.len());
            return None;
        }
        match self.images.save(station, serial, ts, data) {
            Ok(path) => {
                let mut points = serde_json::Map::new();
                let splitter = self.cfg.payload.key_splitter.as_str();
                points.insert(format!("{station}{splitter}PIC_size"), json!(data.len()));
                points.insert(format!("{station}{splitter}PIC_file"), json!(path));
                Some(TbMessage {
                    topic: Topic::Attributes,
                    payload: build_payload(&self.cfg.payload.data_path, &self.cfg.payload.time_path, points, Some(*ts)),
                })
            }
            Err(e) => {
                warn!(station = %station, "图片落盘失败: {e}");
                None
            }
        }
    }

    fn touch(&self, frame: &Frame, remote: &str) {
        self.stations
            .entry(frame.station_addr.clone())
            .and_modify(|s| {
                s.last_seen = Instant::now();
                s.remote = remote.to_string();
                s.frames.fetch_add(1, Ordering::Relaxed);
            })
            .or_insert_with(|| StationState {
                last_seen: Instant::now(),
                last_serial: None,
                remote: remote.to_string(),
                frames: AtomicU64::new(1),
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::MockSink;
    use sl651_protocol::{encode, Direction, Encoding, EndChar, OutFrame};

    fn sessions() -> Sessions {
        Sessions::new(Arc::new(Config::default()))
    }

    fn frame(body: &[u8], func: u8, end: EndChar) -> Frame {
        let bytes = encode(&OutFrame {
            encoding: Encoding::Ascii,
            direction: Direction::Uplink,
            center_addr: 1,
            station_addr: "3301060001",
            password: 0,
            func,
            end,
            m3: None,
            body,
        });
        match sl651_protocol::scan(&bytes) {
            sl651_protocol::Scanned::Frame { frame, .. } => frame,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn timer_frame_ack_and_publish() {
        let s = sessions();
        let mock = Arc::new(MockSink::default());
        let sink: Arc<dyn TbSink> = mock.clone();
        let f = frame(b"0001260909080000ST 5010123456 H TT 2609090800 Z 6.38 VT 12.50 ", 0x32, EndChar::Etx);
        let ack = s.handle_frame(f, "tcp://test", &sink);
        assert!(ack.is_some());
        let msgs = mock.take();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].payload["points"]["3301060001:Z"], json!(6.38));
    }

    #[test]
    fn duplicate_suppressed_but_acked() {
        let s = sessions();
        let mock = Arc::new(MockSink::default());
        let sink: Arc<dyn TbSink> = mock.clone();
        let body: &[u8] = b"0001260909080000ST 5010123456 H TT 2609090800 Z 6.38 VT 12.50 ";
        let ack1 = s.handle_frame(frame(body, 0x32, EndChar::Etx), "t", &sink);
        let ack2 = s.handle_frame(frame(body, 0x32, EndChar::Etx), "t", &sink);
        assert!(ack1.is_some() && ack2.is_some(), "重发帧仍应答答");
        assert_eq!(mock.take().len(), 1, "重发帧不重复发布");
    }

    #[test]
    fn keepalive_no_ack() {
        let s = sessions();
        let mock = Arc::new(MockSink::default());
        let sink: Arc<dyn TbSink> = mock.clone();
        let ack = s.handle_frame(frame(b"0001260909080000", 0x2F, EndChar::Etx), "t", &sink);
        assert!(ack.is_none());
        let msgs = mock.take();
        assert_eq!(msgs.len(), 1, "心跳仍发空 points 刷新在线");
    }

    #[test]
    fn m3_reassembly() {
        let s = sessions();
        let mock = Arc::new(MockSink::default());
        let sink: Arc<dyn TbSink> = mock.clone();
        let full = b"0009260909080000ST 5010123456 H TT 2609090800 Z 6.38 VT 12.50 ".to_vec();
        // 分 3 包
        let n = full.len() / 3;
        let parts: Vec<&[u8]> = vec![&full[..n], &full[n..2 * n], &full[2 * n..]];
        let mut acks = 0;
        for (i, p) in parts.iter().enumerate() {
            let bytes = encode(&OutFrame {
                encoding: Encoding::Ascii,
                direction: Direction::Uplink,
                center_addr: 1,
                station_addr: "3301060001",
                password: 0,
                func: 0x32,
                end: if i == 2 { EndChar::Etx } else { EndChar::Etb },
                m3: Some(M3Seq { total: 3, seq: (i + 1) as u16 }),
                body: p,
            });
            let f = match sl651_protocol::scan(&bytes) {
                sl651_protocol::Scanned::Frame { frame, .. } => frame,
                other => panic!("{other:?}"),
            };
            if s.handle_frame(f, "t", &sink).is_some() {
                acks += 1;
            }
        }
        assert_eq!(acks, 1, "仅末包应答");
        let msgs = mock.take();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].payload["points"]["3301060001:Z"], json!(6.38));
    }

    #[test]
    fn corrupt_m3_nak() {
        let s = sessions();
        let bytes = {
            let mut b = encode(&OutFrame {
                encoding: Encoding::Ascii,
                direction: Direction::Uplink,
                center_addr: 1,
                station_addr: "3301060001",
                password: 0,
                func: 0x31,
                end: EndChar::Etb,
                m3: Some(M3Seq { total: 2, seq: 1 }),
                body: b"xxxx",
            });
            // 破坏正文字节 (不破坏头部结构), 触发 CRC 校验失败而非解析错误
            b[30] ^= 0xFF;
            b
        };
        match sl651_protocol::scan(&bytes) {
            sl651_protocol::Scanned::Skip { partial: Some(f), .. } => {
                let nak = s.handle_corrupt(Some(f), "CRC");
                assert!(nak.is_some());
            }
            other => panic!("应为 Skip(partial): {other:?}"),
        }
    }
}
