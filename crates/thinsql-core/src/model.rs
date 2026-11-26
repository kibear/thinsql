//! 与具体数据库无关的数据模型。UI 只认识这里的类型。

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::value::Value;

/// 支持的数据库种类。新增库时在这里加一项，并在 `drivers` 下注册。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    MySql,
    Postgres,
    Oracle,
}

impl DriverKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::MySql => "MySQL",
            Self::Postgres => "PostgreSQL",
            Self::Oracle => "Oracle",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Self::MySql => 3306,
            Self::Postgres => 5432,
            Self::Oracle => 1521,
        }
    }

    /// 该库的"库/模式"概念是两层还是一层。
    /// MySQL 里 database 与 schema 等价，PostgreSQL 是 database 下再分 schema。
    pub fn has_schema_layer(self) -> bool {
        matches!(self, Self::Postgres | Self::Oracle)
    }

    /// 标识符引用符：MySQL 用反引号，标准 SQL 用双引号。
    pub fn identifier_quote(self) -> char {
        match self {
            Self::MySql => '`',
            Self::Postgres | Self::Oracle => '"',
        }
    }
}

fn quote_ident(name: &str, quote: char) -> String {
    format!(
        "{quote}{}{quote}",
        name.replace(quote, &format!("{quote}{quote}"))
    )
}

/// 生成"预览整张表"的 SQL。分页由我们控制，因此可以安全地 LIMIT/OFFSET，
/// 不需要把用户手写的语句包成子查询。
///
/// `filter` 是不含 `WHERE` 关键字的 SQL 片段（空或空白视为无筛选），
/// 由调用方原样嵌入——与 DBeaver 滤镜框同级，不做 AST 校验。
pub fn table_preview_sql(
    kind: DriverKind,
    table: &TableRef,
    page: Page,
    filter: Option<&str>,
) -> String {
    let q = kind.identifier_quote();
    let target = match &table.schema {
        Some(schema) => format!("{}.{}", quote_ident(schema, q), quote_ident(&table.name, q)),
        None => quote_ident(&table.name, q),
    };
    let where_clause = match filter.map(str::trim).filter(|s| !s.is_empty()) {
        Some(expr) => format!(" WHERE {expr}"),
        None => String::new(),
    };
    format!(
        "SELECT * FROM {target}{where_clause} LIMIT {} OFFSET {}",
        page.limit, page.offset
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SslMode {
    /// 服务器支持就用，不支持则明文（开发环境常用）
    #[default]
    Prefer,
    Disabled,
    Required,
}

/// 一个连接的全部配置。
///
/// 注意：**这里永远不放明文密码**，只放系统钥匙串里的引用键。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub kind: DriverKind,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default)]
    pub ssl: SslMode,
    /// 连接颜色标记，用于区分生产/测试等多环境
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// 默认只读，避免误改数据（编辑功能需要显式关闭）
    #[serde(default)]
    pub read_only: bool,
}

impl ConnectionConfig {
    pub fn new(id: impl Into<String>, name: impl Into<String>, kind: DriverKind) -> Self {
        let id = id.into();
        Self {
            secret_ref: Some(secret_ref_for(&id)),
            id,
            name: name.into(),
            kind,
            host: "127.0.0.1".to_string(),
            port: kind.default_port(),
            user: String::new(),
            database: None,
            ssl: SslMode::default(),
            color: None,
            read_only: true,
        }
    }
}

/// 钥匙串条目的引用键，和连接 id 绑定。
pub fn secret_ref_for(connection_id: &str) -> String {
    format!("thinsql/{connection_id}")
}

/// 表/视图的定位信息。schema 为 None 表示使用连接默认库。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    pub fn new(schema: Option<String>, name: impl Into<String>) -> Self {
        Self {
            schema,
            name: name.into(),
        }
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.schema {
            Some(s) => write!(f, "{s}.{}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaMeta {
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableKind {
    Table,
    View,
}

/// 侧栏对象分组种类。方言通过 Capabilities 声明支持哪些。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Procedure,
    Function,
    Sequence,
    Trigger,
    Event,
}

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::MaterializedView => "matview",
            Self::Procedure => "procedure",
            Self::Function => "function",
            Self::Sequence => "sequence",
            Self::Trigger => "trigger",
            Self::Event => "event",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "table" => Some(Self::Table),
            "view" => Some(Self::View),
            "matview" => Some(Self::MaterializedView),
            "procedure" => Some(Self::Procedure),
            "function" => Some(Self::Function),
            "sequence" => Some(Self::Sequence),
            "trigger" => Some(Self::Trigger),
            "event" => Some(Self::Event),
            _ => None,
        }
    }

    /// 可走表数据预览的对象（表/视图/物化视图）。
    pub fn is_relation(self) -> bool {
        matches!(self, Self::Table | Self::View | Self::MaterializedView)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMeta {
    pub schema: Option<String>,
    pub name: String,
    pub kind: TableKind,
    /// 大致行数，取不到时为 None（不要为此做全表 count）
    pub approx_rows: Option<u64>,
}

/// 侧栏对象节点的轻量元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    /// 数据库原生类型名，原样展示
    pub type_name: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub is_primary_key: bool,
    /// 自增/序列列，编辑时需要写回
    pub is_auto_increment: bool,
    /// MySQL 的列注释；不支持注释的驱动填 None
    pub comment: Option<String>,
    /// MySQL 的列字符集；取不到填 None
    pub charset: Option<String>,
    /// MySQL 的列排序规则；取不到填 None
    pub collation: Option<String>,
}

