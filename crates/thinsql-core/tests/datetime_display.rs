//! MySQL 日期/时间列在文本协议下应显示为可读标准格式，而不是 hex。
//!
//! ```bash
//! THINSQL_TEST_MYSQL=1 THINSQL_TEST_MYSQL_PASS=root \
//!   cargo test -p thinsql-core --test datetime_display_probe -- --nocapture
//! ```
use std::sync::atomic::AtomicBool;

use thinsql_core::model::{ConnectionConfig, DriverKind, Page};
use thinsql_core::provider::{CancelToken, Registry};
use thinsql_core::Value;

fn token() -> CancelToken {
    std::sync::Arc::new(AtomicBool::new(false))
}

fn test_config() -> Option<(ConnectionConfig, String)> {
    if std::env::var("THINSQL_TEST_MYSQL").ok().as_deref() != Some("1") {
        eprintln!("跳过：未设置 THINSQL_TEST_MYSQL=1");
        return None;
    }
    let mut cfg = ConnectionConfig::new("it", "datetime", DriverKind::MySql);
    cfg.host = std::env::var("THINSQL_TEST_MYSQL_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    cfg.port = std::env::var("THINSQL_TEST_MYSQL_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3306);
    cfg.user = std::env::var("THINSQL_TEST_MYSQL_USER").unwrap_or_else(|_| "root".into());
    let pass = std::env::var("THINSQL_TEST_MYSQL_PASS").unwrap_or_else(|_| "root".into());
    Some((cfg, pass))
}

#[test]
fn mysql_temporal_columns_display_as_readable_text() {
    let Some((cfg, password)) = test_config() else {
        return;
    };
    let mut provider = Registry::create(DriverKind::MySql).expect("创建驱动");
    provider.connect(&cfg, Some(&password)).expect("连接失败");

    let result = provider
        .query(
            "SELECT \
                CAST('2024-01-15' AS DATE) AS d, \
                CAST('2024-01-15 13:45:30' AS DATETIME) AS dt, \
                CAST('2024-01-15 13:45:30.123456' AS DATETIME(6)) AS dt6, \
                TIME('13:45:30') AS t, \
                FROM_UNIXTIME(1700000000) AS from_unix",
            Page::first(),
            &token(),
        )
        .expect("查询失败");

    let row = &result.rows[0];
    for (i, col) in result.columns.iter().enumerate() {
        let shown = row[i].display();
        assert!(
            !shown.starts_with("0x"),
            "列 {} 不应显示为 hex，实际: {shown} / {:?}",
            col.name,
            row[i]
        );
    }

    assert_eq!(
        row[0],
        Value::DateTime {
            raw: "2024-01-15".into(),
            tz: None
        }
    );
    assert_eq!(
        row[1],
        Value::DateTime {
            raw: "2024-01-15 13:45:30".into(),
            tz: None
        }
    );
    assert_eq!(
        row[2],
        Value::DateTime {
            raw: "2024-01-15 13:45:30.123456".into(),
            tz: None
        }
    );
    assert_eq!(row[3], Value::Text("13:45:30".into()));
    assert!(
        matches!(&row[4], Value::DateTime { raw, .. } if raw.starts_with("2023-11-14") || raw.starts_with("2023-11-15")),
        "FROM_UNIXTIME 应是可读 DateTime，实际: {:?}",
        row[4]
    );

    provider.disconnect().expect("断开");
}
