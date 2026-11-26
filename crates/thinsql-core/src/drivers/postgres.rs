//! PostgreSQL 驱动。
//!
//! 与 MySQL 的差异都收敛在这里：schema 是真正的一层、引号是双引号、
//! UPDATE 不支持 LIMIT、NUMERIC 需要自己解二进制。

use std::time::Instant;

use fallible_iterator::FallibleIterator;
use postgres::types::{FromSql, Type};
use postgres::{Client, NoTls};

use crate::error::{Error, ErrorKind, ServerError};
use crate::model::{
    ApplyOutcome, ColumnMeta, ConnectionConfig, DriverKind, ForeignKeyMeta, IndexMeta, ObjectKind,
    ObjectMeta, Page, QueryResult, ResultColumn, RowChange, SchemaMeta, SslMode, TableKind,
    TableMeta, TableRef,
};
use crate::provider::{self, CancelToken, Capabilities, DatabaseProvider, EscapeStyle};
use crate::value::Value;

use super::pg_numeric::decode_numeric;

pub struct PostgresProvider {
    client: Option<Client>,
    server_version: Option<String>,
}

impl Default for PostgresProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl PostgresProvider {
    pub fn new() -> Self {
        Self {
            client: None,
            server_version: None,
        }
    }

    fn client(&mut self) -> Result<&mut Client, Error> {
        self.client
            .as_mut()
            .ok_or_else(|| Error::new(ErrorKind::Network, "连接尚未建立"))
    }
}

/// NUMERIC 的包装类型：postgres-types 没有对应实现，借 `FromSql` 拿到原始字节自己解。
///
/// 解码失败时保留原始字节，由 [`convert`] 转成可辨识的 hex 文本，
/// 而不是静默变成空串。
struct RawNumeric(Result<String, Vec<u8>>);

impl<'a> FromSql<'a> for RawNumeric {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawNumeric(decode_numeric(raw).ok_or_else(|| raw.to_vec())))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::NUMERIC
    }
}

/// JSON/JSONB：jsonb 的二进制前面有一个版本字节，需要跳过。
struct RawJson(String);

impl<'a> FromSql<'a> for RawJson {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let bytes = if *ty == Type::JSONB && !raw.is_empty() {
            &raw[1..]
        } else {
            raw
        };
        Ok(RawJson(String::from_utf8_lossy(bytes).into_owned()))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::JSON || *ty == Type::JSONB
    }
}

fn map_pg_error(err: postgres::Error) -> Error {
    if let Some(db) = err.as_db_error() {
        use postgres::error::SqlState;
        let kind = match *db.code() {
            SqlState::INVALID_PASSWORD
            | SqlState::INVALID_AUTHORIZATION_SPECIFICATION
            | SqlState::INSUFFICIENT_PRIVILEGE => ErrorKind::Auth,
            SqlState::INVALID_CATALOG_NAME => ErrorKind::UnknownDatabase,
            _ => ErrorKind::ServerError,
        };
        Error::from_server(
            kind,
            ServerError {
                code: Some(db.code().code().to_string()),
                sql_state: Some(db.code().code().to_string()),
                message: db.message().to_string(),
                position: db.position().and_then(|p| match p {
                    postgres::error::ErrorPosition::Original(pos)
                    | postgres::error::ErrorPosition::Internal { position: pos, .. } => {
                        Some(*pos as u32)
                    }
                }),
            },
        )
    } else if err.is_closed() {
        Error::with_source(ErrorKind::Network, "与 PostgreSQL 的连接已关闭", err)
    } else {
        Error::with_source(
            ErrorKind::Protocol,
            format!("PostgreSQL 协议错误: {err}"),
            err,
        )
    }
}

