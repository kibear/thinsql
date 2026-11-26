//! 行级改动的 SQL 生成。
//!
//! 这里的输出同时用于两处：界面上"提交前预览"，以及驱动真正执行的语句。
//! 两者必须来自同一个函数——否则用户确认的和实际跑的不是一回事，
//! 这是数据编辑功能里最容易出事的偏差。

use crate::model::{DriverKind, ResultColumn, RowChange, StructureChange, TableRef};
use crate::value::{EscapeStyle, Value, sql_literal};

fn quote_ident(name: &str, quote: char) -> String {
    format!(
        "{quote}{}{quote}",
        name.replace(quote, &format!("{quote}{quote}"))
    )
}

fn quote_table(kind: DriverKind, table: &TableRef) -> String {
    let q = kind.identifier_quote();
    match &table.schema {
        Some(schema) => format!("{}.{}", quote_ident(schema, q), quote_ident(&table.name, q)),
        None => quote_ident(&table.name, q),
    }
}

fn escape_style(kind: DriverKind) -> EscapeStyle {
    match kind {
        DriverKind::MySql => EscapeStyle::Backslash,
        DriverKind::Postgres | DriverKind::Oracle => EscapeStyle::Standard,
    }
}

/// 生成一条 UPDATE。`LIMIT 1` 是 MySQL 的语法糖，用来防止主键判断有误时
/// 一次改掉整张表；PostgreSQL/Oracle 不支持该子句，因此按方言省略。
pub fn build_sql(kind: DriverKind, table: &TableRef, change: &RowChange) -> String {
    match change {
        RowChange::Update {
            key,
            column,
            new_value,
        } => {
            let q = kind.identifier_quote();
            let escape = escape_style(kind);
            let target = quote_table(kind, table);
            let suffix = match kind {
                DriverKind::MySql => " LIMIT 1",
                _ => "",
            };
            format!(
                "UPDATE {target} SET {} = {} WHERE {}{suffix}",
                quote_ident(column, q),
                sql_literal(new_value, escape),
                where_clause(kind, key)
            )
        }
        RowChange::Delete { key } => build_delete(kind, table, key),
    }
}

/// 生成一条 DELETE。定位条件与 UPDATE 完全同构（主键等值、NULL 走 IS NULL），
/// 并同样带方言 LIMIT 保护。
pub fn build_delete(kind: DriverKind, table: &TableRef, key: &[(String, Value)]) -> String {
    let target = quote_table(kind, table);
    let suffix = match kind {
        DriverKind::MySql => " LIMIT 1",
        _ => "",
    };
    format!("DELETE FROM {target} WHERE {}{suffix}", where_clause(kind, key))
}

fn where_clause(kind: DriverKind, key: &[(String, Value)]) -> String {
    let q = kind.identifier_quote();
    let escape = escape_style(kind);
    let mut predicates = Vec::with_capacity(key.len());
    for (column, value) in key {
        let quoted = quote_ident(column, q);
        if matches!(value, Value::Null) {
            // NULL 不能用等号比较，必须用 IS NULL
            predicates.push(format!("{quoted} IS NULL"));
        } else {
            predicates.push(format!("{quoted} = {}", sql_literal(value, escape)));
        }
    }
    predicates.join(" AND ")
}

/// 按主键列名从结果行里取出定位值。
///
/// 单独抽出来是因为这里的下标映射最容易错：结果集的列顺序由 SQL 决定，
/// 未必和表结构一致，所以必须按列名查下标，不能假设顺序。
pub fn locate_key(
    primary_key: &[String],
    columns: &[ResultColumn],
    row: &[Value],
) -> Result<Vec<(String, Value)>, String> {
    if primary_key.is_empty() {
        return Err("该表没有主键，无法安全定位要修改的行".into());
    }
    let mut key = Vec::with_capacity(primary_key.len());
    for pk in primary_key {
        let idx = columns
            .iter()
            .position(|c| &c.name == pk)
            .ok_or_else(|| format!("结果集里找不到主键列 {pk}，请显式查询该列"))?;
        let value = row.get(idx).cloned().unwrap_or(Value::Null);
        if value.is_null() {
            return Err("主键值为 NULL，无法定位该行".into());
        }
        key.push((pk.clone(), value));
    }
    Ok(key)
}

