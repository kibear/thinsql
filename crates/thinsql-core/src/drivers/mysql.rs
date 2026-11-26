//! MySQL 驱动。
//!
//! 这里的每一条数据库专有知识（分页语法、元数据 SQL、类型映射、转义风格）
//! 都只允许出现在本文件内。

use std::time::Instant;

use mysql::prelude::Queryable;
use mysql::{Column as MyColumn, Conn, Opts, OptsBuilder, Row as MyRow, TxOpts, Value as MyValue};

use crate::error::{Error, ErrorKind, ServerError};
use crate::model::{ObjectKind, ObjectMeta, 
    ApplyOutcome, ColumnMeta, ColumnOp, ColumnSpec, ConnectionConfig, ForeignKeyMeta, IndexMeta,
    IndexOp, Page, QueryResult, ResultColumn, RowChange, SchemaMeta, SslMode, StructureChange,
    TableKind, TableMeta, TableRef,
};
use crate::provider::{self, CancelToken, Capabilities, DatabaseProvider, EscapeStyle};
use crate::value::{Value, sql_literal};

/// MySQL 里 database 与 schema 等价，没有独立的 schema 层。
pub struct MySqlProvider {
    conn: Option<Conn>,
    server_version: Option<String>,
}

impl Default for MySqlProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MySqlProvider {
    pub fn new() -> Self {
        Self {
            conn: None,
            server_version: None,
        }
    }

    fn conn(&mut self) -> Result<&mut Conn, Error> {
        self.conn
            .as_mut()
            .ok_or_else(|| Error::new(ErrorKind::Network, "连接尚未建立"))
    }
}

fn map_mysql_error(err: mysql::Error) -> Error {
    match err {
        mysql::Error::MySqlError(server) => {
            // MySQL 错误码是稳定契约，按码分类比匹配文案可靠
            let kind = match server.code {
                1045 => ErrorKind::Auth,            // ER_ACCESS_DENIED_ERROR
                1049 => ErrorKind::UnknownDatabase, // ER_BAD_DB_ERROR
                2002 | 2003 | 2005 | 2006 => ErrorKind::Network,
                2013 => ErrorKind::Network,     // lost connection
                1064 => ErrorKind::ServerError, // syntax error
                _ => ErrorKind::ServerError,
            };
            Error::from_server(
                kind,
                ServerError {
                    code: Some(server.code.to_string()),
                    sql_state: Some(server.state.clone()),
                    message: server.message.clone(),
                    position: None,
                },
            )
        }
        mysql::Error::IoError(io) => Error::with_source(
            ErrorKind::Network,
            format!("与 MySQL 服务器通信失败: {io}"),
            io,
        ),
        other => Error::new(ErrorKind::Protocol, format!("MySQL 协议错误: {other}")),
    }
}