/// 表结构编辑里的列定义。UI 改动先落到这份结构上，
/// 再由驱动翻译成对应方言的 ALTER 语句。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    pub name: String,
    /// 原生类型名，如 varchar / int / decimal
    pub data_type: String,
    /// 类型参数：字符串长度，或 `精度,小数位`
    pub length: Option<String>,
    pub nullable: bool,
    pub primary_key: bool,
    /// 原始表达式或字面量，由驱动决定是否加引号
    pub default: Option<String>,
    pub comment: Option<String>,
    pub charset: Option<String>,
    pub collation: Option<String>,
    pub auto_increment: bool,
}

impl ColumnSpec {
    /// 带长度参数的完整类型名：`varchar(64)` / `decimal(10,2)`。
    pub fn type_sql(&self) -> String {
        match self.length.as_deref() {
            Some(length) if !length.trim().is_empty() => {
                format!("{}({})", self.data_type, length.trim())
            }
            _ => self.data_type.clone(),
        }
    }
}

/// 一条列结构变更。`old_primary_key` 让驱动能生成正确的主键增删语句。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnOp {
    Add {
        column: ColumnSpec,
    },
    Alter {
        old_name: String,
        old_primary_key: bool,
        column: ColumnSpec,
    },
    Drop {
        name: String,
    },
}

/// 一条索引变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexOp {
    Create {
        name: String,
        columns: Vec<String>,
        unique: bool,
    },
    Drop {
        name: String,
    },
}

/// 一次表结构编辑的暂存集合。顺序：删索引 → 列变更 → 建索引。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructureChange {
    pub column_ops: Vec<ColumnOp>,
    pub index_ops: Vec<IndexOp>,
}

