//! 端到端集成测试: 模拟站端 -> TCP/UDP 真实 socket -> bridge -> MockSink。

use std::sync::Arc;

use sl651_bridge::config::Config;
use sl651_bridge::mqtt::{MockSink, TbSink};
use sl651_bridge::session::Sessions;
use sl651_bridge::tbmsg::Topic;
use sl651_protocol::{encode, scan, Direction, Encoding, EndChar, M3Seq, OutFrame, Scanned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

async fn setup() -> (u16, u16, Arc<MockSink>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_port = listener.local_addr().unwrap().port();
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let udp_port = udp.local_addr().unwrap().port();

    let mock = MockSink::new();
    let sink: Arc<dyn TbSink> = mock.clone();

    let mut cfg = Config::default();
    cfg.images.enabled = true;
    cfg.images.dir = std::env::temp_dir().join(format!("sl651-e2e-{}", std::process::id()));
    let sessions = Arc::new(Sessions::new(Arc::new(cfg)));

    tokio::spawn(sl651_bridge::server::run_tcp(listener, sessions.clone(), sink.clone()));
    tokio::spawn(sl651_bridge::server::run_udp(Arc::new(udp), sessions, sink));
    (tcp_port, udp_port, mock)
}

fn timer_frame() -> Vec<u8> {
    encode(&OutFrame {
        encoding: Encoding::Ascii,
        direction: Direction::Uplink,
        center_addr: 1,
        station_addr: "3301060001",
        password: 0,
        func: 0x32,
        end: EndChar::Etx,
        m3: None,
        body: b"0001260909080000ST 5010123456 H TT 2609090800 Z 6.38 VT 12.50 ",
    })
}

async fn read_ack(stream: &mut TcpStream) -> sl651_protocol::Frame {
    let mut buf = vec![0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    buf.truncate(n);
    match scan(&buf) {
        Scanned::Frame { frame, .. } => frame,
        other => panic!("应答帧解析失败: {other:?}"),
    }
}

#[tokio::test]
async fn tcp_timer_report_with_ack() {
    let (tcp_port, _, mock) = setup().await;
    let mut stream = TcpStream::connect(("127.0.0.1", tcp_port)).await.unwrap();

    stream.write_all(&timer_frame()).await.unwrap();
    let ack = read_ack(&mut stream).await;

    // M2 确认: 下行, EOT, 回显流水号, 功能码一致
    assert_eq!(ack.direction, Direction::Downlink);
    assert_eq!(ack.end, EndChar::Eot);
    assert_eq!(ack.func, 0x32);
    assert_eq!(ack.station_addr, "3301060001");
    assert_eq!(&ack.body[..4], b"0001");

    let msgs = mock.take();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].topic, Topic::Telemetry);
    assert_eq!(msgs[0].payload["points"]["3301060001:Z"], serde_json::json!(6.38));
    assert_eq!(msgs[0].payload["time"], "2026-09-09 08:00:00");
}

