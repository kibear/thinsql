//! MySQL 驱动的端到端验证。
//!
//! 需要一台真实 MySQL，通过环境变量开启，默认跳过：
//!
//! ```bash
//! THINSQL_TEST_MYSQL=1 THINSQL_TEST_MYSQL_PASS=root \
//!   cargo test -p thinsql-core --test mysql_integration -- --nocapture
//! ```
//!
//! 测试会新建并最终删除独立库 `thinsql_it`，不会碰任何既有数据。

use std::sync::atomic::AtomicBool;

use thinsql_core::model::{ConnectionConfig, DriverKind, Page, TableRef};
use thinsql_core::provider::{CancelToken, DatabaseProvider, Registry};
use thinsql_core::{ErrorKind, Value};

const TEST_DB: &str = "thinsql_it";

fn test_config() -> Option<(ConnectionConfig, String)> {
    if std::env::var("THINSQL_TEST_MYSQL").ok().as_deref() != Some("1") {
        eprintln!("跳过：未设置 THINSQL_TEST_MYSQL=1");
        return None;
    }
    let mut cfg = ConnectionConfig::new("it", "integration", DriverKind::MySql);
    cfg.host = std::env::var("THINSQL_TEST_MYSQL_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    cfg.port = std::env::var("THINSQL_TEST_MYSQL_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3306);
    cfg.user = std::env::var("THINSQL_TEST_MYSQL_USER").unwrap_or_else(|_| "root".into());
    cfg.read_only = false;
    let pass = std::env::var("THINSQL_TEST_MYSQL_PASS").unwrap_or_else(|_| "root".into());
    Some((cfg, pass))
}

fn token() -> CancelToken {
    std::sync::Arc::new(AtomicBool::new(false))
}