/// 把 mysql 原生值收敛到统一值模型。
///
/// 关键判断：`Bytes` 到底算文本还是二进制，由列的字符集决定（63 = binary），
/// 而不是靠"能不能转成 UTF-8"来猜——后者会把非法编码的字符串静默变成乱码。
///
/// 例外：DATE / DATETIME / TIMESTAMP / TIME 在协议里也标成 binary charset，
/// 但文本协议下内容是可读的 `YYYY-MM-DD…` 字节；若按 binary 展示会变成 hex，
/// 所以这类列一律按声明类型解码成 `DateTime` / 文本。
fn convert(my_col: &MyColumn, value: MyValue) -> Value {
    use mysql::consts::ColumnType as T;

    let is_binary_charset = my_col.character_set() == 63;

    // 文本协议下数值也可能以字节串到达（例如 `SELECT 1` 的常量列，
    // 列类型是 longlong，值却是 b"1"）。所以先按**声明类型**解析，
    // 不能只看收到的 Value 变体，否则数字会显示成 0x31。
    if let MyValue::Bytes(bytes) = &value {
        let text = std::str::from_utf8(bytes).ok();
        match my_col.column_type() {
            T::MYSQL_TYPE_TINY
            | T::MYSQL_TYPE_SHORT
            | T::MYSQL_TYPE_LONG
            | T::MYSQL_TYPE_LONGLONG
            | T::MYSQL_TYPE_INT24
            | T::MYSQL_TYPE_YEAR => {
                if let Some(v) = text.and_then(|t| t.parse::<i64>().ok()) {
                    return Value::Int(v);
                }
                if let Some(v) = text.and_then(|t| t.parse::<u64>().ok()) {
                    return Value::UInt(v);
                }
            }
            T::MYSQL_TYPE_FLOAT | T::MYSQL_TYPE_DOUBLE => {
                if let Some(v) = text.and_then(|t| t.parse::<f64>().ok()) {
                    return Value::Float(v);
                }
            }
            T::MYSQL_TYPE_DATE
            | T::MYSQL_TYPE_NEWDATE
            | T::MYSQL_TYPE_DATETIME
            | T::MYSQL_TYPE_DATETIME2
            | T::MYSQL_TYPE_TIMESTAMP
            | T::MYSQL_TYPE_TIMESTAMP2 => {
                let raw = text
                    .map(str::to_owned)
                    .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned());
                return Value::DateTime { raw, tz: None };
            }
            T::MYSQL_TYPE_TIME | T::MYSQL_TYPE_TIME2 => {
                let raw = text
                    .map(str::to_owned)
                    .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned());
                return Value::Text(raw);
            }
            _ => {}
        }
    }

    match value {
        MyValue::NULL => Value::Null,
        MyValue::Int(v) => Value::Int(v),
        MyValue::UInt(v) => Value::UInt(v),
        MyValue::Float(v) => Value::Float(v as f64),
        MyValue::Double(v) => Value::Float(v),
        MyValue::Date(y, mo, d, h, mi, s, us) => {
            // DATE 只出日期；DATETIME/TIMESTAMP 即使是 00:00:00 也保留时间，
            // 避免把真实的午夜时刻误显示成纯日期。
            let is_date_only = matches!(
                my_col.column_type(),
                T::MYSQL_TYPE_DATE | T::MYSQL_TYPE_NEWDATE
            );
            let raw = if is_date_only {
                format!("{y:04}-{mo:02}-{d:02}")
            } else if us == 0 {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
            } else {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
            };
            Value::DateTime { raw, tz: None }
        }
        MyValue::Time(neg, days, hours, minutes, seconds, micros) => {
            let sign = if neg { "-" } else { "" };
            let total_hours = days * 24 + hours as u32;
            let raw = if micros == 0 {
                format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}")
            } else {
                format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}.{micros:06}")
            };
            Value::Text(raw)
        }
        MyValue::Bytes(bytes) => match my_col.column_type() {
            T::MYSQL_TYPE_DECIMAL | T::MYSQL_TYPE_NEWDECIMAL => {
                Value::Decimal(String::from_utf8_lossy(&bytes).into_owned())
            }
            T::MYSQL_TYPE_JSON => Value::Json(String::from_utf8_lossy(&bytes).into_owned()),
            _ if is_binary_charset => Value::Bytes(bytes),
            _ => match String::from_utf8(bytes) {
                Ok(text) => Value::Text(text),
                // 声明为文本但内容不是合法 UTF-8：保留原始字节，交给 UI 展示 hex
                Err(err) => Value::Bytes(err.into_bytes()),
            },
        },
    }
}

fn type_name(col: &MyColumn) -> String {
    let debug = format!("{:?}", col.column_type());
    let base = debug.strip_prefix("MYSQL_TYPE_").unwrap_or(&debug);
    base.to_ascii_lowercase()
}

/// `Row::unwrap()` 在 mysql 0.28 中只是取出已经解析好的值列，不会失败；
/// 升级 mysql crate 时需复核该假设。
fn row_to_values(cols: &[MyColumn], row: MyRow) -> Vec<Value> {
    row.unwrap()
        .into_iter()
        .enumerate()
        .map(|(i, v)| match cols.get(i) {
            Some(col) => convert(col, v),
            None => Value::Null,
        })
        .collect()
}