/// 逐条生成整批改动的 SQL（UPDATE 与 DELETE 混排，顺序即执行顺序）。
pub fn build_sqls(kind: DriverKind, table: &TableRef, changes: &[RowChange]) -> Vec<String> {
    changes
        .iter()
        .map(|change| build_sql(kind, table, change))
        .collect()
}

/// 结构变更的预览 SQL，与实际执行共用驱动里的同一生成器。
/// 尚未支持结构编辑的方言返回空列表，界面据此提示不支持。
pub fn build_structure_sql(
    kind: DriverKind,
    table: &TableRef,
    change: &StructureChange,
) -> Vec<String> {
    match kind {
        DriverKind::MySql => crate::drivers::mysql::build_mysql_alter(table, change),
        DriverKind::Postgres | DriverKind::Oracle => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> TableRef {
        TableRef::new(Some("db".into()), "users")
    }

    fn change(key: Vec<(&str, Value)>, column: &str, new_value: Value) -> RowChange {
        RowChange::Update {
            key: key.into_iter().map(|(c, v)| (c.to_string(), v)).collect(),
            column: column.to_string(),
            new_value,
        }
    }

    fn delete(key: Vec<(&str, Value)>) -> RowChange {
        RowChange::Delete {
            key: key.into_iter().map(|(c, v)| (c.to_string(), v)).collect(),
        }
    }

    #[test]
    fn mysql_update_quotes_and_limits() {
        let sql = build_sql(
            DriverKind::MySql,
            &table(),
            &change(
                vec![("id", Value::Int(7))],
                "name",
                Value::Text("张三".into()),
            ),
        );
        assert_eq!(
            sql,
            "UPDATE `db`.`users` SET `name` = '张三' WHERE `id` = 7 LIMIT 1"
        );
    }

    #[test]
    fn postgres_update_has_no_limit_clause() {
        let sql = build_sql(
            DriverKind::Postgres,
            &table(),
            &change(vec![("id", Value::Int(7))], "name", Value::Text("x".into())),
        );
        assert_eq!(
            sql,
            "UPDATE \"db\".\"users\" SET \"name\" = 'x' WHERE \"id\" = 7"
        );
        assert!(!sql.contains("LIMIT"), "PostgreSQL 不支持 UPDATE ... LIMIT");
    }

    #[test]
    fn null_key_uses_is_null() {
        let sql = build_sql(
            DriverKind::MySql,
            &table(),
            &change(vec![("code", Value::Null)], "flag", Value::Bool(true)),
        );
        assert!(sql.contains("`code` IS NULL"), "实际: {sql}");
        assert!(!sql.contains("= NULL"), "NULL 不能用等号");
    }

    #[test]
    fn composite_key_is_joined_with_and() {
        let sql = build_sql(
            DriverKind::MySql,
            &table(),
            &change(
                vec![("a", Value::Int(1)), ("b", Value::Text("x".into()))],
                "c",
                Value::Null,
            ),
        );
        assert!(sql.contains("WHERE `a` = 1 AND `b` = 'x'"), "实际: {sql}");
        assert!(sql.contains("`c` = NULL"), "写入 NULL 是合法的赋值");
    }

    #[test]
    fn embedded_quotes_are_escaped_in_both_dialects() {
        let weird = TableRef::new(Some("d`b".into()), "t");
        let sql = build_sql(
            DriverKind::MySql,
            &weird,
            &change(
                vec![("id", Value::Int(1))],
                "n",
                Value::Text("a'b\\c".into()),
            ),
        );
        assert!(sql.contains("`d``b`"), "标识符里的反引号要翻倍");
        assert!(sql.contains("'a''b\\\\c'"), "字符串引号与反斜杠都要转义");
    }

    #[test]
    fn bulk_build_keeps_order() {
        let changes = vec![
            change(vec![("id", Value::Int(1))], "a", Value::Int(1)),
            change(vec![("id", Value::Int(2))], "a", Value::Int(2)),
        ];
        let sqls = build_sqls(DriverKind::MySql, &table(), &changes);
        assert_eq!(sqls.len(), 2);
        assert!(sqls[0].contains("`id` = 1"));
        assert!(sqls[1].contains("`id` = 2"));
    }

    #[test]
    fn mysql_delete_quotes_and_limits() {
        let sql = build_delete(
            DriverKind::MySql,
            &table(),
            &[("id".to_string(), Value::Int(7))],
        );
        assert_eq!(
            sql,
            "DELETE FROM `db`.`users` WHERE `id` = 7 LIMIT 1",
            "实际: {sql}"
        );
    }

    #[test]
    fn postgres_delete_has_no_limit_clause() {
        let sql = build_delete(
            DriverKind::Postgres,
            &table(),
            &[("id".to_string(), Value::Int(7))],
        );
        assert_eq!(sql, "DELETE FROM \"db\".\"users\" WHERE \"id\" = 7", "实际: {sql}");
        assert!(!sql.contains("LIMIT"), "PostgreSQL 不支持 DELETE ... LIMIT");
    }

    #[test]
    fn delete_null_key_uses_is_null() {
        let sql = build_delete(
            DriverKind::MySql,
            &table(),
            &[("code".to_string(), Value::Null)],
        );
        assert!(sql.contains("`code` IS NULL"), "实际: {sql}");
        assert!(!sql.contains("= NULL"), "NULL 不能用等号");
    }

    #[test]
    fn mixed_changes_keep_order_and_types() {
        let changes = vec![
            change(vec![("id", Value::Int(1))], "name", Value::Text("x".into())),
            delete(vec![("id", Value::Int(2))]),
            delete(vec![("id", Value::Int(3))]),
        ];
        let sqls = build_sqls(DriverKind::MySql, &table(), &changes);
        assert_eq!(sqls.len(), 3);
        assert!(sqls[0].starts_with("UPDATE "), "实际: {}", sqls[0]);
        assert!(sqls[1].starts_with("DELETE FROM "), "实际: {}", sqls[1]);
        assert!(sqls[2].contains("`id` = 3"), "实际: {}", sqls[2]);
    }

    #[test]
    fn change_key_accessor_covers_both_variants() {
        assert_eq!(
            change(vec![("id", Value::Int(1))], "a", Value::Int(1)).key(),
            &[("id".to_string(), Value::Int(1))]
        );
        assert_eq!(
            delete(vec![("id", Value::Int(1))]).key(),
            &[("id".to_string(), Value::Int(1))]
        );
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    fn columns(names: &[&str]) -> Vec<ResultColumn> {
        names
            .iter()
            .map(|n| ResultColumn {
                name: (*n).to_string(),
                type_name: "int".into(),
            })
            .collect()
    }

    #[test]
    fn key_is_located_by_name_not_position() {
        // 结果集里 name 在前、id 在后，主键仍是 id
        let cols = columns(&["name", "id"]);
        let row = vec![Value::Text("x".into()), Value::Int(9)];
        let key = locate_key(&["id".to_string()], &cols, &row).unwrap();
        assert_eq!(key, vec![("id".to_string(), Value::Int(9))]);
    }

    #[test]
    fn missing_key_column_is_reported() {
        let cols = columns(&["name"]);
        let err = locate_key(&["id".to_string()], &cols, &[Value::Text("x".into())]).unwrap_err();
        assert!(err.contains("找不到主键列 id"), "实际: {err}");
    }

    #[test]
    fn null_key_is_rejected() {
        let cols = columns(&["id"]);
        let err = locate_key(&["id".to_string()], &cols, &[Value::Null]).unwrap_err();
        assert!(err.contains("NULL"), "实际: {err}");
    }

    #[test]
    fn empty_primary_key_is_rejected() {
        let cols = columns(&["id"]);
        assert!(locate_key(&[], &cols, &[Value::Int(1)]).is_err());
    }

    #[test]
    fn composite_key_keeps_declared_order() {
        let cols = columns(&["b", "a"]);
        let row = vec![Value::Int(2), Value::Int(1)];
        let key = locate_key(&["a".to_string(), "b".to_string()], &cols, &row).unwrap();
        assert_eq!(key[0].0, "a");
        assert_eq!(key[1].0, "b");
    }
}
