//! ThinSQL 核心层：与 UI 完全解耦的连接、元数据、查询与配置能力。
//!
//! 设计约束（来自 Phase 0 结论）：
//! - 所有驱动相关语法差异收敛在 `drivers` 目录，公共层不得出现 `if mysql` 这类分支。
//! - 全部接口同步、由专用工作线程调用，不引入 async 运行时。
//! - 对外只暴露统一的 [`Value`] 值模型，UI 不需要认识任何数据库原生类型。

pub mod config;
pub mod drivers;
pub mod edit;
pub mod error;
pub mod export;
pub mod history;
pub mod model;
pub mod provider;
pub mod session;
pub mod sql;
pub mod value;

pub use drivers::{
    common_charsets, common_collations, common_column_types, default_column_type,
    default_length_for_type, navigator_object_kinds, type_allows_auto_increment, type_uses_charset,
    type_uses_length,
};
pub use edit::{build_delete, build_sql, build_sqls, build_structure_sql};
pub use error::{Error, ErrorKind, ServerError};
pub use history::{History, HistoryEntry};
pub use model::{
    ApplyOutcome, ColumnMeta, ColumnOp, ColumnSpec, ConnectionConfig, DriverKind, ForeignKeyMeta,
    IndexMeta, IndexOp, ObjectKind, ObjectMeta, Page, QueryResult, ResultColumn, RowChange,
    SchemaMeta, SslMode, StructureChange, TableKind, TableMeta, TableRef, table_preview_sql,
};
pub use provider::{Capabilities, DatabaseProvider, EscapeStyle, Registry};
pub use value::Value;
