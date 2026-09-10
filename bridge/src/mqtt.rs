//! TB MQTT 发布: rumqttc 客户端 + 断线溢出缓冲。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, EventLoop, MqttOptions, QoS};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::tbmsg::TbMessage;

/// TB 消息出口 (发布器抽象, 便于测试注入)。
pub trait TbSink: Send + Sync {
    fn publish(&self, msg: TbMessage) -> anyhow::Result<()>;
}

/// 进程内 mock (测试用)。
#[derive(Default)]
pub struct MockSink {
    pub messages: std::sync::Mutex<Vec<TbMessage>>,
}

impl MockSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn take(&self) -> Vec<TbMessage> {
        self.messages.lock().unwrap().drain(..).collect()
    }
}

impl TbSink for MockSink {
    fn publish(&self, msg: TbMessage) -> anyhow::Result<()> {
        self.messages.lock().unwrap().push(msg);
        Ok(())
    }
}

/// 本地接入验证模式: 不连接 MQTT, 上行载荷直接打印日志。
pub struct LogSink;

impl TbSink for LogSink {
    fn publish(&self, msg: TbMessage) -> anyhow::Result<()> {
        tracing::info!(
            topic = msg.topic.as_str(),
            payload = %serde_json::to_string(&msg.payload).unwrap_or_default(),
            "[TB-LOG] 本地模式上行 (未中转)"
        );
        Ok(())
    }
}

/// MQTT 发布器: 内部通道 + 转发任务 (持续 poll eventloop 以驱动收发与重连)。
pub struct MqttSink {
    tx: mpsc::Sender<TbMessage>,
}

impl MqttSink {
    /// 启动转发任务。`announce` 在首次连接成功时调用 (可为 None)。
    pub async fn start(cfg: &Config) -> anyhow::Result<Arc<Self>> {
        let mut opts = MqttOptions::new(&cfg.tb.client_id, &cfg.tb.host, cfg.tb.port);
        opts.set_keep_alive(Duration::from_secs(cfg.tb.keepalive_secs.max(5)));
        if !cfg.tb.username.is_empty() {
            opts.set_credentials(&cfg.tb.username, &cfg.tb.password);
        }
        // 会话保持: 断线期间 QoS1 消息由 broker 补投
        opts.set_clean_session(false);
        let (client, eventloop) = AsyncClient::new(opts, cfg.tb.buffer.max(64));
        let (tx, rx) = mpsc::channel::<TbMessage>(cfg.tb.buffer.clamp(64, 4096));
        let qos = match cfg.tb.qos {
            0 => QoS::AtMostOnce,
            _ => QoS::AtLeastOnce,
        };
        let max_overflow = cfg.tb.buffer;
        tokio::spawn(forwarder(client, eventloop, rx, qos, max_overflow));
        Ok(Arc::new(Self { tx }))
    }
}

impl TbSink for MqttSink {
    fn publish(&self, msg: TbMessage) -> anyhow::Result<()> {
        self.tx
            .try_send(msg)
            .map_err(|e| anyhow::anyhow!("MQTT 通道满或关闭: {e}"))
    }
}

async fn forwarder(
    client: AsyncClient,
    mut eventloop: EventLoop,
    mut rx: mpsc::Receiver<TbMessage>,
    qos: QoS,
    max_overflow: usize,
) {
    let mut overflow: VecDeque<(String, Vec<u8>)> = VecDeque::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        // 先冲刷溢出缓冲
        while let Some((topic, payload)) = overflow.front().cloned() {
            match client.publish(&topic, qos, false, payload).await {
                Ok(()) => {
                    overflow.pop_front();
                }
                Err(e) => {
                    tracing::warn!("MQTT 溢出缓冲重投失败: {e}");
                    break;
                }
            }
        }
        tokio::select! {
            maybe = rx.recv() => {
                match maybe {
                    Some(msg) => {
                        let (topic, payload) = (msg.topic.as_str().to_string(), msg.payload_bytes());
                        if let Err(e) = client.publish(&topic, qos, false, payload.clone()).await {
                            tracing::warn!("MQTT 入队失败, 进入溢出缓冲: {e}");
                            overflow.push_back((topic, payload));
                            while overflow.len() > max_overflow {
                                let dropped = overflow.pop_front();
                                tracing::error!("溢出缓冲已满, 丢弃最旧消息: {:?}", dropped.map(|d| d.0));
                            }
                        }
                    }
                    None => break, // 主进程关闭
                }
            }
            ev = eventloop.poll() => {
                match ev {
                    Ok(_) => backoff = Duration::from_secs(1),
                    Err(e) => {
                        tracing::warn!("MQTT 连接异常, {}s 后重试: {e}", backoff.as_secs());
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                    }
                }
            }
        }
    }
    tracing::info!("MQTT 转发任务退出");
}

#[cfg(test)]
mod tests {
    
    use crate::tbmsg::Topic;

    #[test]
    fn topic_strings() {
        assert_eq!(Topic::Telemetry.as_str(), "v1/devices/me/telemetry");
        assert_eq!(Topic::Attributes.as_str(), "v1/devices/me/attributes");
    }
}
