//! TCP / UDP 接入服务。
//!
//! TCP: 站端长连接, 流式分帧 + 空闲超时; UDP: 每数据报独立处理 (兼容粘包多帧)。

use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use sl651_protocol::{scan, Scanned};

use crate::mqtt::TbSink;
use crate::session::Sessions;

const RAW_LOG_LIMIT: usize = 1024;

fn hex_preview(data: &[u8]) -> String {
    let shown = data.len().min(RAW_LOG_LIMIT);
    let mut out = data[..shown]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ");
    if data.len() > shown {
        out.push_str(" ...");
    }
    out
}

/// 运行 TCP 接入直至监听器失效。
pub async fn run_tcp(listener: TcpListener, sessions: Arc<Sessions>, sink: Arc<dyn TbSink>) {
    let local = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
    info!(%local, "TCP 接入已启动");
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let sessions = sessions.clone();
                let sink = sink.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, peer.to_string(), sessions, sink).await {
                        debug!(peer = %peer, "TCP 连接结束: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("TCP accept 失败: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    peer: String,
    sessions: Arc<Sessions>,
    sink: Arc<dyn TbSink>,
) -> anyhow::Result<()> {
    let idle = std::time::Duration::from_secs(sessions.idle_timeout_secs());
    let max_buf = sessions.max_buffer();
    let mut acc: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    use tokio::io::AsyncWriteExt;
    loop {
        let n = match timeout(idle, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break, // 对端关闭
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                warn!(peer = %peer, "TCP 连接空闲超时");
                break;
            }
        };
        acc.extend_from_slice(&chunk[..n]);
        if acc.len() > max_buf {
            warn!(peer = %peer, raw_len = acc.len(), raw_hex = %hex_preview(&acc), "累积缓冲超限, 清空 {} 字节", acc.len());
            acc.clear();
            continue;
        }
        loop {
            match scan(&acc) {
                Scanned::Frame { frame, consumed } => {
                    let remote = format!("tcp://{peer}");
                    let ack = sessions.handle_frame(frame, &remote, &sink);
                    if let Some(a) = ack {
                        stream.write_all(&a).await.ok();
                    }
                    acc.drain(..consumed);
                }
                Scanned::NeedMore => break,
                Scanned::Skip { skip, reason, partial } => {
                    warn!(
                        peer = %peer,
                        raw_len = acc.len(),
                        raw_hex = %hex_preview(&acc),
                        "丢弃损坏数据 {skip} 字节: {reason}"
                    );
                    let nak = sessions.handle_corrupt(partial, &reason.to_string());
                    if let Some(a) = nak {
                        stream.write_all(&a).await.ok();
                    }
                    acc.drain(..skip);
                }
            }
        }
    }
    Ok(())
}

/// 运行 UDP 接入直至 socket 失效。
pub async fn run_udp(socket: Arc<UdpSocket>, sessions: Arc<Sessions>, sink: Arc<dyn TbSink>) {
    let local = socket.local_addr().map(|a| a.to_string()).unwrap_or_default();
    info!(%local, "UDP 接入已启动");
    let mut buf = vec![0u8; 65536];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((n, peer)) => {
                let data = buf[..n].to_vec();
                let sessions = sessions.clone();
                let sink = sink.clone();
                let sock = socket.clone();
                tokio::spawn(async move {
                    let mut pending = data;
                    loop {
                        match scan(&pending) {
                            Scanned::Frame { frame, consumed } => {
                                let remote = format!("udp://{peer}");
                                let ack = sessions.handle_frame(frame, &remote, &sink);
                                if let Some(a) = ack {
                                    sock.send_to(&a, peer).await.ok();
                                }
                                pending.drain(..consumed);
                            }
                            Scanned::NeedMore => break,
                            Scanned::Skip { skip, reason, partial } => {
                                warn!(
                                    peer = %peer.to_string(),
                                    raw_len = pending.len(),
                                    raw_hex = %hex_preview(&pending),
                                    "丢弃损坏数据 {skip} 字节: {reason}"
                                );
                                let nak = sessions.handle_corrupt(partial, &reason.to_string());
                                if let Some(a) = nak {
                                    sock.send_to(&a, peer).await.ok();
                                }
                                pending.drain(..skip);
                            }
                        }
                        if pending.is_empty() {
                            break;
                        }
                    }
                });
            }
            Err(e) => {
                warn!("UDP recv 失败: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}