//! 报文正文解析 (§6.6)。
//!
//! 正文固定头: 流水号(2B HEX) + 发报时间(6B BCD YYMMDDHHmmSS)。
//! ASCII 编码正文: 信息组以单个空格分隔, 尾随空格不可省略 (§6.6.2)。
//! HEX/BCD 编码正文: `标识符引导符 + 数据定义字节 + 数据` 顺序紧凑排列 (§6.6.3)。
//!
//! 已知简化: HEX/BCD 编码的均匀时段报 (31H) 按要素对解析, 不做步长时间戳展开
//! (该功能码实际部署几乎都走 ASCII 编码)。

use chrono::NaiveDateTime;

use crate::bcd::{bcd_to_digits, parse_bcd_time, parse_time_digits};
use crate::error::{Error, Result};
use crate::frame::Encoding;
use crate::ident::{
    is_class_char, LEAD_DATA, LEAD_DRP, LEAD_DRZ1, LEAD_DRZ8, LEAD_PIC, LEAD_RGZS, LEAD_ST,
    LEAD_STEP, LEAD_TT, LEAD_ZT,
};

/// 解析后的报文正文。
#[derive(Debug, Clone)]
pub struct Body {
    /// 流水号
    pub serial: u16,
    /// 发报时间
    pub send_time: NaiveDateTime,
    /// 功能码
    pub func: u8,
    pub content: Content,
}

#[derive(Debug, Clone)]
pub enum Content {
    /// 链路维持报 (2FH)
    KeepAlive,
    /// 定时/加报/测试/小时报等要素报文 (30H 32H 33H 34H)
    Elements(ElementsBody),
    /// 均匀时段水文信息报 (31H, ASCII 编码)
    Uniform(UniformBody),
    /// 人工置数报 (35H)
    Manual(ManualBody),
    /// 图片报 (36H), data 为本包图片分片
    Image(ImageBody),
}

/// 要素信息组报文。
#[derive(Debug, Clone, Default)]
pub struct ElementsBody {
    pub station_addr: Option<String>,
    pub station_class: Option<char>,
    /// 观测时间
    pub obs_time: Option<NaiveDateTime>,
    pub elements: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct Element {
    /// SL651 标识符 ASCII 名 (Z/VT/DRP/...)
    pub ident: String,
    pub value: Value,
}

/// 均匀时段报。
#[derive(Debug, Clone)]
pub struct UniformBody {
    pub station_addr: Option<String>,
    pub station_class: Option<char>,
    /// 第一组数据的观测时间
    pub obs_time: Option<NaiveDateTime>,
    pub step: Step,
    /// 要素标识符序列
    pub idents: Vec<String>,
    /// 按观测时间分组的数据 (表30: 时间优先, 组内按 idents 次序)
    pub groups: Vec<Vec<Value>>,
}

/// 时间步长码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Days(u32),
    Hours(u32),
    Mins(u32),
    /// 全 0: 其后为 DRP/DRZ 类 1 小时时段组合数据 (固定搭配)
    HourlyBlocks,
}

impl Step {
    /// 步长秒数 (HourlyBlocks 无意义)。
    pub fn seconds(&self) -> Option<i64> {
        Some(match self {
            Step::Days(d) => *d as i64 * 86400,
            Step::Hours(h) => *h as i64 * 3600,
            Step::Mins(m) => *m as i64 * 60,
            Step::HourlyBlocks => return None,
        })
    }
}

/// 人工置数报。
#[derive(Debug, Clone)]
pub struct ManualBody {
    /// RGZS 原编码数据
    pub raw: String,
}

/// 图片报。
#[derive(Debug, Clone)]
pub struct ImageBody {
    pub station_addr: Option<String>,
    pub station_class: Option<char>,
    pub obs_time: Option<NaiveDateTime>,
    /// JPG 分片数据 (原编码)
    pub data: Vec<u8>,
}

/// 要素值。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    /// 非数值 (DT 历时 HH.mm、RGZS 原文等), 保留原文
    Str(String),
    /// ZT 状态位图 (表58)
    Status(u32),
    /// DRP: 12 组 5 分钟时段雨量 (毫米), None=FF 非法
    HourlyRain(Vec<Option<f64>>),
    /// DRZ1..8: 12 组 5 分钟间隔相对水位 (米), None=FFFF 非法
    HourlyLevel(Vec<Option<f64>>),
    /// 缺测 (ASCII 'M' / HEX 数据位全 F)
    Missing,
}

