//! sl651-sim: SL651-2014 遥测站模拟器 (ASCII 编码)。
//!
//! 生成合规 SL651 报文经 TCP/UDP 发往中心站 (sl651-bridge 或其他),
//! 用于联调与 TB 端到端验证。超长正文自动 M3 分包。

use std::time::Duration;

use chrono::{Datelike, Local, Timelike};
use clap::{Args, Parser, Subcommand};
use sl651_protocol::{
    encode, scan, Direction, Encoding, EndChar, M3Seq, OutFrame, Scanned,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

#[derive(Parser, Debug)]
#[command(name = "sl651-sim", version, about = "SL651-2014 遥测站模拟器 (ASCII 编码)")]
struct Cli {
    /// 中心站地址
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value = "9000")]
    port: u16,
    /// tcp | udp
    #[arg(long, default_value = "tcp")]
    transport: String,
    /// 遥测站地址 (10 位)
    #[arg(long, default_value = "5010123456")]
    station: String,
    /// 中心站编号 (帧头地址, 1~255)
    #[arg(long, default_value = "1")]
    center: u8,
    /// 流水号起点 (每轮自动递增)
    #[arg(long, default_value = "1")]
    serial: u16,
    /// 观测时间 YYMMDDHHmm (缺省取当前分钟)
    #[arg(long)]
    time: Option<String>,
    /// 发送轮数
    #[arg(long, default_value = "1")]
    r#loop: u32,
    /// 轮间隔 (秒)
    #[arg(long, default_value = "1.0")]
    interval: f64,
    /// 不等待应答
    #[arg(long)]
    no_ack: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 链路维持报 (2FH)
    Keepalive,
    /// 遥测站定时报 (32H)
    Timer {
        #[command(flatten)]
        args: ElementArgs,
    },
    /// 遥测站加报报 (33H)
    Add {
        #[command(flatten)]
        args: ElementArgs,
    },
    /// 遥测站小时报 (34H): DRP 12 组 5 分钟雨量 + PT + VT
    Hourly {
        /// 逗号分隔的 12 组 5 分钟雨量 (毫米), 如 0.5,0.0,1.2,...
        #[arg(long)]
        rain: Option<String>,
        /// DRZ1 相对水位 (米), 逗号分隔 12 组
        #[arg(long)]
        level: Option<String>,
        /// 降水量累计值 PT (毫米)
        #[arg(long, default_value = "12.5")]
        pt: f64,
        #[command(flatten)]
        common: CommonArgs,
    },
    /// 均匀时段报 (31H): 多要素多时段, 超长自动 M3 分包
    Uniform {
        /// 时间步长: N05=5分钟, H01=1小时, D01=1天
        #[arg(long, default_value = "N05")]
        step: String,
        /// 逗号分隔要素表, 如 Z,Q
        #[arg(long, default_value = "Z")]
        idents: String,
        /// 逗号分隔数据 (时间优先排列), 如 6.3,12.5,6.35,12.6
        #[arg(long)]
        values: String,
        #[command(flatten)]
        common: CommonArgs,
    },
    /// 人工置数报 (35H)
    Manual {
        /// 原编码数据 (SL330, 省略 NN 结束符)
        #[arg(long, default_value = "MSL 5010123456 202609090800 Z 6.4")]
        data: String,
    },
    /// 图片报 (36H, 自动 M3 分包)
    Image {
        /// JPG 文件路径
        #[arg(long)]
        file: String,
        /// 每包正文字节
        #[arg(long, default_value = "180")]
        chunk: usize,
    },
    /// 演示序列: 链路维持 + 定时 + 加报 + 小时报
    Demo {
        #[command(flatten)]
        common: CommonArgs,
    },
    /// 自定义 ASCII 正文报文 (帧头/CRC 自动生成)
    Custom {
        /// 功能码十六进制, 如 32 / 0x33
        #[arg(long)]
        func: String,
        /// ASCII 正文信息组, 如 "ST 5010123456 H TT 2609090800 Z 6.38 VT 12.5 " (尾部空格建议带上)
        #[arg(long)]
        body: String,
        /// 不自动前缀 流水号+发报时间 (正文完全按 --body 原样发送)
        #[arg(long)]
        no_prefix: bool,
    },
    /// 原样回放十六进制帧 (现场抓包字节级验证)
    Raw {
        /// 帧的十六进制字节 (可含空格/换行), 如 7E7E01...E43B; 或 @文件路径
        hex: String,
    },
}