/// 执行查询并消费所有结果集。
///
/// 单独拆出来是为了让调用方能在**所有**返回路径（成功、取消、错误）之后
/// 统一恢复会话状态。
fn run_query(
    conn: &mut Conn,
    sql: &str,
    page: Page,
    cancel: &CancelToken,
    started: Instant,
) -> Result<QueryResult, Error> {
    let mut query_result = conn.query_iter(sql).map_err(map_mysql_error)?;

    let mut result = QueryResult::empty(started.elapsed());
    let mut saw_tabular_set = false;
    let mut affected_total = 0u64;

    // 逐个结果集消费：多语句时把非查询语句的影响行数累加，
    // 只保留第一组带列的结果集用于展示。ResultSet 被丢弃时会自动排空，
    // 保证连接不会被留在"结果未读完"的状态。
    while let Some(mut set) = query_result.iter() {
        let set_columns: Vec<MyColumn> = set.columns().as_ref().to_vec();

        if set_columns.is_empty() {
            affected_total = affected_total.saturating_add(set.affected_rows());
            continue;
        }

        if saw_tabular_set {
            continue;
        }

        result.columns = set_columns
            .iter()
            .map(|c| ResultColumn {
                name: c.name_str().into_owned(),
                type_name: type_name(c),
            })
            .collect();

        let limit = page.limit as usize;
        let mut seen = 0usize;
        for row in set.by_ref() {
            if provider::is_cancelled(cancel) {
                return Err(Error::cancelled());
            }
            seen += 1;
            if seen > limit {
                // 多取到的这一行只用来判断"还有更多"，不放进结果集
                result.has_more = true;
                break;
            }
            let row = row.map_err(map_mysql_error)?;
            result.rows.push(row_to_values(&set_columns, row));
        }
        saw_tabular_set = true;
    }

    if !saw_tabular_set {
        result.affected_rows = Some(affected_total);
    }

    result.elapsed = started.elapsed();
    Ok(result)
}

impl DatabaseProvider for MySqlProvider {
    fn connect(&mut self, cfg: &ConnectionConfig, password: Option<&str>) -> Result<(), Error> {
        let mut builder = OptsBuilder::new()
            .ip_or_hostname(Some(cfg.host.clone()))
            .tcp_port(cfg.port)
            .user(Some(cfg.user.clone()))
            .db_name(cfg.database.clone())
            .prefer_socket(false)
            .tcp_keepalive_time_ms(Some(30_000));

        if let Some(pw) = password {
            builder = builder.pass(Some(pw));
        }

        // Prefer 模式下接受自签证书：内网数据库绝大多数用的是自签 CA，
        // 强制校验会让用户连不上。Required 才做完整校验。
        match cfg.ssl {
            SslMode::Disabled => {}
            SslMode::Prefer => {
                builder = builder.ssl_opts(Some(
                    mysql::SslOpts::default().with_danger_accept_invalid_certs(true),
                ));
            }
            SslMode::Required => {
                builder = builder.ssl_opts(Some(mysql::SslOpts::default()));
            }
        }

        let conn = Conn::new(Opts::from(builder)).map_err(map_mysql_error)?;
        self.conn = Some(conn);

        let version: Option<String> = self
            .conn()?
            .query_first("SELECT VERSION()")
            .map_err(map_mysql_error)?;
        self.server_version = version;
        tracing::info!(
            host = %cfg.host,
            port = cfg.port,
            version = ?self.server_version,
            "MySQL 已连接"
        );
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), Error> {
        if let Some(mut conn) = self.conn.take() {
            use mysql::prelude::Queryable as _;
            // 这是"尽力而为"的清理；失败不应阻止断开，但必须留下日志。
            if let Err(err) = conn.query_drop("DO 1") {
                tracing::warn!(error = %err, "MySQL 断开前清理失败");
            }
        }
        Ok(())
    }

