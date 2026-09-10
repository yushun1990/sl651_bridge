//! CRC-16/MODBUS。
//!
//! 规约表11: 校验码前所有字节的 CRC 校验, 生成多项式 X^16+X^15+X^2+1,
//! 高位字节在前, 低位字节在后。即 CRC-16/MODBUS 参数:
//! 多项式 0x8005 (反射 0xA001), 初值 0xFFFF, 无输出异或。

/// 计算 CRC-16/MODBUS。
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // CRC-16/MODBUS 标准校验值
        assert_eq!(crc16(b"123456789"), 0x4B37);
        assert_eq!(crc16(b""), 0xFFFF);
        // 参考实现逐字节: 0x01 -> 0x807E
        assert_eq!(crc16(&[0x01]), 0x807E);
    }
}
