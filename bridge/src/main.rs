//! sl651-bridge: SL651-2014 遥测站接入网关。
//!
//! TCP/UDP 接收遥测站报文 -> 协议解析/应答/M3 重组 -> ThingsBoard MQTT 上行
//! (单网关设备 + PLC虚拟网关链兼容载荷)。

use std::sync::Arc;

use clap::Parser;
use sl651_bridge::{config::Config, mqtt, server, session};
use tokio::net::{TcpListener, UdpSocket};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "sl651-bridge", version, about = "SL651-2014 -> ThingsBoard MQTT 接入网关")]
struct Args {
    /// 配置文件路径 (默认 $SL651_CONFIG 或 ./bridge.toml)
    #[arg(short, long)]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = Arc::new(Config::load(args.config.as_deref())?);
    init_tracing(&cfg.log);

    let sink: Arc<dyn mqtt::TbSink> = if cfg.tb.is_log_mode() {
        info!("[tb] mode=log: 不连接 ThingsBoard, 上行载荷仅打印日志");
        Arc::new(mqtt::LogSink)
    } else {
        mqtt::MqttSink::start(&cfg).await?
    };
    let sessions = Arc::new(session::Sessions::new(cfg.clone()));

    let mut tasks = Vec::new();

    if !cfg.listen.tcp.is_empty() {
        let listener = TcpListener::bind(&cfg.listen.tcp)
            .await
            .map_err(|e| anyhow::anyhow!("TCP 监听 {} 失败: {e}", cfg.listen.tcp))?;
        tasks.push(tokio::spawn(server::run_tcp(listener, sessions.clone(), sink.clone())));
    }
    if !cfg.listen.udp.is_empty() {
        let socket = UdpSocket::bind(&cfg.listen.udp)
            .await
            .map_err(|e| anyhow::anyhow!("UDP 监听 {} 失败: {e}", cfg.listen.udp))?;
        tasks.push(tokio::spawn(server::run_udp(Arc::new(socket), sessions.clone(), sink.clone())));
    }

    info!(
        tcp = %cfg.listen.tcp,
        udp = %cfg.listen.udp,
        tb = format!("{}:{}", cfg.tb.host, cfg.tb.port),
        "sl651-bridge 已启动"
    );

    // 等待退出信号
    tokio::signal::ctrl_c().await?;
    info!("收到退出信号, 关闭");
    for t in tasks {
        t.abort();
    }
    Ok(())
}

fn init_tracing(log: &sl651_bridge::config::LogCfg) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&log.level));
    if log.format == "pretty" {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    }
}