impl StructureChange {
    pub fn is_empty(&self) -> bool {
        self.column_ops.is_empty() && self.index_ops.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyMeta {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

/// 分页参数。默认只取一页，绝不静默全量拉取。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub offset: u64,
    pub limit: u32,
}

impl Page {
    pub const DEFAULT_LIMIT: u32 = 500;

    pub fn first() -> Self {
        Self {
            offset: 0,
            limit: Self::DEFAULT_LIMIT,
        }
    }

    pub fn with_limit(limit: u32) -> Self {
        Self { offset: 0, limit }
    }

    /// 下一页；行数不足一页说明到底了。
    pub fn next(&self) -> Self {
        Self {
            offset: self.offset + self.limit as u64,
            limit: self.limit,
        }
    }

    /// 多取一行的上限，"多取的那一行"用来判断是否还有更多。
    pub fn probe_limit(&self) -> u32 {
        self.limit.saturating_add(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultColumn {
    pub name: String,
    pub type_name: String,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Value>>,
    pub elapsed: Duration,
    /// INSERT/UPDATE/DELETE 的影响行数；查询语句为 None
    pub affected_rows: Option<u64>,
    /// 是否还有更多数据（由 provider 多取一行判断）
    pub has_more: bool,
}

impl QueryResult {
    pub fn empty(elapsed: Duration) -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            elapsed,
            affected_rows: None,
            has_more: false,
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

/// 一条行级改动：`Update` 修改某行某列，`Delete` 删除整行。
/// `key` 是定位该行的主键列值；两种形态都必须能唯一定位一行。
#[derive(Debug, Clone, PartialEq)]
pub enum RowChange {
    Update {
        key: Vec<(String, Value)>,
        column: String,
        new_value: Value,
    },
    Delete {
        key: Vec<(String, Value)>,
    },
}

impl RowChange {
    /// 定位主键值。预览、执行、去重逻辑都只关心 key，不区分改动类型。
    pub fn key(&self) -> &[(String, Value)] {
        match self {
            Self::Update { key, .. } | Self::Delete { key } => key,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub affected: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_advances_by_limit() {
        let page = Page::first();
        assert_eq!(page.offset, 0);
        assert_eq!(page.probe_limit(), page.limit + 1);

        let next = page.next();
        assert_eq!(next.offset, page.limit as u64);
        assert_eq!(next.limit, page.limit);
    }

    #[test]
    fn connection_defaults_to_read_only_and_derives_secret_ref() {
        let cfg = ConnectionConfig::new("c1", "本地", DriverKind::MySql);
        assert!(cfg.read_only);
        assert_eq!(cfg.port, 3306);
        assert_eq!(cfg.secret_ref.as_deref(), Some("thinsql/c1"));
    }

    #[test]
    fn preview_sql_quotes_per_dialect_and_pages() {
        let table = TableRef::new(Some("db".into()), "users");
        let sql = table_preview_sql(DriverKind::MySql, &table, Page::with_limit(50), None);
        assert_eq!(sql, "SELECT * FROM `db`.`users` LIMIT 50 OFFSET 0");

        let sql = table_preview_sql(
            DriverKind::Postgres,
            &table,
            Page {
                offset: 100,
                limit: 50,
            },
            None,
        );
        assert_eq!(sql, "SELECT * FROM \"db\".\"users\" LIMIT 50 OFFSET 100");
    }

    #[test]
    fn preview_sql_escapes_embedded_quote_in_identifier() {
        let table = TableRef::new(None, "we`ird");
        let sql = table_preview_sql(DriverKind::MySql, &table, Page::first(), None);
        assert!(sql.contains("`we``ird`"), "反引号必须翻倍，实际: {sql}");
    }

    #[test]
    fn preview_sql_inserts_where_when_filter_present() {
        let table = TableRef::new(Some("db".into()), "users");
        let sql = table_preview_sql(
            DriverKind::MySql,
            &table,
            Page::with_limit(50),
            Some("status = 1 AND name LIKE '%a%'"),
        );
        assert_eq!(
            sql,
            "SELECT * FROM `db`.`users` WHERE status = 1 AND name LIKE '%a%' LIMIT 50 OFFSET 0"
        );
    }

    #[test]
    fn preview_sql_ignores_blank_filter() {
        let table = TableRef::new(None, "t");
        let sql = table_preview_sql(DriverKind::Postgres, &table, Page::with_limit(10), Some("  "));
        assert_eq!(sql, "SELECT * FROM \"t\" LIMIT 10 OFFSET 0");
        let sql = table_preview_sql(DriverKind::Postgres, &table, Page::with_limit(10), Some(""));
        assert_eq!(sql, "SELECT * FROM \"t\" LIMIT 10 OFFSET 0");
    }

    #[test]
    fn table_ref_display_includes_schema_when_present() {
        assert_eq!(TableRef::new(None, "users").to_string(), "users");
        assert_eq!(
            TableRef::new(Some("public".into()), "users").to_string(),
            "public.users"
        );
    }
}
