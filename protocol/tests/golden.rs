//! 黄金字节用例: 与规约表16逐字段构造的独立实现 (Python) 交叉验证,
//! 防止编解码实现"自洽但错误"。

use sl651_protocol::{
    encode, scan, Content, Direction, Encoding, EndChar, OutFrame, Scanned,
};

/// 独立 Python 实现构造的完整 ASCII 上行帧 (定时报 32H)。
const GOLDEN_ASCII_TIMER: &str = "013031353031303132333435363030303033323030334502303030313236303930393038303030305354203530313031323334353620482054542032363039303930383030205A20362E33382056542031322E3530200336454241";

#[test]
fn golden_ascii_timer_frame() {
    let body = b"0001260909080000ST 5010123456 H TT 2609090800 Z 6.38 VT 12.50 ";
    let bytes = encode(&OutFrame {
        encoding: Encoding::Ascii,
        direction: Direction::Uplink,
        center_addr: 1,
        station_addr: "5010123456",
        password: 0,
        func: 0x32,
        end: EndChar::Etx,
        m3: None,
        body,
    });
    assert_eq!(hex(&bytes), GOLDEN_ASCII_TIMER, "Rust 编码须与独立实现逐字节一致");

    // 解析回路
    match scan(&bytes) {
        Scanned::Frame { frame, consumed } => {
            assert_eq!(consumed, bytes.len());
            assert!(frame.crc_ok());
            assert_eq!(frame.body.to_vec(), body.to_vec());
        }
        other => panic!("scan 失败: {other:?}"),
    }
}

#[test]
fn golden_body_parse() {
    let body = b"0001260909080000ST 5010123456 H TT 2609090800 Z 6.38 VT 12.50 ";
    let b = sl651_protocol::parse_body(0x32, body, Encoding::Ascii).unwrap();
    assert_eq!(b.serial, 1);
    let Content::Elements(e) = b.content else { panic!() };
    assert_eq!(e.station_addr.as_deref(), Some("5010123456"));
    assert_eq!(e.station_class, Some('H'));
    assert_eq!(e.elements[0].ident, "Z");
    assert_eq!(e.elements[0].value, sl651_protocol::Value::Float(6.38));
    assert_eq!(e.elements[1].ident, "VT");
    assert_eq!(e.elements[1].value, sl651_protocol::Value::Float(12.5));
}

/// 真实设备抓包的 HEX/BCD 编码测试报 (用户提供, 2026-11)。
/// 覆盖: F1F1/F0F0 固定数据定义字节 (表C.1 注a/b)、单字节分类码、
/// 引导符+数据定义(2019/2619/3923/3812/7A08)、FF 扩展标识符 (FFA011)。
const REAL_DEVICE_HEX_TEST_FRAME: &str = "7E7E0101000000011234300033020040201126142746F1F1010000000148F0F0201126142720190000002619000000392300000075381212087A0829FFA011028803E43B";

#[test]
fn golden_real_device_hex_frame() {
    let bytes = hex_bytes(REAL_DEVICE_HEX_TEST_FRAME);
    let frame = match scan(&bytes) {
        Scanned::Frame { frame, consumed } => {
            assert_eq!(consumed, bytes.len());
            frame
        }
        other => panic!("scan 失败: {other:?}"),
    };
    // 帧层
    assert!(frame.crc_ok(), "CRC 应为 E43B");
    assert_eq!(frame.encoding, Encoding::Hex);
    assert_eq!(frame.center_addr, 0x01);
    assert_eq!(frame.station_addr, "0100000001");
    assert_eq!(frame.password, 0x1234);
    assert_eq!(frame.func, 0x30); // 测试报
    assert_eq!(frame.end, EndChar::Etx);
    assert_eq!(frame.body.len(), 0x33);

    // 正文层 (发报时间 201126142746 -> 2020-11-26 14:27:46)
    let b = sl651_protocol::parse_body(0x30, &frame.body, Encoding::Hex).unwrap();
    assert_eq!(b.serial, 0x40);
    assert_eq!(
        b.send_time.format("%Y%m%d%H%M%S").to_string(),
        "20201126142746"
    );
    let Content::Elements(e) = b.content else { panic!() };
    assert_eq!(e.station_addr.as_deref(), Some("0100000001"));
    assert_eq!(e.station_class, Some('H')); // 0x48 河道
    assert_eq!(e.obs_time.map(|t| t.format("%Y%m%d%H%M").to_string()).as_deref(), Some("202011261427"));

    // 要素: 引导符 + 数据定义(字节数/小数位) + BCD 数据
    let get = |k: &str| e.elements.iter().find(|x| x.ident == k).map(|x| x.value.clone());
    assert_eq!(get("PJ"), Some(sl651_protocol::Value::Float(0.0))); // 2019: 3字节/1位
    assert_eq!(get("PT"), Some(sl651_protocol::Value::Float(0.0))); // 2619: 3字节/1位
    assert_eq!(get("Z"), Some(sl651_protocol::Value::Float(0.075))); // 3923: 4字节/3位
    assert_eq!(get("VT"), Some(sl651_protocol::Value::Float(12.08))); // 3812: 2字节/2位
    assert_eq!(get("7A"), Some(sl651_protocol::Value::Int(29))); // 厂商自定义信号强度 (BCD), 透传
    assert_eq!(get("FFA0"), Some(sl651_protocol::Value::Float(28.8))); // 用户扩展设备温度
    assert_eq!(e.elements.len(), 6);
}

fn hex_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}
