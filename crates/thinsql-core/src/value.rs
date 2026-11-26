//! 统一值模型：所有驱动把原生类型收敛到这里，UI 只认识这一套。

use std::fmt;

/// 单元格的统一表示。
///
/// 定点数用字符串保存原始文本，避免 f64 精度损失（金额类字段很常见）。
/// 时间不做归一化，保留数据库给出的原文 + 时区描述，避免往返丢精度。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    DateTime { raw: String, tz: Option<String> },
    Json(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// 供网格展示的紧凑文本。二进制只展示长度与前若干字节的十六进制。
    pub fn display(&self) -> String {
        match self {
            Self::Null => "NULL".to_string(),
            Self::Bool(v) => v.to_string(),
            Self::Int(v) => v.to_string(),
            Self::UInt(v) => v.to_string(),
            Self::Float(v) => {
                if v.fract() == 0.0 && v.is_finite() {
                    format!("{v:.1}")
                } else {
                    v.to_string()
                }
            }
            Self::Decimal(v) => v.clone(),
            Self::Text(v) => v.clone(),
            Self::Bytes(v) => {
                const PREVIEW: usize = 16;
                let head: String = v.iter().take(PREVIEW).map(|b| format!("{b:02x}")).collect();
                if v.len() > PREVIEW {
                    format!("0x{head}… ({} bytes)", v.len())
                } else {
                    format!("0x{head}")
                }
            }
            Self::DateTime { raw, tz } => match tz {
                Some(tz) => format!("{raw} {tz}"),
                None => raw.clone(),
            },
            Self::Json(v) => v.clone(),
        }
    }

    /// 估算单元格的字符宽度，供网格自适应列宽使用。
    pub fn display_width(&self) -> usize {
        match self {
            Self::Bytes(v) => 40.min(2 + v.len() * 2),
            other => other.display().chars().count(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// 字符串转义风格：MySQL 默认把反斜杠当转义符，PostgreSQL 标准字符串不认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeStyle {
    /// SQL 标准：单引号翻倍即可，反斜杠是普通字符
    Standard,
    /// MySQL：反斜杠是转义符，需要一起处理
    Backslash,
}

/// 把值渲染成可安全嵌入 SQL 的字面量。
///
/// 这是编辑功能的正确性基础：任何漏转义都会导致语句错位甚至注入，
/// 因此这里对每种类型都显式处理，不依赖驱动的占位符能力。
pub fn sql_literal(value: &Value, style: EscapeStyle) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(v) => {
            if *v {
                "TRUE".into()
            } else {
                "FALSE".into()
            }
        }
        Value::Int(v) => v.to_string(),
        Value::UInt(v) => v.to_string(),
        Value::Float(v) => {
            // NaN/Inf 不是合法 SQL 数字字面量，按文本处理
            if v.is_finite() {
                v.to_string()
            } else {
                quote_str(&v.to_string(), style)
            }
        }
        Value::Decimal(v) => {
            if v.chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
                && v.chars().any(|c| c.is_ascii_digit())
            {
                v.clone()
            } else {
                quote_str(v, style)
            }
        }
        Value::Text(v) | Value::Json(v) => quote_str(v, style),
        Value::Bytes(v) => {
            let hex: String = v.iter().map(|b| format!("{b:02x}")).collect();
            match style {
                EscapeStyle::Standard => format!("'\\x{hex}'::bytea"),
                EscapeStyle::Backslash => format!("X'{hex}'"),
            }
        }
        Value::DateTime { raw, .. } => quote_str(raw, style),
    }
}

fn quote_str(s: &str, style: EscapeStyle) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' if style == EscapeStyle::Backslash => out.push_str("\\\\"),
            '\0' if style == EscapeStyle::Backslash => out.push_str("\\0"),
            '\n' if style == EscapeStyle::Backslash => out.push_str("\\n"),
            '\r' if style == EscapeStyle::Backslash => out.push_str("\\r"),
            '\u{1a}' if style == EscapeStyle::Backslash => out.push_str("\\Z"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_numbers_are_bare_literals() {
        assert_eq!(sql_literal(&Value::Null, EscapeStyle::Standard), "NULL");
        assert_eq!(
            sql_literal(&Value::Bool(true), EscapeStyle::Standard),
            "TRUE"
        );
        assert_eq!(sql_literal(&Value::Int(-42), EscapeStyle::Standard), "-42");
        assert_eq!(
            sql_literal(&Value::Decimal("12.30".into()), EscapeStyle::Standard),
            "12.30"
        );
    }

    #[test]
    fn single_quotes_are_doubled_in_both_dialects() {
        let v = Value::Text("O'Brien".into());
        assert_eq!(sql_literal(&v, EscapeStyle::Standard), "'O''Brien'");
        assert_eq!(sql_literal(&v, EscapeStyle::Backslash), "'O''Brien'");
    }

    #[test]
    fn backslash_is_escaped_only_for_mysql_style() {
        let v = Value::Text("C:\\temp".into());
        assert_eq!(
            sql_literal(&v, EscapeStyle::Standard),
            "'C:\\temp'",
            "标准方言里反斜杠是普通字符，不能动"
        );
        assert_eq!(sql_literal(&v, EscapeStyle::Backslash), "'C:\\\\temp'");
    }

    #[test]
    fn mysql_special_control_chars_are_escaped() {
        let v = Value::Text("a\nb\rc\0d".into());
        assert_eq!(sql_literal(&v, EscapeStyle::Backslash), "'a\\nb\\rc\\0d'");
    }

    #[test]
    fn non_numeric_decimal_falls_back_to_text() {
        let v = Value::Decimal("1 UNION SELECT 1".into());
        assert_eq!(sql_literal(&v, EscapeStyle::Standard), "'1 UNION SELECT 1'");
    }

    #[test]
    fn bytes_render_per_dialect() {
        let v = Value::Bytes(vec![0xde, 0xad]);
        assert_eq!(sql_literal(&v, EscapeStyle::Standard), "'\\xdead'::bytea");
        assert_eq!(sql_literal(&v, EscapeStyle::Backslash), "X'dead'");
    }

    #[test]
    fn nan_is_rendered_as_quoted_text_not_number() {
        let lit = sql_literal(&Value::Float(f64::NAN), EscapeStyle::Standard);
        assert!(
            lit.starts_with('\''),
            "NaN 不能作为裸数字输出，否则语句非法"
        );
    }

    #[test]
    fn bytes_display_is_truncated_with_length() {
        let v = Value::Bytes(vec![0u8; 40]);
        let shown = v.display();
        assert!(shown.contains("40 bytes"));
    }
}

/// 把界面输入框里的文本按**列声明类型**转成 `Value`。
///
/// 不做这一层的话，用户在 INT 列里输入 123 会被当成字符串 '123' 写回，
/// 虽然 MySQL 通常能隐式转换，但严格的库/严格模式下会直接报错。
pub fn value_from_input(type_name: &str, text: &str) -> Value {
    let t = type_name.to_ascii_lowercase();

    // 数值列：解析失败就原样当文本，交给数据库报错，不要静默改成 0
    let numeric = [
        "int", "longlong", "short", "tiny", "medium", "year", "serial",
    ];
    if numeric.iter().any(|k| t.contains(k)) && !t.contains("point") {
        if let Ok(v) = text.trim().parse::<i64>() {
            return Value::Int(v);
        }
        if let Ok(v) = text.trim().parse::<u64>() {
            return Value::UInt(v);
        }
        return Value::Text(text.to_string());
    }

    if t.contains("decimal") || t.contains("numeric") {
        return Value::Decimal(text.trim().to_string());
    }

    if t.contains("double") || t.contains("float") || t.contains("real") {
        if let Ok(v) = text.trim().parse::<f64>() {
            return Value::Float(v);
        }
        return Value::Text(text.to_string());
    }

    if t.contains("bool") || t == "bit(1)" {
        match text.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "t" => return Value::Bool(true),
            "0" | "false" | "f" => return Value::Bool(false),
            _ => {}
        }
    }

    Value::Text(text.to_string())
}