/// 按功能码与编码方式解析正文。
pub fn parse_body(func: u8, raw: &[u8], enc: Encoding) -> Result<Body> {
    match enc {
        Encoding::Ascii => parse_ascii(func, raw),
        Encoding::Hex => parse_hex(func, raw),
    }
}

// ---------------- ASCII 正文 ----------------

/// 词法: 单空格分隔的 token, 保留位置以便取原始剩余字节 (PIC/RGZS)。
struct Lexer<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// 取下一 token 并越过其后的单个分隔空格。
    fn next_token(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let start = self.pos;
        while self.pos < self.buf.len() && self.buf[self.pos] != b' ' {
            self.pos += 1;
        }
        let tok = &self.buf[start..self.pos];
        if self.pos < self.buf.len() {
            self.pos += 1;
        }
        Some(tok)
    }

    /// 预读下一 token (不推进)。
    fn peek_token(&self) -> Option<&'a [u8]> {
        let mut probe = Self { buf: self.buf, pos: self.pos };
        probe.next_token()
    }

    /// 当前位置之后的原始剩余字节 (用于 PIC/RGZS)。
    fn remainder(&self) -> &'a [u8] {
        &self.buf[self.pos.min(self.buf.len())..]
    }
}

fn parse_ascii(func: u8, raw: &[u8]) -> Result<Body> {
    if raw.len() < 16 {
        return Err(Error::TooShort { need: 16, have: raw.len() });
    }
    let serial = parse_ascii_hex_u16(&raw[0..4])?;
    let send_time = parse_time_digits(
        std::str::from_utf8(&raw[4..16]).map_err(|_| Error::BadBody("发报时间非ASCII".into()))?,
    )
    .ok_or_else(|| Error::BadBody("发报时间无效".into()))?;
    let rest = &raw[16..];

    let content = match func {
        0x2F => Content::KeepAlive,
        0x35 => Content::Manual(parse_manual(rest)?),
        0x36 => Content::Image(parse_image(rest)?),
        _ => parse_elements(rest)?,
    };
    Ok(Body { serial, send_time, func, content })
}

/// 人工置数: `RGZS <原编码数据> `
fn parse_manual(rest: &[u8]) -> Result<ManualBody> {
    if !rest.starts_with(b"RGZS ") {
        return Err(Error::BadBody("人工置数缺少 RGZS 标识符".into()));
    }
    let mut data = &rest[5..];
    if data.last() == Some(&b' ') {
        data = &data[..data.len() - 1];
    }
    Ok(ManualBody { raw: String::from_utf8_lossy(data).into_owned() })
}