#[test]
fn mysql_end_to_end() {
    let Some((cfg, password)) = test_config() else {
        return;
    };

    let mut provider = Registry::create(DriverKind::MySql).expect("创建驱动");

    // 1. 认证失败必须归类为 Auth，而不是笼统的 ServerError
    let err = provider
        .connect(&cfg, Some("definitely-not-the-password"))
        .expect_err("错误密码不应该连接成功");
    assert_eq!(
        err.kind,
        ErrorKind::Auth,
        "错误密码应归类为 Auth，实际: {:?} / {}",
        err.kind,
        err.message
    );

    // 2. 正常连接
    provider.connect(&cfg, Some(&password)).expect("连接失败");
    provider.ping().expect("ping 失败");
    let caps = provider.capabilities();
    assert_eq!(caps.identifier_quote, '`');

    // 3. 准备独立测试库与表
    run(&mut provider, &format!("DROP DATABASE IF EXISTS {TEST_DB}"));
    run(&mut provider, &format!("CREATE DATABASE {TEST_DB}"));
    run(
        &mut provider,
        &format!(
            "CREATE TABLE {TEST_DB}.t (
                id INT PRIMARY KEY AUTO_INCREMENT,
                name VARCHAR(64) NOT NULL,
                amount DECIMAL(10,2) NULL,
                note TEXT NULL,
                payload VARBINARY(16) NULL
            )"
        ),
    );
    run(
        &mut provider,
        &format!(
            "CREATE TABLE {TEST_DB}.child (
                id INT PRIMARY KEY AUTO_INCREMENT,
                parent_id INT NOT NULL,
                CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES {TEST_DB}.t(id)
            )"
        ),
    );
    let inserted = run(
        &mut provider,
        &format!(
            "INSERT INTO {TEST_DB}.t (name, amount, note, payload) VALUES
             ('alpha', 12.34, '第一行', NULL),
             ('O''Brien', NULL, NULL, X'DEADBEEF'),
             ('gamma', 0.00, '含\\反斜杠', NULL)"
        ),
    );
    assert_eq!(inserted.affected_rows, Some(3), "多行插入的影响行数");

    // 4. 查询与类型映射
    let result = run(
        &mut provider,
        &format!("SELECT id, name, amount, note, payload FROM {TEST_DB}.t ORDER BY id"),
    );
    assert_eq!(result.columns.len(), 5);
    assert_eq!(result.row_count(), 3);
    assert!(!result.has_more);
    assert_eq!(result.rows[0][1], Value::Text("alpha".into()));
    assert_eq!(
        result.rows[0][2],
        Value::Decimal("12.34".into()),
        "定点数必须保留原始精度，不能转成 f64"
    );
    assert!(result.rows[0][3].is_null() || result.rows[0][3] == Value::Text("第一行".into()));
    assert_eq!(
        result.rows[1][4],
        Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        "二进制列必须映射为 Bytes 而不是乱码文本"
    );
    assert!(
        matches!(result.rows[1][2], Value::Null),
        "NULL 要保留为 Null"
    );

    // 5. 分页：只取 2 行时应该告诉调用方还有更多
    let page = run_with_page(
        &mut provider,
        &format!("SELECT id, name FROM {TEST_DB}.t ORDER BY id"),
        Page::with_limit(2),
    );
    assert_eq!(page.row_count(), 2);
    assert!(page.has_more, "总共有 3 行，取 2 行时必须报告 has_more");
    let rest = run_with_page(
        &mut provider,
        &format!("SELECT id, name FROM {TEST_DB}.t ORDER BY id"),
        Page {
            offset: 0,
            limit: 10,
        },
    );
    assert_eq!(rest.row_count(), 3);
    assert!(!rest.has_more);

    // 6. 元数据
    let schemas = provider.list_schemas().expect("list_schemas");
    assert!(
        schemas.iter().any(|s| s.name == TEST_DB),
        "新建的库应出现在 schema 列表里"
    );

    let tables = provider.list_tables(Some(TEST_DB)).expect("list_tables");
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"t") && names.contains(&"child"),
        "表列表: {names:?}"
    );

    let table = TableRef::new(Some(TEST_DB.into()), "t");
    let columns = provider.list_columns(&table).expect("list_columns");
    let id_col = columns.iter().find(|c| c.name == "id").expect("id 列");
    assert!(id_col.is_primary_key, "id 应被识别为主键");
    assert!(id_col.is_auto_increment, "id 应被识别为自增");
    assert!(!id_col.nullable, "主键不应被识别为可空");
    assert!(columns.iter().any(|c| c.name == "payload"));

    let indexes = provider.list_indexes(&table).expect("list_indexes");
    assert!(
        indexes.iter().any(|i| i.name == "PRIMARY" && i.unique),
        "应识别出唯一主键索引: {indexes:?}"
    );

    let fks = provider
        .list_foreign_keys(&TableRef::new(Some(TEST_DB.into()), "child"))
        .expect("list_foreign_keys");
    let fk = fks.iter().find(|f| f.name == "fk_parent").expect("外键");
    assert_eq!(fk.ref_table, "t");
    assert_eq!(fk.columns, vec!["parent_id".to_string()]);

    // 7. 行内编辑：按主键回写，并确认写入内容与转义正确
    use thinsql_core::RowChange;
    let change = RowChange::Update {
        key: vec![("id".into(), Value::Int(1))],
        column: "note".into(),
        new_value: Value::Text("改过的'值'\\含转义".into()),
    };
    let outcome = provider
        .apply_changes(&table, std::slice::from_ref(&change))
        .expect("apply_changes");
    assert_eq!(outcome.affected, 1);

    let verify = run(
        &mut provider,
        &format!("SELECT note FROM {TEST_DB}.t WHERE id = 1"),
    );
    assert_eq!(
        verify.rows[0][0],
        Value::Text("改过的'值'\\含转义".into()),
        "含引号与反斜杠的文本必须原样往返"
    );

    // 把值改回 NULL（NULL 与等号比较不兼容，属易错点）
    let null_change = RowChange::Update {
        key: vec![("id".into(), Value::Int(1))],
        column: "note".into(),
        new_value: Value::Null,
    };
    provider
        .apply_changes(&table, &[null_change])
        .expect("写入 NULL");
    let verify_null = run(
        &mut provider,
        &format!("SELECT note FROM {TEST_DB}.t WHERE id = 1"),
    );
    assert!(verify_null.rows[0][0].is_null());

    // 7b. 删除行：按主键 DELETE，确认该行真的没了
    let del = RowChange::Delete {
        key: vec![("id".into(), Value::Int(1))],
    };
    let del_outcome = provider
        .apply_changes(&table, std::slice::from_ref(&del))
        .expect("删除行");
    assert_eq!(del_outcome.affected, 1);
    let verify_del = run(
        &mut provider,
        &format!("SELECT id FROM {TEST_DB}.t WHERE id = 1"),
    );
    assert_eq!(verify_del.row_count(), 0, "id=1 应已被删除");

    // 8. 语法错误必须保留服务器原文与错误码
    let err = provider
        .query("SELEC oops", Page::first(), &token())
        .expect_err("非法语句应报错");
    assert_eq!(err.kind, ErrorKind::ServerError);
    let server = err.server.as_ref().expect("应保留服务器错误详情");
    assert_eq!(server.code.as_deref(), Some("1064"), "应保留 MySQL 错误码");

    // 9. 清理
    run(&mut provider, &format!("DROP DATABASE IF EXISTS {TEST_DB}"));
    provider.disconnect().expect("断开连接");
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
fn scalar_constants_keep_their_numeric_type() {
    let Some((cfg, password)) = test_config() else {
        return;
    };
    let mut provider = Registry::create(DriverKind::MySql).expect("创建驱动");
    provider.connect(&cfg, Some(&password)).expect("连接失败");

    let result = provider
        .query("SELECT 1 AS a, 2.5 AS b, 'x' AS c", Page::first(), &token())
        .expect("查询失败");

    eprintln!(
        "列类型: {:?}",
        result
            .columns
            .iter()
            .map(|c| (&c.name, &c.type_name))
            .collect::<Vec<_>>()
    );
    eprintln!("首行: {:?}", result.rows[0]);

    assert_eq!(
        result.rows[0][0],
        Value::Int(1),
        "整数常量必须是 Int，实际列类型 {:?}",
        result.columns[0].type_name
    );
    assert_eq!(result.rows[0][2], Value::Text("x".into()));
}

