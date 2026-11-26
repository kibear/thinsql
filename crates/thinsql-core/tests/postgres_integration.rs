//! PostgreSQL 驱动的端到端验证。
//!
//! ```bash
//! THINSQL_TEST_PG=1 THINSQL_TEST_PG_PASS=root \
//!   cargo test -p thinsql-core --test postgres_integration -- --nocapture
//! ```
//!
//! 只使用独立 schema `thinsql_it`，结束后整体 DROP CASCADE，不碰既有数据。

use std::sync::atomic::AtomicBool;

use thinsql_core::edit::{build_sql, locate_key};
use thinsql_core::model::{ConnectionConfig, DriverKind, Page, RowChange, TableRef};
use thinsql_core::provider::{CancelToken, DatabaseProvider, Registry};
use thinsql_core::{ErrorKind, Value};

const SCHEMA: &str = "thinsql_it";

fn test_config() -> Option<(ConnectionConfig, String)> {
    if std::env::var("THINSQL_TEST_PG").ok().as_deref() != Some("1") {
        eprintln!("跳过：未设置 THINSQL_TEST_PG=1");
        return None;
    }
    let mut cfg = ConnectionConfig::new("pg-it", "pg-integration", DriverKind::Postgres);
    cfg.host = std::env::var("THINSQL_TEST_PG_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    cfg.port = std::env::var("THINSQL_TEST_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5432);
    cfg.user = std::env::var("THINSQL_TEST_PG_USER").unwrap_or_else(|_| "root".into());
    cfg.database = Some(std::env::var("THINSQL_TEST_PG_DB").unwrap_or_else(|_| "testdb".into()));
    cfg.read_only = false;
    let pass = std::env::var("THINSQL_TEST_PG_PASS").unwrap_or_else(|_| "root".into());
    Some((cfg, pass))
}

fn token() -> CancelToken {
    std::sync::Arc::new(AtomicBool::new(false))
}

fn run(provider: &mut Box<dyn DatabaseProvider>, sql: &str) -> thinsql_core::QueryResult {
    run_with_page(provider, sql, Page::first())
}

fn run_with_page(
    provider: &mut Box<dyn DatabaseProvider>,
    sql: &str,
    page: Page,
) -> thinsql_core::QueryResult {
    provider
        .query(sql, page, &token())
        .unwrap_or_else(|err| panic!("执行失败 [{sql}]: {} / {}", err.message, err.hint()))
}

