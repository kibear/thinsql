//! 结果集导出。纯函数，不碰文件系统，便于单测。
//!
//! 三种格式各有取舍：CSV 给人看、JSON 给程序吃、INSERT 用来搬到另一个库。

use crate::model::ResultColumn;
use crate::value::{EscapeStyle, Value, sql_literal};

/// CSV：字段含逗号/引号/换行时用双引号包裹，内部引号翻倍。
/// NULL 导出为空串（CSV 没有 null 概念），这是导入端最容易出错的点，README 需注明。
pub fn to_csv(columns: &[ResultColumn], rows: &[Vec<Value>]) -> String {
    let mut out = String::new();
    out.push_str(
        &columns
            .iter()
            .map(|c| csv_field(&c.name))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');

    for row in rows {
        let line = row
            .iter()
            .map(|v| match v {
                Value::Null => String::new(),
                other => csv_field(&other.display()),
            })
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

/// JSON：数值保持数字、空值保持 null，其余按字符串。
pub fn to_json(columns: &[ResultColumn], rows: &[Vec<Value>]) -> String {
    let mut out = String::from("[\n");
    for (row_ix, row) in rows.iter().enumerate() {
        out.push_str("  {");
        for (col_ix, column) in columns.iter().enumerate() {
            if col_ix > 0 {
                out.push_str(", ");
            }
            out.push_str(&json_string(&column.name));
            out.push_str(": ");
            out.push_str(&json_value(row.get(col_ix).unwrap_or(&Value::Null)));
        }
        out.push('}');
        if row_ix + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]\n");
    out
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(v) => v.to_string(),
        Value::Int(v) => v.to_string(),
        Value::UInt(v) => v.to_string(),
        Value::Float(v) if v.is_finite() => v.to_string(),
        Value::Decimal(v) if v.parse::<f64>().is_ok() => v.clone(),
        Value::Bytes(bytes) => json_string(&hex(bytes)),
        other => json_string(&other.display()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// INSERT 语句，用于把结果搬到另一个库。表名由调用方给出（含引用符）。
pub fn to_insert(
    table: &str,
    columns: &[ResultColumn],
    rows: &[Vec<Value>],
    escape: EscapeStyle,
) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = columns
        .iter()
        .map(|c| c.name.clone())
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = format!("INSERT INTO {table} ({cols}) VALUES\n");
    for (row_ix, row) in rows.iter().enumerate() {
        let values = row
            .iter()
            .map(|v| sql_literal(v, escape))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("  ({values})"));
        if row_ix + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(";\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns() -> Vec<ResultColumn> {
        ["id", "name", "note"]
            .iter()
            .map(|n| ResultColumn {
                name: (*n).to_string(),
                type_name: "text".into(),
            })
            .collect()
    }

    fn rows() -> Vec<Vec<Value>> {
        vec![
            vec![Value::Int(1), Value::Text("alpha".into()), Value::Null],
            vec![
                Value::Int(2),
                Value::Text("含,逗号\"引号".into()),
                Value::Text("多行\n文本".into()),
            ],
        ]
    }

    #[test]
    fn csv_quotes_only_when_needed() {
        let csv = to_csv(&columns(), &rows());
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "id,name,note");
        assert_eq!(lines[1], "1,alpha,", "NULL 应导出为空字段");
        assert_eq!(lines[2], "2,\"含,逗号\"\"引号\",\"多行");
        assert!(csv.contains("\n文本\""), "换行字段必须被引号包裹");
    }

    #[test]
    fn json_keeps_types_and_nulls() {
        let json = to_json(&columns(), &rows());
        assert!(json.contains("\"id\": 1"));
        assert!(json.contains("\"note\": null"));
        assert!(json.contains("\"name\": \"含,逗号\\\"引号\""));
    }

    #[test]
    fn json_escapes_control_chars() {
        let cols = vec![ResultColumn {
            name: "c".into(),
            type_name: "text".into(),
        }];
        let json = to_json(&cols, &[vec![Value::Text("a\tb\nc".into())]]);
        assert!(json.contains("a\\tb\\nc"));
    }

    #[test]
    fn insert_renders_literals_and_handles_empty() {
        let sql = to_insert("`db`.`t`", &columns(), &rows(), EscapeStyle::Backslash);
        assert!(sql.starts_with("INSERT INTO `db`.`t` (id, name, note) VALUES"));
        assert!(sql.contains("(1, 'alpha', NULL)"));
        assert!(sql.contains("'含,逗号\"引号'"));
        assert!(to_insert("t", &columns(), &[], EscapeStyle::Backslash).is_empty());
    }
}