/// 编辑闭环：预览 SQL → 提交两行 → 只改到目标行；一条失败则整批回滚。
#[test]
fn editing_flow_updates_only_target_rows_and_rolls_back_on_error() {
    let Some((cfg, password)) = test_config() else {
        return;
    };
    const DB: &str = "thinsql_it_edit";

    let mut provider = Registry::create(DriverKind::MySql).expect("创建驱动");
    provider.connect(&cfg, Some(&password)).expect("连接失败");

    run(&mut provider, &format!("DROP DATABASE IF EXISTS {DB}"));
    run(&mut provider, &format!("CREATE DATABASE {DB}"));
    run(
        &mut provider,
        &format!(
            "CREATE TABLE {DB}.t (
                id INT PRIMARY KEY,
                name VARCHAR(64) NOT NULL,
                amount DECIMAL(10,2) NULL
            )"
        ),
    );
    run(
        &mut provider,
        &format!(
            "INSERT INTO {DB}.t (id, name, amount) VALUES (1,'a',1.00),(2,'b',2.00),(3,'c',3.00)"
        ),
    );

    let table = TableRef::new(Some(DB.into()), "t");
    let page = run(
        &mut provider,
        &format!("SELECT id, name, amount FROM {DB}.t ORDER BY id"),
    );
    assert_eq!(page.row_count(), 3);

    // 主键从元数据来，而不是猜
    let columns = provider.list_columns(&table).expect("list_columns");
    let pk: Vec<String> = columns
        .iter()
        .filter(|c| c.is_primary_key)
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(pk, vec!["id".to_string()]);

    use thinsql_core::edit::{build_sql, locate_key};
    use thinsql_core::model::RowChange;

    let key1 = locate_key(&pk, &page.columns, &page.rows[0]).expect("定位第 1 行");
    let key2 = locate_key(&pk, &page.columns, &page.rows[1]).expect("定位第 2 行");

    let changes = vec![
        RowChange::Update {
            key: key1,
            column: "name".into(),
            new_value: Value::Text("改过的 a".into()),
        },
        RowChange::Update {
            key: key2,
            column: "amount".into(),
            new_value: Value::Decimal("99.99".into()),
        },
    ];

    // 预览文本必须与执行语义一致
    let preview = build_sql(DriverKind::MySql, &table, &changes[0]);
    assert!(preview.contains("WHERE `id` = 1"), "预览: {preview}");
    assert!(preview.ends_with("LIMIT 1"), "预览: {preview}");

    let outcome = provider.apply_changes(&table, &changes).expect("提交改动");
    assert_eq!(outcome.affected, 2);

    let after = run(
        &mut provider,
        &format!("SELECT id, name, amount FROM {DB}.t ORDER BY id"),
    );
    assert_eq!(after.row_count(), 3, "不应该多行或少行");
    assert_eq!(
        after.rows[0][1],
        Value::Text("改过的 a".into()),
        "第 1 行应被改"
    );
    assert_eq!(
        after.rows[1][2],
        Value::Decimal("99.99".into()),
        "第 2 行应被改"
    );
    assert_eq!(
        after.rows[2][1],
        Value::Text("c".into()),
        "第 3 行不得被波及"
    );
    assert_eq!(after.rows[2][2], Value::Decimal("3.00".into()));

    // 一批里混入非法列：必须整批回滚，第一处改动也不能留下
    let bad = vec![
        RowChange::Update {
            key: locate_key(&pk, &after.columns, &after.rows[2]).unwrap(),
            column: "name".into(),
            new_value: Value::Text("不该生效".into()),
        },
        RowChange::Update {
            key: vec![("id".into(), Value::Int(2))],
            column: "no_such_column".into(),
            new_value: Value::Int(1),
        },
    ];
    let err = provider
        .apply_changes(&table, &bad)
        .expect_err("非法列应当报错");
    assert!(err.message.len() > 0);

    let after_rollback = run(
        &mut provider,
        &format!("SELECT name FROM {DB}.t ORDER BY id"),
    );
    assert_eq!(
        after_rollback.rows[2][0],
        Value::Text("c".into()),
        "前一条改动必须随事务一起回滚"
    );

    run(&mut provider, &format!("DROP DATABASE IF EXISTS {DB}"));
    provider.disconnect().expect("断开");
}

