# sl651-bridge

SL651-2014《水文监测数据通信规约》遥测站接入网关:接收遥测站 TCP/UDP 报文,
解析、确认、重组后以 ThingsBoard MQTT 单网关设备上报,载荷与
`water-spirit/onboard-tool` 的 **PLC虚拟网关规则链** 兼容,实现最小损失转报
(原始标识符 + 原始观测时间)。

```
SL651 遥测站 ──TCP──►┐                        MQTT QoS1                 ┌───────────────┐
                ┌────┴──────────┐  v1/devices/me/telemetry             │  ThingsBoard   │
SL651 遥测站 ──UDP──►│  sl651-bridge  │ ───────────────────────────────► │  网关设备      │
                │ 解析/应答/M3重组 │  {"points":{"站:要素":v},           │  规则链拆分    │
                └───────────────┘   "time":"观测时间"}                  │  → 子设备时序  │
                                                                     └───────────────┘
```

## 功能

- **传输**: TCP(流式分帧+空闲超时) 与 UDP(每报文独立,兼容粘包) 同时监听
- **编码**: ASCII 字符编码(§6.4) 与 HEX/BCD 编码(§6.5) 帧自动识别
- **功能码**: 链路维持 2F / 测试 30 / 均匀时段 31 / 定时 32 / 加报 33 / 小时 34 /
  人工置数 35 / 图片 36;E0~FF 用户自定义透传
- **要素**: 附录C 全标识符表(内置 N(D,d) 与单位);ZT 状态位图按表58 展开
  为 `ZT_交流停电` 等布尔键;缺测 M/F 跳过;负数 BCD(FF 前缀)
- **小时报**: DRP 12×5min 雨量、DRZ1..8 12×5min 水位按观测时间逐点展开
  (表36: 观测时间=第一组数据时间),同一观测时间的其他要素合并同帧
- **均匀时段**: 时间步长码 DRD/DRH/DRN(含全 0 固定搭配) + 时间优先数据组
  (表30) 按步长展开时间戳
- **应答**: M2 确认(ACK/EOT/ESC 可配);M3 坏包 NAK、集齐后 EOT;2F 不应答;
  重发帧(同流水号同正文)去重不重复入库但仍应答
- **图片**: M3 重组后 JPG 落盘(原子写),大小/文件名作为属性上报
- **可靠**: MQTT QoS1 + 断线溢出缓冲(满丢最旧告警) + 指数退避重连;
  CRC 校验失败自动重扫恢复同步
- **sl651-sim**: 遥测站模拟器(演示序列/定时/加报/小时/均匀/图片/人工置数),
  可用于本地与 TB 联调

## 构建

```bash
cargo build --release --bins
# 产物: target/release/sl651-bridge  target/release/sim
```

## 运行

### 本地接入验证 (不连接 ThingsBoard)

证明设备能接入、报文能正确解析的最小闭环 —— `[tb] mode = "log"` 时网关**不建立
MQTT 连接**,每条上行载荷以 `[TB-LOG]` 打印到日志:

```bash
cargo build --bins

# 终端 1: 网关 (log 模式, 监听 0.0.0.0:9000 TCP+UDP)
./target/debug/sl651-bridge --config configs/local-test.toml

# 终端 2: 模拟设备
./target/debug/sim demo                                   # 演示: 链路维持+定时+加报+小时
./target/debug/sim custom --func 33 --body "ST 5010123456 H TT 2609090800 Z 6.38 Q 12.5 VT 11.9 "
./target/debug/sim raw 7E7E0101...E43B                    # 回放现场抓包 (十六进制原样发送)
./target/debug/sim --transport udp demo                   # UDP 接入验证
```

终端 1 即可看到 (无需任何外部服务):

```
INFO sl651_bridge::mqtt: [TB-LOG] 本地模式上行 (未中转) topic="v1/devices/me/telemetry"
     payload={"points":{"寨上站:VT":12.5,"寨上站:Z":6.38},"time":"2026-09-10 12:02:00"}
INFO sl651_bridge::session: 报文处理完成 station=寨上站 func="32" serial=1 transport="tcp://..."
```

模拟器侧同步打印 `<== 应答: 方向=下行 结束符=Eot 功能码=32 流水号=0001` 证明
M2 确认链路正常。接入真实设备时,把设备上行地址指向本机 9000 端口即可观察同样日志。

### 正式中转 ThingsBoard

```bash
cp configs/bridge.example.toml bridge.toml
# 编辑 [tb] 段: mode 保持 "mqtt", 填 ThingsBoard MQTT 地址与网关设备凭据
./target/release/sl651-bridge --config bridge.toml
```

Docker / systemd 见 `deploy/`。

## 模拟器联调

```bash
# 演示: 链路维持 + 定时 + 加报 + 小时 (默认 127.0.0.1:9000, TCP, ASCII)
./target/release/sim demo

# 指定水位/电压的定时报, 走 UDP
./target/release/sim --transport udp timer --z 6.38 --vt 12.5 -e PT=12.5

# 均匀时段 (自动 M3 分包): 5 分钟步长, 要素 Z,Q 两组数据
./target/release/sim uniform --step N05 --idents Z,Q --values 6.3,12.5,6.35,12.6

# 小时报: 12 组 5 分钟雨量 + 水位
./target/release/sim hourly --rain 0.5,0.0,1.2,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0 \
                            --level 1.0,1.01,1.02,1.03,1.04,1.05,1.06,1.07,1.08,1.09,1.1,1.11

# 图片报 (M3 分包)
./target/release/sim image --file cam.jpg

# 自定义正文 (帧头/CRC/流水号/发报时间自动生成, --func 十六进制)
./target/release/sim custom --func 33 --body "ST 5010123456 H TT 2609090800 Z 6.38 ZT 00000003 VT 12.5 "

# 回放现场抓包帧 (字节级原样发送, 可 @文件)
./target/release/sim raw 7E7E0101000000011234300033020040...03E43B
```