fn convert(ty: &Type, row: &postgres::Row, idx: usize) -> Value {
    // NUMERIC 单独处理：解码失败时不能静默变成空串，否则用户无法区分
    // "真 NULL / 空值 / 解码失败"。
    if *ty == Type::NUMERIC {
        return match row.try_get::<usize, Option<RawNumeric>>(idx) {
            Ok(Some(RawNumeric(Ok(text)))) => Value::Decimal(text),
            Ok(Some(RawNumeric(Err(bytes)))) => {
                tracing::warn!(len = bytes.len(), "NUMERIC 解码失败，返回原始字节");
                Value::Text(format!("\\x{}", to_hex(&bytes)))
            }
            Ok(None) => Value::Null,
            Err(err) => {
                tracing::warn!(error = %err, "读取 NUMERIC 失败");
                Value::Text("<numeric: 读取失败>".into())
            }
        };
    }

    macro_rules! opt {
        ($t:ty, $map:expr) => {
            match row.try_get::<usize, Option<$t>>(idx) {
                Ok(Some(v)) => return $map(v),
                Ok(None) => return Value::Null,
                Err(_) => {}
            }
        };
    }

    opt!(bool, Value::Bool);
    opt!(i16, |v| Value::Int(v as i64));
    opt!(i32, |v| Value::Int(v as i64));
    opt!(i64, Value::Int);
    opt!(f32, |v| Value::Float(v as f64));
    opt!(f64, Value::Float);
    opt!(Vec<u8>, Value::Bytes);
    opt!(RawJson, |v: RawJson| Value::Json(v.0));
    opt!(String, Value::Text);
    opt!(chrono::NaiveDate, |v: chrono::NaiveDate| Value::DateTime {
        raw: v.format("%Y-%m-%d").to_string(),
        tz: None
    });
    opt!(chrono::NaiveDateTime, |v: chrono::NaiveDateTime| {
        Value::DateTime {
            raw: v.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
            tz: None,
        }
    });
    opt!(chrono::DateTime<chrono::Utc>, |v: chrono::DateTime<
        chrono::Utc,
    >| {
        Value::DateTime {
            raw: v.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
            tz: Some("UTC".into()),
        }
    });

    // 兜底：不认识就标明类型，而不是静默当成 NULL
    Value::Text(format!("<{}>", ty.name()))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 逐行解析元数据：单行失败记 warn 并跳过，全部失败才返回 Err。
///
/// 这样可以避免 `filter_map(|row| ... .ok()?)` 把"少表/少列"静默吞掉。
fn collect_meta<T>(
    rows: &[postgres::Row],
    what: &str,
    mut parse: impl FnMut(&postgres::Row) -> Result<T, postgres::Error>,
) -> Result<Vec<T>, Error> {
    let mut out = Vec::with_capacity(rows.len());
    let mut failed = 0usize;
    for (index, row) in rows.iter().enumerate() {
        match parse(row) {
            Ok(value) => out.push(value),
            Err(err) => {
                failed += 1;
                tracing::warn!(what = %what, index, error = %err, "元数据行解析失败，跳过");
            }
        }
    }
    if failed > 0 {
        tracing::warn!(what = %what, failed, total = rows.len(), "部分元数据行解析失败");
    }
    if out.is_empty() && !rows.is_empty() {
        return Err(Error::new(
            ErrorKind::Protocol,
            format!("{what}：{failed} 行全部解析失败"),
        ));
    }
    Ok(out)
}

impl DatabaseProvider for PostgresProvider {
    fn connect(&mut self, cfg: &ConnectionConfig, password: Option<&str>) -> Result<(), Error> {
        let mut pc = postgres::Config::new();
        pc.host(&cfg.host)
            .port(cfg.port)
            .user(&cfg.user)
            .dbname(cfg.database.as_deref().unwrap_or(&cfg.user))
            .application_name("thinsql");

        if let Some(pw) = password {
            pc.password(pw);
        }

        match cfg.ssl {
            SslMode::Disabled => {
                pc.ssl_mode(postgres::config::SslMode::Disable);
            }
            SslMode::Prefer => {
                // 同步 TLS 连接器尚未接入；显式降级为明文，但要让用户知道。
                tracing::warn!(
                    host = %cfg.host,
                    port = cfg.port,
                    "PostgreSQL TLS 尚未接入，Prefer 已降级为明文连接"
                );
                pc.ssl_mode(postgres::config::SslMode::Disable);
            }
            SslMode::Required => {
                return Err(Error::unsupported(
                    "PostgreSQL TLS 尚未接入，无法满足 Required SSL 模式；请改用 Prefer/Disabled",
                ));
            }
        }

        let mut client = pc.connect(NoTls).map_err(map_pg_error)?;
        let version: Option<String> = match client.query_opt("SHOW server_version", &[]) {
            Ok(Some(row)) => match row.try_get::<usize, String>(0) {
                Ok(version) => Some(version),
                Err(err) => {
                    tracing::warn!(error = %err, "读取 PostgreSQL 版本列失败");
                    None
                }
            },
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(error = %err, "查询 PostgreSQL 版本失败");
                None
            }
        };
        self.server_version = version;
        self.client = Some(client);

        tracing::info!(host = %cfg.host, port = cfg.port, version = ?self.server_version, "PostgreSQL 已连接");
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), Error> {
        self.client = None;
        Ok(())
    }

    fn ping(&mut self) -> Result<(), Error> {
        self.client()?
            .simple_query("SELECT 1")
            .map(|_| ())
            .map_err(map_pg_error)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            escape: EscapeStyle::Standard,
            identifier_quote: '"',
            supports_multi_statement: true,
            supports_transactions: true,
            has_schema_layer: true,
            object_kinds: &[
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::MaterializedView,
                ObjectKind::Procedure,
                ObjectKind::Function,
                ObjectKind::Sequence,
                ObjectKind::Trigger,
            ],
        }
    }

    fn query(&mut self, sql: &str, page: Page, cancel: &CancelToken) -> Result<QueryResult, Error> {
        let started = Instant::now();
        let statements = crate::sql::split_statements(sql);

        let mut result = QueryResult::empty(started.elapsed());
        if statements.is_empty() {
            return Ok(result);
        }

        let mut affected_total = 0u64;
        let mut collected = false;

        for stmt in &statements {
            if provider::is_cancelled(cancel) {
                return Err(Error::cancelled());
            }

            let client = self.client()?;
            // 以 prepare 后的列信息判断是否返回结果集，代替对 SQL 文本的启发式判断。
            // 对没有列的语句（INSERT/UPDATE/DDL 等）走 execute 拿影响行数。
            let statement = client.prepare(stmt.as_str()).map_err(map_pg_error)?;

            if statement.columns().is_empty() {
                let affected = client.execute(&statement, &[]).map_err(map_pg_error)?;
                affected_total = affected_total.saturating_add(affected);
                continue;
            }

            // 多语句里只展示第一组有列的结果集，与 MySQL 的行为保持一致；
            // 后续结果集仍然要执行，只是丢弃其行，避免"后面的语句没跑"。
            if collected {
                client.execute(&statement, &[]).map_err(map_pg_error)?;
                continue;
            }

            result.columns = statement
                .columns()
                .iter()
                .map(|c| ResultColumn {
                    name: c.name().to_string(),
                    type_name: c.type_().name().to_string(),
                })
                .collect();

            let empty: std::iter::Empty<&(dyn postgres::types::ToSql + Sync)> = std::iter::empty();
            let mut rows = client.query_raw(&statement, empty).map_err(map_pg_error)?;
            let limit = page.limit as usize;

            while let Some(row) = rows.next().map_err(map_pg_error)? {
                if provider::is_cancelled(cancel) {
                    return Err(Error::cancelled());
                }
                if result.rows.len() >= limit {
                    result.has_more = true;
                    break;
                }
                let values = statement
                    .columns()
                    .iter()
                    .enumerate()
                    .map(|(i, col)| convert(col.type_(), &row, i))
                    .collect();
                result.rows.push(values);
            }
            collected = true;
        }

        if !collected {
            result.affected_rows = Some(affected_total);
        }
        result.elapsed = started.elapsed();
        Ok(result)
    }

    fn list_schemas(&mut self) -> Result<Vec<SchemaMeta>, Error> {
        let client = self.client()?;
        let rows = client
            .query(
                "SELECT nspname FROM pg_catalog.pg_namespace \
                 WHERE nspname NOT LIKE 'pg_%' AND nspname <> 'information_schema' \
                 ORDER BY nspname",
                &[],
            )
            .map_err(map_pg_error)?;
        collect_meta(&rows, "schema 列表", |row| {
            Ok(SchemaMeta {
                name: row.try_get::<usize, String>(0)?,
            })
        })
    }

    fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<TableMeta>, Error> {
        let client = self.client()?;
        let rows = client
            .query(
                "SELECT n.nspname, c.relname, c.relkind \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r','v','m','p') \
                   AND n.nspname = COALESCE($1, current_schema()) \
                 ORDER BY c.relname",
                &[&schema],
            )
            .map_err(map_pg_error)?;

        collect_meta(&rows, "表列表", |row| {
            let schema: String = row.try_get(0)?;
            let name: String = row.try_get(1)?;
            let kind: i8 = row.try_get(2)?;
            Ok(TableMeta {
                schema: Some(schema),
                name,
                kind: if kind == b'r' as i8 || kind == b'p' as i8 {
                    TableKind::Table
                } else {
                    TableKind::View
                },
                approx_rows: None,
            })
        })
    }


    fn list_objects(
        &mut self,
        schema: Option<&str>,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectMeta>, Error> {
        let client = self.client()?;
        match kind {
            ObjectKind::Table => {
                let rows = client
                    .query(
                        "SELECT n.nspname, c.relname                          FROM pg_catalog.pg_class c                          JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace                          WHERE c.relkind IN ('r','p')                            AND n.nspname = COALESCE($1, current_schema())                          ORDER BY c.relname",
                        &[&schema],
                    )
                    .map_err(map_pg_error)?;
                collect_meta(&rows, "表列表", |row| {
                    Ok(ObjectMeta {
                        schema: Some(row.try_get(0)?),
                        name: row.try_get(1)?,
                        kind: ObjectKind::Table,
                    })
                })
            }
            ObjectKind::View => {
                let rows = client
                    .query(
                        "SELECT n.nspname, c.relname                          FROM pg_catalog.pg_class c                          JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace                          WHERE c.relkind = 'v'                            AND n.nspname = COALESCE($1, current_schema())                          ORDER BY c.relname",
                        &[&schema],
                    )
                    .map_err(map_pg_error)?;
                collect_meta(&rows, "视图列表", |row| {
                    Ok(ObjectMeta {
                        schema: Some(row.try_get(0)?),
                        name: row.try_get(1)?,
                        kind: ObjectKind::View,
                    })
                })
            }
            ObjectKind::MaterializedView => {
                let rows = client
                    .query(
                        "SELECT n.nspname, c.relname                          FROM pg_catalog.pg_class c                          JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace                          WHERE c.relkind = 'm'                            AND n.nspname = COALESCE($1, current_schema())                          ORDER BY c.relname",
                        &[&schema],
                    )
                    .map_err(map_pg_error)?;
                collect_meta(&rows, "物化视图列表", |row| {
                    Ok(ObjectMeta {
                        schema: Some(row.try_get(0)?),
                        name: row.try_get(1)?,
                        kind: ObjectKind::MaterializedView,
                    })
                })
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                // prokind: f=function, p=procedure, a=aggregate, w=window
                let prokind = if kind == ObjectKind::Procedure { "p" } else { "f" };
                let rows = client
                    .query(
                        "SELECT n.nspname, p.proname                          FROM pg_catalog.pg_proc p                          JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace                          WHERE p.prokind = $2                            AND n.nspname = COALESCE($1, current_schema())                          ORDER BY p.proname",
                        &[&schema, &prokind],
                    )
                    .map_err(map_pg_error)?;
                collect_meta(&rows, "例程列表", |row| {
                    Ok(ObjectMeta {
                        schema: Some(row.try_get(0)?),
                        name: row.try_get(1)?,
                        kind,
                    })
                })
            }
            ObjectKind::Sequence => {
                let rows = client
                    .query(
                        "SELECT schemaname, sequencename                          FROM pg_catalog.pg_sequences                          WHERE schemaname = COALESCE($1, current_schema())                          ORDER BY sequencename",
                        &[&schema],
                    )
                    .map_err(map_pg_error)?;
                collect_meta(&rows, "序列列表", |row| {
                    Ok(ObjectMeta {
                        schema: Some(row.try_get(0)?),
                        name: row.try_get(1)?,
                        kind: ObjectKind::Sequence,
                    })
                })
            }
            ObjectKind::Trigger => {
                let rows = client
                    .query(
                        "SELECT trigger_schema, trigger_name                          FROM information_schema.triggers                          WHERE trigger_schema = COALESCE($1, current_schema())                          ORDER BY trigger_name",
                        &[&schema],
                    )
                    .map_err(map_pg_error)?;
                // information_schema may duplicate trigger names per event; uniquify
                let mut seen = std::collections::BTreeSet::new();
                let mut out = Vec::new();
                for row in &rows {
                    let schema_name: String = row.try_get(0).map_err(map_pg_error)?;
                    let name: String = row.try_get(1).map_err(map_pg_error)?;
                    if seen.insert((schema_name.clone(), name.clone())) {
                        out.push(ObjectMeta {
                            schema: Some(schema_name),
                            name,
                            kind: ObjectKind::Trigger,
                        });
                    }
                }
                Ok(out)
            }
            ObjectKind::Event => Ok(Vec::new()),
        }
    }

    fn set_schema(&mut self, schema: &str) -> Result<(), Error> {
        let q = self.capabilities().quote_identifier(schema);
        // 当前 schema 优先，保留 public 作为回退
        let sql = format!("SET search_path TO {q}, public");
        self.client()?
            .execute(sql.as_str(), &[])
            .map(|_| ())
            .map_err(map_pg_error)
    }

    fn list_columns(&mut self, table: &TableRef) -> Result<Vec<ColumnMeta>, Error> {
        let client = self.client()?;
        let rows = client
            .query(
                "SELECT c.column_name, c.data_type, c.is_nullable, c.column_default, \
                        EXISTS ( \
                          SELECT 1 FROM information_schema.table_constraints tc \
                          JOIN information_schema.key_column_usage k \
                            ON k.constraint_name = tc.constraint_name \
                           AND k.table_schema = tc.table_schema \
                           AND k.table_name = tc.table_name \
                          WHERE tc.constraint_type = 'PRIMARY KEY' \
                            AND tc.table_schema = c.table_schema \
                            AND tc.table_name = c.table_name \
                            AND k.column_name = c.column_name \
                        ) AS is_pk \
                 FROM information_schema.columns c \
                 WHERE c.table_schema = COALESCE($1, current_schema()) AND c.table_name = $2 \
                 ORDER BY c.ordinal_position",
                &[&table.schema.as_deref(), &table.name],
            )
            .map_err(map_pg_error)?;

        collect_meta(&rows, "列信息", |row| {
            let name: String = row.try_get(0)?;
            let type_name: String = row.try_get(1)?;
            let nullable: String = row.try_get(2)?;
            let default: Option<String> = row.try_get(3)?;
            let is_pk: bool = row.try_get(4)?;
            let is_serial = default
                .as_deref()
                .map(|d| d.starts_with("nextval("))
                .unwrap_or(false);
            Ok(ColumnMeta {
                name,
                type_name,
                nullable: nullable.eq_ignore_ascii_case("YES"),
                default,
                is_primary_key: is_pk,
                is_auto_increment: is_serial,
                comment: None,
                charset: None,
                collation: None,
            })
        })
    }

    fn list_indexes(&mut self, table: &TableRef) -> Result<Vec<IndexMeta>, Error> {
        let client = self.client()?;
        let rows = client
            .query(
                "SELECT i.relname, ix.indisunique, \
                        array_to_string(ARRAY( \
                          SELECT a.attname FROM pg_catalog.pg_attribute a \
                          WHERE a.attrelid = t.oid AND a.attnum = ANY(ix.indkey) \
                        ), ',') AS cols \
                 FROM pg_catalog.pg_class t \
                 JOIN pg_catalog.pg_index ix ON t.oid = ix.indrelid \
                 JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace \
                 WHERE n.nspname = COALESCE($1, current_schema()) AND t.relname = $2 \
                 ORDER BY i.relname",
                &[&table.schema.as_deref(), &table.name],
            )
            .map_err(map_pg_error)?;

        collect_meta(&rows, "索引列表", |row| {
            let name: String = row.try_get(0)?;
            let unique: bool = row.try_get(1)?;
            let cols: Option<String> = row.try_get(2)?;
            Ok(IndexMeta {
                name,
                columns: cols
                    .unwrap_or_default()
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect(),
                unique,
            })
        })
    }

    fn list_foreign_keys(&mut self, table: &TableRef) -> Result<Vec<ForeignKeyMeta>, Error> {
        let client = self.client()?;
        let rows = client
            .query(
                "SELECT tc.constraint_name, kcu.column_name, \
                        ccu.table_name AS ref_table, ccu.column_name AS ref_column \
                 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON kcu.constraint_name = tc.constraint_name \
                  AND kcu.table_schema = tc.table_schema \
                 JOIN information_schema.constraint_column_usage ccu \
                   ON ccu.constraint_name = tc.constraint_name \
                  AND ccu.table_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'FOREIGN KEY' \
                   AND tc.table_schema = COALESCE($1, current_schema()) \
                   AND tc.table_name = $2 \
                 ORDER BY tc.constraint_name",
                &[&table.schema.as_deref(), &table.name],
            )
            .map_err(map_pg_error)?;

        let rows = collect_meta(&rows, "外键列表", |row| {
            Ok((
                row.try_get::<usize, String>(0)?,
                row.try_get::<usize, String>(1)?,
                row.try_get::<usize, String>(2)?,
                row.try_get::<usize, String>(3)?,
            ))
        })?;

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

    fn apply_changes(
        &mut self,
        table: &TableRef,
        changes: &[RowChange],
    ) -> Result<ApplyOutcome, Error> {
        if changes.is_empty() {
            return Ok(ApplyOutcome { affected: 0 });
        }

        let client = self.client()?;
        let mut tx = client.transaction().map_err(map_pg_error)?;
        let mut affected = 0u64;

        for change in changes {
            if change.key().is_empty() {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "该表没有主键，无法安全地定位要修改的行",
                ));
            }
            // 与界面预览共用同一生成函数
            let sql = crate::edit::build_sql(DriverKind::Postgres, table, change);
            tracing::debug!(sql = %sql, "执行行内更新");
            affected += tx.execute(&sql, &[]).map_err(map_pg_error)?;
        }

        tx.commit().map_err(map_pg_error)?;
        Ok(ApplyOutcome { affected })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric_raw(weight: i16, sign: u16, dscale: u16, digits: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(digits.len() as i16).to_be_bytes());
        out.extend_from_slice(&weight.to_be_bytes());
        out.extend_from_slice(&sign.to_be_bytes());
        out.extend_from_slice(&dscale.to_be_bytes());
        for digit in digits {
            out.extend_from_slice(&digit.to_be_bytes());
        }
        out
    }

    #[test]
    fn malformed_numeric_keeps_raw_bytes() {
        let raw = [0u8, 1];
        let parsed = RawNumeric::from_sql(&Type::NUMERIC, &raw).expect("from_sql 不应失败");
        assert_eq!(
            parsed.0,
            Err(raw.to_vec()),
            "解码失败必须保留原始字节，不能静默变成空串"
        );
    }

    #[test]
    fn valid_numeric_still_decodes() {
        let raw = numeric_raw(0, 0x0000, 2, &[12, 3000]);
        let parsed = RawNumeric::from_sql(&Type::NUMERIC, &raw).expect("from_sql 不应失败");
        assert_eq!(parsed.0, Ok("12.30".to_string()));
    }

    #[test]
    fn hex_format_is_lowercase_and_stable() {
        assert_eq!(to_hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(to_hex(&[]), "");
    }
}