/// 分页查询会临时设置会话级 sql_select_limit；查询结束后必须恢复，
/// 否则随后的 information_schema 元数据查询会被静默截断。
#[test]
fn paginated_query_does_not_truncate_later_metadata() {
    let Some((cfg, password)) = test_config() else {
        return;
    };
    const DB: &str = "thinsql_it_limit";

    let mut provider = Registry::create(DriverKind::MySql).expect("创建驱动");
    provider.connect(&cfg, Some(&password)).expect("连接失败");

    run(&mut provider, &format!("DROP DATABASE IF EXISTS {DB}"));
    run(&mut provider, &format!("CREATE DATABASE {DB}"));

    let cols: String = (0..600)
        .map(|i| format!("c{i} INT NULL"))
        .collect::<Vec<_>>()
        .join(", ");
    run(&mut provider, &format!("CREATE TABLE {DB}.wide ({cols})"));

    // 这次分页查询会把会话变量设成 2；修复前它不会被恢复。
    let _ = run_with_page(
        &mut provider,
        &format!("SELECT * FROM {DB}.wide"),
        Page::with_limit(1),
    );

    let table = TableRef::new(Some(DB.into()), "wide");
    let columns = provider.list_columns(&table).expect("list_columns");
    assert_eq!(
        columns.len(),
        600,
        "分页查询后元数据被 sql_select_limit 截断：只拿到 {} 列",
        columns.len()
    );

    run(&mut provider, &format!("DROP DATABASE IF EXISTS {DB}"));
    provider.disconnect().expect("断开连接");
}

