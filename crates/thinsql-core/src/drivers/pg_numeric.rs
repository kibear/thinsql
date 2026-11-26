//! PostgreSQL NUMERIC 的二进制解码。
//!
//! postgres-types 没有为 NUMERIC 实现任何 Rust 类型（需要额外 feature），
//! 而不做这一步，所有金额/小数列都会读不出来——这是不可接受的。
//!
//! 二进制格式（网络字节序）：
//! ```text
//! int16 ndigits      有效数字组数
//! int16 weight       第一组的权重（以 10000 为底）
//! uint16 sign        0x0000 正、0x4000 负、0xC000 NaN
//! uint16 dscale      小数点后的十进制位数
//! int16 digits[]     每组 0..=9999
//! ```

/// 解成十进制字符串，保留 dscale 指定的尾随零（12.30 不会变成 12.3）。
pub fn decode_numeric(raw: &[u8]) -> Option<String> {
    if raw.len() < 8 {
        return None;
    }
    let ndigits = i16::from_be_bytes([raw[0], raw[1]]);
    let weight = i16::from_be_bytes([raw[2], raw[3]]);
    let sign = u16::from_be_bytes([raw[4], raw[5]]);
    let dscale = u16::from_be_bytes([raw[6], raw[7]]) as usize;

    if sign == 0xC000 {
        return Some("NaN".to_string());
    }
    if sign != 0x0000 && sign != 0x4000 {
        return None;
    }
    if ndigits < 0 {
        return None;
    }

    let mut groups = Vec::with_capacity(ndigits as usize);
    for i in 0..ndigits as usize {
        let start = 8 + i * 2;
        let bytes = raw.get(start..start + 2)?;
        groups.push(u16::from_be_bytes([bytes[0], bytes[1]]) as i32);
    }

    let base: i64 = 10_000;
    // 整数部分：weight 表示首组的权位，从 weight 递减到 -1
    let mut int_part = String::new();
    if weight >= 0 {
        for exp in (0..=weight).rev() {
            let idx = weight - exp;
            let digit = groups.get(idx as usize).copied().unwrap_or(0);
            if int_part.is_empty() {
                int_part.push_str(&digit.to_string());
            } else {
                int_part.push_str(&format!("{digit:04}"));
            }
        }
    }
    if int_part.is_empty() {
        int_part.push('0');
    }

    // 小数部分：不足 dscale 位要补零
    let mut frac = String::new();
    let mut exp: i64 = -1;
    while frac.len() < dscale {
        let idx = weight - exp as i16;
        let digit = if idx >= 0 {
            groups.get(idx as usize).copied().unwrap_or(0)
        } else {
            0
        };
        frac.push_str(&format!("{digit:04}"));
        exp -= 1;
        if exp < -(dscale as i64) - 8 {
            break;
        }
    }
    frac.truncate(dscale);
    let _ = base;

    let negative = sign == 0x4000;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&int_part);
    if dscale > 0 {
        out.push('.');
        out.push_str(&frac);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(weight: i16, sign: u16, dscale: u16, digits: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(digits.len() as i16).to_be_bytes());
        out.extend_from_slice(&weight.to_be_bytes());
        out.extend_from_slice(&sign.to_be_bytes());
        out.extend_from_slice(&dscale.to_be_bytes());
        for d in digits {
            out.extend_from_slice(&d.to_be_bytes());
        }
        out
    }

    #[test]
    fn simple_integer() {
        // 12345 => digits [1, 2345], weight 1, scale 0
        let raw = build(1, 0x0000, 0, &[1, 2345]);
        assert_eq!(decode_numeric(&raw).as_deref(), Some("12345"));
    }

    #[test]
    fn keeps_trailing_zeros_from_dscale() {
        // 12.30 => digits [12, 3000], weight 0, scale 2
        let raw = build(0, 0x0000, 2, &[12, 3000]);
        assert_eq!(decode_numeric(&raw).as_deref(), Some("12.30"));
    }

    #[test]
    fn negative_value() {
        let raw = build(0, 0x4000, 2, &[12, 3000]);
        assert_eq!(decode_numeric(&raw).as_deref(), Some("-12.30"));
    }

    #[test]
    fn zero_and_small_fraction() {
        // 0.05 => weight -1, digits [500]
        let raw = build(-1, 0x0000, 2, &[500]);
        assert_eq!(decode_numeric(&raw).as_deref(), Some("0.05"));
    }

    #[test]
    fn nan_is_preserved() {
        let raw = build(0, 0xC000, 0, &[]);
        assert_eq!(decode_numeric(&raw).as_deref(), Some("NaN"));
    }

    #[test]
    fn truncated_input_is_rejected() {
        assert!(decode_numeric(&[0, 1]).is_none());
    }
}