#[tokio::test]
async fn udp_hex_frame() {
    let (_, udp_port, mock) = setup().await;
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // HEX/BCD 编码定时报 (正文亦为 HEX/BCD 格式)
    let mut body = vec![0x00, 0x01, 0x26, 0x09, 0x09, 0x08, 0x00, 0x00]; // 流水号+发报时间
    body.extend_from_slice(&[0xF1, 0x50, 0x10, 0x12, 0x34, 0x56]); // ST
    body.extend_from_slice(&[0x48, 0x00]); // 分类码 H
    body.extend_from_slice(&[0xF0, 0x26, 0x09, 0x09, 0x08, 0x00]); // TT
    body.extend_from_slice(&[0x39, 0x13, 0x63, 0x80]); // Z 6.38
    body.extend_from_slice(&[0x38, 0x12, 0x12, 0x50]); // VT 12.50
    let frame = encode(&OutFrame {
        encoding: Encoding::Hex,
        direction: Direction::Uplink,
        center_addr: 1,
        station_addr: "3301060001",
        password: 0,
        func: 0x32,
        end: EndChar::Etx,
        m3: None,
        body: &body,
    });
    sock.send_to(&frame, ("127.0.0.1", udp_port)).await.unwrap();

    // 等处理完成 (UDP 异步)
    for _ in 0..50 {
        if !mock.messages.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let msgs = mock.take();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].payload["points"]["3301060001:Z"], serde_json::json!(6.38));

    // 收到 EOT 应答
    let mut buf = [0u8; 1024];
    let (n, _) = sock.recv_from(&mut buf).await.unwrap();
    match scan(&buf[..n]) {
        Scanned::Frame { frame, .. } => {
            assert_eq!(frame.direction, Direction::Downlink);
            assert_eq!(frame.encoding, Encoding::Hex);
            assert_eq!(frame.end, EndChar::Eot);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn tcp_m3_uniform_report() {
    let (tcp_port, _, mock) = setup().await;
    let mut stream = TcpStream::connect(("127.0.0.1", tcp_port)).await.unwrap();

    let full = b"0006260909080000ST 5010123456 H TT 2609090800 DRN05 Z Q 6.30 12.5 6.35 12.6 ".to_vec();
    let n = full.len() / 2;
    let parts: [&[u8]; 2] = [&full[..n], &full[n..]];
    for (i, p) in parts.iter().enumerate() {
        let bytes = encode(&OutFrame {
            encoding: Encoding::Ascii,
            direction: Direction::Uplink,
            center_addr: 1,
            station_addr: "3301060001",
            password: 0,
            func: 0x31,
            end: if i == 1 { EndChar::Etx } else { EndChar::Etb },
            m3: Some(M3Seq { total: 2, seq: (i + 1) as u16 }),
            body: p,
        });
        stream.write_all(&bytes).await.unwrap();
        if i == 0 {
            // 中间包不应答
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    let ack = read_ack(&mut stream).await;
    assert_eq!(ack.end, EndChar::Eot);
    assert_eq!(ack.m3, Some(M3Seq { total: 2, seq: 2 }));

    let msgs = mock.take();
    assert_eq!(msgs.len(), 2, "两组观测时间");
    assert_eq!(msgs[0].payload["time"], "2026-09-09 08:00:00");
    assert_eq!(msgs[1].payload["time"], "2026-09-09 08:05:00");
    assert_eq!(msgs[1].payload["points"]["3301060001:Z"], serde_json::json!(6.35));
}

#[tokio::test]
async fn tcp_image_report_saves_file() {
    let (tcp_port, _, mock) = setup().await;
    let mut stream = TcpStream::connect(("127.0.0.1", tcp_port)).await.unwrap();

    let jpg: Vec<u8> = [0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x42, 0x00, 0xFF, 0xD9].to_vec();
    let mut body = b"0007260909080000ST 5010123456 H TT 2609090800 PIC ".to_vec();
    body.extend_from_slice(&jpg);
    body.push(b' ');
    let frame = encode(&OutFrame {
        encoding: Encoding::Ascii,
        direction: Direction::Uplink,
        center_addr: 1,
        station_addr: "3301060001",
        password: 0,
        func: 0x36,
        end: EndChar::Etx,
        m3: None,
        body: &body,
    });
    stream.write_all(&frame).await.unwrap();
    let ack = read_ack(&mut stream).await;
    assert_eq!(ack.end, EndChar::Eot);

    for _ in 0..50 {
        if !mock.messages.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let msgs = mock.take();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].topic, Topic::Attributes);
    assert_eq!(msgs[0].payload["points"]["3301060001:PIC_size"], serde_json::json!(jpg.len()));
    let file = msgs[0].payload["points"]["3301060001:PIC_file"].as_str().unwrap();
    let path = std::env::temp_dir().join(format!("sl651-e2e-{}", std::process::id())).join(file);
    assert_eq!(std::fs::read(&path).unwrap(), jpg);
}

#[tokio::test]
async fn tcp_keepalive_no_ack_but_heartbeat() {
    let (tcp_port, _, mock) = setup().await;
    let mut stream = TcpStream::connect(("127.0.0.1", tcp_port)).await.unwrap();
    let frame = encode(&OutFrame {
        encoding: Encoding::Ascii,
        direction: Direction::Uplink,
        center_addr: 1,
        station_addr: "3301060001",
        password: 0,
        func: 0x2F,
        end: EndChar::Etx,
        m3: None,
        body: b"0009260909080000",
    });
    stream.write_all(&frame).await.unwrap();
    // 2F 不应答: 短时间内无数据可读
    let mut buf = [0u8; 64];
    let r = tokio::time::timeout(std::time::Duration::from_millis(300), stream.read(&mut buf)).await;
    assert!(r.is_err(), "链路维持报不应答");
    let msgs = mock.take();
    assert_eq!(msgs.len(), 1, "心跳空 points 消息");
    assert_eq!(msgs[0].payload["points"].as_object().unwrap().len(), 0);
}