本地无真实站时,可快速验证(参考 `mosquitto`):
`mosquitto -d -p 18830` + `mosquitto_sub -p 18830 -t '#' -v`,
再以 `[tb] host=127.0.0.1 port=18830` 启动网关、运行 sim,即可看到
`v1/devices/me/telemetry` 载荷。

## ThingsBoard 侧配置

网关作为**单一 TB 设备**(MQTT_BASIC 凭据)接入,载荷完全兼容
onboard-tool `rule_chain/plc__*.json`(PLC虚拟网关链):

```json
{"points": {"3301060001:Z": 6.38, "3301060001:VT": 12.5}, "time": "2026-09-09 08:00:00"}
```

- `points` = `设备名:标识符`,**全部使用协议源码不翻译**:设备名=上报站码
  (`[stations]` 别名除外),点位键=规约附录C 标识符原样(Z/VT/PT/DRZ1/7A/FFxx),
  ZT 状态位展开为 `ZT_交流停电` 等布尔键(表58 中文名);`time` = 观测时间(北京时区),
  规则链解析为 `metadata.ts`,**断网补传时间不失真**
- 一次报文含多个观测时间(小时报/均匀时段展开)会拆成多条消息,各自带 time
- 链路维持(2F)发空 `points` 帧:仅刷网关 `lastUploadTime`,不产生遥测
- 图片元数据/人工置数原文走 `v1/devices/me/attributes`

### 物模型 Excel (兰溪水亭模板)

`tools/gen_thing_model.py` 按「兰溪水亭」模板整理协议物模型
(onboard-tool `parse_wu_model` 实测解析通过):

```bash
uv run --project ~/Project/water-spirit/onboard-tool \
  python tools/gen_thing_model.py            # 默认生成 configs/sl651-thing-model.xlsx
```

结构:
- **物模型**: 协议全部 13 类设备(雨量计/水位计/流量计/蒸发器/气象站/墒情仪/
  地下水监测仪/水质仪/闸门/水泵/水表/水压计/遥测终端)及各类型点位字典
  (字段名=规约源码标识符, 142 点)
- **设施类型页** (每设施类型一页): 设备类型/具体设备/属性/中文名/寄存器地址
  —— SL651 无寄存器概念, 该列留空
- **设施清单**: 设施及其设施类型; 默认生成设施类型与名称均为「薄天雅安」的设施,
  含协议支持的全部设备类型, 每类型一台具体设备


规则链的两种键映射方式均可承载:

1. **keyMapping 模式**(上述 Excel 自动生成):网关设备服务端属性
   `keySplitter='|'`、`dataPath=points`,`keyMapping` 按完整键映射
2. **PLC 平铺模式**(手动):不配 keyMapping,网关设备命名 `{设施名}-PLC网关`
   (链脚本按此后缀推导 place),键 `设备名:点位` 自动路由到子设备

> 注: 若希望网关命名为 `-SL651网关` 后缀,需在 onboard-tool 中克隆
> plc 链模板并调整拆分脚本的 suffix 判定(后续工作)。

## 配置说明

见 `configs/bridge.example.toml` 内注释。要点:

| 段 | 作用 |
|---|---|
| `listen` | TCP/UDP 监听、空闲超时、缓冲上限 |
| `center` | 中心站地址/密码及校验开关 |
| `ack` | 应答开关与 ETX 后终止符(EOT 释放 / ESC 保持在线 10 分钟) |
| `m3` / `dedup` | 分包重组超时 / 流水号去重窗口 |
| `stations` | 站码→设备别名(缺省站码原样) |
| `tb` | 转发模式(`mqtt`/`log`)、MQTT 地址、凭据、QoS、断线缓冲 |
| `payload` | 载荷字段名(与规则链 ss_* 属性对应) |
| `images` | 图片落盘目录 |

## 测试

```bash
cargo test   # 协议编解码 / golden 交叉验证 / 正文解析 / 管道 / M3 / e2e(TCP+UDP+图片)
```

golden 用例与独立 Python 实现逐字节交叉验证,防止编解码"自洽但错误"。

## 已知简化

- HEX/BCD 编码的均匀时段报(31H)按要素对解析,不做步长展开(实际部署该
  功能码几乎都走 ASCII 编码)
- M3 NAK 应答正文流水号填 0(坏包正文不可读,站端按终止符+坏包序号处理)
- 下行查询/参数设置(M4: 37H~51H)暂未实现(TB RPC 通道后续接入)

## 目录结构

```
protocol/   SL651 编解码纯库 (帧/正文/标识符表/应答, 无 IO, 可独立复用)
bridge/     网关主程序 + sl651-sim 模拟器
  src/server.rs   TCP/UDP 接入
  src/session.rs  站点注册/去重/M3 重组/应答/图片
  src/pipeline.rs 正文 -> TB 消息
  src/mqtt.rs     rumqttc 发布器 (断线缓冲)
configs/    配置示例
deploy/     Dockerfile / docker-compose / systemd
```