    fn ping(&mut self) -> Result<(), Error> {
        self.conn()?.ping().map_err(map_mysql_error)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            escape: EscapeStyle::Backslash,
            identifier_quote: '`',
            supports_multi_statement: true,
            supports_transactions: true,
            has_schema_layer: false,
            object_kinds: &[
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::Procedure,
                ObjectKind::Function,
                ObjectKind::Trigger,
                ObjectKind::Event,
            ],
        }
    }

    fn query(&mut self, sql: &str, page: Page, cancel: &CancelToken) -> Result<QueryResult, Error> {
        let started = Instant::now();

        // 用会话级 sql_select_limit 让服务器只发送一页多一点。
        // 否则一条 select * from 大表 会把百万行全推进内存——这正是
        // 我们要避免的"内存占用大"。
        //
        // 这是会话级状态：查询结束后必须恢复，否则同一连接后续的元数据查询
        // （information_schema）也会被这个上限静默截断，表现为"少表/少列"。
        let conn = self.conn()?;
        let original_limit = match conn.query_first::<u64, _>("SELECT @@SESSION.sql_select_limit") {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(error = %err, "读取 sql_select_limit 失败，恢复时将使用 DEFAULT");
                None
            }
        };

        conn.query_drop(format!(
            "SET SESSION sql_select_limit = {}",
            page.probe_limit()
        ))
        .map_err(map_mysql_error)?;

        let outcome = run_query(conn, sql, page, cancel, started);

        let restore = match original_limit {
            Some(limit) => format!("SET SESSION sql_select_limit = {limit}"),
            None => "SET SESSION sql_select_limit = DEFAULT".to_string(),
        };
        if let Err(err) = conn.query_drop(restore) {
            tracing::warn!(error = %err, "恢复 sql_select_limit 失败，连接后续查询可能被截断");
        }

        outcome
    }

    fn list_schemas(&mut self) -> Result<Vec<SchemaMeta>, Error> {
        let conn = self.conn()?;
        let names: Vec<String> = conn
            .query(
                "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
                 WHERE SCHEMA_NAME NOT IN \
                 ('information_schema','mysql','performance_schema','sys') \
                 ORDER BY SCHEMA_NAME",
            )
            .map_err(map_mysql_error)?;
        Ok(names.into_iter().map(|name| SchemaMeta { name }).collect())
    }

    fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<TableMeta>, Error> {
        let conn = self.conn()?;
        let rows: Vec<(String, String, String, Option<u64>)> = conn
            .exec(
                "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, TABLE_ROWS \
                 FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) \
                 ORDER BY TABLE_NAME",
                (schema,),
            )
            .map_err(map_mysql_error)?;

        Ok(rows
            .into_iter()
            .map(|(schema_name, name, table_type, approx_rows)| TableMeta {
                kind: if table_type.to_ascii_uppercase().contains("VIEW") {
                    TableKind::View
                } else {
                    TableKind::Table
                },
                schema: Some(schema_name),
                name,
                approx_rows,
            })
            .collect())
    }


    fn list_objects(
        &mut self,
        schema: Option<&str>,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectMeta>, Error> {
        let conn = self.conn()?;
        match kind {
            ObjectKind::Table | ObjectKind::View => {
                let want_view = kind == ObjectKind::View;
                let rows: Vec<(String, String, String)> = conn
                    .exec(
                        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE                          FROM information_schema.TABLES                          WHERE TABLE_SCHEMA = COALESCE(?, DATABASE())                          ORDER BY TABLE_NAME",
                        (schema,),
                    )
                    .map_err(map_mysql_error)?;
                Ok(rows
                    .into_iter()
                    .filter(|(_, _, ty)| {
                        let is_view = ty.to_ascii_uppercase().contains("VIEW");
                        is_view == want_view
                    })
                    .map(|(schema_name, name, _)| ObjectMeta {
                        schema: Some(schema_name),
                        name,
                        kind,
                    })
                    .collect())
            }
            ObjectKind::Procedure | ObjectKind::Function => {
                let routine = if kind == ObjectKind::Procedure {
                    "PROCEDURE"
                } else {
                    "FUNCTION"
                };
                let rows: Vec<(String, String)> = conn
                    .exec(
                        "SELECT ROUTINE_SCHEMA, ROUTINE_NAME                          FROM information_schema.ROUTINES                          WHERE ROUTINE_SCHEMA = COALESCE(?, DATABASE())                            AND ROUTINE_TYPE = ?                          ORDER BY ROUTINE_NAME",
                        (schema, routine),
                    )
                    .map_err(map_mysql_error)?;
                Ok(rows
                    .into_iter()
                    .map(|(schema_name, name)| ObjectMeta {
                        schema: Some(schema_name),
                        name,
                        kind,
                    })
                    .collect())
            }
            ObjectKind::Trigger => {
                let rows: Vec<(String, String)> = conn
                    .exec(
                        "SELECT TRIGGER_SCHEMA, TRIGGER_NAME                          FROM information_schema.TRIGGERS                          WHERE TRIGGER_SCHEMA = COALESCE(?, DATABASE())                          ORDER BY TRIGGER_NAME",
                        (schema,),
                    )
                    .map_err(map_mysql_error)?;
                Ok(rows
                    .into_iter()
                    .map(|(schema_name, name)| ObjectMeta {
                        schema: Some(schema_name),
                        name,
                        kind,
                    })
                    .collect())
            }
            ObjectKind::Event => {
                let rows: Vec<(String, String)> = conn
                    .exec(
                        "SELECT EVENT_SCHEMA, EVENT_NAME                          FROM information_schema.EVENTS                          WHERE EVENT_SCHEMA = COALESCE(?, DATABASE())                          ORDER BY EVENT_NAME",
                        (schema,),
                    )
                    .map_err(map_mysql_error)?;
                Ok(rows
                    .into_iter()
                    .map(|(schema_name, name)| ObjectMeta {
                        schema: Some(schema_name),
                        name,
                        kind,
                    })
                    .collect())
            }
            ObjectKind::MaterializedView | ObjectKind::Sequence => Ok(Vec::new()),
        }
    }

    fn set_schema(&mut self, schema: &str) -> Result<(), Error> {
        let q = self.capabilities().quote_identifier(schema);
        let conn = self.conn()?;
        conn.query_drop(format!("USE {q}")).map_err(map_mysql_error)
    }

    fn list_columns(&mut self, table: &TableRef) -> Result<Vec<ColumnMeta>, Error> {
        let conn = self.conn()?;
        let rows: Vec<(
            String,
            String,
            String,
            Option<String>,
            String,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = conn
            .exec(
                "SELECT c.COLUMN_NAME, c.COLUMN_TYPE, c.IS_NULLABLE, c.COLUMN_DEFAULT, c.EXTRA, \
                        IF(k.COLUMN_NAME IS NULL, 0, 1), \
                        c.COLUMN_COMMENT, c.CHARACTER_SET_NAME, c.COLLATION_NAME \
                 FROM information_schema.COLUMNS c \
                 LEFT JOIN information_schema.KEY_COLUMN_USAGE k \
                   ON k.TABLE_SCHEMA = c.TABLE_SCHEMA \
                  AND k.TABLE_NAME = c.TABLE_NAME \
                  AND k.COLUMN_NAME = c.COLUMN_NAME \
                  AND k.CONSTRAINT_NAME = 'PRIMARY' \
                 WHERE c.TABLE_SCHEMA = COALESCE(?, DATABASE()) AND c.TABLE_NAME = ? \
                 ORDER BY c.ORDINAL_POSITION",
                (table.schema.as_deref(), table.name.as_str()),
            )
            .map_err(map_mysql_error)?;

        Ok(rows
            .into_iter()
            .map(
                |(name, type_name, nullable, default, extra, is_pk, comment, charset, collation)| {
                    ColumnMeta {
                        name,
                        type_name,
                        nullable: nullable.eq_ignore_ascii_case("YES"),
                        default,
                        is_primary_key: is_pk.unwrap_or(0) == 1,
                        is_auto_increment: extra.to_ascii_lowercase().contains("auto_increment"),
                        comment: comment.filter(|s| !s.is_empty()),
                        charset,
                        collation,
                    }
                },
            )
            .collect())
    }

    fn list_indexes(&mut self, table: &TableRef) -> Result<Vec<IndexMeta>, Error> {
        let conn = self.conn()?;
        let rows: Vec<(String, String, Option<i64>)> = conn
            .exec(
                "SELECT INDEX_NAME, \
                        GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX) AS cols, \
                        MIN(NON_UNIQUE) \
                 FROM information_schema.STATISTICS \
                 WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ? \
                 GROUP BY INDEX_NAME \
                 ORDER BY INDEX_NAME",
                (table.schema.as_deref(), table.name.as_str()),
            )
            .map_err(map_mysql_error)?;

        Ok(rows
            .into_iter()
            .map(|(name, cols, non_unique)| IndexMeta {
                columns: cols.split(',').map(|s| s.to_string()).collect(),
                unique: non_unique.unwrap_or(1) == 0,
                name,
            })
            .collect())
    }

    fn list_foreign_keys(&mut self, table: &TableRef) -> Result<Vec<ForeignKeyMeta>, Error> {
        let conn = self.conn()?;
        let rows: Vec<(String, String, String, String)> = conn
            .exec(
                "SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, \
                        REFERENCED_COLUMN_NAME \
                 FROM information_schema.KEY_COLUMN_USAGE \
                 WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ? \
                   AND REFERENCED_TABLE_NAME IS NOT NULL \
                 ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
                (table.schema.as_deref(), table.name.as_str()),
            )
            .map_err(map_mysql_error)?;

        // 复合外键会返回多行，按约束名归并
        let mut out: Vec<ForeignKeyMeta> = Vec::new();
        for (name, column, ref_table, ref_column) in rows {
            match out.iter_mut().find(|fk| fk.name == name) {
                Some(fk) => {
                    fk.columns.push(column);
                    fk.ref_columns.push(ref_column);
                }
                None => out.push(ForeignKeyMeta {
                    name,
                    columns: vec![column],
                    ref_table,
                    ref_columns: vec![ref_column],
                }),
            }
        }
        Ok(out)
    }

    /// 按主键回写改动，整体一个事务：任何一条失败都全部回滚。
    ///
    /// 执行的 SQL 与界面上"提交前预览"的 SQL 是同一份文本，
    /// 避免预览与实际执行不一致这种最容易出事的偏差。
    fn apply_changes(
        &mut self,
        table: &TableRef,
        changes: &[RowChange],
    ) -> Result<ApplyOutcome, Error> {
        if changes.is_empty() {
            return Ok(ApplyOutcome { affected: 0 });
        }

        let conn = self.conn()?;

        let mut tx = conn
            .start_transaction(TxOpts::default())
            .map_err(map_mysql_error)?;

        let mut affected = 0u64;
        for change in changes {
            if change.key().is_empty() {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "该表没有主键，无法安全地定位要修改的行",
                ));
            }

            // 与界面上"提交前预览"共用同一个生成函数，保证两者永远一致
            let sql = crate::edit::build_sql(crate::model::DriverKind::MySql, table, change);
            tracing::debug!(sql = %sql, "执行行内更新");
            tx.query_drop(&sql).map_err(map_mysql_error)?;
            affected += tx.affected_rows();
        }

        tx.commit().map_err(map_mysql_error)?;
        Ok(ApplyOutcome { affected })
    }

    fn table_ddl(&mut self, table: &TableRef) -> Result<String, Error> {
        let sql = format!("SHOW CREATE TABLE {}", self.capabilities().quote_table(table));
        let conn = self.conn()?;
        // SHOW 用文本协议读取，避免不同版本对预处理 SHOW 语句的兼容差异
        let row: Option<(String, Option<String>)> =
            conn.query_first(sql).map_err(map_mysql_error)?;
        row.and_then(|(_, ddl)| ddl)
            .ok_or_else(|| Error::new(ErrorKind::Internal, "未取到建表语句"))
    }

    fn generate_alter(
        &mut self,
        table: &TableRef,
        change: &StructureChange,
    ) -> Result<Vec<String>, Error> {
        Ok(build_mysql_alter(table, change))
    }
}

fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// 默认值：已知表达式（当前时间、NULL、数字、已加引号的字面量）原样保留，
/// 其余按字符串字面量安全转义，避免用户输入拼进 DDL。
fn default_sql(value: &str) -> String {
    let trimmed = value.trim();
    let upper = trimmed.to_ascii_uppercase();
    if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || upper == "NULL"
        || upper == "CURRENT_TIMESTAMP"
        || upper.starts_with("CURRENT_TIMESTAMP(")
        || trimmed.parse::<i64>().is_ok()
        || trimmed.parse::<f64>().is_ok()
    {
        trimmed.to_string()
    } else {
        sql_literal(&Value::Text(trimmed.to_string()), EscapeStyle::Backslash)
    }
}

/// 字符集 / 排序规则是标识符而非字符串字面量，只放行安全 token。
fn ident_token_sql(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn column_definition(column: &ColumnSpec) -> String {
    let mut parts = vec![quote_ident(&column.name), column.type_sql()];
    parts.push(if column.nullable {
        "NULL".into()
    } else {
        "NOT NULL".into()
    });
    if column.auto_increment {
        parts.push("AUTO_INCREMENT".into());
    }
    // MySQL 要求自增列在定义时就成为某个键
    if column.primary_key && column.auto_increment {
        parts.push("PRIMARY KEY".into());
    }
    if let Some(default) = &column.default {
        parts.push(format!("DEFAULT {}", default_sql(default)));
    }
    if let Some(charset) = column.charset.as_deref().and_then(ident_token_sql) {
        parts.push(format!("CHARACTER SET {charset}"));
    }
    if let Some(collation) = column.collation.as_deref().and_then(ident_token_sql) {
        parts.push(format!("COLLATE {collation}"));
    }
    if let Some(comment) = &column.comment {
        parts.push(format!(
            "COMMENT {}",
            sql_literal(&Value::Text(comment.clone()), EscapeStyle::Backslash)
        ));
    }
    parts.join(" ")
}

/// 生成整批 MySQL ALTER 语句。顺序固定：删索引 → 列变更 → 建索引，
/// 避免改列时被旧索引阻挡；语句逐条交给用户预览确认。
pub fn build_mysql_alter(table: &TableRef, change: &StructureChange) -> Vec<String> {
    if change.is_empty() {
        return Vec::new();
    }
    let target = Capabilities {
        escape: EscapeStyle::Backslash,
        identifier_quote: '`',
        supports_multi_statement: true,
        supports_transactions: true,
        has_schema_layer: false,
        object_kinds: &[],
    }
    .quote_table(table);
    let mut sqls = Vec::new();

    for op in &change.index_ops {
        if let IndexOp::Drop { name } = op {
            sqls.push(format!(
                "ALTER TABLE {target} DROP INDEX {}",
                quote_ident(name)
            ));
        }
    }

    for op in &change.column_ops {
        match op {
            ColumnOp::Add { column } => {
                sqls.push(format!(
                    "ALTER TABLE {target} ADD COLUMN {}",
                    column_definition(column)
                ));
                if column.primary_key && !column.auto_increment {
                    sqls.push(format!(
                        "ALTER TABLE {target} ADD PRIMARY KEY ({})",
                        quote_ident(&column.name)
                    ));
                }
            }
            ColumnOp::Alter {
                old_name,
                old_primary_key,
                column,
            } => {
                if *old_primary_key && !column.primary_key {
                    sqls.push(format!("ALTER TABLE {target} DROP PRIMARY KEY"));
                }
                let head = if old_name == &column.name {
                    format!("MODIFY COLUMN {}", column_definition(column))
                } else {
                    format!(
                        "CHANGE COLUMN {} {}",
                        quote_ident(old_name),
                        column_definition(column)
                    )
                };
                sqls.push(format!("ALTER TABLE {target} {head}"));
                if column.primary_key && !old_primary_key && !column.auto_increment {
                    sqls.push(format!(
                        "ALTER TABLE {target} ADD PRIMARY KEY ({})",
                        quote_ident(&column.name)
                    ));
                }
            }
            ColumnOp::Drop { name } => {
                sqls.push(format!(
                    "ALTER TABLE {target} DROP COLUMN {}",
                    quote_ident(name)
                ));
            }
        }
    }

    for op in &change.index_ops {
        if let IndexOp::Create {
            name,
            columns,
            unique,
        } = op
        {
            let cols = columns
                .iter()
                .map(|c| quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            let kind = if *unique { "UNIQUE INDEX" } else { "INDEX" };
            let name = if name.trim().is_empty() {
                String::new()
            } else {
                format!("{} ", quote_ident(name))
            };
            sqls.push(format!(
                "ALTER TABLE {target} ADD {kind} {name}({cols})"
            ));
        }
    }

    sqls
}

#[cfg(test)]
mod alter_tests {
    use super::*;
    use crate::model::{ColumnOp, ColumnSpec, IndexOp, StructureChange};

    fn table() -> TableRef {
        TableRef::new(Some("db".into()), "users")
    }

    fn col(name: &str, data_type: &str) -> ColumnSpec {
        ColumnSpec {
            name: name.into(),
            data_type: data_type.into(),
            length: None,
            nullable: false,
            primary_key: false,
            default: None,
            comment: None,
            charset: None,
            collation: None,
            auto_increment: false,
        }
    }

    fn alter(change: &StructureChange) -> Vec<String> {
        build_mysql_alter(&table(), change)
    }

    #[test]
    fn add_column_renders_type_length_comment_and_charset() {
        let mut column = col("nick_name", "varchar");
        column.length = Some("64".into());
        column.comment = Some("昵称".into());
        column.charset = Some("utf8mb4".into());
        column.collation = Some("utf8mb4_unicode_ci".into());
        let change = StructureChange {
            column_ops: vec![ColumnOp::Add { column }],
            index_ops: vec![],
        };
        assert_eq!(
            alter(&change),
            vec![
                "ALTER TABLE `db`.`users` ADD COLUMN `nick_name` varchar(64) NOT NULL \
                 CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci COMMENT '昵称'"
            ]
        );
    }

    #[test]
    fn auto_increment_primary_key_is_defined_with_the_column() {
        let mut column = col("id", "bigint");
        column.primary_key = true;
        column.auto_increment = true;
        let change = StructureChange {
            column_ops: vec![ColumnOp::Add { column }],
            index_ops: vec![],
        };
        assert_eq!(
            alter(&change)[0],
            "ALTER TABLE `db`.`users` ADD COLUMN `id` bigint NOT NULL AUTO_INCREMENT PRIMARY KEY"
        );
    }

    #[test]
    fn rename_uses_change_column_and_drop_removes_column() {
        let change = StructureChange {
            column_ops: vec![
                ColumnOp::Drop {
                    name: "obsolete".into(),
                },
                ColumnOp::Alter {
                    old_name: "a".into(),
                    old_primary_key: false,
                    column: col("b", "int"),
                },
            ],
            index_ops: vec![],
        };
        assert_eq!(
            alter(&change),
            vec![
                "ALTER TABLE `db`.`users` DROP COLUMN `obsolete`",
                "ALTER TABLE `db`.`users` CHANGE COLUMN `a` `b` int NOT NULL",
            ]
        );
    }

    #[test]
    fn primary_key_toggle_emits_key_statements() {
        let change = StructureChange {
            column_ops: vec![
                ColumnOp::Alter {
                    old_name: "id".into(),
                    old_primary_key: true,
                    column: col("id", "int"),
                },
                ColumnOp::Alter {
                    old_name: "code".into(),
                    old_primary_key: false,
                    column: {
                        let mut c = col("code", "varchar");
                        c.primary_key = true;
                        c
                    },
                },
            ],
            index_ops: vec![],
        };
        let sqls = alter(&change);
        assert!(sqls.iter().any(|s| s == "ALTER TABLE `db`.`users` DROP PRIMARY KEY"));
        assert!(sqls.iter().any(|s| s == "ALTER TABLE `db`.`users` ADD PRIMARY KEY (`code`)"));
    }

    #[test]
    fn index_drops_come_before_column_changes_and_creates_last() {
        let change = StructureChange {
            column_ops: vec![ColumnOp::Drop {
                name: "obsolete".into(),
            }],
            index_ops: vec![
                IndexOp::Drop {
                    name: "idx_old".into(),
                },
                IndexOp::Create {
                    name: "uk_email".into(),
                    columns: vec!["email".into()],
                    unique: true,
                },
            ],
        };
        let sqls = alter(&change);
        assert_eq!(sqls.len(), 3);
        assert_eq!(sqls[0], "ALTER TABLE `db`.`users` DROP INDEX `idx_old`");
        assert!(sqls[1].contains("DROP COLUMN"), "实际: {}", sqls[1]);
        assert_eq!(
            sqls[2],
            "ALTER TABLE `db`.`users` ADD UNIQUE INDEX `uk_email` (`email`)"
        );
    }

    #[test]
    fn composite_index_keeps_column_order_and_uniqueness() {
        let change = StructureChange {
            column_ops: Vec::new(),
            index_ops: vec![
                IndexOp::Create {
                    name: "idx_user_status".into(),
                    columns: vec!["user_id".into(), "status".into()],
                    unique: false,
                },
                IndexOp::Create {
                    name: "uk_tenant_email".into(),
                    columns: vec!["tenant_id".into(), "email".into(), "deleted_at".into()],
                    unique: true,
                },
            ],
        };
        let sqls = alter(&change);
        assert_eq!(
            sqls,
            vec![
                "ALTER TABLE `db`.`users` ADD INDEX `idx_user_status` (`user_id`, `status`)",
                "ALTER TABLE `db`.`users` ADD UNIQUE INDEX `uk_tenant_email` (`tenant_id`, `email`, `deleted_at`)",
            ]
        );
    }

    #[test]
    fn identifiers_and_comments_are_escaped() {
        let mut column = col("we`ird", "varchar");
        column.length = Some("10".into());
        column.comment = Some("it's fine".into());
        column.default = Some("oops".into());
        let change = StructureChange {
            column_ops: vec![ColumnOp::Add { column }],
            index_ops: vec![],
        };
        let sql = alter(&change)[0].clone();
        assert!(sql.contains("`we``ird`"), "实际: {sql}");
        assert!(sql.contains("'it''s fine'"), "实际: {sql}");
        assert!(sql.contains("DEFAULT 'oops'"), "实际: {sql}");
    }
}