#[test]
fn postgres_end_to_end() {
    let Some((cfg, password)) = test_config() else {
        return;
    };

    let mut provider = Registry::create(DriverKind::Postgres).expect("创建驱动");

    // 认证失败必须归类为 Auth
    let err = provider
        .connect(&cfg, Some("definitely-wrong"))
        .expect_err("错误密码不应成功");
    assert_eq!(
        err.kind,
        ErrorKind::Auth,
        "实际: {:?} {}",
        err.kind,
        err.message
    );

    provider.connect(&cfg, Some(&password)).expect("连接失败");
    provider.ping().expect("ping 失败");

    run(
        &mut provider,
        &format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"),
    );
    run(&mut provider, &format!("CREATE SCHEMA {SCHEMA}"));
    run(
        &mut provider,
        &format!(
            "CREATE TABLE {SCHEMA}.t (
                id serial PRIMARY KEY,
                name text NOT NULL,
                amount numeric(10,2) NULL,
                created timestamp NOT NULL DEFAULT '2024-01-02 03:04:05',
                payload bytea NULL,
                doc jsonb NULL
            )"
        ),
    );
    run(
        &mut provider,
        &format!(
            "CREATE TABLE {SCHEMA}.child (
                id serial PRIMARY KEY,
                parent_id int NOT NULL REFERENCES {SCHEMA}.t(id)
            )"
        ),
    );
    let inserted = run(
        &mut provider,
        &format!(
            "INSERT INTO {SCHEMA}.t (name, amount, payload, doc) VALUES
             ('alpha', 12.30, '\\xdeadbeef'::bytea, '{{\"k\":1}}'::jsonb),
             ('beta', NULL, NULL, NULL),
             ('gamma', 0.05, NULL, NULL)"
        ),
    );
    assert_eq!(inserted.affected_rows, Some(3), "INSERT 的影响行数");

    // 类型映射：定点数、时间、JSON、二进制
    let result = run(
        &mut provider,
        &format!("SELECT id, name, amount, created, payload, doc FROM {SCHEMA}.t ORDER BY id"),
    );
    assert_eq!(result.columns.len(), 6);
    assert_eq!(result.row_count(), 3);
    assert_eq!(result.rows[0][1], Value::Text("alpha".into()));
    assert_eq!(
        result.rows[0][2],
        Value::Decimal("12.30".into()),
        "NUMERIC 必须保留原始精度，实际 {:?}",
        result.rows[0][2]
    );
    assert!(
        matches!(&result.rows[0][3], Value::DateTime { raw, .. } if raw.starts_with("2024-01-02 03:04:05")),
        "timestamp 应映射为 DateTime，实际 {:?}",
        result.rows[0][3]
    );
    assert_eq!(
        result.rows[0][4],
        Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        "bytea 应映射为 Bytes"
    );
    assert!(
        matches!(&result.rows[0][5], Value::Json(v) if v.contains("\"k\"")),
        "jsonb 应映射为 Json，实际 {:?}",
        result.rows[0][5]
    );
    assert!(result.rows[1][2].is_null(), "NULL 要保留");

    // 分页
    let page = run_with_page(
        &mut provider,
        &format!("SELECT id FROM {SCHEMA}.t ORDER BY id"),
        Page::with_limit(2),
    );
    assert_eq!(page.row_count(), 2);
    assert!(page.has_more, "共 3 行，取 2 行必须报告还有更多");

    // 元数据
    let schemas = provider.list_schemas().expect("list_schemas");
    assert!(schemas.iter().any(|s| s.name == SCHEMA));

    let tables = provider.list_tables(Some(SCHEMA)).expect("list_tables");
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"t") && names.contains(&"child"),
        "表列表: {names:?}"
    );

    let table = TableRef::new(Some(SCHEMA.into()), "t");
    let columns = provider.list_columns(&table).expect("list_columns");
    let id_col = columns.iter().find(|c| c.name == "id").expect("id 列");
    assert!(id_col.is_primary_key, "id 应为主键");
    assert!(id_col.is_auto_increment, "serial 应识别为自增");
    let amount = columns.iter().find(|c| c.name == "amount").expect("amount");
    assert!(amount.nullable);

    let indexes = provider.list_indexes(&table).expect("list_indexes");
    assert!(
        indexes.iter().any(|i| i.name.ends_with("pkey") && i.unique),
        "应识别主键索引: {indexes:?}"
    );

    let fks = provider
        .list_foreign_keys(&TableRef::new(Some(SCHEMA.into()), "child"))
        .expect("list_foreign_keys");
    let fk = fks.first().expect("应有外键");
    assert_eq!(fk.ref_table, "t");
    assert_eq!(fk.columns, vec!["parent_id".to_string()]);

    // 行级编辑：预览与执行一致，且不波及无关行
    let pk: Vec<String> = columns
        .iter()
        .filter(|c| c.is_primary_key)
        .map(|c| c.name.clone())
        .collect();
    let rows = run(
        &mut provider,
        &format!("SELECT id, name, amount FROM {SCHEMA}.t ORDER BY id"),
    );
    let key1 = locate_key(&pk, &rows.columns, &rows.rows[0]).expect("定位第 1 行");
    let key3 = locate_key(&pk, &rows.columns, &rows.rows[2]).expect("定位第 3 行");

    let changes = vec![
        RowChange::Update {
            key: key1,
            column: "name".into(),
            new_value: Value::Text("改过的 alpha".into()),
        },
        RowChange::Update {
            key: key3,
            column: "amount".into(),
            new_value: Value::Decimal("88.88".into()),
        },
    ];
    let preview = build_sql(DriverKind::Postgres, &table, &changes[0]);
    assert!(
        !preview.contains("LIMIT"),
        "PostgreSQL 不应出现 LIMIT: {preview}"
    );

    let outcome = provider.apply_changes(&table, &changes).expect("提交改动");
    assert_eq!(outcome.affected, 2);

    let after = run(
        &mut provider,
        &format!("SELECT id, name, amount FROM {SCHEMA}.t ORDER BY id"),
    );
    assert_eq!(after.rows[0][1], Value::Text("改过的 alpha".into()));
    assert_eq!(
        after.rows[1][1],
        Value::Text("beta".into()),
        "第 2 行不得被波及"
    );
    assert_eq!(after.rows[2][2], Value::Decimal("88.88".into()));

    // 一条失败整批回滚
    let bad = vec![
        RowChange::Update {
            key: locate_key(&pk, &after.columns, &after.rows[1]).unwrap(),
            column: "name".into(),
            new_value: Value::Text("不该生效".into()),
        },
        RowChange::Update {
            key: vec![("id".into(), Value::Int(1))],
            column: "no_such_column".into(),
            new_value: Value::Int(1),
        },
    ];
    assert!(
        provider.apply_changes(&table, &bad).is_err(),
        "非法列应报错"
    );
    let rolled_back = run(
        &mut provider,
        &format!("SELECT name FROM {SCHEMA}.t ORDER BY id"),
    );
    assert_eq!(
        rolled_back.rows[1][0],
        Value::Text("beta".into()),
        "前一条改动必须随事务回滚"
    );

    // 删除行：按主键 DELETE，确认只剩 2 行且删的是第 2 行
    let del = RowChange::Delete {
        key: locate_key(&pk, &rolled_back.columns, &rolled_back.rows[1]).unwrap(),
    };
    let del_outcome = provider.apply_changes(&table, std::slice::from_ref(&del)).expect("删除行");
    assert_eq!(del_outcome.affected, 1);
    let after_del = run(
        &mut provider,
        &format!("SELECT id, name FROM {SCHEMA}.t ORDER BY id"),
    );
    assert_eq!(after_del.row_count(), 2, "应只剩 2 行");
    assert_eq!(after_del.rows[1][1], Value::Text("gamma".into()), "第 2 行应已被删除");

    run(&mut provider, &format!("DROP SCHEMA {SCHEMA} CASCADE"));
    provider.disconnect().expect("断开");
}