/// 图片报: `ST <站址> [分类码] TT <时间> PIC <JPG原编码>`
fn parse_image(rest: &[u8]) -> Result<ImageBody> {
    let mut lex = Lexer::new(rest);
    let mut out = ImageBody {
        station_addr: None,
        station_class: None,
        obs_time: None,
        data: Vec::new(),
    };
    while let Some(tok) = lex.next_token() {
        match tok {
            b"ST" => {
                if let Some(a) = lex.next_token() {
                    out.station_addr = Some(String::from_utf8_lossy(a).into_owned());
                }
            }
            b"TT" => {
                if let Some(t) = lex.next_token() {
                    out.obs_time = parse_time_digits(&String::from_utf8_lossy(t));
                }
            }
            b"PIC" => {
                let mut data = lex.remainder().to_vec();
                if data.last() == Some(&b' ') {
                    data.pop();
                }
                out.data = data;
                break;
            }
            other => {
                if other.len() == 1 {
                    let c = other[0] as char;
                    if is_class_char(c) {
                        out.station_class = Some(c);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// 要素报文/均匀时段报解析 (30H 31H 32H 33H 34H 及查询响应)。
fn parse_elements(rest: &[u8]) -> Result<Content> {
    let mut lex = Lexer::new(rest);
    let mut station_addr = None;
    let mut station_class: Option<char> = None;
    let mut obs_time = None;
    let mut elements: Vec<Element> = Vec::new();

    let mut step: Option<Step> = None;
    let mut u_idents: Vec<String> = Vec::new();
    let mut u_values: Vec<Value> = Vec::new();

    while let Some(tok) = lex.next_token() {
        let s = String::from_utf8_lossy(tok).into_owned();
        match s.as_str() {
            "ST" => {
                if let Some(a) = lex.next_token() {
                    station_addr = Some(String::from_utf8_lossy(a).into_owned());
                }
            }
            "TT" => {
                if let Some(t) = lex.next_token() {
                    obs_time = parse_time_digits(&String::from_utf8_lossy(t));
                }
            }
            "DRP" => {
                if let Some(v) = lex.next_token() {
                    let val = parse_hourly_rain(v)?;
                    if step.is_some() {
                        u_idents.push("DRP".into());
                        u_values.push(val);
                    } else {
                        elements.push(Element { ident: "DRP".into(), value: val });
                    }
                }
            }
            "RGZS" => {
                let mut data = lex.remainder().to_vec();
                if data.last() == Some(&b' ') {
                    data.pop();
                }
                elements.push(Element {
                    ident: "RGZS".into(),
                    value: Value::Str(String::from_utf8_lossy(&data).into_owned()),
                });
                break;
            }
            "PIC" => {
                let mut data = lex.remainder().to_vec();
                if data.last() == Some(&b' ') {
                    data.pop();
                }
                return Ok(Content::Image(ImageBody {
                    station_addr,
                    station_class,
                    obs_time,
                    data,
                }));
            }
            _ => {
                if let Some(st) = parse_step_token(&s) {
                    step = Some(st);
                    continue;
                }
                if s.len() == 1
                    && is_class_char(s.chars().next().unwrap())
                    && lex.peek_token() == Some(&b"TT"[..])
                {
                    station_class = Some(s.chars().next().unwrap());
                    continue;
                }
                if s.len() == 4 && s.starts_with("DRZ") && s.as_bytes()[3].is_ascii_digit() {
                    if let Some(v) = lex.next_token() {
                        let val = parse_hourly_level(&s, v)?;
                        if step.is_some() {
                            u_idents.push(s.clone());
                            u_values.push(val);
                        } else {
                            elements.push(Element { ident: s.clone(), value: val });
                        }
                        continue;
                    }
                }
                match &step {
                    None => match lex.next_token() {
                        Some(v) => {
                            let value = parse_value(&s, v);
                            elements.push(Element { ident: s, value });
                        }
                        None => break,
                    },
                    Some(_) => {
                        if looks_like_value(tok) {
                            u_values.push(parse_value("", tok));
                        } else {
                            u_idents.push(s);
                        }
                    }
                }
            }
        }
    }

    match step {
        Some(step) => {
            let groups: Vec<Vec<Value>> = if step == Step::HourlyBlocks {
                if u_values.is_empty() { Vec::new() } else { vec![u_values] }
            } else {
                let n = u_idents.len().max(1);
                u_values.chunks(n).map(|c| c.to_vec()).collect()
            };
            Ok(Content::Uniform(UniformBody {
                station_addr,
                station_class,
                obs_time,
                step,
                idents: u_idents,
                groups,
            }))
        }
        None => Ok(Content::Elements(ElementsBody {
            station_addr,
            station_class,
            obs_time,
            elements,
        })),
    }
}

fn looks_like_value(tok: &[u8]) -> bool {
    if tok == b"M" || tok == b"F" {
        return true;
    }
    let t = String::from_utf8_lossy(tok);
    let t = t.trim_start_matches('-');
    !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

fn parse_step_token(s: &str) -> Option<Step> {
    let b = s.as_bytes();
    if b.len() != 5 || &b[0..2] != b"DR" {
        return None;
    }
    let nn: u32 = s[3..5].parse().ok()?;
    match b[2] {
        b'D' if (1..=31).contains(&nn) => Some(Step::Days(nn)),
        b'H' if nn == 0 => Some(Step::HourlyBlocks),
        b'H' if (1..=23).contains(&nn) => Some(Step::Hours(nn)),
        b'N' if (1..=59).contains(&nn) => Some(Step::Mins(nn)),
        _ => None,
    }
}

fn parse_hourly_rain(tok: &[u8]) -> Result<Value> {
    let s = String::from_utf8_lossy(tok);
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() || !cleaned.len().is_multiple_of(2) {
        return Err(Error::BadBody(format!("DRP 数据长度非法: {cleaned}")));
    }
    let mut vals = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.as_bytes().chunks(2) {
        let hex = std::str::from_utf8(pair).map_err(|_| Error::BadHex('?'))?;
        let b = u8::from_str_radix(hex, 16).map_err(|_| Error::BadHex(hex.chars().next().unwrap()))?;
        vals.push(if b == 0xFF { None } else { Some(b as f64 / 10.0) });
    }
    Ok(Value::HourlyRain(vals))
}

fn parse_hourly_level(ident: &str, tok: &[u8]) -> Result<Value> {
    let s = String::from_utf8_lossy(tok);
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() || !cleaned.len().is_multiple_of(4) {
        return Err(Error::BadBody(format!("{ident} 数据长度非法: {cleaned}")));
    }
    let mut vals = Vec::with_capacity(cleaned.len() / 4);
    for quad in cleaned.as_bytes().chunks(4) {
        let hex = std::str::from_utf8(quad).map_err(|_| Error::BadHex('?'))?;
        let v = u16::from_str_radix(hex, 16).map_err(|_| Error::BadHex(hex.chars().next().unwrap()))?;
        vals.push(if v == 0xFFFF { None } else { Some(v as f64 / 100.0) });
    }
    Ok(Value::HourlyLevel(vals))
}

fn parse_value(ident: &str, tok: &[u8]) -> Value {
    let s = String::from_utf8_lossy(tok).into_owned();
    if s == "M" || s == "F" {
        return Value::Missing;
    }
    if ident == "ZT" {
        if let Ok(v) = u32::from_str_radix(&s, 16) {
            return Value::Status(v);
        }
        if let Ok(v) = s.parse::<u32>() {
            return Value::Status(v);
        }
        return Value::Str(s);
    }
    let t = s.trim_start_matches('-');
    let dots = t.bytes().filter(|&b| b == b'.').count();
    if !t.is_empty()
        && t.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && dots <= 1
        && s.parse::<f64>().is_ok()
    {
        if s.contains('.') {
            return Value::Float(s.parse::<f64>().unwrap());
        }
        if let Ok(i) = s.parse::<i64>() {
            return Value::Int(i);
        }
    }
    Value::Str(s)
}

fn parse_ascii_hex_u16(s: &[u8]) -> Result<u16> {
    let t = std::str::from_utf8(s).map_err(|_| Error::BadHex('?'))?;
    u16::from_str_radix(t, 16).map_err(|_| Error::BadHex(t.chars().next().unwrap_or('?')))
}

// ---------------- HEX/BCD 正文 ----------------

fn parse_hex(func: u8, raw: &[u8]) -> Result<Body> {
    if raw.len() < 8 {
        return Err(Error::TooShort { need: 8, have: raw.len() });
    }
    let serial = u16::from_be_bytes([raw[0], raw[1]]);
    let send_time =
        parse_bcd_time(&raw[2..8]).ok_or_else(|| Error::BadBody("发报时间BCD无效".into()))?;
    let rest = &raw[8..];

    let content = match func {
        0x2F => Content::KeepAlive,
        0x35 => Content::Manual(ManualBody { raw: hex_encode(rest) }),
        _ => parse_hex_elements(rest)?,
    };
    Ok(Body { serial, send_time, func, content })
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02X}")).collect()
}

/// HEX/BCD 正文: 每组为 `标识符引导符 + 数据定义字节 + 数据` (§6.6.3.2 表26)。
/// 数据定义高5位=数据字节数, 低3位=小数位数; TT/ST/RGZS/PIC 的定义字节固定为
/// F0/F1/F2/F3 (表C.1 注a~d), DATA 固定为 F6 (注f)。
/// 分类码为单字节 (§6.6.3.5), 紧随 ST 组 (§6.6.2.6 固定组合)。
/// 兼容个别设备省略 TT/ST 固定定义字节的形式。
/// 简化: 均匀时段步长码被识别并跳过, 数据按要素对返回。
fn parse_hex_elements(mut rest: &[u8]) -> Result<Content> {
    let mut station_addr = None;
    let mut station_class: Option<char> = None;
    let mut obs_time = None;
    let mut elements: Vec<Element> = Vec::new();

    while !rest.is_empty() {
        let lead = rest[0];
        match lead {
            LEAD_TT => {
                let has_def = rest.len() >= 2 && rest[1] == LEAD_TT;
                let need = if has_def { 7 } else { 6 };
                if rest.len() < need {
                    return Err(Error::TooShort { need, have: rest.len() });
                }
                obs_time = if has_def {
                    parse_bcd_time(&rest[2..7])
                } else {
                    parse_bcd_time(&rest[1..6])
                };
                rest = &rest[need..];
            }
            LEAD_ST => {
                let has_def = rest.len() >= 2 && rest[1] == LEAD_ST;
                let need = if has_def { 7 } else { 6 };
                if rest.len() < need {
                    return Err(Error::TooShort { need, have: rest.len() });
                }
                station_addr = crate::frame::station_to_string(if has_def {
                    &rest[2..7]
                } else {
                    &rest[1..6]
                })
                .ok();
                rest = &rest[need..];
                if let Some(&c) = rest.first() {
                    if let Some(ch) = crate::class_hex_to_char(c) {
                        station_class = Some(ch);
                        rest = &rest[1..];
                        if rest.first() == Some(&0x00) {
                            rest = &rest[1..];
                        }
                    }
                }
            }
            LEAD_RGZS | LEAD_PIC | LEAD_DATA => {
                let fixed_def = match lead {
                    LEAD_RGZS => 0xF2,
                    LEAD_PIC => 0xF3,
                    _ => 0xF6,
                };
                let skip = if rest.get(1) == Some(&fixed_def) { 2 } else { 1 };
                let name = match lead {
                    LEAD_RGZS => "RGZS",
                    LEAD_PIC => "PIC",
                    _ => "DATA",
                };
                elements.push(Element {
                    ident: name.into(),
                    value: Value::Str(hex_encode(&rest[skip..])),
                });
                rest = &[];
            }
            LEAD_DRP => {
                if rest.len() < 14 {
                    return Err(Error::TooShort { need: 14, have: rest.len() });
                }
                let mut vals = Vec::with_capacity(12);
                for &b in &rest[2..14] {
                    vals.push(if b == 0xFF { None } else { Some(b as f64 / 10.0) });
                }
                elements.push(Element { ident: "DRP".into(), value: Value::HourlyRain(vals) });
                rest = &rest[14..];
            }
            LEAD_DRZ1..=LEAD_DRZ8 => {
                if rest.len() < 26 {
                    return Err(Error::TooShort { need: 26, have: rest.len() });
                }
                let mut vals = Vec::with_capacity(12);
                for chunk in rest[2..26].chunks(2) {
                    let v = u16::from_be_bytes([chunk[0], chunk[1]]);
                    vals.push(if v == 0xFFFF { None } else { Some(v as f64 / 100.0) });
                }
                let name = crate::ident::drz_name(lead).unwrap_or("DRZ?").to_string();
                elements.push(Element { ident: name, value: Value::HourlyLevel(vals) });
                rest = &rest[26..];
            }
            LEAD_STEP => {
                if rest.len() < 5 {
                    return Err(Error::TooShort { need: 5, have: rest.len() });
                }
                rest = &rest[5..];
            }
            LEAD_ZT => {
                if rest.len() < 6 {
                    return Err(Error::TooShort { need: 6, have: rest.len() });
                }
                let v = Value::Status(u32::from_be_bytes([rest[2], rest[3], rest[4], rest[5]]));
                elements.push(Element { ident: "ZT".into(), value: v });
                rest = &rest[6..];
            }
            0xFF => {
                if rest.len() < 3 {
                    return Err(Error::TooShort { need: 3, have: rest.len() });
                }
                let ext = rest[1];
                let def = rest[2];
                let (v, used) = parse_def_data(def, &rest[3..])?;
                elements.push(Element { ident: format!("FF{ext:02X}"), value: v });
                rest = &rest[3 + used..];
            }
            _ => {
                if rest.len() < 2 {
                    return Err(Error::TooShort { need: 2, have: rest.len() });
                }
                let def = rest[1];
                if def == 0 {
                    if let Some(c) = crate::class_hex_to_char(lead) {
                        station_class = Some(c);
                        rest = &rest[2..];
                        continue;
                    }
                }
                let name = crate::ident::by_lead(lead)
                    .map(|d| d.name.to_string())
                    .unwrap_or_else(|| format!("{lead:02X}"));
                let (v, used) = parse_def_data(def, &rest[2..])?;
                elements.push(Element { ident: name, value: v });
                rest = &rest[2 + used..];
            }
        }
    }

    Ok(Content::Elements(ElementsBody {
        station_addr,
        station_class,
        obs_time,
        elements,
    }))
}

/// 数据定义字节: 高5位=数据字节数(包含负数符号位), 低3位=小数位数。
/// BCD 数据全 F 表示缺测；负数的最高位字节为 FF，但符号字节已计入数据定义长度 (§6.6.3.3)。
fn parse_def_data(def: u8, data: &[u8]) -> Result<(Value, usize)> {
    let nbytes = (def >> 3) as usize;
    let decimals = (def & 0x07) as u32;
    if data.len() < nbytes {
        return Err(Error::TooShort { need: nbytes, have: data.len() });
    }

    let field = &data[..nbytes];
    if nbytes > 0 && field.iter().all(|&b| b == 0xFF) {
        return Ok((Value::Missing, nbytes));
    }

    let (neg, digits_bytes) = if nbytes > 0 && field[0] == 0xFF {
        (true, &field[1..])
    } else {
        (false, field)
    };
    let digits = bcd_to_digits(digits_bytes).unwrap_or_default();
    let mag: f64 = if digits.is_empty() {
        0.0
    } else {
        digits.parse::<f64>().unwrap_or(0.0)
    };
    let v = if decimals == 0 {
        let i = mag as i64;
        Value::Int(if neg { -i } else { i })
    } else {
        let f = mag / 10f64.powi(decimals as i32);
        Value::Float(if neg { -f } else { f })
    };
    Ok((v, nbytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|pair| u8::from_str_radix(pair, 16).unwrap())
            .collect()
    }

    #[test]
    fn keepalive() {
        let b = parse_body(0x2F, &body("0001260909080000"), Encoding::Ascii).unwrap();
        assert_eq!(b.serial, 1);
        assert_eq!(b.send_time, parse_time_digits("260909080000").unwrap());
        assert!(matches!(b.content, Content::KeepAlive));
    }

    #[test]
    fn timer_report() {
        let raw = "0001260909080000ST 5010123456 H TT 2609090800 Z 6.38 VT 12.50 ";
        let b = parse_body(0x32, &body(raw), Encoding::Ascii).unwrap();
        let Content::Elements(e) = &b.content else { panic!() };
        assert_eq!(e.station_addr.as_deref(), Some("5010123456"));
        assert_eq!(e.station_class, Some('H'));
        assert_eq!(e.obs_time, parse_time_digits("2609090800"));
        assert_eq!(e.elements.len(), 2);
        assert_eq!(e.elements[0].ident, "Z");
        assert_eq!(e.elements[0].value, Value::Float(6.38));
        assert_eq!(e.elements[1].ident, "VT");
        assert_eq!(e.elements[1].value, Value::Float(12.5));
    }

    #[test]
    fn add_report_with_zt_and_missing() {
        let raw = "0002260910031500ST 5010123456 H TT 2609100315 Z 7.123 Q M ZT 00000018 VT 11.80 ";
        let b = parse_body(0x33, &body(raw), Encoding::Ascii).unwrap();
        let Content::Elements(e) = &b.content else { panic!() };
        assert_eq!(e.elements[0].value, Value::Float(7.123));
        assert_eq!(e.elements[1].value, Value::Missing);
        assert_eq!(e.elements[2].value, Value::Status(0x18));
        assert_eq!(e.elements[3].value, Value::Float(11.8));
    }

    #[test]
    fn hourly_report() {
        let raw = "0003260909090000ST 5010123456 H TT 2609090900 DRP 0102030405060708090A0B0C PT 12.5 DRZ1 00640065006600670068FFFF0069006A006B006C006D006E VT 12.00 ";
        let b = parse_body(0x34, &body(raw), Encoding::Ascii).unwrap();
        let Content::Elements(e) = &b.content else { panic!() };
        assert_eq!(e.elements[0].ident, "DRP");
        match &e.elements[0].value {
            Value::HourlyRain(v) => {
                assert_eq!(v.len(), 12);
                assert_eq!(v[0], Some(0.1));
                assert_eq!(v[11], Some(1.2));
            }
            _ => panic!(),
        }
        assert_eq!(e.elements[1].ident, "PT");
        match &e.elements[2].value {
            Value::HourlyLevel(v) => {
                assert_eq!(v.len(), 12);
                assert_eq!(v[0], Some(1.0));
                assert_eq!(v[5], None);
            }
            _ => panic!(),
        }
        assert_eq!(e.elements[3].value, Value::Float(12.0));
    }

    #[test]
    fn uniform_report() {
        let raw = "0004260909080000ST 5010123456 H TT 2609090800 DRN05 Z Q 6.30 12.5 6.35 12.6 ";
        let b = parse_body(0x31, &body(raw), Encoding::Ascii).unwrap();
        let Content::Uniform(u) = &b.content else { panic!() };
        assert_eq!(u.step, Step::Mins(5));
        assert_eq!(u.idents, vec!["Z", "Q"]);
        assert_eq!(u.groups.len(), 2);
        assert_eq!(u.groups[0][0], Value::Float(6.30));
        assert_eq!(u.groups[0][1], Value::Float(12.5));
        assert_eq!(u.groups[1][0], Value::Float(6.35));
        assert_eq!(u.groups[1][1], Value::Float(12.6));
        assert_eq!(u.step.seconds(), Some(300));
    }

    #[test]
    fn uniform_missing_value() {
        let raw = "0005260909080000ST 5010123456 H TT 2609090800 DRH01 Z 6.30 M 6.35 ";
        let b = parse_body(0x31, &body(raw), Encoding::Ascii).unwrap();
        let Content::Uniform(u) = &b.content else { panic!() };
        assert_eq!(u.idents, vec!["Z"]);
        assert_eq!(u.groups.len(), 3);
        assert_eq!(u.groups[1][0], Value::Missing);
    }

    #[test]
    fn manual_report() {
        let raw = "0005260909080000RGZS MSL 5010123456 202609090800 Z 6.4 ";
        let b = parse_body(0x35, &body(raw), Encoding::Ascii).unwrap();
        let Content::Manual(m) = &b.content else { panic!() };
        assert_eq!(m.raw, "MSL 5010123456 202609090800 Z 6.4");
    }

    #[test]
    fn image_report() {
        let jpg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b' ', 0x00, 0xFF, 0xD9];
        let mut raw = b"0006260909080000ST 5010123456 H TT 2609090800 PIC ".to_vec();
        raw.extend_from_slice(&jpg);
        raw.push(b' ');
        let b = parse_body(0x36, &raw, Encoding::Ascii).unwrap();
        let Content::Image(img) = &b.content else { panic!() };
        assert_eq!(img.station_addr.as_deref(), Some("5010123456"));
        assert_eq!(img.station_class, Some('H'));
        assert_eq!(img.data, jpg);
    }

    #[test]
    fn hex_body_elements() {
        let mut raw = vec![0x00, 0x01, 0x26, 0x09, 0x09, 0x08, 0x00, 0x00];
        raw.extend_from_slice(&[0xF1, 0x50, 0x10, 0x12, 0x34, 0x56]);
        raw.extend_from_slice(&[0x48, 0x00]);
        raw.extend_from_slice(&[0xF0, 0x26, 0x09, 0x09, 0x08, 0x00]);
        raw.extend_from_slice(&[0x39, 0x13, 0x63, 0x80]);
        raw.extend_from_slice(&[0x38, 0x12, 0x12, 0x50]);
        let b = parse_body(0x32, &raw, Encoding::Hex).unwrap();
        assert_eq!(b.serial, 1);
        let Content::Elements(e) = &b.content else { panic!() };
        assert_eq!(e.station_addr.as_deref(), Some("5010123456"));
        assert_eq!(e.station_class, Some('H'));
        assert_eq!(e.obs_time, parse_time_digits("2609090800"));
        assert_eq!(e.elements[0].ident, "Z");
        assert_eq!(e.elements[0].value, Value::Float(6.38));
        assert_eq!(e.elements[1].ident, "VT");
        assert_eq!(e.elements[1].value, Value::Float(12.50));
    }

    #[test]
    fn hex_negative_value() {
        // 数据定义中的字节数包含符号字节: FF + 0123 共 3 字节, 小数位 2 -> 0x1A。
        let mut raw = vec![0x00, 0x02, 0x26, 0x09, 0x09, 0x08, 0x00, 0x00];
        raw.extend_from_slice(&[0x39, 0x1A, 0xFF, 0x01, 0x23]);
        let b = parse_body(0x32, &raw, Encoding::Hex).unwrap();
        let Content::Elements(e) = &b.content else { panic!() };
        assert_eq!(e.elements[0].value, Value::Float(-1.23));
    }

    #[test]
    fn hex_real_hourly_missing_value_does_not_shift_following_fields() {
        // 现场 34H 正文。Z=FFFFFFFF 为缺测，之后 VT / 7A / FFA0 必须保持字段边界。
        let raw = hex(
            "01 50 26 09 10 17 00 45 \
             F1 F1 00 26 09 08 01 48 \
             F0 F0 26 09 10 16 05 \
             F4 60 FF FF FF FF FF FF FF FF FF 00 00 00 \
             F0 F0 26 09 10 17 00 26 19 00 00 15 \
             F0 F0 26 09 10 16 05 \
             F5 C0 FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF FF \
             F0 F0 26 09 10 17 00 \
             1A 19 00 00 00 \
             20 19 00 00 00 \
             39 23 FF FF FF FF \
             38 12 12 07 \
             7A 08 31 \
             FF A0 11 03 07",
        );
        assert_eq!(raw.len(), 117);

        let b = parse_body(0x34, &raw, Encoding::Hex).unwrap();
        let Content::Elements(e) = &b.content else { panic!() };
        assert_eq!(e.station_addr.as_deref(), Some("0026090801"));
        assert_eq!(e.station_class, Some('H'));
        assert_eq!(e.elements.len(), 9);
        assert_eq!(e.elements[5].ident, "Z");
        assert_eq!(e.elements[5].value, Value::Missing);
        assert_eq!(e.elements[6].ident, "VT");
        assert_eq!(e.elements[6].value, Value::Float(12.07));
        assert_eq!(e.elements[7].ident, "7A");
        assert_eq!(e.elements[7].value, Value::Int(31));
        assert_eq!(e.elements[8].ident, "FFA0");
        assert_eq!(e.elements[8].value, Value::Float(30.7));
    }

    #[test]
    fn hex_zt_and_rain() {
        let mut raw = vec![0x00, 0x03, 0x26, 0x09, 0x09, 0x09, 0x00, 0x00];
        raw.extend_from_slice(&[0xF0, 0x26, 0x09, 0x09, 0x09, 0x00]);
        raw.extend_from_slice(&[0xF4, 0x60, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        raw.extend_from_slice(&[0x45, 0x20, 0x00, 0x00, 0x00, 0x18]);
        let b = parse_body(0x34, &raw, Encoding::Hex).unwrap();
        let Content::Elements(e) = &b.content else { panic!() };
        assert_eq!(e.elements[0].ident, "DRP");
        match &e.elements[0].value {
            Value::HourlyRain(v) => assert_eq!(v[11], Some(1.2)),
            _ => panic!(),
        }
        assert_eq!(e.elements[1].value, Value::Status(0x18));
    }

    #[test]
    fn no_station_group() {
        let raw = "0007260909080000TT 2609090800 Z 6.38 ";
        let b = parse_body(0x32, &body(raw), Encoding::Ascii).unwrap();
        let Content::Elements(e) = &b.content else { panic!() };
        assert!(e.station_addr.is_none());
        assert_eq!(e.obs_time, parse_time_digits("2609090800"));
        assert_eq!(e.elements.len(), 1);
    }

    #[test]
    fn integer_value() {
        let raw = "0008260909080000TT 2609090800 NS 3 UC 8 ";
        let b = parse_body(0x32, &body(raw), Encoding::Ascii).unwrap();
        let Content::Elements(e) = &b.content else { panic!() };
        assert_eq!(e.elements[0].value, Value::Int(3));
        assert_eq!(e.elements[1].value, Value::Int(8));
    }

    #[test]
    fn bcd_val_helper() {
        let bcd = |b: u8| ((b >> 4) as u32) * 10 + (b & 0x0F) as u32;
        assert_eq!(bcd(0x05), 5);
        assert_eq!(bcd(0x23), 23);
    }
}