/// 表结构编辑端到端：DDL 读取、注释元数据、ALTER 预览与应用。
#[test]
fn structure_editor_round_trip() {
    let Some((cfg, password)) = test_config() else {
        return;
    };
    use thinsql_core::model::{ColumnOp, ColumnSpec, IndexOp, StructureChange};
    use thinsql_core::build_structure_sql;

    const DB: &str = "thinsql_it_structure";
    let mut provider = Registry::create(DriverKind::MySql).expect("创建驱动");
    provider.connect(&cfg, Some(&password)).expect("连接失败");
    run(
        &mut provider,
        &format!("DROP DATABASE IF EXISTS {DB}"),
    );
    run(&mut provider, &format!("CREATE DATABASE {DB}"));
    run(
        &mut provider,
        &format!(
            "CREATE TABLE {DB}.t (\
             id INT PRIMARY KEY AUTO_INCREMENT, \
             name VARCHAR(32) NOT NULL COMMENT '姓名')"
        ),
    );
    let table = TableRef::new(Some(DB.into()), "t");

    let ddl = provider.table_ddl(&table).expect("table_ddl");
    assert!(ddl.contains("CREATE TABLE"), "实际 DDL: {ddl}");
    let columns = provider.list_columns(&table).expect("list_columns");
    let name = columns.iter().find(|c| c.name == "name").expect("name 列");
    assert_eq!(name.comment.as_deref(), Some("姓名"), "注释应可从元数据读到");

    let change = StructureChange {
        column_ops: vec![ColumnOp::Add {
            column: ColumnSpec {
                name: "note".into(),
                data_type: "varchar".into(),
                length: Some("64".into()),
                nullable: true,
                primary_key: false,
                default: None,
                comment: Some("备注".into()),
                charset: None,
                collation: None,
                auto_increment: false,
            },
        }],
        index_ops: Vec::new(),
    };
    let sqls = build_structure_sql(DriverKind::MySql, &table, &change);
    assert_eq!(sqls.len(), 1);
    assert!(sqls[0].contains("ADD COLUMN `note` varchar(64)"), "实际: {}", sqls[0]);
    provider
        .apply_structure(&table, &change)
        .expect("apply_structure");
    let after = provider.list_columns(&table).expect("list_columns");
    assert!(
        after.iter().any(|c| c.name == "note" && c.comment.as_deref() == Some("备注")),
        "ALTER 后的新列与注释应可读"
    );

    let composite = StructureChange {
        column_ops: Vec::new(),
        index_ops: vec![IndexOp::Create {
            name: "idx_compound".into(),
            columns: vec!["name".into(), "note".into()],
            unique: true,
        }],
    };
    let sqls = build_structure_sql(DriverKind::MySql, &table, &composite);
    assert_eq!(
        sqls,
        vec!["ALTER TABLE `thinsql_it_structure`.`t` ADD UNIQUE INDEX `idx_compound` (`name`, `note`)"]
    );
    provider
        .apply_structure(&table, &composite)
        .expect("apply composite index");
    let indexes = provider.list_indexes(&table).expect("list_indexes");
    let created = indexes
        .iter()
        .find(|idx| idx.name == "idx_compound")
        .expect("组合索引应出现在索引元数据里");
    assert_eq!(created.columns, vec!["name", "note"], "组合索引列顺序应保持");
    assert!(created.unique, "组合唯一索引应回读为唯一");

    run(&mut provider, &format!("DROP DATABASE {DB}"));
    provider.disconnect().expect("断开连接");
}
