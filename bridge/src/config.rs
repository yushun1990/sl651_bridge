//! 配置: TOML 文件 + 关键项环境变量覆盖。

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub listen: ListenCfg,
    pub center: CenterCfg,
    pub ack: AckCfg,
    pub m3: M3Cfg,
    pub dedup: DedupCfg,
    /// 遥测站地址 -> 设备别名 (可选; 缺省站码原样)
    pub stations: HashMap<String, StationCfg>,
    pub tb: TbCfg,
    pub payload: PayloadCfg,
    pub images: ImagesCfg,
    pub log: LogCfg,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListenCfg {
    /// TCP 监听地址, 置空禁用
    pub tcp: String,
    /// UDP 监听地址, 置空禁用
    pub udp: String,
    /// TCP 连接空闲超时 (秒)
    pub idle_timeout_secs: u64,
    /// 单连接累积缓冲上限 (字节, 防内存膨胀)
    pub max_buffer: usize,
}

impl Default for ListenCfg {
    fn default() -> Self {
        Self {
            tcp: "0.0.0.0:9000".into(),
            udp: "0.0.0.0:9000".into(),
            idle_timeout_secs: 900,
            max_buffer: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CenterCfg {
    /// 本中心站地址 (1~255)
    pub address: u8,
    /// 期望密码 (十六进制 4 字符)
    pub password: String,
    pub check_password: bool,
    pub check_address: bool,
}

impl Default for CenterCfg {
    fn default() -> Self {
        Self { address: 1, password: "0000".into(), check_password: false, check_address: false }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AckCfg {
    pub enabled: bool,
    /// ETX 后的最终终止符: "EOT" 立即释放 | "ESC" 保持在线 10 分钟
    pub final_terminator: String,
}

impl Default for AckCfg {
    fn default() -> Self {
        Self { enabled: true, final_terminator: "EOT".into() }
    }
}

impl AckCfg {
    pub fn terminator(&self) -> sl651_protocol::FinalTerminator {
        if self.final_terminator.eq_ignore_ascii_case("ESC") {
            sl651_protocol::FinalTerminator::Esc
        } else {
            sl651_protocol::FinalTerminator::Eot
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct M3Cfg {
    /// M3 分包重组超时 (秒)
    pub reassembly_timeout_secs: u64,
}

impl Default for M3Cfg {
    fn default() -> Self {
        Self { reassembly_timeout_secs: 180 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DedupCfg {
    /// 流水号去重窗口 (秒); M2 重发不重复入库
    pub enabled: bool,
    pub window_secs: u64,
}

impl Default for DedupCfg {
    fn default() -> Self {
        Self { enabled: true, window_secs: 600 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct StationCfg {
    /// 站点别名, 用于 TB 键 `别名:要素`
    pub name: Option<String>,
}


#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TbCfg {
    /// "mqtt" 正式中转 | "log" 仅打印不连接 (本地接入验证)
    pub mode: String,
    pub host: String,
    pub port: u16,
    pub client_id: String,
    pub username: String,
    pub password: String,
    pub keepalive_secs: u64,
    pub qos: u8,
    /// 断线溢出缓冲上限 (条数, 满则丢最旧)
    pub buffer: usize,
}

impl Default for TbCfg {
    fn default() -> Self {
        Self {
            mode: "mqtt".into(),
            host: "127.0.0.1".into(),
            port: 1883,
            client_id: "sl651-bridge".into(),
            username: String::new(),
            password: String::new(),
            keepalive_secs: 60,
            qos: 1,
            buffer: 10_000,
        }
    }
}

impl TbCfg {
    pub fn is_log_mode(&self) -> bool {
        self.mode.eq_ignore_ascii_case("log")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PayloadCfg {
    /// TB 载荷数据容器字段 (与网关规则链 ss_dataPath 对应)
    pub data_path: String,
    /// 帧时间字段 (与 ss_timePath 对应)
    pub time_path: String,
    /// 站名与要素的分隔符 (与网关规则链 ss_keySplitter 对应)
    pub key_splitter: String,
}

impl Default for PayloadCfg {
    fn default() -> Self {
        Self { data_path: "points".into(), time_path: "time".into(), key_splitter: ":".into() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImagesCfg {
    pub enabled: bool,
    /// 图片落盘目录
    pub dir: PathBuf,
}

impl Default for ImagesCfg {
    fn default() -> Self {
        Self { enabled: true, dir: PathBuf::from("images") }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogCfg {
    pub level: String,
    /// json | pretty
    pub format: String,
}

impl Default for LogCfg {
    fn default() -> Self {
        Self { level: "info".into(), format: "json".into() }
    }
}


impl Config {
    /// 依次尝试: 显式路径 > $SL651_CONFIG > ./bridge.toml > 全默认。
    pub fn load(path: Option<&str>) -> anyhow::Result<Self> {
        let candidates: Vec<PathBuf> = match path {
            Some(p) => vec![PathBuf::from(p)],
            None => {
                let mut v = Vec::new();
                if let Ok(p) = std::env::var("SL651_CONFIG") {
                    v.push(PathBuf::from(p));
                }
                v.push(PathBuf::from("bridge.toml"));
                v
            }
        };
        let mut cfg = Config::default();
        for p in &candidates {
            if p.is_file() {
                let raw = std::fs::read_to_string(p)?;
                let file_cfg: Config = toml::from_str(&raw)
                    .map_err(|e| anyhow::anyhow!("解析配置 {p:?} 失败: {e}"))?;
                cfg = file_cfg;
                break;
            }
        }
        if !candidates.iter().any(|p| p.is_file()) && path.is_some() {
            anyhow::bail!("配置文件不存在: {:?}", candidates[0]);
        }
        cfg.apply_env();
        Ok(cfg)
    }

    /// 关键项环境变量覆盖 (容器/临时调试用)。
    fn apply_env(&mut self) {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("SL651_LISTEN_TCP") {
            self.listen.tcp = v;
        }
        if let Some(v) = env("SL651_LISTEN_UDP") {
            self.listen.udp = v;
        }
        if let Some(v) = env("SL651_TB_HOST") {
            self.tb.host = v;
        }
        if let Some(v) = env("SL651_TB_PORT") {
            if let Ok(p) = v.parse() {
                self.tb.port = p;
            }
        }
        if let Some(v) = env("SL651_TB_USERNAME") {
            self.tb.username = v;
        }
        if let Some(v) = env("SL651_TB_PASSWORD") {
            self.tb.password = v;
        }
        if let Some(v) = env("SL651_TB_CLIENT_ID") {
            self.tb.client_id = v;
        }
        if let Some(v) = env("SL651_LOG_LEVEL") {
            self.log.level = v;
        }
    }

    /// 站点显示名: 别名优先, 否则 10 位站码。
    pub fn station_display(&self, addr: &str) -> String {
        self.stations
            .get(addr)
            .and_then(|s| s.name.clone())
            .unwrap_or_else(|| addr.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_example() {
        let raw = r#"
[listen]
tcp = "0.0.0.0:5000"

[center]
address = 2

[stations]
"5010123456" = { name = "寨上站" }

[tb]
host = "tb.example.com"
username = "gw1"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.listen.tcp, "0.0.0.0:5000");
        assert_eq!(cfg.listen.udp, "0.0.0.0:9000");
        assert_eq!(cfg.center.address, 2);
        assert_eq!(cfg.station_display("5010123456"), "寨上站");
        assert_eq!(cfg.station_display("6010123456"), "6010123456");
        assert_eq!(cfg.tb.host, "tb.example.com");
        assert_eq!(cfg.ack.terminator(), sl651_protocol::FinalTerminator::Eot);
    }
}