/// 多语句脚本：DDL + DML + 查询一次性执行，且字符串里的分号不能被切开。
#[test]
fn multi_statement_script_runs_in_order() {
    let Some((cfg, password)) = test_config() else {
        return;
    };
    const S: &str = "thinsql_it_multi";

    let mut provider = Registry::create(DriverKind::Postgres).expect("创建驱动");
    provider.connect(&cfg, Some(&password)).expect("连接失败");
    assert!(
        provider.capabilities().supports_multi_statement,
        "PostgreSQL 现在应声明支持多语句"
    );

    run(&mut provider, &format!("DROP SCHEMA IF EXISTS {S} CASCADE"));
    run(&mut provider, &format!("CREATE SCHEMA {S}"));

    // 最后一条是查询，因此应当拿到结果集而不是影响行数
    let result = run(
        &mut provider,
        &format!(
            "CREATE TABLE {S}.t (id int primary key, name text);
             INSERT INTO {S}.t VALUES (1, 'a;b'), (2, 'c');
             SELECT id, name FROM {S}.t ORDER BY id;"
        ),
    );
    assert_eq!(result.columns.len(), 2, "应展示最后一条查询的列");
    assert_eq!(result.row_count(), 2);
    assert_eq!(
        result.rows[0][1],
        Value::Text("a;b".into()),
        "字符串里的分号不能被切开，否则插入的值会被截断"
    );

    run(&mut provider, &format!("DROP SCHEMA {S} CASCADE"));
    provider.disconnect().expect("断开");
}