#[cfg(test)]
mod input_tests {
    use super::*;

    #[test]
    fn integer_columns_become_int() {
        assert_eq!(value_from_input("bigint", "42"), Value::Int(42));
        assert_eq!(value_from_input("int(11)", "-7"), Value::Int(-7));
    }

    #[test]
    fn unparsable_number_keeps_text_for_database_to_reject() {
        assert_eq!(
            value_from_input("int", "abc"),
            Value::Text("abc".into()),
            "不能静默转成 0，应该让数据库报错"
        );
    }

    #[test]
    fn decimal_keeps_raw_text() {
        assert_eq!(
            value_from_input("decimal(10,2)", " 12.30 "),
            Value::Decimal("12.30".into())
        );
    }

    #[test]
    fn float_columns_become_float() {
        assert_eq!(value_from_input("double", "1.5"), Value::Float(1.5));
    }

    #[test]
    fn boolean_accepts_common_literals() {
        assert_eq!(value_from_input("boolean", "true"), Value::Bool(true));
        assert_eq!(value_from_input("boolean", "0"), Value::Bool(false));
    }

    #[test]
    fn text_columns_stay_text_including_empty() {
        assert_eq!(
            value_from_input("varchar(64)", ""),
            Value::Text(String::new())
        );
    }
}
