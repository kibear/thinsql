//! 驱动抽象：UI 与数据库之间唯一的契约。
//!
//! 所有方法都是同步的，由 [`crate::session`] 里的专用工作线程调用，
//! 因此实现里可以直接做阻塞 I/O，不需要 async 运行时。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, ErrorKind};
use crate::model::{
    ApplyOutcome, ColumnMeta, ConnectionConfig, DriverKind, ForeignKeyMeta, IndexMeta, Page,
    ObjectKind, ObjectMeta, QueryResult, RowChange, SchemaMeta, StructureChange, TableMeta,
    TableRef,
};
pub use crate::value::EscapeStyle;

/// 取消标志。UI 线程直接置位，工作线程在行之间检查。
pub type CancelToken = Arc<AtomicBool>;

pub fn new_cancel_token() -> CancelToken {
    Arc::new(AtomicBool::new(false))
}

pub fn is_cancelled(token: &CancelToken) -> bool {
    token.load(Ordering::Relaxed)
}

pub fn reset_cancel(token: &CancelToken) {
    token.store(false, Ordering::Relaxed);
}

pub fn request_cancel(token: &CancelToken) {
    token.store(true, Ordering::Relaxed);
}

/// 一个数据库方言的能力声明。公共层据此生成标识符与字面量，不做 `if mysql` 分支。
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    /// 字符串字面量转义风格
    pub escape: EscapeStyle,
    /// 标识符引用符：MySQL 用反引号，标准 SQL 用双引号
    pub identifier_quote: char,
    pub supports_multi_statement: bool,
    pub supports_transactions: bool,
    /// 是否有 schema 层（PostgreSQL/Oracle 有，MySQL 没有）
    pub has_schema_layer: bool,
    /// 侧栏可展示的对象分组（方言声明）
    pub object_kinds: &'static [ObjectKind],
}

impl Capabilities {
    /// 把标识符安全地引用起来。
    pub fn quote_identifier(&self, name: &str) -> String {
        let q = self.identifier_quote;
        let escaped = name.replace(q, &format!("{q}{q}"));
        format!("{q}{escaped}{q}")
    }

    pub fn quote_table(&self, table: &TableRef) -> String {
        match &table.schema {
            Some(schema) => format!(
                "{}.{}",
                self.quote_identifier(schema),
                self.quote_identifier(&table.name)
            ),
            None => self.quote_identifier(&table.name),
        }
    }
}

pub trait DatabaseProvider: Send {
    /// 建立连接。密码单独传入而不是放在 `ConnectionConfig` 里，
    /// 是为了让配置对象可以安全地序列化、打印、落盘。
    fn connect(&mut self, cfg: &ConnectionConfig, password: Option<&str>) -> Result<(), Error>;
    fn disconnect(&mut self) -> Result<(), Error>;
    fn ping(&mut self) -> Result<(), Error>;
    fn capabilities(&self) -> Capabilities;

    /// 执行任意 SQL。驱动负责在多语句、无结果集语句之间做出正确行为。
    fn query(&mut self, sql: &str, page: Page, cancel: &CancelToken) -> Result<QueryResult, Error>;

    fn list_schemas(&mut self) -> Result<Vec<SchemaMeta>, Error>;
    fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<TableMeta>, Error>;
    /// 按对象种类列出侧栏分组内容。不支持的 kind 返回空列表或 Unsupported。
    fn list_objects(
        &mut self,
        schema: Option<&str>,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectMeta>, Error>;
    /// 切换会话默认库/schema：MySQL 用 USE，PostgreSQL 用 search_path。
    fn set_schema(&mut self, schema: &str) -> Result<(), Error>;
    fn list_columns(&mut self, table: &TableRef) -> Result<Vec<ColumnMeta>, Error>;
    fn list_indexes(&mut self, table: &TableRef) -> Result<Vec<IndexMeta>, Error>;
    fn list_foreign_keys(&mut self, table: &TableRef) -> Result<Vec<ForeignKeyMeta>, Error>;

    /// 按主键回写若干单元格改动。实现必须在一个事务内完成，失败整体回滚。
    fn apply_changes(
        &mut self,
        table: &TableRef,
        changes: &[RowChange],
    ) -> Result<ApplyOutcome, Error>;

    /// 读取建表语句。不支持的驱动返回 `Unsupported`。
    fn table_ddl(&mut self, _table: &TableRef) -> Result<String, Error> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "当前驱动不支持读取表 DDL",
        ))
    }

    /// 把结构变更翻译成方言 SQL（仅预览，不执行）。
    fn generate_alter(
        &mut self,
        _table: &TableRef,
        _change: &StructureChange,
    ) -> Result<Vec<String>, Error> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "当前驱动不支持修改表结构",
        ))
    }

    /// 生成并执行结构变更。默认实现逐条执行预览里的语句，
    /// 由驱动按需覆盖（例如需要在事务里做多步校验时）。
    fn apply_structure(&mut self, table: &TableRef, change: &StructureChange) -> Result<(), Error> {
        let sqls = self.generate_alter(table, change)?;
        for sql in sqls {
            let _ = self.query(&sql, Page::first(), &new_cancel_token())?;
        }
        Ok(())
    }
}

/// 驱动注册表。新增数据库：在 `drivers` 下加一个文件，然后在这里加一行。
pub struct Registry;

impl Registry {
    pub fn create(kind: DriverKind) -> Result<Box<dyn DatabaseProvider>, Error> {
        crate::drivers::create(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mysql_caps() -> Capabilities {
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

    #[test]
    fn identifier_quoting_escapes_embedded_quote() {
        let caps = mysql_caps();
        assert_eq!(caps.quote_identifier("users"), "`users`");
        assert_eq!(caps.quote_identifier("we`ird"), "`we``ird`");
    }

    #[test]
    fn table_quoting_includes_schema_when_present() {
        let caps = mysql_caps();
        assert_eq!(caps.quote_table(&TableRef::new(None, "t")), "`t`");
        assert_eq!(
            caps.quote_table(&TableRef::new(Some("s".into()), "t")),
            "`s`.`t`"
        );
    }

    #[test]
    fn cancel_token_is_observable_across_threads() {
        let token = new_cancel_token();
        assert!(!is_cancelled(&token));
        let t = token.clone();
        std::thread::spawn(move || request_cancel(&t))
            .join()
            .unwrap();
        assert!(is_cancelled(&token));
        reset_cancel(&token);
        assert!(!is_cancelled(&token));
    }
}
