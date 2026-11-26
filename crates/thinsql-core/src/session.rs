//! 会话层：一条连接 = 一个工作线程 + 一对 channel。
//!
//! UI 线程只做两件事：`send(Command)` 和轮询 `try_recv()`。
//! 数据库 I/O 永远不会阻塞渲染线程；取消通过共享的原子标志**绕过队列**，
//! 因为工作线程此刻正阻塞在查询里，排队等它处理取消命令是没有意义的。

use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::error::{Error, ErrorKind};
use crate::model::{
    ApplyOutcome, ColumnMeta, ConnectionConfig, ForeignKeyMeta, IndexMeta, ObjectKind, ObjectMeta,
    Page, QueryResult, RowChange, SchemaMeta, StructureChange, TableMeta, TableRef,
};
use crate::provider::{self, CancelToken, DatabaseProvider};

/// 查询请求的唯一标识，用于把响应与发起它的请求对上。
pub type RequestId = u64;

#[derive(Debug)]
pub enum Command {
    Connect {
        cfg: Box<ConnectionConfig>,
        /// 明文密码只在这一次调用中出现，不进入配置、不进入日志
        password: Option<String>,
    },
    Disconnect,
    Query {
        request_id: RequestId,
        sql: String,
        page: Page,
    },
    ListSchemas,
    ListTables {
        schema: Option<String>,
    },
    ListObjects {
        schema: Option<String>,
        kind: ObjectKind,
    },
    SetSchema {
        schema: String,
    },
    ListColumns {
        request_id: RequestId,
        table: TableRef,
    },
    Apply {
        request_id: RequestId,
        table: TableRef,
        changes: Vec<RowChange>,
    },
    /// 一次性加载列、索引、外键与 DDL，避免 UI 发四条命令后自己拼装。
    LoadStructure {
        request_id: RequestId,
        table: TableRef,
    },
    ApplyStructure {
        request_id: RequestId,
        table: TableRef,
        change: StructureChange,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum Event {
    Connected,
    Disconnected,
    Result {
        request_id: RequestId,
        result: Box<QueryResult>,
    },
    Schemas(Vec<SchemaMeta>),
    Tables {
        /// 回显请求里的 schema，空结果时也能记到正确的 schema 上
        schema: Option<String>,
        tables: Vec<TableMeta>,
    },
    Objects {
        schema: Option<String>,
        kind: ObjectKind,
        objects: Vec<ObjectMeta>,
    },
    SchemaSet {
        schema: String,
    },
    Columns {
        request_id: RequestId,
        columns: Vec<ColumnMeta>,
    },
    Applied {
        request_id: RequestId,
        outcome: ApplyOutcome,
    },
    StructureLoaded {
        request_id: RequestId,
        columns: Vec<ColumnMeta>,
        indexes: Vec<IndexMeta>,
        foreign_keys: Vec<ForeignKeyMeta>,
        /// DDL 取不到（驱动不支持/视图）时保留错误文案，由面板展示。
        ddl: Result<String, String>,
        /// 可选元数据失败时不丢弃已取到的列，只把错误摘要带给界面。
        warnings: Vec<String>,
    },
    StructureApplied {
        request_id: RequestId,
    },
    Cancelled {
        request_id: Option<RequestId>,
    },
    Failed {
        request_id: Option<RequestId>,
        error: Error,
    },
}

pub struct Session {
    tx: Sender<Command>,
    rx: Receiver<Event>,
    cancel: CancelToken,
    worker: Option<JoinHandle<()>>,
}

impl Session {
    pub fn spawn(
        provider: Box<dyn DatabaseProvider>,
        thread_name: impl Into<String>,
    ) -> Result<Self, Error> {
        let (tx, cmd_rx) = unbounded();
        let (event_tx, rx) = unbounded();
        let cancel = provider::new_cancel_token();
        let worker_cancel = cancel.clone();

        let worker = thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || run(provider, cmd_rx, event_tx, worker_cancel))
            .map_err(|err| {
                Error::with_source(ErrorKind::Internal, "无法创建数据库工作线程", err)
            })?;

        Ok(Self {
            tx,
            rx,
            cancel,
            worker: Some(worker),
        })
    }

    pub fn send(&self, cmd: Command) -> Result<(), Error> {
        self.tx
            .send(cmd)
            .map_err(|_| Error::internal("工作线程已退出"))
    }

    /// 非阻塞取事件，UI 每帧调用。
    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// 请求取消当前操作。直接置位，不排队。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // 先请求取消，尽力让卡在查询里的工作线程尽快返回；
        // 再把 join 放到后台，绝不阻塞 UI 线程（查询可能卡在网络 I/O 上）。
        provider::request_cancel(&self.cancel);
        let _ = self.tx.send(Command::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = thread::Builder::new()
                .name("db-session-reaper".into())
                .spawn(move || {
                    let _ = handle.join();
                });
        }
    }
}

