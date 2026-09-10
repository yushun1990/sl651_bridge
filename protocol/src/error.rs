//! 错误类型。

/// 库级 Result。
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("数据不足: 需 {need} 字节, 实际 {have} 字节")]
    TooShort { need: usize, have: usize },

    #[error("无效的帧起始/报文起始符: 0x{0:02X}")]
    BadStart(u8),

    #[error("无效的报文结束符: 0x{0:02X}")]
    BadEnd(u8),

    #[error("无效的上下行标识: 0x{0:02X}")]
    BadDirection(u8),

    #[error("无效的报文正文长度: {0}")]
    BadLength(usize),

    #[error("CRC 校验失败: 报文 {got:04X}, 计算 {expect:04X}")]
    CrcMismatch { got: u16, expect: u16 },

    #[error("无效的 ASCII-HEX 字符: {0:?}")]
    BadHex(char),

    #[error("无效的 BCD 字符: {0:?}")]
    BadBcd(char),

    #[error("报文正文格式错误: {0}")]
    BadBody(String),

    #[error("无效的站址(应为10位BCD数字): {0:?}")]
    BadStationAddr(String),
}