#[derive(Args, Debug)]
struct ElementArgs {
    /// 瞬时水位 Z (米)
    #[arg(long)]
    z: Option<f64>,
    /// 瞬时流量 Q (立方米/秒)
    #[arg(long)]
    q: Option<f64>,
    /// 电源电压 VT
    #[arg(long, default_value = "12.5")]
    vt: f64,
    /// 状态报警 ZT (十六进制 8 位, 如 00000018)
    #[arg(long)]
    zt: Option<String>,
    /// 遥测站分类码字符
    #[arg(long, default_value = "H")]
    class: char,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args, Debug)]
struct CommonArgs {
    /// 额外要素: 标识符=值, 可重复, 如 -e PT=12.5 -e PD=0.0
    #[arg(short = 'e', long = "elem")]
    elems: Vec<String>,
}

fn now_digits() -> String {
    let t = Local::now().naive_local();
    format!(
        "{:02}{:02}{:02}{:02}{:02}{:02}",
        t.year() % 100,
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// 功能码解析: "32" / "0x33" (十六进制)。
fn parse_func(s: &str) -> anyhow::Result<u8> {
    let t = s.trim_start_matches("0x").trim_start_matches("0X");
    u8::from_str_radix(t, 16).map_err(|_| anyhow::anyhow!("功能码应为十六进制: {s}"))
}

/// 加载十六进制字节: 直接字符串 (可含空白) 或 @文件。
fn load_hex(arg: &str) -> anyhow::Result<Vec<u8>> {
    let text = if let Some(path) = arg.strip_prefix('@') {
        std::fs::read_to_string(path)?
    } else {
        arg.to_string()
    };
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() || !cleaned.len().is_multiple_of(2) {
        anyhow::bail!("十六进制字节长度非法: {cleaned}");
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(|e| anyhow::anyhow!("{e}")))
        .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let mut tcp: Option<TcpStream> = None;
    let mut udp: Option<UdpSocket> = None;
    match cli.transport.as_str() {
        "udp" => udp = Some(UdpSocket::bind("0.0.0.0:0").await?),
        _ => tcp = Some(TcpStream::connect((cli.host.as_str(), cli.port)).await?),
    }

    // 字节级回放: 帧原样发送, 不做任何加工
    if let Command::Raw { hex } = &cli.command {
        let bytes = load_hex(hex)?;
        print_frame(&format!("回放帧 ({} 字节)", bytes.len()), &bytes);
        match (&mut tcp, &mut udp) {
            (Some(stream), _) => {
                stream.write_all(&bytes).await?;
                if !cli.no_ack {
                    wait_ack(stream, None).await?;
                }
            }
            (_, Some(sock)) => {
                sock.send_to(&bytes, (cli.host.as_str(), cli.port)).await?;
            }
            _ => unreachable!(),
        }
        return Ok(());
    }

    let mut serial = cli.serial;
    for round in 0..cli.r#loop {
        if round > 0 {
            tokio::time::sleep(Duration::from_secs_f64(cli.interval)).await;
        }
        let bodies = command_bodies(&cli, serial)?;
        for (func, body) in bodies {
            let frames = build_frames(&cli, func, serial, &body);
            for (desc, bytes, m3) in frames {
                print_frame(&desc, &bytes);
                match (&mut tcp, &mut udp) {
                    (Some(stream), _) => {
                        stream.write_all(&bytes).await?;
                        if !cli.no_ack && func != 0x2F {
                            wait_ack(stream, m3).await?;
                        }
                    }
                    (_, Some(sock)) => {
                        sock.send_to(&bytes, (cli.host.as_str(), cli.port)).await?;
                    }
                    _ => unreachable!(),
                }
            }
        }
        serial = serial.wrapping_add(1);
    }
    Ok(())
}

/// 一次命令产生的 (功能码, 正文) 序列。
fn command_bodies(cli: &Cli, serial: u16) -> anyhow::Result<Vec<(u8, Vec<u8>)>> {
    let obs = cli.obs_time();
    match &cli.command {
        Command::Keepalive => Ok(vec![(0x2F, format!("{serial:04X}{}", now_digits()).into_bytes())]),
        Command::Timer { args: a } | Command::Add { args: a } => {
            Ok(vec![(cli.command_func(), element_body(cli, serial, a, &obs))])
        }
        Command::Hourly { rain, level, pt, common } => {
            let mut s = format!(
                "{serial:04X}{}ST {} H TT {} ",
                now_digits(),
                cli.station,
                obs
            );
            if let Some(r) = rain {
                let hex: String = r
                    .split(',')
                    .map(|v| -> anyhow::Result<String> {
                        let b = (v.parse::<f64>()? * 10.0).round() as u8;
                        Ok(format!("{b:02X}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .concat();
                s.push_str(&format!("DRP {hex} "));
            }
            s.push_str(&format!("PT {pt} "));
            if let Some(l) = level {
                let hex: String = l
                    .split(',')
                    .map(|v| -> anyhow::Result<String> {
                        let b = (v.parse::<f64>()? * 100.0).round() as u16;
                        Ok(format!("{b:04X}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .concat();
                s.push_str(&format!("DRZ1 {hex} "));
            }
            for e in &common.elems {
                if let Some((k, v)) = e.split_once('=') {
                    s.push_str(&format!("{k} {v} "));
                }
            }
            s.push_str("VT 12.00 ");
            Ok(vec![(0x34, s.into_bytes())])
        }
        Command::Uniform { step, idents, values, common } => {
            let _ = common;
            let mut s = format!(
                "{serial:04X}{}ST {} H TT {} DR{step} {} ",
                now_digits(),
                cli.station,
                obs,
                idents.split(',').collect::<Vec<_>>().join(" ")
            );
            for v in values.split(',') {
                s.push_str(&format!("{v} "));
            }
            Ok(vec![(0x31, s.into_bytes())])
        }
        Command::Manual { data } => Ok(vec![(
            0x35,
            format!("{serial:04X}{}RGZS {data} ", now_digits()).into_bytes(),
        )]),
        Command::Image { file, chunk } => {
            let jpg = std::fs::read(file)?;
            let mut v = format!(
                "{serial:04X}{}ST {} H TT {} PIC ",
                now_digits(),
                cli.station,
                obs
            )
            .into_bytes();
            v.extend_from_slice(&jpg);
            v.push(b' ');
            let _ = chunk;
            Ok(vec![(0x36, v)])
        }
        Command::Demo { common } => Ok(vec![
            (0x2F, format!("{serial:04X}{}", now_digits()).into_bytes()),
            (
                0x32,
                {
                    let mut s = format!(
                        "{serial:04X}{}ST {} H TT {} ",
                        now_digits(),
                        cli.station,
                        obs
                    );
                    for e in &common.elems {
                        if let Some((k, v)) = e.split_once('=') {
                            s.push_str(&format!("{k} {v} "));
                        }
                    }
                    s.push_str("Z 6.38 VT 12.50 ");
                    s.into_bytes()
                },
            ),
            (
                0x33,
                format!("{serial:04X}{}ST {} H TT {} Z 6.41 ZT 00000003 VT 12.48 ", now_digits(), cli.station, obs)
                    .into_bytes(),
            ),
            (
                0x34,
                format!(
                    "{serial:04X}{}ST {} H TT {} DRP 0102030405060708090A0B0C PT 6.0 DRZ1 00640065006600670068FFFF0069006A006B006C006D006E VT 12.00 ",
                    now_digits(),
                    cli.station,
                    obs
                )
                .into_bytes(),
            ),
        ]),
        Command::Custom { func, body, no_prefix } => {
            let f = parse_func(func)?;
            let text = if *no_prefix {
                body.clone()
            } else {
                // 自动前缀: 流水号(4位HEX) + 发报时间(YYMMDDHHmmSS), 与正文信息组直连
                format!("{serial:04X}{}{body}", now_digits())
            };
            Ok(vec![(f, text.into_bytes())])
        }
        Command::Raw { .. } => unreachable!("raw 已在 main 提前处理"),
    }
}

impl Cli {
    fn command_func(&self) -> u8 {
        match &self.command {
            Command::Keepalive | Command::Demo { .. } => 0x32,
            Command::Timer { .. } => 0x32,
            Command::Add { .. } => 0x33,
            Command::Hourly { .. } => 0x34,
            Command::Uniform { .. } => 0x31,
            Command::Manual { .. } => 0x35,
            Command::Image { .. } => 0x36,
            Command::Custom { func, .. } => parse_func(func).unwrap_or(0x32),
            Command::Raw { .. } => 0x00,
        }
    }

    fn obs_time(&self) -> String {
        match &self.time {
            Some(t) => t.clone(),
            None => {
                let t = Local::now().naive_local();
                format!(
                    "{:02}{:02}{:02}{:02}{:02}",
                    t.year() % 100,
                    t.month(),
                    t.day(),
                    t.hour(),
                    t.minute()
                )
            }
        }
    }
}

fn element_body(cli: &Cli, serial: u16, a: &ElementArgs, obs: &str) -> Vec<u8> {
    let mut s = format!(
        "{serial:04X}{}ST {} {} TT {} ",
        now_digits(),
        cli.station,
        a.class,
        obs
    );
    if let Some(z) = a.z {
        s.push_str(&format!("Z {z} "));
    }
    if let Some(q) = a.q {
        s.push_str(&format!("Q {q} "));
    }
    for e in &a.common.elems {
        if let Some((k, v)) = e.split_once('=') {
            s.push_str(&format!("{k} {v} "));
        }
    }
    if let Some(zt) = &a.zt {
        s.push_str(&format!("ZT {zt} "));
    }
    s.push_str(&format!("VT {} ", a.vt));
    s.into_bytes()
}

/// 生成一帧 (或多帧 M3 分包)。
fn build_frames(cli: &Cli, func: u8, serial: u16, body: &[u8]) -> Vec<(String, Vec<u8>, Option<M3Seq>)> {
    let chunk = match &cli.command {
        Command::Image { chunk, .. } => *chunk,
        _ => 180,
    };
    if body.len() + 6 > 1000 && body.len() > chunk {
        let total = body.len().div_ceil(chunk);
        let mut out = Vec::new();
        for (i, part) in body.chunks(chunk).enumerate() {
            let seq = (i + 1) as u16;
            let m3 = M3Seq { total: total as u16, seq };
            let end = if seq == total as u16 { EndChar::Etx } else { EndChar::Etb };
            let bytes = encode(&OutFrame {
                encoding: Encoding::Ascii,
                direction: Direction::Uplink,
                center_addr: cli.center,
                station_addr: &cli.station,
                password: 0,
                func,
                end,
                m3: Some(m3),
                body: part,
            });
            out.push((format!("M3 包{seq}/{total}"), bytes, Some(m3)));
        }
        out
    } else {
        let bytes = encode(&OutFrame {
            encoding: Encoding::Ascii,
            direction: Direction::Uplink,
            center_addr: cli.center,
            station_addr: &cli.station,
            password: 0,
            func,
            end: EndChar::Etx,
            m3: None,
            body,
        });
        vec![(format!("功能码 {func:02X} 流水号 {serial:04X}"), bytes, None)]
    }
}

fn print_frame(desc: &str, bytes: &[u8]) {
    let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    println!("== {desc} ({} 字节) ==\n{hex}", bytes.len());
}

async fn wait_ack(stream: &mut TcpStream, m3: Option<M3Seq>) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await??;
    match scan(&buf[..n]) {
        Scanned::Frame { frame, .. } => {
            // 流水号按编码取: ASCII=前4字符HEX, HEX=前2字节BE
            let serial = match frame.encoding {
                Encoding::Ascii => String::from_utf8_lossy(frame.body.get(..4).unwrap_or(b"??")).into_owned(),
                _ => format!("{:04X}", u16::from_be_bytes([frame.body.first().copied().unwrap_or(0), frame.body.get(1).copied().unwrap_or(0)])),
            };
            println!(
                "<== 应答: 方向={} 结束符={:?} 功能码={:02X} 流水号={serial}",
                if frame.direction == Direction::Downlink { "下行" } else { "上行" },
                frame.end,
                frame.func,
            );
            if let Some(expect) = m3 {
                anyhow::ensure!(frame.m3 == Some(expect), "M3 应答序号不符: {:?} != {expect:?}", frame.m3);
            }
            Ok(())
        }
        other => Err(anyhow::anyhow!("应答帧解析失败: {other:?}")),
    }
}