fn run(
    mut provider: Box<dyn DatabaseProvider>,
    cmd_rx: Receiver<Command>,
    event_tx: Sender<Event>,
    cancel: CancelToken,
) {
    let send = |event: Event| {
        if event_tx.send(event).is_err() {
            tracing::warn!("事件通道已关闭，丢弃事件");
        }
    };

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Command::Shutdown => break,
            Command::Disconnect => {
                if let Err(err) = provider.disconnect() {
                    tracing::warn!(error = %err.log_summary(), "断开连接失败");
                }
                send(Event::Disconnected);
            }
            Command::Connect { cfg, password } => {
                match provider.connect(&cfg, password.as_deref()) {
                    Ok(()) => send(Event::Connected),
                    Err(err) => send(Event::Failed {
                        request_id: None,
                        error: err,
                    }),
                }
            }
            Command::Query {
                request_id,
                sql,
                page,
            } => {
                provider::reset_cancel(&cancel);
                let quote = provider.capabilities().identifier_quote;
                let sql = crate::sql::quote_reserved_relations(&sql, quote);
                match provider.query(&sql, page, &cancel) {
                    Ok(result) => send(Event::Result {
                        request_id,
                        result: Box::new(result),
                    }),
                    Err(err) if err.kind == crate::error::ErrorKind::Cancelled => {
                        send(Event::Cancelled {
                            request_id: Some(request_id),
                        })
                    }
                    Err(err) => send(Event::Failed {
                        request_id: Some(request_id),
                        error: err,
                    }),
                }
            }
            Command::ListSchemas => match provider.list_schemas() {
                Ok(v) => send(Event::Schemas(v)),
                Err(err) => send(Event::Failed {
                    request_id: None,
                    error: err,
                }),
            },
            Command::ListTables { schema } => match provider.list_tables(schema.as_deref()) {
                Ok(tables) => send(Event::Tables { schema, tables }),
                Err(err) => send(Event::Failed {
                    request_id: None,
                    error: err,
                }),
            },
            Command::ListObjects { schema, kind } => {
                match provider.list_objects(schema.as_deref(), kind) {
                    Ok(objects) => send(Event::Objects {
                        schema,
                        kind,
                        objects,
                    }),
                    Err(err) => send(Event::Failed {
                        request_id: None,
                        error: err,
                    }),
                }
            }
            Command::SetSchema { schema } => match provider.set_schema(&schema) {
                Ok(()) => send(Event::SchemaSet { schema }),
                Err(err) => send(Event::Failed {
                    request_id: None,
                    error: err,
                }),
            },
            Command::ListColumns { request_id, table } => match provider.list_columns(&table) {
                Ok(columns) => send(Event::Columns {
                    request_id,
                    columns,
                }),
                Err(err) => send(Event::Failed {
                    request_id: Some(request_id),
                    error: err,
                }),
            },
            Command::Apply {
                request_id,
                table,
                changes,
            } => match provider.apply_changes(&table, &changes) {
                Ok(outcome) => send(Event::Applied {
                    request_id,
                    outcome,
                }),
                Err(err) => send(Event::Failed {
                    request_id: Some(request_id),
                    error: err,
                }),
            },
            Command::LoadStructure { request_id, table } => {
                // 列是必需项：取不到就没有可编辑的结构，整批失败。
                // 索引、外键、DDL 是附加信息：单项失败不应让用户看不到字段。
                let columns = match provider.list_columns(&table) {
                    Ok(columns) => columns,
                    Err(err) => {
                        send(Event::Failed {
                            request_id: Some(request_id),
                            error: err,
                        });
                        continue;
                    }
                };
                let mut warnings = Vec::new();
                let indexes = provider.list_indexes(&table).unwrap_or_else(|err| {
                    warnings.push(format!("索引：{}", err.log_summary()));
                    Vec::new()
                });
                let foreign_keys = provider.list_foreign_keys(&table).unwrap_or_else(|err| {
                    warnings.push(format!("外键：{}", err.log_summary()));
                    Vec::new()
                });
                let ddl = match provider.table_ddl(&table) {
                    Ok(ddl) => Ok(ddl),
                    Err(err) => {
                        warnings.push(format!("DDL：{}", err.log_summary()));
                        Err(err.message)
                    }
                };
                for warning in &warnings {
                    tracing::warn!(warning, "表结构附加元数据读取失败");
                }
                send(Event::StructureLoaded {
                    request_id,
                    columns,
                    indexes,
                    foreign_keys,
                    ddl,
                    warnings,
                });
            }
            Command::ApplyStructure {
                request_id,
                table,
                change,
            } => match provider.apply_structure(&table, &change) {
                Ok(()) => send(Event::StructureApplied { request_id }),
                Err(err) => send(Event::Failed {
                    request_id: Some(request_id),
                    error: err,
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::error::ErrorKind;
    use crate::provider::{Capabilities, EscapeStyle};
    use crate::value::Value;

    /// 不连数据库的假驱动，用来验证会话层的线程与事件流转。
    struct FakeProvider {
        fail_next_query: bool,
    }

    impl DatabaseProvider for FakeProvider {
        fn connect(
            &mut self,
            _cfg: &ConnectionConfig,
            _password: Option<&str>,
        ) -> Result<(), Error> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn ping(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                escape: EscapeStyle::Standard,
                identifier_quote: '"',
                supports_multi_statement: false,
                supports_transactions: true,
                has_schema_layer: true,
                object_kinds: &[ObjectKind::Table, ObjectKind::View],
            }
        }
        fn query(
            &mut self,
            _sql: &str,
            page: Page,
            _cancel: &CancelToken,
        ) -> Result<QueryResult, Error> {
            if self.fail_next_query {
                self.fail_next_query = false;
                return Err(Error::new(ErrorKind::ServerError, "boom"));
            }
            Ok(QueryResult {
                columns: Vec::new(),
                rows: vec![vec![Value::Int(page.offset as i64)]],
                elapsed: Duration::ZERO,
                affected_rows: None,
                has_more: false,
            })
        }
        fn list_schemas(&mut self) -> Result<Vec<SchemaMeta>, Error> {
            Ok(vec![SchemaMeta {
                name: "public".into(),
            }])
        }
        fn list_tables(&mut self, _schema: Option<&str>) -> Result<Vec<TableMeta>, Error> {
            Ok(Vec::new())
        }
        fn list_objects(
            &mut self,
            _schema: Option<&str>,
            _kind: ObjectKind,
        ) -> Result<Vec<ObjectMeta>, Error> {
            Ok(Vec::new())
        }
        fn set_schema(&mut self, _schema: &str) -> Result<(), Error> {
            Ok(())
        }
        fn list_columns(&mut self, _table: &TableRef) -> Result<Vec<ColumnMeta>, Error> {
            Ok(Vec::new())
        }
        fn list_indexes(
            &mut self,
            _table: &TableRef,
        ) -> Result<Vec<crate::model::IndexMeta>, Error> {
            Ok(Vec::new())
        }
        fn list_foreign_keys(
            &mut self,
            _table: &TableRef,
        ) -> Result<Vec<crate::model::ForeignKeyMeta>, Error> {
            Ok(Vec::new())
        }
        fn apply_changes(
            &mut self,
            _table: &TableRef,
            _changes: &[RowChange],
        ) -> Result<ApplyOutcome, Error> {
            Ok(ApplyOutcome { affected: 0 })
        }
    }

    fn wait_event(session: &Session) -> Event {
        for _ in 0..200 {
            if let Some(event) = session.try_recv() {
                return event;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("等待事件超时");
    }

    #[test]
    fn connect_and_query_round_trip_through_worker_thread() {
        let session = Session::spawn(
            Box::new(FakeProvider {
                fail_next_query: false,
            }),
            "fake-db",
        )
        .expect("spawn");
        session
            .send(Command::Connect {
                cfg: Box::new(ConnectionConfig::new(
                    "c1",
                    "test",
                    crate::model::DriverKind::Postgres,
                )),
                password: None,
            })
            .unwrap();
        assert!(matches!(wait_event(&session), Event::Connected));

        session
            .send(Command::Query {
                request_id: 7,
                sql: "select 1".into(),
                page: Page::with_limit(10),
            })
            .unwrap();
        match wait_event(&session) {
            Event::Result { request_id, result } => {
                assert_eq!(request_id, 7);
                assert_eq!(result.row_count(), 1);
                assert_eq!(result.rows[0][0], Value::Int(0));
            }
            other => panic!("期望 Result，实际 {other:?}"),
        }
    }

    #[test]
    fn server_error_surfaces_as_failed_event() {
        let session = Session::spawn(
            Box::new(FakeProvider {
                fail_next_query: true,
            }),
            "fake-db-err",
        )
        .expect("spawn");
        session
            .send(Command::Query {
                request_id: 9,
                sql: "boom".into(),
                page: Page::first(),
            })
            .unwrap();
        match wait_event(&session) {
            Event::Failed { request_id, error } => {
                assert_eq!(request_id, Some(9));
                assert_eq!(error.kind, ErrorKind::ServerError);
            }
            other => panic!("期望 Failed，实际 {other:?}"),
        }
    }

    #[test]
    fn session_drop_shuts_worker_down_without_hanging() {
        let session = Session::spawn(
            Box::new(FakeProvider {
                fail_next_query: false,
            }),
            "fake-db-drop",
        )
        .expect("spawn");
        drop(session);
    }
}
