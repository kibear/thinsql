//! 各数据库的具体实现。新增数据库只允许新增一个文件 + 在 `create` 里加一行，
//! 不允许在公共层出现数据库专有分支。

use crate::error::Error;
use crate::model::{DriverKind, ObjectKind};
use crate::provider::DatabaseProvider;

/// 侧栏导航展示的对象分组（与各驱动 Capabilities.object_kinds 保持一致）。
pub fn navigator_object_kinds(kind: DriverKind) -> &'static [ObjectKind] {
    match kind {
        DriverKind::MySql => &[
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::Procedure,
            ObjectKind::Function,
            ObjectKind::Trigger,
            ObjectKind::Event,
        ],
        DriverKind::Postgres => &[
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
            ObjectKind::Procedure,
            ObjectKind::Function,
            ObjectKind::Sequence,
            ObjectKind::Trigger,
        ],
        DriverKind::Oracle => &[],
    }
}


pub mod mysql;
pub mod pg_numeric;
pub mod postgres;

/// 按种类创建驱动实例（尚未连接）。
pub fn create(kind: DriverKind) -> Result<Box<dyn DatabaseProvider>, Error> {
    match kind {
        DriverKind::MySql => Ok(Box::new(mysql::MySqlProvider::new())),
        DriverKind::Postgres => Ok(Box::new(postgres::PostgresProvider::new())),
        DriverKind::Oracle => Err(Error::unsupported("Oracle 驱动将在二期接入")),
    }
}

/// 新建/编辑列时可选的常用数据类型（不含长度，长度走独立 Length 栏）。
pub fn common_column_types(kind: DriverKind) -> &'static [&'static str] {
    match kind {
        DriverKind::MySql => &[
            "INT",
            "BIGINT",
            "TINYINT",
            "VARCHAR",
            "CHAR",
            "TEXT",
            "LONGTEXT",
            "DECIMAL",
            "FLOAT",
            "DOUBLE",
            "BOOLEAN",
            "DATE",
            "DATETIME",
            "TIMESTAMP",
            "JSON",
            "BLOB",
        ],
        DriverKind::Postgres => &[
            "integer",
            "bigint",
            "smallint",
            "varchar",
            "char",
            "text",
            "numeric",
            "real",
            "double precision",
            "boolean",
            "date",
            "timestamp",
            "timestamptz",
            "jsonb",
            "bytea",
            "uuid",
        ],
        DriverKind::Oracle => &[
            "NUMBER",
            "VARCHAR2",
            "CHAR",
            "CLOB",
            "DATE",
            "TIMESTAMP",
            "BLOB",
            "RAW",
        ],
    }
}

/// 新建列时的默认数据类型。
pub fn default_column_type(kind: DriverKind) -> &'static str {
    match kind {
        DriverKind::MySql => "VARCHAR",
        DriverKind::Postgres => "varchar",
        DriverKind::Oracle => "VARCHAR2",
    }
}

/// 列编辑器可选的常用字符集。空切片表示该驱动不展示 charset UI。
pub fn common_charsets(kind: DriverKind) -> &'static [&'static str] {
    match kind {
        DriverKind::MySql => &["utf8mb4", "utf8", "latin1", "ascii", "binary"],
        DriverKind::Postgres | DriverKind::Oracle => &[],
    }
}

/// 指定字符集下可选的常用排序规则。空切片表示无（或 charset 为空）。
pub fn common_collations(kind: DriverKind, charset: &str) -> &'static [&'static str] {
    if kind != DriverKind::MySql {
        return &[];
    }
    match charset.trim().to_ascii_lowercase().as_str() {
        "utf8mb4" => &[
            "utf8mb4_unicode_ci",
            "utf8mb4_general_ci",
            "utf8mb4_bin",
            "utf8mb4_0900_ai_ci",
        ],
        "utf8" => &["utf8_unicode_ci", "utf8_general_ci", "utf8_bin"],
        "latin1" => &["latin1_swedish_ci", "latin1_general_ci", "latin1_bin"],
        "ascii" => &["ascii_general_ci", "ascii_bin"],
        "binary" => &["binary"],
        _ => &[],
    }
}

/// 该类型是否支持列级字符集 / 排序规则。
pub fn type_uses_charset(kind: DriverKind, data_type: &str) -> bool {
    let ty = base_type(data_type);
    match kind {
        DriverKind::MySql => matches!(
            ty.as_str(),
            "char"
                | "varchar"
                | "tinytext"
                | "text"
                | "mediumtext"
                | "longtext"
                | "enum"
                | "set"
        ),
        DriverKind::Postgres => matches!(ty.as_str(), "char" | "character" | "varchar" | "text"),
        DriverKind::Oracle => matches!(ty.as_str(), "char" | "varchar2" | "nvarchar2" | "clob"),
    }
}

/// 该类型是否需要 Length / 精度参数栏。
pub fn type_uses_length(kind: DriverKind, data_type: &str) -> bool {
    let ty = base_type(data_type);
    match kind {
        DriverKind::MySql => matches!(
            ty.as_str(),
            "char"
                | "varchar"
                | "binary"
                | "varbinary"
                | "decimal"
                | "numeric"
                | "float"
                | "double"
                | "bit"
        ),
        DriverKind::Postgres => matches!(
            ty.as_str(),
            "char" | "character" | "varchar" | "numeric" | "decimal" | "bit" | "bit varying"
        ),
        DriverKind::Oracle => {
            matches!(ty.as_str(), "char" | "varchar2" | "nvarchar2" | "number" | "raw")
        }
    }
}

/// 该类型是否允许自增 / serial。
pub fn type_allows_auto_increment(kind: DriverKind, data_type: &str) -> bool {
    let ty = base_type(data_type);
    match kind {
        DriverKind::MySql => matches!(
            ty.as_str(),
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint"
        ),
        DriverKind::Postgres => {
            matches!(ty.as_str(), "smallint" | "integer" | "bigint" | "serial" | "bigserial")
        }
        DriverKind::Oracle => matches!(ty.as_str(), "number"),
    }
}

/// 切换类型时的建议 Length；无参数类型返回 `None`。
pub fn default_length_for_type(kind: DriverKind, data_type: &str) -> Option<&'static str> {
    let ty = base_type(data_type);
    match kind {
        DriverKind::MySql => match ty.as_str() {
            "varchar" => Some("64"),
            "char" => Some("1"),
            "decimal" | "numeric" => Some("10,2"),
            "binary" | "varbinary" => Some("64"),
            "float" | "double" | "bit" => None,
            _ => None,
        },
        DriverKind::Postgres => match ty.as_str() {
            "varchar" => Some("64"),
            "char" | "character" => Some("1"),
            "numeric" | "decimal" => Some("10,2"),
            _ => None,
        },
        DriverKind::Oracle => match ty.as_str() {
            "varchar2" | "nvarchar2" => Some("64"),
            "char" => Some("1"),
            "number" => Some("10,2"),
            "raw" => Some("16"),
            _ => None,
        },
    }
}

fn base_type(data_type: &str) -> String {
    data_type
        .split_once('(')
        .map(|(base, _)| base)
        .unwrap_or(data_type)
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_column_types_cover_mysql_and_postgres() {
        assert!(common_column_types(DriverKind::MySql).contains(&"VARCHAR"));
        assert!(common_column_types(DriverKind::Postgres).contains(&"varchar"));
        assert_eq!(default_column_type(DriverKind::MySql), "VARCHAR");
    }

    #[test]
    fn charset_helpers_match_driver_capabilities() {
        assert!(common_charsets(DriverKind::MySql).contains(&"utf8mb4"));
        assert!(common_charsets(DriverKind::Postgres).is_empty());
        assert!(!common_collations(DriverKind::MySql, "utf8mb4").is_empty());
        assert!(common_collations(DriverKind::MySql, "unknown").is_empty());
        assert!(type_uses_charset(DriverKind::MySql, "VARCHAR"));
        assert!(!type_uses_charset(DriverKind::MySql, "INT"));
        assert!(type_allows_auto_increment(DriverKind::MySql, "BIGINT"));
        assert!(!type_allows_auto_increment(DriverKind::MySql, "VARCHAR"));
        assert_eq!(
            default_length_for_type(DriverKind::MySql, "VARCHAR"),
            Some("64")
        );
        assert!(type_uses_length(DriverKind::MySql, "DECIMAL"));
        assert!(!type_uses_length(DriverKind::MySql, "TEXT"));
    }
}
