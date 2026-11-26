//! 共享视图状态 + 后台事件泵。
//!
//! UI 不直接持有数据库句柄：驱动器跑在会话工作线程里，这里只做
//! 「发命令 → 轮询事件 → 更新视图模型」这一件事。
//!
//! 多标签模型：每个标签拥有自己的结果集、执行上下文与待提交改动。
//! 请求 id 携带标签归属，结果写回发起它的标签，不再按「当前连接」丢弃
//! 非活动标签的结果。编辑器实体不在这一层，由工作区面板按标签 id 持有。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use thinsql_core::Value;
use thinsql_core::config::{AppConfig, secret};
use thinsql_core::edit::{build_sqls, build_structure_sql};
use thinsql_core::export;
use thinsql_core::history::{History, HistoryEntry, now_ms};
use thinsql_core::model::{
    ColumnMeta, ColumnOp, ColumnSpec, ConnectionConfig, DriverKind, ForeignKeyMeta, IndexMeta,
    IndexOp, ObjectKind, ObjectMeta, Page, QueryResult, RowChange, StructureChange, TableMeta,
    TableRef, table_preview_sql,
};
use thinsql_core::provider::Registry;
use thinsql_core::session::{Command, Event, RequestId, Session};
use thinsql_core::sql::split_statements;

use crate::i18n::{t, tr};

/// 「执行所有」单次语句数上限：避免误把整段脚本无限跑下去。
const MAX_STATEMENTS_PER_RUN: usize = 200;

/// 视图状态变化事件。面板订阅它来重建自己的展示数据，
/// 比依赖 `observe` 更明确：非 Render 实体上 `notify` 的语义不够可靠。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateEvent {
    Changed,
}

pub type TabId = u64;

#[derive(Debug, Clone, PartialEq)]
pub enum ConnStatus {
    Idle,
    Connecting,
    Connected,
    Failed(String),
}

impl ConnStatus {
    /// 树节点上用的短标签，避免长错误信息把树撑开。
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::Idle => t("未连接"),
            Self::Connecting => t("连接中"),
            Self::Connected => t("已连接"),
            Self::Failed(_) => t("连接失败"),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Idle => t("未连接").into(),
            Self::Connecting => t("连接中…").into(),
            Self::Connected => t("已连接").into(),
            Self::Failed(msg) => tr("失败: {msg}", &[("msg", msg)]),
        }
    }
}

pub struct ConnEntry {
    pub cfg: ConnectionConfig,
    pub session: Option<Session>,
    pub status: ConnStatus,
    pub schemas: Vec<String>,
    /// schema → kind → 对象名列表；键存在即已加载（可为空）
    pub objects: BTreeMap<String, BTreeMap<ObjectKind, Vec<String>>>,
    pub schema_loaded: bool,
    /// 已发出但尚未返回的查询：request_id → 归属标签与执行方式
    pub in_flight: HashMap<RequestId, InFlight>,
}

impl ConnEntry {
    pub fn new(cfg: ConnectionConfig) -> Self {
        Self {
            cfg,
            session: None,
            status: ConnStatus::Idle,
            schemas: Vec::new(),
            objects: BTreeMap::new(),
            schema_loaded: false,
            in_flight: HashMap::new(),
        }
    }

    /// 兼容旧路径：某 schema 下 Tables 分组的表名。
    pub fn table_names(&self, schema: &str) -> Option<&Vec<String>> {
        self.objects
            .get(schema)
            .and_then(|m| m.get(&ObjectKind::Table))
    }
}

/// 在途查询的归属。结果只能写回这里记录的标签。
#[derive(Debug, Clone, Copy)]
pub struct InFlight {
    pub tab: TabId,
    pub append: bool,
    pub statement: Option<usize>,
}

/// 一次查询请求。保留下来是为了"加载更多"能复用同一条语句。
#[derive(Debug, Clone)]
pub struct QueryRequest {
    pub sql: String,
    pub page: Page,
    /// true 表示把结果追加到已有结果之后（分页加载），false 表示替换
    pub append: bool,
    /// 由表预览发起时记录表名，导出 INSERT 时用得上
    pub table: Option<TableRef>,
    /// 「执行所有」里的语句序号；单条执行为 None
    pub statement: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    Insert,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Insert => "sql",
        }
    }
}

/// 一次单元格编辑的上下文：定位该行需要主键，回写时也靠它。
#[derive(Debug, Clone)]
pub struct EditTarget {
    pub row: usize,
    pub col: usize,
    pub column_name: String,
    pub type_name: String,
    pub original: Value,
    pub key: Vec<(String, Value)>,
}

/// 一条语句的执行结果摘要。
#[derive(Debug, Clone, PartialEq)]
pub enum StatementStatus {
    Pending,
    Running,
    Succeeded,
    Failed(String),
    Cancelled,
    Skipped,
}

impl StatementStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pending => t("等待"),
            Self::Running => t("执行中"),
            Self::Succeeded => t("成功"),
            Self::Failed(_) => t("失败"),
            Self::Cancelled => t("已取消"),
            Self::Skipped => t("已跳过"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatementLog {
    pub index: usize,
    pub status: StatementStatus,
    pub rows: usize,
    pub affected: Option<u64>,
    pub elapsed_ms: u64,
}

/// 一个工作区标签的全部数据。这里不能放任何 UI 实体。
pub struct QueryTab {
    pub id: TabId,
    pub title: String,
    pub conn_id: Option<String>,
    pub schema: Option<String>,
    pub table: Option<TableRef>,
    /// 表结构编辑标签；与 `table` 互斥，避免结构页与数据页互相误命中
    pub structure: Option<TableRef>,
    pub structure_loading: bool,
    /// 原始列元数据（只读基准），specs 是可编辑工作副本
    pub structure_columns: Vec<ColumnMeta>,
    /// 与 specs 对齐：Some 是原始列名（重命名时保留身份），None 表示新增列
    pub structure_original_names: Vec<Option<String>>,
    pub structure_specs: Vec<ColumnSpec>,
    pub structure_indexes: Vec<IndexMeta>,
    pub structure_fks: Vec<ForeignKeyMeta>,
    pub structure_ddl: String,
    pub structure_changes: StructureChange,
    pub structure_rev: u64,
    /// 初始 SQL 只在 UI 创建编辑器时读取；之后的文本由编辑器实体持有
    pub initial_sql: String,
    pub result: Option<Arc<QueryResult>>,
    pub result_rev: u64,
    pub last_query: Option<QueryRequest>,
    pub status: String,
    pub pending_changes: Vec<RowChange>,
    pub current_pk: Vec<String>,
    /// 表数据筛选片段（不含 WHERE）；空表示不筛
    pub data_filter: String,
    pub statements: Vec<StatementLog>,
    /// 「执行所有」为 true，结果区显示语句摘要
    pub summary_mode: bool,
    /// 顺序执行时待下发的语句
    queue: VecDeque<String>,
    query_in_flight: bool,
    /// 点表时连接尚未建立：连上后自动执行 last_query
    waiting_connect: bool,
}

impl QueryTab {
    fn new(id: TabId, title: String, conn_id: Option<String>, schema: Option<String>) -> Self {
        Self {
            id,
            title,
            conn_id,
            schema,
            table: None,
            structure: None,
            structure_loading: false,
            structure_columns: Vec::new(),
            structure_original_names: Vec::new(),
            structure_specs: Vec::new(),
            structure_indexes: Vec::new(),
            structure_fks: Vec::new(),
            structure_ddl: String::new(),
            structure_changes: StructureChange::default(),
            structure_rev: 0,
            initial_sql: String::new(),
            result: None,
            result_rev: 0,
            last_query: None,
            status: t("就绪").into(),
            pending_changes: Vec::new(),
            current_pk: Vec::new(),
            data_filter: String::new(),
            statements: Vec::new(),
            summary_mode: false,
            queue: VecDeque::new(),
            query_in_flight: false,
            waiting_connect: false,
        }
    }

    pub fn is_running(&self) -> bool {
        self.query_in_flight || self.waiting_connect
    }

    /// 表数据标签：整个区域渲染数据网格；查询标签：编辑器与结果上下分栏。
    pub fn is_table_tab(&self) -> bool {
        self.table.is_some()
    }

    pub fn is_structure_tab(&self) -> bool {
        self.structure.is_some()
    }

    pub fn has_failed_statement(&self) -> bool {
        self.statements
            .iter()
            .any(|log| matches!(log.status, StatementStatus::Failed(_)))
    }
}

/// 行在暂存区里的状态，供网格着色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowPendingKind {
    Modified,
    Deleted,
}

/// 按主键 key 判断该行是否有未提交的 Update / Delete。
pub fn row_pending_kind(
    pending: &[RowChange],
    key: &[(String, Value)],
) -> Option<RowPendingKind> {
    let mut modified = false;
    for change in pending {
        if change.key() != key {
            continue;
        }
        match change {
            RowChange::Delete { .. } => return Some(RowPendingKind::Deleted),
            RowChange::Update { .. } => modified = true,
        }
    }
    modified.then_some(RowPendingKind::Modified)
}

/// 某行某列是否已有未提交的新值（供网格直接展示，不必等提交后刷新）。
pub fn pending_cell_value(
    pending: &[RowChange],
    key: &[(String, Value)],
    column: &str,
) -> Option<Value> {
    pending.iter().rev().find_map(|change| match change {
        RowChange::Update {
            key: change_key,
            column: change_column,
            new_value,
        } if change_key.as_slice() == key && change_column == column => Some(new_value.clone()),
        _ => None,
    })
}

/// 列元数据 → 可编辑结构定义。类型里的 `(64)` / `(10,2)` 拆成 Length 栏。
pub fn column_spec_from_meta(column: &ColumnMeta) -> ColumnSpec {
    let (data_type, length) = match column.type_name.split_once('(') {
        Some((base, rest)) => (
            base.trim().to_string(),
            Some(rest.trim_end_matches(')').trim().to_string()),
        ),
        None => (column.type_name.clone(), None),
    };
    ColumnSpec {
        name: column.name.clone(),
        data_type,
        length,
        nullable: column.nullable,
        primary_key: column.is_primary_key,
        default: column.default.clone(),
        comment: column.comment.clone(),
        charset: column.charset.clone(),
        collation: column.collation.clone(),
        auto_increment: column.is_auto_increment,
    }
}

/// 表预览标签的标题：`表名@schema`。
fn table_tab_title(table: &TableRef) -> String {
    match &table.schema {
        Some(schema) => format!("{}@{schema}", table.name),
        None => table.name.clone(),
    }
}

pub struct AppState {
    pub config: AppConfig,
    pub conns: Vec<ConnEntry>,
    pub expanded: HashSet<String>,
    /// 最近使用的连接，只作为新建标签的默认值，不再决定结果归属
    pub current: Option<String>,
    pub status: String,
    pub history: History,
    pub tabs: Vec<QueryTab>,
    pub active_tab: Option<TabId>,
    /// 侧栏当前点击选中的节点 id
    pub tree_selection: Option<String>,
    /// 正在加载对象列表的 (连接 id, schema, kind)
    pending_objects: HashSet<(String, String, ObjectKind)>,
    /// 单元格提交与主键查询也要路由回标签，否则多标签下会串数据。
    /// 连接 id 用于断开时整批清理：这些请求不在 in_flight 里记账，
    /// 断开后回包永远不会来，残留的映射只能在这里移除。
    apply_requests: HashMap<RequestId, (String, TabId)>,
    column_requests: HashMap<RequestId, (String, TabId)>,
    structure_loads: HashMap<RequestId, (String, TabId)>,
    structure_applies: HashMap<RequestId, (String, TabId)>,
    next_request_id: RequestId,
    next_tab_id: TabId,
    next_query_no: u32,
}

/// 按 schema 整体替换表列表：空列表也算已加载，重复响应不会累加。
fn store_objects(
    objects: &mut BTreeMap<String, BTreeMap<ObjectKind, Vec<String>>>,
    schema: String,
    kind: ObjectKind,
    incoming: Vec<ObjectMeta>,
) {
    let names: BTreeSet<String> = incoming.into_iter().map(|o| o.name).collect();
    objects
        .entry(schema)
        .or_default()
        .insert(kind, names.into_iter().collect());
}

fn store_tables_as_objects(
    objects: &mut BTreeMap<String, BTreeMap<ObjectKind, Vec<String>>>,
    schema: String,
    incoming: Vec<TableMeta>,
) {
    let mut tables = BTreeSet::new();
    let mut views = BTreeSet::new();
    for table in incoming {
        match table.kind {
            thinsql_core::model::TableKind::View => views.insert(table.name),
            thinsql_core::model::TableKind::Table => tables.insert(table.name),
        };
    }
    let entry = objects.entry(schema).or_default();
    entry.insert(ObjectKind::Table, tables.into_iter().collect());
    entry.insert(ObjectKind::View, views.into_iter().collect());
}


/// 从树节点 id 解析「新建查询」应继承的连接与库。
fn tree_selection_query_context(id: &str) -> Option<(String, Option<String>)> {
    let mut parts = id.splitn(2, ':');
    match parts.next()? {
        "conn" => Some((parts.next()?.to_string(), None)),
        "schema" => {
            let rest = parts.next()?;
            let mut bits = rest.splitn(2, ':');
            Some((bits.next()?.to_string(), Some(bits.next()?.to_string())))
        }
        "kind" | "obj" => {
            let rest = parts.next()?;
            let mut bits = rest.splitn(3, ':');
            let conn = bits.next()?.to_string();
            let schema = bits.next()?.to_string();
            Some((conn, Some(schema)))
        }
        _ => None,
    }
}

impl EventEmitter<StateEvent> for AppState {}

impl AppState {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut config_outcome = AppConfig::load_with_warning();
        let config_warning = config_outcome.warning.take();
        let config = config_outcome.value;
        let conns = config
            .connections
            .iter()
            .cloned()
            .map(ConnEntry::new)
            .collect();
        let current = config.connections.first().map(|c| c.id.clone());

        let mut history_outcome = History::load_with_warning();
        let history_warning = history_outcome.warning.take();
        let history = history_outcome.value;

        let mut startup_warnings: Vec<String> = Vec::new();
        if let Some(warning) = config_warning {
            startup_warnings.push(warning);
        }
        if let Some(warning) = history_warning {
            startup_warnings.push(warning);
        }
        let status = if startup_warnings.is_empty() {
            t("就绪").to_string()
        } else {
            startup_warnings.join("；")
        };

        // 事件泵：80ms 轮询一次。相比"回调里跨线程 update"，
        // 轮询的代价可控（一次 try_recv），且不会因为 UI 卡顿堆积回调。
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(80))
                    .await;
                let alive = this.update(cx, |state, cx| state.drain_events(cx)).is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        let mut state = Self {
            config,
            conns,
            expanded: HashSet::new(),
            current,
            status,
            history,
            tabs: Vec::new(),
            active_tab: None,
            tree_selection: None,
            pending_objects: HashSet::new(),
            apply_requests: HashMap::new(),
            column_requests: HashMap::new(),
            structure_loads: HashMap::new(),
            structure_applies: HashMap::new(),
            next_request_id: 1,
            next_tab_id: 1,
            next_query_no: 1,
        };

        // 启动即给一个空查询标签，用户打开就能写 SQL
        state.new_query_tab();

        // 无桌面会话时的自动化入口：启动即连接所有已配置连接
        if std::env::var("THINSQL_AUTOCONNECT").as_deref() == Ok("1") {
            let ids: Vec<String> = state.conns.iter().map(|c| c.cfg.id.clone()).collect();
            for id in ids {
                state.connect(&id);
            }
        }

        state
    }

    pub fn entry(&self, id: &str) -> Option<&ConnEntry> {
        self.conns.iter().find(|c| c.cfg.id == id)
    }

    fn entry_mut(&mut self, id: &str) -> Option<&mut ConnEntry> {
        self.conns.iter_mut().find(|c| c.cfg.id == id)
    }

    pub fn current_entry(&self) -> Option<&ConnEntry> {
        self.current.as_deref().and_then(|id| self.entry(id))
    }

    pub fn tab(&self, id: TabId) -> Option<&QueryTab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut QueryTab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn active(&self) -> Option<&QueryTab> {
        self.active_tab.and_then(|id| self.tab(id))
    }

    pub fn active_tab_id(&self) -> Option<TabId> {
        self.active_tab
    }

    /// 新建查询标签：优先继承树选中的连接/库，否则回退当前连接与连接默认库。
    pub fn new_query_tab(&mut self) -> TabId {
        let (conn_id, schema) = self
            .tree_selection
            .as_deref()
            .and_then(tree_selection_query_context)
            .map(|(conn, schema)| (Some(conn), schema))
            .unwrap_or_else(|| (self.current.clone(), None));
        if let Some(ref conn) = conn_id {
            self.current = Some(conn.clone());
        }
        self.new_query_tab_on(conn_id, schema, String::new())
    }

    /// 在指定连接上新建查询标签；`schema` 显式传入时优先，否则回退连接默认库。
    pub fn new_query_tab_on(
        &mut self,
        conn_id: Option<String>,
        schema: Option<String>,
        initial_sql: String,
    ) -> TabId {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let no = self.next_query_no;
        self.next_query_no += 1;
        let schema = schema.or_else(|| {
            conn_id
                .as_deref()
                .and_then(|id| self.entry(id))
                .and_then(|entry| entry.cfg.database.clone())
        });
        let mut tab = QueryTab::new(id, tr("查询 {no}", &[("no", &no.to_string())]), conn_id.clone(), schema.clone());
        tab.initial_sql = initial_sql;
        self.tabs.push(tab);
        self.active_tab = Some(id);
        if let (Some(conn_id), Some(schema)) = (conn_id, schema)
            && let Some(entry) = self.entry(&conn_id)
            && matches!(entry.status, ConnStatus::Connected)
            && let Some(session) = entry.session.as_ref()
        {
            let _ = session.send(Command::SetSchema { schema });
        }
        id
    }

    /// 切换标签绑定的库/schema，并在已连接时下发会话级切换。
    pub fn set_tab_schema(&mut self, tab_id: TabId, schema: String) {
        let Some(tab) = self.tab(tab_id) else {
            return;
        };
        if tab.schema.as_deref() == Some(schema.as_str()) {
            return;
        }
        let Some(conn_id) = tab.conn_id.clone() else {
            return;
        };
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.schema = Some(schema.clone());
            tab.status = tr("已切换到库 {schema}", &[("schema", &schema)]);
        }
        if let Some(entry) = self.entry(&conn_id)
            && matches!(entry.status, ConnStatus::Connected)
            && let Some(session) = entry.session.as_ref()
        {
            let _ = session.send(Command::SetSchema { schema });
        }
    }

    pub fn set_tree_selection(&mut self, id: Option<String>) {
        self.tree_selection = id;
    }

    /// 第一页大小跟随设置里的"默认每页条数"。
    fn first_page(&self) -> Page {
        Page::with_limit(self.config.settings.default_page_limit)
    }

    /// 同一连接下同库同名表视为同一份数据标签，重复点击复用并聚焦。
    fn find_table_tab(&self, conn_id: &str, table: &TableRef) -> Option<TabId> {
        self.tabs.iter().find_map(|tab| {
            let same = tab.conn_id.as_deref() == Some(conn_id)
                && tab.table.as_ref().is_some_and(|t| {
                    t.name == table.name && t.schema == table.schema
                });
            same.then_some(tab.id)
        })
    }

    /// 点结构树里的表：开一个新标签，填入预览语句并立即执行。
    /// 连接未建立时先连接，连上后自动执行。
    /// 已有同表的标签时直接复用并置为焦点，不重复建标签、不重复查询。
    pub fn open_table_tab(&mut self, conn_id: &str, table: TableRef) -> TabId {
        if let Some(id) = self.find_table_tab(conn_id, &table) {
            self.active_tab = Some(id);
            self.current = Some(conn_id.to_string());
            return id;
        }

        let Some(entry) = self.entry(conn_id) else {
            self.status = t("连接不存在").into();
            return self.new_query_tab();
        };
        let kind = entry.cfg.kind;
        let schema = table.schema.clone().or_else(|| entry.cfg.database.clone());
        let connected = matches!(entry.status, ConnStatus::Connected) && entry.session.is_some();

        let page = self.first_page();
        let sql = table_preview_sql(kind, &table, page, None);
        let title = table_tab_title(&table);
        let req = QueryRequest {
            sql: sql.clone(),
            page,
            append: false,
            table: Some(table.clone()),
            statement: None,
        };

        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let mut tab = QueryTab::new(id, title, Some(conn_id.to_string()), schema);
        tab.table = Some(table);
        tab.initial_sql = sql;
        self.tabs.push(tab);
        self.active_tab = Some(id);
        self.current = Some(conn_id.to_string());

        if connected {
            self.send_query(id, req);
        } else {
            self.connect(conn_id);
            if let Some(tab) = self.tab_mut(id) {
                tab.waiting_connect = true;
                tab.status = t("连接中…").into();
                tab.last_query = Some(req);
            }
        }
        id
    }

    /// 结构标签与数据标签各占一个 tab，同表互不干扰。
    fn find_structure_tab(&self, conn_id: &str, table: &TableRef) -> Option<TabId> {
        self.tabs.iter().find_map(|tab| {
            let same = tab.conn_id.as_deref() == Some(conn_id)
                && tab.structure.as_ref().is_some_and(|t| {
                    t.name == table.name && t.schema == table.schema
                });
            same.then_some(tab.id)
        })
    }

    /// 表节点右键"修改表结构"：打开（或聚焦）结构标签并异步加载元数据。
    pub fn open_structure_tab(&mut self, conn_id: &str, table: TableRef) -> TabId {
        if let Some(id) = self.find_structure_tab(conn_id, &table) {
            self.active_tab = Some(id);
            self.current = Some(conn_id.to_string());
            return id;
        }

        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let schema = table.schema.clone().or_else(|| {
            self.entry(conn_id)
                .and_then(|entry| entry.cfg.database.clone())
        });
        let mut tab = QueryTab::new(
            id,
            tr("结构 · {name}", &[("name", &table.name)]),
            Some(conn_id.to_string()),
            schema,
        );
        tab.structure = Some(table);
        self.tabs.push(tab);
        self.active_tab = Some(id);
        self.current = Some(conn_id.to_string());
        self.load_structure(id);
        id
    }

    /// 一条命令加载列/索引/外键/DDL，避免 UI 自己拼装四份异步结果。
    pub fn load_structure(&mut self, tab_id: TabId) {
        let Some(tab) = self.tab(tab_id) else { return };
        let Some((conn_id, table)) = tab
            .conn_id
            .clone()
            .zip(tab.structure.clone())
        else {
            return;
        };
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let Some(session) = self
            .entry(&conn_id)
            .and_then(|entry| entry.session.as_ref())
        else {
            self.set_tab_status(tab_id, t("连接未建立").into());
            return;
        };
        if let Err(err) = session.send(Command::LoadStructure {
            request_id,
            table,
        }) {
            self.set_tab_status(tab_id, err.message);
            return;
        }
        self.structure_loads.insert(request_id, (conn_id, tab_id));
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.structure_loading = true;
            tab.status = t("加载表结构…").into();
        }
    }

    pub fn tab_driver_kind(&self, tab_id: TabId) -> Option<thinsql_core::DriverKind> {
        self.tab(tab_id)
            .and_then(|tab| tab.conn_id.as_deref())
            .and_then(|id| self.entry(id))
            .map(|entry| entry.cfg.kind)
    }

    /// 结构是否只读：由连接的只读开关决定，界面据此禁用编辑按钮。
    pub fn is_structure_read_only(&self, tab_id: TabId) -> bool {
        self.tab(tab_id)
            .and_then(|tab| tab.conn_id.as_deref())
            .and_then(|id| self.entry(id))
            .is_some_and(|entry| entry.cfg.read_only)
    }

    /// 根据工作副本与原始元数据的差异，重算待执行的列变更。
    fn sync_structure_ops(tab: &mut QueryTab) {
        let originals = tab.structure_original_names.clone();
        let mut ops = Vec::new();
        for (spec, original) in tab.structure_specs.iter().zip(originals.iter()) {
            match original {
                Some(name) => {
                    let meta = tab.structure_columns.iter().find(|c| &c.name == name);
                    if meta.is_none_or(|m| column_spec_from_meta(m) != *spec) {
                        ops.push(ColumnOp::Alter {
                            old_name: name.clone(),
                            old_primary_key: meta.is_some_and(|m| m.is_primary_key),
                            column: spec.clone(),
                        });
                    }
                }
                None => ops.push(ColumnOp::Add {
                    column: spec.clone(),
                }),
            }
        }
        for column in &tab.structure_columns {
            if !originals
                .iter()
                .any(|name| name.as_deref() == Some(column.name.as_str()))
            {
                ops.push(ColumnOp::Drop {
                    name: column.name.clone(),
                });
            }
        }
        tab.structure_changes.column_ops = ops;
        tab.structure_rev += 1;
    }

    pub fn structure_add_column(&mut self, tab_id: TabId, spec: ColumnSpec) {
        if self.is_structure_read_only(tab_id) {
            return;
        }
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.structure_specs.push(spec);
            tab.structure_original_names.push(None);
            Self::sync_structure_ops(tab);
        }
    }

    pub fn structure_edit_column(&mut self, tab_id: TabId, row: usize, spec: ColumnSpec) {
        if self.is_structure_read_only(tab_id) {
            return;
        }
        if let Some(tab) = self.tab_mut(tab_id)
            && let Some(slot) = tab.structure_specs.get_mut(row)
        {
            *slot = spec;
            Self::sync_structure_ops(tab);
        }
    }

    pub fn structure_remove_column(&mut self, tab_id: TabId, row: usize) {
        if self.is_structure_read_only(tab_id) {
            return;
        }
        if let Some(tab) = self.tab_mut(tab_id)
            && row < tab.structure_specs.len()
        {
            tab.structure_specs.remove(row);
            tab.structure_original_names.remove(row);
            Self::sync_structure_ops(tab);
        }
    }

    pub fn structure_add_index(
        &mut self,
        tab_id: TabId,
        name: String,
        columns: Vec<String>,
        unique: bool,
    ) {
        if self.is_structure_read_only(tab_id) || columns.is_empty() {
            return;
        }
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.structure_changes.index_ops.push(IndexOp::Create {
                name,
                columns,
                unique,
            });
            tab.structure_rev += 1;
        }
    }

    /// 删除索引：已有索引生成 DROP，刚新增还未提交的直接撤销。
    pub fn structure_remove_index(&mut self, tab_id: TabId, name: &str) {
        if self.is_structure_read_only(tab_id) {
            return;
        }
        if let Some(tab) = self.tab_mut(tab_id) {
            let pending = tab
                .structure_changes
                .index_ops
                .iter()
                .position(|op| matches!(op, IndexOp::Create { name: n, .. } if n == name));
            if let Some(ix) = pending {
                tab.structure_changes.index_ops.remove(ix);
            } else {
                tab.structure_changes.index_ops.push(IndexOp::Drop {
                    name: name.to_string(),
                });
            }
            tab.structure_rev += 1;
        }
    }

    /// 修改索引：未提交的 Create 原地替换；已有索引折叠为 Drop(旧名)+Create(新定义)。
    pub fn structure_update_index(
        &mut self,
        tab_id: TabId,
        old_name: &str,
        name: String,
        columns: Vec<String>,
        unique: bool,
    ) {
        if self.is_structure_read_only(tab_id) || columns.is_empty() {
            return;
        }
        let Some(tab) = self.tab_mut(tab_id) else {
            return;
        };

        let pending = tab
            .structure_changes
            .index_ops
            .iter()
            .position(|op| matches!(op, IndexOp::Create { name: n, .. } if n == old_name));
        if let Some(ix) = pending {
            let unchanged = matches!(
                &tab.structure_changes.index_ops[ix],
                IndexOp::Create {
                    name: n,
                    columns: c,
                    unique: u,
                } if n == &name && c == &columns && *u == unique
            );
            if unchanged {
                return;
            }
            tab.structure_changes.index_ops[ix] = IndexOp::Create {
                name,
                columns,
                unique,
            };
            tab.structure_rev += 1;
            return;
        }

        let Some(original) = tab.structure_indexes.iter().find(|idx| idx.name == old_name) else {
            return;
        };
        if original.name == name && original.columns == columns && original.unique == unique {
            return;
        }

        let has_drop = tab
            .structure_changes
            .index_ops
            .iter()
            .any(|op| matches!(op, IndexOp::Drop { name: n } if n == old_name));
        if !has_drop {
            tab.structure_changes.index_ops.push(IndexOp::Drop {
                name: old_name.to_string(),
            });
        }
        tab.structure_changes.index_ops.push(IndexOp::Create {
            name,
            columns,
            unique,
        });
        tab.structure_rev += 1;
    }

    pub fn structure_discard(&mut self, tab_id: TabId) {
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.structure_changes = StructureChange::default();
            // 回到原始列集合，工作副本与原始列名一一对齐
            tab.structure_specs = tab
                .structure_columns
                .iter()
                .map(column_spec_from_meta)
                .collect();
            tab.structure_original_names = tab
                .structure_columns
                .iter()
                .map(|c| Some(c.name.clone()))
                .collect();
            tab.structure_rev += 1;
            tab.status = t("已放弃结构改动").into();
        }
    }

    /// 与真正执行共用同一个驱动生成器。
    pub fn structure_preview_sql(&self, tab_id: TabId) -> Vec<String> {
        let Some(tab) = self.tab(tab_id) else {
            return Vec::new();
        };
        let Some(table) = tab.structure.as_ref() else {
            return Vec::new();
        };
        let Some(kind) = tab
            .conn_id
            .as_deref()
            .and_then(|id| self.entry(id))
            .map(|entry| entry.cfg.kind)
        else {
            return Vec::new();
        };
        build_structure_sql(kind, table, &tab.structure_changes)
    }

    /// 应用结构变更：语句已在预览里展示，这里只负责送到会话线程执行。
    pub fn apply_structure(&mut self, tab_id: TabId) {
        if self.is_structure_read_only(tab_id) {
            self.set_tab_status(tab_id, t("当前连接是只读模式").into());
            return;
        }
        let Some(tab) = self.tab(tab_id) else { return };
        if tab.structure_changes.is_empty() {
            self.set_tab_status(tab_id, t("没有待提交的结构改动").into());
            return;
        }
        let Some((conn_id, table, change)) = tab
            .conn_id
            .clone()
            .zip(tab.structure.clone())
            .map(|(conn, table)| (conn, table, tab.structure_changes.clone()))
        else {
            return;
        };
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let Some(session) = self
            .entry(&conn_id)
            .and_then(|entry| entry.session.as_ref())
        else {
            self.set_tab_status(tab_id, t("连接未建立").into());
            return;
        };
        if let Err(err) = session.send(Command::ApplyStructure {
            request_id,
            table,
            change,
        }) {
            self.set_tab_status(tab_id, err.message);
            return;
        }
        self.structure_applies.insert(request_id, (conn_id, tab_id));
        self.set_tab_status(tab_id, t("正在应用结构变更…").into());
    }

    pub fn activate_tab(&mut self, id: TabId) {
        if self.tab(id).is_some() {
            self.active_tab = Some(id);
        }
    }

    /// 关闭标签：释放结果与待提交改动，清理该标签的在途请求。
    /// 不主动取消连接级查询，避免误伤同连接上的其他标签。
    pub fn close_tab(&mut self, id: TabId) {
        let Some(ix) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        self.tabs.remove(ix);
        for conn in &mut self.conns {
            conn.in_flight.retain(|_, req| req.tab != id);
        }
        self.apply_requests.retain(|_, (_, tab)| *tab != id);
        self.column_requests.retain(|_, (_, tab)| *tab != id);
        self.structure_loads.retain(|_, (_, tab)| *tab != id);
        self.structure_applies.retain(|_, (_, tab)| *tab != id);
        if self.active_tab == Some(id) {
            self.active_tab = self
                .tabs
                .get(ix)
                .or_else(|| ix.checked_sub(1).and_then(|prev| self.tabs.get(prev)))
                .map(|t| t.id);
        }
    }

    /// 切换标签绑定的连接。旧结果与新上下文不匹配，必须清空。
    pub fn set_tab_connection(&mut self, tab_id: TabId, conn_id: String) {
        let old_conn_id = self.tab(tab_id).and_then(|tab| tab.conn_id.clone());
        if old_conn_id.as_deref() == Some(conn_id.as_str()) {
            // 重复选择同一连接：状态没变，不能顺手丢掉在途查询的结果
            self.current = Some(conn_id);
            return;
        }
        if let Some(old_conn_id) = old_conn_id
            && let Some(entry) = self.entry_mut(&old_conn_id)
        {
            entry.in_flight.retain(|_, request| request.tab != tab_id);
        }
        self.apply_requests.retain(|_, (_, tab)| *tab != tab_id);
        self.column_requests.retain(|_, (_, tab)| *tab != tab_id);
        self.structure_loads.retain(|_, (_, tab)| *tab != tab_id);
        self.structure_applies.retain(|_, (_, tab)| *tab != tab_id);
        let schema = self
            .entry(&conn_id)
            .and_then(|entry| entry.cfg.database.clone());
        if let Some(tab) = self.tab_mut(tab_id) {
            // 在途请求已随旧连接丢弃，不能让标签一直显示“执行中…”
            tab.query_in_flight = false;
            tab.conn_id = Some(conn_id.clone());
            tab.schema = schema;
            // 表数据标签的表归属旧连接，切连接后降级为普通查询标签
            tab.table = None;
            tab.result = None;
            tab.result_rev += 1;
            tab.last_query = None;
            tab.pending_changes.clear();
            tab.current_pk.clear();
            tab.statements.clear();
            tab.summary_mode = false;
            tab.queue.clear();
            tab.status = t("已切换连接，结果已清空").into();
        }
        self.current = Some(conn_id);
    }

    /// 保存连接配置：新建与编辑共用。配置变化后重连，避免旧会话指向旧地址。
    /// 密码为空表示不修改钥匙串里已有的密码。
    pub fn save_connection(&mut self, cfg: ConnectionConfig, password: Option<&str>) {
        if let Some(pw) = password.filter(|p| !p.is_empty()) {
            if let Err(err) = secret::store(&cfg, pw) {
                self.status = tr("密码保存失败：{message}", &[("message", &err.message)]);
            }
        }
        let id = cfg.id.clone();
        self.config.upsert(cfg.clone());
        self.config.save_async();
        if let Some(entry) = self.entry_mut(&id) {
            entry.cfg = cfg;
        } else {
            self.conns.push(ConnEntry::new(cfg));
        }
        self.current = Some(id.clone());
        self.disconnect(&id);
        self.connect(&id);
    }

    pub fn toggle_read_only(&mut self, id: &str) {
        let Some(entry) = self.entry_mut(id) else {
            return;
        };
        entry.cfg.read_only = !entry.cfg.read_only;
        let cfg = entry.cfg.clone();
        self.config.upsert(cfg);
        self.config.save_async();
    }

    /// 建立连接。密码从系统钥匙串取，取不到就明确提示。
    pub fn connect(&mut self, id: &str) {
        let Some(entry) = self.entry(id) else {
            return;
        };
        if matches!(entry.status, ConnStatus::Connecting)
            || (matches!(entry.status, ConnStatus::Connected) && entry.session.is_some())
        {
            return;
        }
        let cfg = entry.cfg.clone();
        // 优先系统钥匙串；取不到时退回环境变量，便于无桌面会话/自动化场景
        let password = secret::load(&cfg).or_else(|| std::env::var("THINSQL_PASSWORD").ok());

        let provider = match Registry::create(cfg.kind) {
            Ok(p) => p,
            Err(err) => {
                self.set_status(id, ConnStatus::Failed(err.message.clone()));
                self.status = err.message;
                return;
            }
        };

        let session = match Session::spawn(provider, format!("db-{}", cfg.id)) {
            Ok(session) => session,
            Err(err) => {
                self.set_status(id, ConnStatus::Failed(err.message.clone()));
                self.status = err.message;
                return;
            }
        };
        if let Err(err) = session.send(Command::Connect {
            cfg: Box::new(cfg.clone()),
            password,
        }) {
            self.set_status(id, ConnStatus::Failed(err.message.clone()));
            return;
        }

        self.current = Some(cfg.id.clone());
        if let Some(entry) = self.entry_mut(id) {
            entry.session = Some(session);
            entry.status = ConnStatus::Connecting;
        }
        self.status = tr("正在连接 {name}…", &[("name", &cfg.name)]);
    }

    pub fn disconnect(&mut self, id: &str) {
        if let Some(entry) = self.entry_mut(id) {
            let session = entry.session.take();
            entry.status = ConnStatus::Idle;
            entry.schemas.clear();
            entry.objects.clear();
            entry.schema_loaded = false;
            entry.in_flight.clear();
            if let Some(session) = session
                && let Err(err) = session.send(Command::Disconnect)
            {
                tracing::warn!(conn = %id, error = %err.message, "发送断开命令失败");
            }
        }
        self.column_requests.retain(|_, (conn_id, _)| conn_id != id);
        self.apply_requests.retain(|_, (conn_id, _)| conn_id != id);
        self.structure_loads.retain(|_, (conn_id, _)| conn_id != id);
        self.structure_applies.retain(|_, (conn_id, _)| conn_id != id);
        self.pending_objects.retain(|(conn_id, _, _)| conn_id != id);
    }

    /// 删除连接的内存状态，返回配置供持久化与钥匙串清理。
    fn detach_connection(&mut self, id: &str) -> Option<ConnectionConfig> {
        self.disconnect(id);
        let ix = self.conns.iter().position(|c| c.cfg.id == id)?;
        let entry = self.conns.remove(ix);
        self.config.remove(id);

        let conn_node = format!("conn:{id}");
        let schema_prefix = format!("schema:{id}:");
        self.expanded
            .retain(|node| node != &conn_node && !node.starts_with(&schema_prefix));

        // 该连接的标签一并关闭，避免留下指向已删除配置的标签
        let doomed: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|t| t.conn_id.as_deref() == Some(id))
            .map(|t| t.id)
            .collect();
        for tab_id in doomed {
            self.close_tab(tab_id);
        }

        if self.current.as_deref() == Some(id) {
            self.current = self.conns.first().map(|c| c.cfg.id.clone());
        }
        Some(entry.cfg)
    }

    /// 删除连接：内存、配置文件与系统钥匙串一起清理。
    pub fn remove_connection(&mut self, id: &str) {
        let Some(cfg) = self.detach_connection(id) else {
            return;
        };
        if let Err(err) = secret::delete(&cfg) {
            tracing::warn!(conn = %cfg.id, error = %err.log_summary(), "删除系统钥匙串条目失败");
        }
        self.config.save_async();
        self.status = tr("已删除连接 {name}", &[("name", &cfg.name)]);
    }

    /// 刷新连接的库与表列表。未连接时直接发起连接。
    pub fn refresh_connection(&mut self, id: &str) {
        let has_session = self.entry(id).map(|e| e.session.is_some()).unwrap_or(false);
        if let Some(entry) = self.entry_mut(id) {
            entry.schema_loaded = false;
            entry.objects.clear();
            if let Some(session) = &entry.session {
                if let Err(err) = session.send(Command::ListSchemas) {
                    tracing::debug!(error = %err.message, "发送 schema 查询失败");
                }
            }
        }
        if has_session {
            self.status = t("正在刷新库列表…").into();
        } else {
            self.connect(id);
        }
    }

    /// 执行：选区优先由 UI 决定，这里只负责整条语句。
    pub fn execute(&mut self, tab_id: TabId, sql: String) {
        if sql.trim().is_empty() {
            return;
        }
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.statements.clear();
            tab.summary_mode = false;
            tab.queue.clear();
        }
        self.send_query(
            tab_id,
            QueryRequest {
                sql,
                page: self.first_page(),
                append: false,
                table: None,
                statement: None,
            },
        );
    }

    /// 执行所有：切分语句后逐条下发，遇错停止。
    pub fn execute_all(&mut self, tab_id: TabId, sql: String) {
        let mut statements = split_statements(&sql);
        if statements.is_empty() {
            self.set_tab_status(tab_id, t("没有可执行的语句").into());
            return;
        }
        let truncated = statements.len() > MAX_STATEMENTS_PER_RUN;
        statements.truncate(MAX_STATEMENTS_PER_RUN);
        let total = statements.len();
        let mut iter = statements.into_iter();
        let Some(first) = iter.next() else {
            return;
        };
        let rest: VecDeque<String> = iter.collect();

        {
            let Some(tab) = self.tab_mut(tab_id) else {
                return;
            };
            tab.statements = (0..total)
                .map(|index| StatementLog {
                    index,
                    status: StatementStatus::Pending,
                    rows: 0,
                    affected: None,
                    elapsed_ms: 0,
                })
                .collect();
            tab.summary_mode = true;
            tab.result = None;
            tab.result_rev += 1;
            tab.last_query = None;
            tab.pending_changes.clear();
            tab.current_pk.clear();
            tab.queue = rest;
            tab.status = if truncated {
                tr(
                    "共 {total} 条语句（已截断到 {max} 条）",
                    &[("total", &total.to_string()), ("max", &MAX_STATEMENTS_PER_RUN.to_string())],
                )
            } else {
                tr("共 {total} 条语句", &[("total", &total.to_string())])
            };
        }

        self.send_query(
            tab_id,
            QueryRequest {
                sql: first,
                page: self.first_page(),
                append: false,
                table: None,
                statement: Some(0),
            },
        );
        if truncated {
            self.set_tab_status(
                tab_id,
                tr(
                    "执行中…（语句已截断到 {max} 条）",
                    &[("max", &MAX_STATEMENTS_PER_RUN.to_string())],
                ),
            );
        }
    }

    /// 加载下一页。只有在"还有更多"时才发请求。
    pub fn load_more(&mut self, tab_id: TabId) {
        let Some(req) = self.tab(tab_id).and_then(|t| t.last_query.clone()) else {
            self.set_tab_status(tab_id, t("没有可继续加载的结果").into());
            return;
        };
        let has_more = self
            .tab(tab_id)
            .and_then(|t| t.result.as_ref())
            .map(|r| r.has_more)
            .unwrap_or(false);
        if !has_more {
            self.set_tab_status(tab_id, t("已经是最后一页").into());
            return;
        }
        if self
            .tab(tab_id)
            .map(|tab| tab.summary_mode)
            .unwrap_or(false)
        {
            self.set_tab_status(tab_id, t("多语句执行的结果不支持继续加载").into());
            return;
        }
        self.send_query(
            tab_id,
            QueryRequest {
                sql: req.sql,
                page: req.page.next(),
                append: true,
                table: req.table,
                statement: None,
            },
        );
    }

    pub fn set_tab_status(&mut self, tab_id: TabId, status: String) {
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.status = status;
        }
    }

    fn send_query(&mut self, tab_id: TabId, req: QueryRequest) {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);

        let Some(conn_id) = self.tab(tab_id).and_then(|t| t.conn_id.clone()) else {
            self.set_tab_status(tab_id, t("请先连接一个数据库").into());
            return;
        };
        let Some(entry) = self.entry_mut(&conn_id) else {
            self.set_tab_status(tab_id, t("连接不存在").into());
            return;
        };
        let Some(session) = entry.session.as_ref() else {
            self.set_tab_status(tab_id, t("连接未建立").into());
            return;
        };
        if let Err(err) = session.send(Command::Query {
            request_id,
            sql: req.sql.clone(),
            page: req.page,
        }) {
            self.set_tab_status(tab_id, err.message);
            return;
        }
        entry.status = ConnStatus::Connected;
        entry.in_flight.insert(
            request_id,
            InFlight {
                tab: tab_id,
                append: req.append,
                statement: req.statement,
            },
        );

        // 表预览需要主键才能编辑单元格，列查询与数据查询共用一次请求
        if !req.append {
            if let Some(table) = req.table.clone() {
                let column_id = self.next_request_id;
                self.next_request_id = self.next_request_id.wrapping_add(1);
                if let Some(entry) = self.entry_mut(&conn_id) {
                    if let Some(session) = entry.session.as_ref() {
                        if session
                            .send(Command::ListColumns {
                                request_id: column_id,
                                table,
                            })
                            .is_ok()
                        {
                            self.column_requests
                                .insert(column_id, (conn_id.clone(), tab_id));
                        }
                    }
                }
            }
        }

        if let Some(tab) = self.tab_mut(tab_id) {
            if !req.append {
                // 新查询立刻清空旧结果，避免用户把上一轮数据当成这一轮的
                tab.result = None;
                tab.result_rev += 1;
                tab.pending_changes.clear();
                tab.current_pk.clear();
            }
            if let Some(ix) = req.statement {
                if let Some(log) = tab.statements.get_mut(ix) {
                    log.status = StatementStatus::Running;
                }
            }
            tab.status = t("执行中…").into();
            tab.query_in_flight = true;
            tab.last_query = Some(req.clone());
        }
        tracing::info!(sql = %req.sql.chars().take(200).collect::<String>(), tab = tab_id, "开始执行查询");
    }

    /// 开始编辑某个单元格。所有前置条件在这里一次性判掉，
    /// 避免用户编辑完才发现写不回去。
    pub fn start_edit(
        &mut self,
        tab_id: TabId,
        row: usize,
        col: usize,
    ) -> Result<EditTarget, String> {
        let Some(tab) = self.tab(tab_id) else {
            return Err(t("标签已关闭").into());
        };
        let Some(entry) = tab.conn_id.as_deref().and_then(|id| self.entry(id)) else {
            return Err(t("请先连接一个数据库").into());
        };
        if entry.cfg.read_only {
            return Err(t("当前连接是只读模式，请先关闭只读再编辑").into());
        }
        if tab.current_pk.is_empty() {
            return Err(t("该表没有主键，无法安全定位要修改的行").into());
        }
        let result = tab
            .result
            .as_ref()
            .ok_or_else(|| t("还没有结果集").to_string())?;
        let column = result
            .columns
            .get(col)
            .ok_or_else(|| t("列不存在").to_string())?;
        let row_values = result.rows.get(row).ok_or_else(|| t("行不存在").to_string())?;

        let key = thinsql_core::edit::locate_key(&tab.current_pk, &result.columns, row_values)?;

        Ok(EditTarget {
            row,
            col,
            column_name: column.name.clone(),
            type_name: column.type_name.clone(),
            original: row_values.get(col).cloned().unwrap_or(Value::Null),
            key,
        })
    }

    /// 暂存一处改动。同一行同一列重复编辑时覆盖而不是追加。
    pub fn stage_change(&mut self, tab_id: TabId, target: &EditTarget, new_value: Value) {
        let Some(tab) = self.tab_mut(tab_id) else {
            return;
        };
        tab.pending_changes.retain(|c| {
            !matches!(
                c,
                RowChange::Update { column, key, .. }
                    if column == &target.column_name && key == &target.key
            )
        });
        // 改回原值等于撤销该格 pending，不再追加。
        if target.original != new_value {
            tab.pending_changes.push(RowChange::Update {
                key: target.key.clone(),
                column: target.column_name.clone(),
                new_value,
            });
        }
        tab.status = tr(
            "待提交 {n} 处改动",
            &[("n", &tab.pending_changes.len().to_string())],
        );
    }

    /// 暂存删除一行。该行上已有的未提交修改一并移除——
    /// 行都要删了，再执行它的 UPDATE 只会在提交时炸掉整批事务。
    /// 同一行重复点删除只保留一条，避免重复 DELETE。
    pub fn stage_delete(&mut self, tab_id: TabId, key: Vec<(String, Value)>) {
        let Some(tab) = self.tab_mut(tab_id) else {
            return;
        };
        tab.pending_changes
            .retain(|c| c.key() != key.as_slice());
        tab.pending_changes.push(RowChange::Delete { key });
        tab.status = tr(
            "待提交 {n} 处改动",
            &[("n", &tab.pending_changes.len().to_string())],
        );
    }

    pub fn discard_changes(&mut self, tab_id: TabId) {
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.pending_changes.clear();
            tab.status = t("已放弃改动").into();
        }
    }

    /// 刷新表数据：有未提交改动时拒绝，避免静默丢改。
    pub fn refresh_table_data(&mut self, tab_id: TabId) {
        let (pending, is_table, table, filter, conn_id) = {
            let Some(tab) = self.tab(tab_id) else {
                return;
            };
            (
                !tab.pending_changes.is_empty(),
                tab.is_table_tab(),
                tab.table.clone(),
                tab.data_filter.clone(),
                tab.conn_id.clone(),
            )
        };
        if !is_table {
            self.set_tab_status(tab_id, t("只有表数据标签可以刷新").into());
            return;
        }
        if pending {
            self.set_tab_status(tab_id, t("请先提交或回滚改动后再刷新").into());
            return;
        }
        let Some(table) = table else {
            return;
        };
        let Some(conn_id) = conn_id else {
            self.set_tab_status(tab_id, t("请先连接一个数据库").into());
            return;
        };
        let Some(kind) = self.entry(&conn_id).map(|entry| entry.cfg.kind) else {
            self.set_tab_status(tab_id, t("连接不存在").into());
            return;
        };
        let page = self.first_page();
        let filter = filter.trim();
        let sql = table_preview_sql(
            kind,
            &table,
            page,
            if filter.is_empty() {
                None
            } else {
                Some(filter)
            },
        );
        self.send_query(
            tab_id,
            QueryRequest {
                sql,
                page,
                append: false,
                table: Some(table),
                statement: None,
            },
        );
    }

    /// 写入筛选片段并从第一页重查。有未提交改动时拒绝。
    pub fn apply_table_filter(&mut self, tab_id: TabId, filter: String) {
        let pending = self
            .tab(tab_id)
            .is_some_and(|tab| !tab.pending_changes.is_empty());
        if pending {
            self.set_tab_status(tab_id, t("请先提交或回滚改动后再筛选").into());
            return;
        }
        if let Some(tab) = self.tab_mut(tab_id) {
            if !tab.is_table_tab() {
                tab.status = t("只有表数据标签可以筛选").into();
                return;
            }
            tab.data_filter = filter;
        }
        self.refresh_table_data(tab_id);
    }

    /// 提交前预览：与真正执行的语句来自同一个生成函数。
    pub fn pending_sql(&self, tab_id: TabId) -> Vec<String> {
        let Some(tab) = self.tab(tab_id) else {
            return Vec::new();
        };
        let Some(table) = tab.table.as_ref() else {
            return Vec::new();
        };
        let Some(kind) = tab
            .conn_id
            .as_deref()
            .and_then(|id| self.entry(id))
            .map(|entry| entry.cfg.kind)
        else {
            return Vec::new();
        };
        build_sqls(kind, table, &tab.pending_changes)
    }

    pub fn commit_changes(&mut self, tab_id: TabId) {
        let Some(tab) = self.tab(tab_id) else {
            return;
        };
        let Some(table) = tab.table.clone() else {
            self.set_tab_status(tab_id, t("没有可提交的表").into());
            return;
        };
        if tab.pending_changes.is_empty() {
            self.set_tab_status(tab_id, t("没有待提交的改动").into());
            return;
        }
        let Some(conn_id) = tab.conn_id.clone() else {
            self.set_tab_status(tab_id, t("请先连接一个数据库").into());
            return;
        };
        let changes = tab.pending_changes.clone();
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let Some(entry) = self.entry(&conn_id) else {
            return;
        };
        let Some(session) = entry.session.as_ref() else {
            self.set_tab_status(tab_id, t("连接未建立").into());
            return;
        };
        if let Err(err) = session.send(Command::Apply {
            request_id,
            table,
            changes,
        }) {
            self.set_tab_status(tab_id, err.message);
            return;
        }
        self.apply_requests
            .insert(request_id, (conn_id.clone(), tab_id));
        self.set_tab_status(tab_id, t("提交中…").into());
    }

    fn record_history(&mut self, tab_id: TabId, failed: bool, row_count: usize, elapsed_ms: u64) {
        let (sql, connection_id) = {
            let Some(tab) = self.tab_mut(tab_id) else {
                return;
            };
            if !tab.query_in_flight {
                return;
            }
            tab.query_in_flight = false;
            let Some(req) = tab.last_query.clone() else {
                return;
            };
            (req.sql, tab.conn_id.clone())
        };
        let history_limit = self.config.settings.history_limit as usize;
        self.history.push_with_limit(
            HistoryEntry {
                sql,
                connection_id,
                executed_at_ms: now_ms(),
                elapsed_ms,
                row_count,
                failed,
            },
            history_limit,
        );
        tracing::info!(
            path = %History::path().display(),
            entries = self.history.entries.len(),
            failed,
            "记录查询历史"
        );
        self.history.save_async();
    }

    /// 导出指定标签已加载的结果到文件（HOME 下的 thinsql-export-*）。
    ///
    /// 结果格式化与写盘都放到后台执行器：大结果导出不再卡住 UI。
    pub fn export(&mut self, tab_id: TabId, format: ExportFormat, cx: &mut Context<Self>) {
        let Some(tab) = self.tab(tab_id) else {
            return;
        };
        let Some(result) = tab.result.clone() else {
            self.set_tab_status(tab_id, t("没有可导出的结果").into());
            return;
        };

        let escape = match tab
            .conn_id
            .as_deref()
            .and_then(|id| self.entry(id))
            .map(|entry| entry.cfg.kind)
        {
            Some(DriverKind::MySql) => thinsql_core::provider::EscapeStyle::Backslash,
            _ => thinsql_core::provider::EscapeStyle::Standard,
        };
        let table = tab
            .table
            .as_ref()
            .map(|t| t.to_string())
            .unwrap_or_else(|| "table".to_string());
        let columns = result.columns.clone();
        let rows = result.rows.clone();

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dir = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let path = std::path::Path::new(&dir)
            .join(format!("thinsql-export-{stamp}.{}", format.extension()));

        self.set_tab_status(
            tab_id,
            tr("正在导出：{path}", &[("path", &path.display().to_string())]),
        );

        let export_task = cx.background_executor().spawn(async move {
            let text = match format {
                ExportFormat::Csv => export::to_csv(&columns, &rows),
                ExportFormat::Json => export::to_json(&columns, &rows),
                ExportFormat::Insert => export::to_insert(&table, &columns, &rows, escape),
            };
            if text.trim().is_empty() {
                return Err(t("结果为空，没有可导出的内容").to_string());
            }
            std::fs::write(&path, text.as_bytes())
                .map(|()| path)
                .map_err(|err| err.to_string())
        });

        cx.spawn(async move |this, cx| {
            let outcome = export_task.await;
            let _ = this.update(cx, |state, cx| {
                state.set_tab_status(
                    tab_id,
                    match outcome {
                        Ok(path) => {
                            tr("已导出：{path}", &[("path", &path.display().to_string())])
                        }
                        Err(err) => tr("导出失败：{error}", &[("error", &err)]),
                    },
                );
                cx.notify();
            });
        })
        .detach();
    }

    /// 取消是连接级的：会话工作线程此刻正阻塞在查询里，
    /// 只能请求它放弃当前语句。执行所有时剩余语句一并跳过。
    pub fn cancel(&mut self, tab_id: TabId) {
        let conn_id = self.tab(tab_id).and_then(|t| t.conn_id.clone());
        let Some(conn_id) = conn_id else {
            return;
        };
        if let Some(entry) = self.entry(&conn_id) {
            if let Some(session) = &entry.session {
                session.cancel();
            }
        }
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.queue.clear();
            for log in tab.statements.iter_mut() {
                if matches!(log.status, StatementStatus::Pending) {
                    log.status = StatementStatus::Skipped;
                }
            }
            tab.status = t("已请求取消（作用于该连接当前语句）").into();
        }
    }

    /// 展开/收起树节点，按需触发子节点加载（懒加载）。
    pub fn toggle_node(&mut self, node_id: &str, expanded: bool) {
        if expanded {
            self.expanded.insert(node_id.to_string());
        } else {
            self.expanded.remove(node_id);
            return;
        }

        let mut parts = node_id.splitn(3, ':');
        let kind = parts.next().unwrap_or_default();
        let conn_id = parts.next().unwrap_or_default();
        let rest = parts.next().map(str::to_string);

        match kind {
            "conn" => {
                let need_load = self
                    .entry(conn_id)
                    .map(|e| !e.schema_loaded && matches!(e.status, ConnStatus::Connected))
                    .unwrap_or(false);
                if need_load {
                    if let Some(entry) = self.entry_mut(conn_id) {
                        if let Some(session) = &entry.session {
                            if let Err(err) = session.send(Command::ListSchemas) {
                                tracing::debug!(error = %err.message, "发送 schema 查询失败");
                            }
                        }
                    }
                } else if let Some(entry) = self.entry_mut(conn_id) {
                    // 还没连上就展开：直接发起连接
                    if matches!(entry.status, ConnStatus::Idle | ConnStatus::Failed(_)) {
                        let id = conn_id.to_string();
                        self.connect(&id);
                    }
                }
            }
            "schema" => {
                // 库节点展开只露出分组，不立刻拉对象列表
            }
            "kind" => {
                let Some(rest) = rest else { return };
                let mut bits = rest.splitn(2, ':');
                let Some(schema) = bits.next().map(str::to_string) else {
                    return;
                };
                let Some(kind) = bits.next().and_then(ObjectKind::parse) else {
                    return;
                };
                let Some(entry) = self.entry(conn_id) else {
                    return;
                };
                if entry
                    .objects
                    .get(&schema)
                    .is_some_and(|m| m.contains_key(&kind))
                {
                    return;
                }
                let Some(session) = entry.session.as_ref() else {
                    return;
                };
                if session
                    .send(Command::ListObjects {
                        schema: Some(schema.clone()),
                        kind,
                    })
                    .is_ok()
                {
                    self.pending_objects
                        .insert((conn_id.to_string(), schema, kind));
                }
            }
            _ => {}
        }
    }

    fn set_status(&mut self, id: &str, status: ConnStatus) {
        if let Some(entry) = self.entry_mut(id) {
            entry.status = status;
        }
    }

    fn drain_events(&mut self, cx: &mut Context<Self>) -> bool {
        let mut events = Vec::new();
        for (ix, conn) in self.conns.iter().enumerate() {
            let Some(session) = &conn.session else {
                continue;
            };
            while let Some(event) = session.try_recv() {
                events.push((ix, event));
            }
        }

        if events.is_empty() {
            return true;
        }

        for (ix, event) in events {
            self.apply_event(ix, event);
        }
        cx.emit(StateEvent::Changed);
        cx.notify();
        true
    }

    fn apply_event(&mut self, ix: usize, event: Event) {
        match event {
            Event::Connected => {
                let (id, name) = {
                    let conn = &mut self.conns[ix];
                    conn.status = ConnStatus::Connected;
                    (conn.cfg.id.clone(), conn.cfg.name.clone())
                };
                self.status = tr("已连接 {name}", &[("name", &name)]);
                // 连上后自动展开根节点，让用户马上看到库
                self.expanded.insert(format!("conn:{id}"));
                // 连上立刻拉一次库列表，让树的根节点有内容
                if let Some(entry) = self.entry_mut(&id) {
                    if let Some(session) = &entry.session {
                        let _ = session.send(Command::ListSchemas);
                    }
                }

                // 点表时连接未建立：现在补发那条预览查询
                let waiting: Vec<(TabId, QueryRequest)> = self
                    .tabs
                    .iter()
                    .filter(|t| t.waiting_connect && t.conn_id.as_deref() == Some(id.as_str()))
                    .filter_map(|t| t.last_query.clone().map(|req| (t.id, req)))
                    .collect();
                for (tab_id, req) in waiting {
                    if let Some(tab) = self.tab_mut(tab_id) {
                        tab.waiting_connect = false;
                    }
                    self.send_query(tab_id, req);
                }
            }
            Event::Disconnected => {
                let conn_id = self.conns[ix].cfg.id.clone();
                {
                    let conn = &mut self.conns[ix];
                    conn.status = ConnStatus::Idle;
                    conn.schema_loaded = false;
                    conn.schemas.clear();
                    conn.objects.clear();
                    conn.in_flight.clear();
                }
                self.pending_objects
                    .retain(|(pending, _, _)| pending != &conn_id);
                for tab in self.tabs.iter_mut() {
                    if tab.waiting_connect && tab.conn_id.as_deref() == Some(conn_id.as_str()) {
                        tab.waiting_connect = false;
                        tab.status = t("连接已断开，未能执行").into();
                    }
                }
                self.status = t("已断开").into();
            }
            Event::Schemas(schemas) => {
                let conn_id = self.conns[ix].cfg.id.clone();
                let prefix = format!("schema:{conn_id}:");
                let conn = &mut self.conns[ix];
                conn.schemas = schemas.into_iter().map(|s| s.name).collect();
                conn.schema_loaded = true;
                let valid: HashSet<String> = conn
                    .schemas
                    .iter()
                    .map(|schema| format!("{prefix}{schema}"))
                    .collect();
                // 库被删除后，展开状态里不能留下永久失效的节点。
                self.expanded
                    .retain(|node| !node.starts_with(&prefix) || valid.contains(node));
            }
            Event::Tables {
                schema: requested,
                tables: incoming,
            } => {
                // 兼容旧 ListTables 回包：拆成 Table/View 两组
                let default_schema = self.conns[ix].cfg.database.clone().unwrap_or_default();
                let fallback = requested
                    .clone()
                    .or_else(|| Some(default_schema.clone()))
                    .filter(|s| !s.is_empty());
                let conn = &mut self.conns[ix];
                if incoming.is_empty() {
                    if let Some(schema) = fallback {
                        store_tables_as_objects(&mut conn.objects, schema, Vec::new());
                    }
                } else {
                    let mut grouped: BTreeMap<String, Vec<TableMeta>> = BTreeMap::new();
                    for table in incoming {
                        let schema = table.schema.clone().unwrap_or_else(|| {
                            fallback.clone().unwrap_or_else(|| default_schema.clone())
                        });
                        grouped.entry(schema).or_default().push(table);
                    }
                    for (schema, tables) in grouped {
                        store_tables_as_objects(&mut conn.objects, schema, tables);
                    }
                }
            }
            Event::Objects {
                schema: requested,
                kind,
                objects: incoming,
            } => {
                let conn_id = self.conns[ix].cfg.id.clone();
                if let Some(schema) = &requested {
                    self.pending_objects
                        .remove(&(conn_id, schema.clone(), kind));
                }
                let default_schema = self.conns[ix].cfg.database.clone().unwrap_or_default();
                let fallback = requested
                    .clone()
                    .or_else(|| Some(default_schema.clone()))
                    .filter(|s| !s.is_empty());
                let conn = &mut self.conns[ix];
                if incoming.is_empty() {
                    if let Some(schema) = fallback {
                        store_objects(&mut conn.objects, schema, kind, Vec::new());
                    }
                } else {
                    let mut grouped: BTreeMap<String, Vec<ObjectMeta>> = BTreeMap::new();
                    for obj in incoming {
                        let schema = obj.schema.clone().unwrap_or_else(|| {
                            fallback.clone().unwrap_or_else(|| default_schema.clone())
                        });
                        grouped.entry(schema).or_default().push(obj);
                    }
                    for (schema, objs) in grouped {
                        store_objects(&mut conn.objects, schema, kind, objs);
                    }
                }
            }
            Event::SchemaSet { schema } => {
                self.status = tr("会话已切换到 {schema}", &[("schema", &schema)]);
            }
            Event::Result { request_id, result } => {
                // 只接受仍在途的请求；过期/未知结果直接丢弃。
                let Some(info) = self.conns[ix].in_flight.remove(&request_id) else {
                    tracing::debug!(request_id, "丢弃过期或未知的查询结果");
                    return;
                };
                let Some(tab_ix) = self.tabs.iter().position(|t| t.id == info.tab) else {
                    tracing::debug!(request_id, "标签已关闭，丢弃结果");
                    return;
                };

                let mut incoming = result;
                if !info.append {
                    self.record_history(
                        info.tab,
                        false,
                        incoming.row_count(),
                        incoming.elapsed.as_millis() as u64,
                    );
                }

                let mut next: Option<(String, usize)> = None;
                {
                    let tab = &mut self.tabs[tab_ix];
                    if let Some(stmt_ix) = info.statement {
                        if let Some(log) = tab.statements.get_mut(stmt_ix) {
                            log.status = StatementStatus::Succeeded;
                            log.rows = incoming.row_count();
                            log.affected = incoming.affected_rows;
                            log.elapsed_ms = incoming.elapsed.as_millis() as u64;
                        }
                        next = tab.queue.pop_front().map(|sql| (sql, stmt_ix + 1));
                    }

                    if info.append {
                        let mut merged = match tab.result.take() {
                            Some(existing) => {
                                Arc::try_unwrap(existing).unwrap_or_else(|arc| (*arc).clone())
                            }
                            None => QueryResult::empty(Duration::ZERO),
                        };
                        let max_rows = self.config.settings.max_loaded_rows as usize;
                        let remaining = max_rows.saturating_sub(merged.rows.len());
                        let limit_reached = |status: &mut String, count: usize, max: usize| {
                            *status = tr(
                                "已加载 {n} 行，达到上限 {max} 行",
                                &[("n", &count.to_string()), ("max", &max.to_string())],
                            );
                        };
                        if remaining == 0 {
                            merged.has_more = false;
                            limit_reached(&mut tab.status, merged.row_count(), max_rows);
                        } else if incoming.rows.len() > remaining {
                            merged.rows.extend(incoming.rows.drain(..remaining));
                            merged.has_more = false;
                            merged.elapsed += incoming.elapsed;
                            limit_reached(&mut tab.status, merged.row_count(), max_rows);
                        } else {
                            merged.rows.append(&mut incoming.rows);
                            merged.has_more = incoming.has_more;
                            merged.elapsed += incoming.elapsed;
                            let more = if merged.has_more { t("（还有更多）") } else { "" };
                            tab.status = tr(
                                "已加载 {n} 行{more}",
                                &[("n", &merged.row_count().to_string()), ("more", more)],
                            );
                        }
                        tab.result = Some(Arc::new(merged));
                        tab.query_in_flight = false;
                    } else {
                        // 多语句执行时，只保留有结果集的那一份；没有结果集就显示摘要
                        if info.statement.is_none()
                            || !incoming.columns.is_empty()
                            || tab.result.is_none()
                        {
                            tab.status = tr(
                                "完成 · {n} 行 · {ms}ms",
                                &[
                                    ("n", &incoming.row_count().to_string()),
                                    ("ms", &incoming.elapsed.as_millis().to_string()),
                                ],
                            );
                            tab.result = Some(Arc::new(*incoming));
                        }
                    }
                    tab.result_rev += 1;
                }

                // 执行所有：上一条成功后才发下一条，遇错在 Failed 分支停止
                if let Some((sql, statement)) = next {
                    self.send_query(
                        info.tab,
                        QueryRequest {
                            sql,
                            page: self.first_page(),
                            append: false,
                            table: None,
                            statement: Some(statement),
                        },
                    );
                }
            }
            Event::Applied {
                request_id,
                outcome,
            } => {
                if let Some((_, tab_id)) = self.apply_requests.remove(&request_id) {
                    if let Some(tab) = self.tab_mut(tab_id) {
                        tab.pending_changes.clear();
                        tab.status = tr(
                            "已提交，影响 {n} 行",
                            &[("n", &outcome.affected.to_string())],
                        );
                    }
                    // 重新拉取该表：DELETE 的行与 UPDATE 的新值要立刻反映到网格，
                    // 否则用户会以为提交没生效。
                    if let Some(table) = self.tab(tab_id).and_then(|t| t.table.clone()) {
                        let page = self.first_page();
                        let filter = self
                            .tab(tab_id)
                            .map(|t| t.data_filter.clone())
                            .unwrap_or_default();
                        let filter = filter.trim();
                        let sql = table_preview_sql(
                            self.conns[ix].cfg.kind,
                            &table,
                            page,
                            if filter.is_empty() {
                                None
                            } else {
                                Some(filter)
                            },
                        );
                        self.send_query(
                            tab_id,
                            QueryRequest {
                                sql,
                                page,
                                append: false,
                                table: Some(table),
                                statement: None,
                            },
                        );
                    }
                }
            }
            Event::StructureLoaded {
                request_id,
                columns,
                indexes,
                foreign_keys,
                ddl,
                warnings,
            } => {
                if let Some((_, tab_id)) = self.structure_loads.remove(&request_id)
                    && let Some(tab) = self.tab_mut(tab_id)
                {
                    tab.structure_specs = columns.iter().map(column_spec_from_meta).collect();
                    tab.structure_original_names = columns
                        .iter()
                        .map(|c| Some(c.name.clone()))
                        .collect();
                    tab.structure_columns = columns;
                    tab.structure_indexes = indexes;
                    tab.structure_fks = foreign_keys;
                    tab.structure_ddl = ddl.unwrap_or_else(|message| format!("// {message}"));
                    tab.structure_changes = StructureChange::default();
                    tab.structure_loading = false;
                    tab.structure_rev += 1;
                    tab.status = if warnings.is_empty() {
                        t("表结构已加载").into()
                    } else {
                        tr(
                            "表结构已加载，但部分信息读取失败：{message}",
                            &[("message", &warnings.join("；"))],
                        )
                    };
                }
            }
            Event::StructureApplied { request_id } => {
                if let Some((_, tab_id)) = self.structure_applies.remove(&request_id) {
                    self.set_tab_status(tab_id, t("结构已应用").into());
                    self.load_structure(tab_id);
                }
            }
            Event::Cancelled { request_id } => {
                if let Some(id) = request_id {
                    if let Some(info) = self.conns[ix].in_flight.remove(&id) {
                        if let Some(tab) = self.tab_mut(info.tab) {
                            tab.query_in_flight = false;
                            if let Some(stmt_ix) = info.statement {
                                if let Some(log) = tab.statements.get_mut(stmt_ix) {
                                    log.status = StatementStatus::Cancelled;
                                }
                                for log in tab.statements.iter_mut() {
                                    if matches!(log.status, StatementStatus::Pending) {
                                        log.status = StatementStatus::Skipped;
                                    }
                                }
                                tab.queue.clear();
                            }
                            tab.status = t("已取消").into();
                        }
                    }
                }
            }
            Event::Failed { request_id, error } => {
                if let Some(id) = request_id {
                    if let Some(info) = self.conns[ix].in_flight.remove(&id) {
                        self.record_history(info.tab, true, 0, 0);
                        if let Some(tab) = self.tab_mut(info.tab) {
                            if let Some(stmt_ix) = info.statement {
                                if let Some(log) = tab.statements.get_mut(stmt_ix) {
                                    log.status = StatementStatus::Failed(error.message.clone());
                                }
                                for log in tab.statements.iter_mut() {
                                    if matches!(log.status, StatementStatus::Pending) {
                                        log.status = StatementStatus::Skipped;
                                    }
                                }
                                tab.queue.clear();
                            }
                            tab.status = tr(
                                "{message}（{hint}）",
                                &[("message", &error.message), ("hint", &error.hint())],
                            );
                        }
                        if matches!(
                            error.kind,
                            thinsql_core::ErrorKind::Auth | thinsql_core::ErrorKind::Network
                        ) {
                            self.conns[ix].status = ConnStatus::Failed(error.message.clone());
                        }
                        return;
                    }
                    if let Some((_, tab_id)) = self.column_requests.remove(&id) {
                        self.set_tab_status(
                            tab_id,
                            tr("读取表结构失败：{message}", &[("message", &error.message)]),
                        );
                        return;
                    }
                    if let Some((_, tab_id)) = self.apply_requests.remove(&id) {
                        self.set_tab_status(
                            tab_id,
                            tr(
                                "提交失败：{message}（{hint}）",
                                &[("message", &error.message), ("hint", &error.hint())],
                            ),
                        );
                        return;
                    }
                    if let Some((_, tab_id)) = self.structure_applies.remove(&id) {
                        self.set_tab_status(
                            tab_id,
                            tr(
                                "结构应用失败：{message}（{hint}）",
                                &[("message", &error.message), ("hint", &error.hint())],
                            ),
                        );
                        return;
                    }
                    if let Some((_, tab_id)) = self.structure_loads.remove(&id) {
                        if let Some(tab) = self.tab_mut(tab_id) {
                            tab.structure_loading = false;
                        }
                        self.set_tab_status(
                            tab_id,
                            tr("读取表结构失败：{message}", &[("message", &error.message)]),
                        );
                        return;
                    }
                }

                let conn_id = self.conns[ix].cfg.id.clone();
                self.pending_objects
                    .retain(|(pending, _, _)| pending != &conn_id);
                let conn = &mut self.conns[ix];
                if matches!(
                    error.kind,
                    thinsql_core::ErrorKind::Auth | thinsql_core::ErrorKind::Network
                ) {
                    conn.status = ConnStatus::Failed(error.message.clone());
                }
                self.status = format!("{}（{}）", error.message, error.hint());
            }
            Event::Columns {
                request_id,
                columns,
            } => {
                if let Some((_, tab_id)) = self.column_requests.remove(&request_id) {
                    if let Some(tab) = self.tab_mut(tab_id) {
                        tab.current_pk = columns
                            .iter()
                            .filter(|c| c.is_primary_key)
                            .map(|c| c.name.clone())
                            .collect();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppState, ConnEntry, ConnStatus, EditTarget, InFlight, QueryTab, StatementLog,
        StatementStatus, TabId, pending_cell_value, store_objects, store_tables_as_objects,
        table_tab_title,
    };
    use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
    use std::sync::Arc;
    use std::time::Duration;
    use thinsql_core::Value;
    use thinsql_core::config::AppConfig;
    use thinsql_core::history::History;
    use thinsql_core::model::{
        ColumnSpec, ConnectionConfig, DriverKind, IndexMeta, IndexOp, ObjectKind, ObjectMeta,
        QueryResult, RowChange, SchemaMeta, TableKind, TableMeta, TableRef,
    };
    use thinsql_core::session::Event;

    fn conn(id: &str) -> ConnEntry {
        ConnEntry {
            cfg: ConnectionConfig::new(id, id, DriverKind::MySql),
            session: None,
            status: ConnStatus::Idle,
            schemas: Vec::new(),
            objects: BTreeMap::new(),
            schema_loaded: false,
            in_flight: HashMap::new(),
        }
    }

    fn test_state(conns: Vec<ConnEntry>) -> AppState {
        AppState {
            config: AppConfig::default(),
            conns,
            expanded: HashSet::new(),
            current: None,
            status: String::new(),
            history: History::default(),
            tabs: Vec::new(),
            active_tab: None,
            tree_selection: None,
            pending_objects: HashSet::new(),
            apply_requests: HashMap::new(),
            column_requests: HashMap::new(),
            structure_loads: HashMap::new(),
            structure_applies: HashMap::new(),
            next_request_id: 1,
            next_tab_id: 1,
            next_query_no: 1,
        }
    }

    fn add_tab(state: &mut AppState, conn: &str) -> TabId {
        state.current = Some(conn.into());
        state.new_query_tab()
    }

    fn table_meta(name: &str) -> TableMeta {
        TableMeta {
            schema: Some("public".into()),
            name: name.into(),
            kind: TableKind::Table,
            approx_rows: None,
        }
    }

    fn table_meta_in(schema: &str, name: &str) -> TableMeta {
        TableMeta {
            schema: Some(schema.into()),
            name: name.into(),
            kind: TableKind::Table,
            approx_rows: None,
        }
    }

    fn query_result(rows: usize) -> Box<QueryResult> {
        Box::new(QueryResult {
            columns: Vec::new(),
            rows: (0..rows).map(|i| vec![Value::Int(i as i64)]).collect(),
            elapsed: Duration::ZERO,
            affected_rows: None,
            has_more: false,
        })
    }

    #[test]
    fn stale_query_result_is_ignored() {
        let mut state = test_state(vec![conn("c1")]);
        let tab = add_tab(&mut state, "c1");
        state.conns[0].in_flight.insert(
            1,
            InFlight {
                tab,
                append: false,
                statement: None,
            },
        );

        state.apply_event(
            0,
            Event::Result {
                request_id: 99,
                result: query_result(1),
            },
        );
        assert!(
            state.tab(tab).and_then(|t| t.result.as_ref()).is_none(),
            "未知 request_id 的结果必须丢弃"
        );

        state.apply_event(
            0,
            Event::Result {
                request_id: 1,
                result: query_result(1),
            },
        );
        assert_eq!(
            state
                .tab(tab)
                .and_then(|t| t.result.as_ref())
                .map(|r| r.row_count()),
            Some(1)
        );
    }

    #[test]
    fn switching_tab_connection_drops_old_in_flight_results() {
        let mut state = test_state(vec![conn("c1"), conn("c2")]);
        let tab = add_tab(&mut state, "c1");
        state.conns[0].in_flight.insert(
            41,
            InFlight {
                tab,
                append: false,
                statement: None,
            },
        );

        state.set_tab_connection(tab, "c2".into());
        state.apply_event(
            0,
            Event::Result {
                request_id: 41,
                result: query_result(1),
            },
        );

        assert!(state.conns[0].in_flight.is_empty());
        assert!(state.tab(tab).and_then(|t| t.result.as_ref()).is_none());
    }

    #[test]
    fn reselecting_same_connection_keeps_in_flight_results() {
        let mut state = test_state(vec![conn("c1")]);
        let tab = add_tab(&mut state, "c1");
        state.conns[0].in_flight.insert(
            41,
            InFlight {
                tab,
                append: false,
                statement: None,
            },
        );

        state.set_tab_connection(tab, "c1".into());
        state.apply_event(
            0,
            Event::Result {
                request_id: 41,
                result: query_result(1),
            },
        );

        assert!(
            state.tab(tab).and_then(|t| t.result.as_ref()).is_some(),
            "重复选择同一连接不能丢掉在途结果"
        );
        assert_eq!(state.current.as_deref(), Some("c1"));
    }

    #[test]
    fn switching_tab_connection_resets_query_in_flight() {
        let mut state = test_state(vec![conn("c1"), conn("c2")]);
        let tab = add_tab(&mut state, "c1");
        state.tab_mut(tab).unwrap().query_in_flight = true;

        state.set_tab_connection(tab, "c2".into());

        assert!(
            !state.tab(tab).unwrap().query_in_flight,
            "在途请求已随旧连接丢弃，标签不能一直显示执行中"
        );
    }

    #[test]
    fn result_routes_to_originating_tab_even_when_not_active() {
        let mut state = test_state(vec![conn("c1"), conn("c2")]);
        let tab1 = add_tab(&mut state, "c1");
        let tab2 = add_tab(&mut state, "c2");
        assert_eq!(state.active_tab, Some(tab2));
        state.conns[0].in_flight.insert(
            5,
            InFlight {
                tab: tab1,
                append: false,
                statement: None,
            },
        );

        state.apply_event(
            0,
            Event::Result {
                request_id: 5,
                result: query_result(1),
            },
        );

        assert_eq!(
            state
                .tab(tab1)
                .and_then(|t| t.result.as_ref())
                .map(|r| r.row_count()),
            Some(1),
            "非活动标签的结果也要写回自己的标签"
        );
        assert!(state.tab(tab2).and_then(|t| t.result.as_ref()).is_none());
    }

    #[test]
    fn closing_tab_clears_in_flight_and_drops_late_results() {
        let mut state = test_state(vec![conn("c1")]);
        let tab = add_tab(&mut state, "c1");
        state.conns[0].in_flight.insert(
            3,
            InFlight {
                tab,
                append: false,
                statement: None,
            },
        );

        state.close_tab(tab);
        assert!(state.tabs.is_empty());
        assert!(
            state.conns[0].in_flight.is_empty(),
            "关闭标签要清理在途请求"
        );
        assert_eq!(state.active_tab, None);

        state.apply_event(
            0,
            Event::Result {
                request_id: 3,
                result: query_result(1),
            },
        );
        assert!(state.tabs.is_empty());
    }

    #[test]
    fn closing_active_tab_moves_to_neighbour() {
        let mut state = test_state(vec![conn("c1")]);
        let first = add_tab(&mut state, "c1");
        let second = add_tab(&mut state, "c1");
        state.activate_tab(second);
        state.close_tab(second);
        assert_eq!(state.active_tab, Some(first));
    }

    #[test]
    fn load_more_stops_at_row_cap() {
        let mut state = test_state(vec![conn("c1")]);
        let tab = add_tab(&mut state, "c1");
        let max_rows = state.config.settings.max_loaded_rows as usize;
        state.tab_mut(tab).expect("标签存在").result = Some(Arc::new(*query_result(max_rows - 1)));
        state.conns[0].in_flight.insert(
            2,
            InFlight {
                tab,
                append: true,
                statement: None,
            },
        );

        state.apply_event(
            0,
            Event::Result {
                request_id: 2,
                result: query_result(10),
            },
        );

        let result = state
            .tab(tab)
            .and_then(|t| t.result.as_ref())
            .expect("结果应保留");
        assert_eq!(result.row_count(), max_rows, "追加必须被上限截断");
        assert!(!result.has_more, "达到上限后不应再显示还有更多");
        assert!(
            state
                .tab(tab)
                .map(|t| t.status.contains("上限"))
                .unwrap_or(false),
            "状态栏应提示达到上限"
        );
    }

    #[test]
    fn append_result_clears_running_flag() {
        let mut state = test_state(vec![conn("c1")]);
        let tab = add_tab(&mut state, "c1");
        state.tab_mut(tab).expect("标签存在").result = Some(Arc::new(*query_result(1)));
        state.tab_mut(tab).expect("标签存在").query_in_flight = true;
        state.conns[0].in_flight.insert(
            2,
            InFlight {
                tab,
                append: true,
                statement: None,
            },
        );

        state.apply_event(
            0,
            Event::Result {
                request_id: 2,
                result: query_result(1),
            },
        );

        assert!(
            !state.tab(tab).expect("标签存在").is_running(),
            "分页结果返回后必须结束运行态，否则标签一直显示加载中"
        );
    }

    #[test]
    fn switching_table_tab_connection_degrades_to_query_tab() {
        let mut state = test_state(vec![conn("c1"), conn("c2")]);
        let tab = add_tab(&mut state, "c1");
        state.tab_mut(tab).expect("标签存在").table =
            Some(TableRef::new(Some("public".into()), "orders"));
        assert!(state.tab(tab).unwrap().is_table_tab());

        state.set_tab_connection(tab, "c2".into());

        let tab = state.tab(tab).expect("标签存在");
        assert!(
            !tab.is_table_tab(),
            "切换连接后旧表上下文失效，应降级为查询标签"
        );
        assert!(tab.table.is_none());
    }

    #[test]
    fn switching_tab_connection_clears_result_and_pending_edits() {
        let mut state = test_state(vec![conn("c1"), conn("c2")]);
        let tab = add_tab(&mut state, "c1");
        state.tab_mut(tab).expect("标签存在").result = Some(Arc::new(*query_result(2)));
        state.set_tab_connection(tab, "c2".into());
        let tab = state.tab(tab).expect("标签存在");
        assert_eq!(tab.conn_id.as_deref(), Some("c2"));
        assert!(tab.result.is_none(), "换连接后旧结果必须清空");
    }

    #[test]
    fn execute_all_records_pending_statements_and_truncates_long_scripts() {
        let mut state = test_state(vec![conn("c1")]);
        let tab = add_tab(&mut state, "c1");
        let sql = (0..210)
            .map(|i| format!("SELECT {i}"))
            .collect::<Vec<_>>()
            .join("; ");
        state.execute_all(tab, sql);
        let tab = state.tab(tab).expect("标签存在");
        assert!(tab.summary_mode);
        assert_eq!(tab.statements.len(), 200, "语句数必须截断到上限");
        assert!(tab.status.contains("截断"));
        assert_eq!(tab.queue.len(), 199);
    }

    #[test]
    fn statement_result_updates_log_and_advances_queue() {
        let mut state = test_state(vec![conn("c1")]);
        let tab_id = add_tab(&mut state, "c1");
        {
            let tab = state.tab_mut(tab_id).expect("标签存在");
            tab.summary_mode = true;
            tab.statements = (0..2)
                .map(|index| StatementLog {
                    index,
                    status: StatementStatus::Pending,
                    rows: 0,
                    affected: None,
                    elapsed_ms: 0,
                })
                .collect();
            tab.queue = VecDeque::from(["SELECT 2".to_string()]);
        }
        state.conns[0].in_flight.insert(
            7,
            InFlight {
                tab: tab_id,
                append: false,
                statement: Some(0),
            },
        );

        state.apply_event(
            0,
            Event::Result {
                request_id: 7,
                result: query_result(3),
            },
        );

        let tab = state.tab(tab_id).expect("标签存在");
        assert_eq!(tab.statements[0].status, StatementStatus::Succeeded);
        assert_eq!(tab.statements[0].rows, 3);
        assert!(tab.queue.is_empty(), "下一条语句已被取出下发");
    }

    #[test]
    fn failed_statement_skips_remaining_statements() {
        let mut state = test_state(vec![conn("c1")]);
        let tab_id = add_tab(&mut state, "c1");
        {
            let tab = state.tab_mut(tab_id).expect("标签存在");
            tab.summary_mode = true;
            tab.statements = (0..3)
                .map(|index| StatementLog {
                    index,
                    status: StatementStatus::Pending,
                    rows: 0,
                    affected: None,
                    elapsed_ms: 0,
                })
                .collect();
            tab.queue = VecDeque::from(["SELECT 2".to_string(), "SELECT 3".to_string()]);
        }
        state.conns[0].in_flight.insert(
            8,
            InFlight {
                tab: tab_id,
                append: false,
                statement: Some(0),
            },
        );

        state.apply_event(
            0,
            Event::Failed {
                request_id: Some(8),
                error: thinsql_core::Error::new(thinsql_core::ErrorKind::ServerError, "boom"),
            },
        );

        let tab = state.tab(tab_id).expect("标签存在");
        assert!(matches!(
            tab.statements[0].status,
            StatementStatus::Failed(_)
        ));
        assert_eq!(tab.statements[1].status, StatementStatus::Skipped);
        assert_eq!(tab.statements[2].status, StatementStatus::Skipped);
        assert!(tab.queue.is_empty(), "遇错必须停止后续语句");
        assert!(tab.has_failed_statement());
    }

    #[test]
    fn columns_event_routes_to_requesting_tab() {
        let mut state = test_state(vec![conn("c1")]);
        let tab_id = add_tab(&mut state, "c1");
        state.column_requests.insert(11, ("c1".into(), tab_id));
        state.apply_event(
            0,
            Event::Columns {
                request_id: 11,
                columns: vec![thinsql_core::model::ColumnMeta {
                    name: "id".into(),
                    type_name: "bigint".into(),
                    nullable: false,
                    default: None,
                    is_primary_key: true,
                    is_auto_increment: false,
                    comment: None,
                    charset: None,
                    collation: None,
                }],
            },
        );
        assert_eq!(
            state.tab(tab_id).map(|t| t.current_pk.clone()),
            Some(vec!["id".to_string()])
        );
    }

    #[test]
    fn disconnect_clears_column_and_apply_requests_for_that_connection() {
        let mut state = test_state(vec![conn("c1"), conn("c2")]);
        let tab1 = add_tab(&mut state, "c1");
        let tab2 = add_tab(&mut state, "c2");
        state.column_requests.insert(11, ("c1".into(), tab1));
        state.apply_requests.insert(12, ("c1".into(), tab1));
        state.column_requests.insert(13, ("c2".into(), tab2));
        state.apply_requests.insert(14, ("c2".into(), tab2));

        state.disconnect("c1");

        assert!(!state.column_requests.contains_key(&11));
        assert!(!state.apply_requests.contains_key(&12));
        assert!(
            state.column_requests.contains_key(&13) && state.apply_requests.contains_key(&14),
            "其他连接的请求不能被误清"
        );
    }

    #[test]
    fn concurrent_table_loads_land_on_their_own_schema() {
        let mut state = test_state(vec![conn("c1")]);

        state.apply_event(
            0,
            Event::Tables {
                schema: Some("a".into()),
                tables: vec![table_meta_in("a", "t1")],
            },
        );
        state.apply_event(
            0,
            Event::Tables {
                schema: Some("b".into()),
                tables: vec![table_meta_in("b", "t2")],
            },
        );
        state.apply_event(
            0,
            Event::Tables {
                schema: Some("empty".into()),
                tables: Vec::new(),
            },
        );

        assert_eq!(
            state.conns[0].table_names("a"),
            Some(&vec!["t1".to_string()])
        );
        assert_eq!(
            state.conns[0].table_names("b"),
            Some(&vec!["t2".to_string()])
        );
        assert!(
            state.conns[0].objects.contains_key("empty"),
            "空结果也必须记到请求的 schema 上"
        );
        assert_eq!(
            state.conns[0].table_names("empty"),
            Some(&Vec::<String>::new())
        );
    }

    #[test]
    fn schema_refresh_drops_stale_expanded_nodes() {
        let mut state = test_state(vec![conn("c1")]);
        state.expanded.insert("conn:c1".into());
        state.expanded.insert("schema:c1:public".into());
        state.expanded.insert("schema:c1:old".into());

        state.apply_event(
            0,
            Event::Schemas(vec![SchemaMeta {
                name: "public".into(),
            }]),
        );

        assert!(state.expanded.contains("conn:c1"), "连接根节点应保留");
        assert!(
            state.expanded.contains("schema:c1:public"),
            "仍存在的 schema 应保留"
        );
        assert!(
            !state.expanded.contains("schema:c1:old"),
            "已删除的 schema 不应留在展开集合里"
        );
    }

    #[test]
    fn empty_table_list_marks_schema_as_loaded() {
        let mut objects = BTreeMap::new();
        store_tables_as_objects(&mut objects, "public".into(), Vec::new());
        assert!(objects.contains_key("public"));
        assert_eq!(
            objects["public"].get(&ObjectKind::Table),
            Some(&Vec::<String>::new())
        );
    }

    #[test]
    fn table_lists_are_sorted_unique_and_replaced() {
        let mut objects = BTreeMap::new();
        let incoming = vec![table_meta("z"), table_meta("a"), table_meta("a")];
        store_tables_as_objects(&mut objects, "public".into(), incoming.clone());
        store_tables_as_objects(&mut objects, "public".into(), incoming);
        assert_eq!(
            objects["public"].get(&ObjectKind::Table),
            Some(&vec!["a".to_string(), "z".to_string()])
        );
    }

    #[test]
    fn store_objects_keeps_kinds_independent() {
        let mut objects = BTreeMap::new();
        store_objects(
            &mut objects,
            "public".into(),
            ObjectKind::Function,
            vec![ObjectMeta {
                schema: Some("public".into()),
                name: "f1".into(),
                kind: ObjectKind::Function,
            }],
        );
        store_objects(&mut objects, "public".into(), ObjectKind::Table, Vec::new());
        assert_eq!(
            objects["public"].get(&ObjectKind::Function),
            Some(&vec!["f1".to_string()])
        );
        assert_eq!(
            objects["public"].get(&ObjectKind::Table),
            Some(&Vec::<String>::new())
        );
    }

    #[test]
    fn new_query_tab_inherits_explicit_schema() {
        let mut state = test_state(vec![conn("c1")]);
        let id = state.new_query_tab_on(Some("c1".into()), Some("hishop".into()), String::new());
        assert_eq!(state.tab(id).unwrap().schema.as_deref(), Some("hishop"));
        let id2 = state.new_query_tab_on(Some("c1".into()), None, String::new());
        assert!(state.tab(id2).unwrap().schema.is_none());
    }

    #[test]
    fn new_query_tab_inherits_tree_selection_schema() {
        let mut state = test_state(vec![conn("c1")]);
        state.current = Some("c1".into());
        state.tree_selection = Some("schema:c1:hishop".into());
        let id = state.new_query_tab();
        assert_eq!(state.tab(id).unwrap().schema.as_deref(), Some("hishop"));
        assert_eq!(state.tab(id).unwrap().conn_id.as_deref(), Some("c1"));

        state.tree_selection = Some("kind:c1:shop:table".into());
        let id2 = state.new_query_tab();
        assert_eq!(state.tab(id2).unwrap().schema.as_deref(), Some("shop"));
    }

    #[test]
    fn table_tab_title_includes_schema_when_present() {
        assert_eq!(
            table_tab_title(&TableRef::new(Some("public".into()), "orders")),
            "orders@public"
        );
        assert_eq!(table_tab_title(&TableRef::new(None, "orders")), "orders");
    }

    #[test]
    fn removing_connection_closes_its_tabs() {
        let mut state = test_state(vec![conn("c1"), conn("c2")]);
        state.config.connections = state.conns.iter().map(|c| c.cfg.clone()).collect();
        let tab1 = add_tab(&mut state, "c1");
        let tab2 = add_tab(&mut state, "c2");
        state.expanded.insert("conn:c1".into());
        state.expanded.insert("schema:c1:public".into());
        state.expanded.insert("conn:c2".into());

        let removed = state.detach_connection("c1");

        assert_eq!(removed.map(|cfg| cfg.id), Some("c1".to_string()));
        assert!(state.conns.iter().all(|c| c.cfg.id != "c1"));
        assert!(state.config.connections.iter().all(|c| c.id != "c1"));
        assert_eq!(state.current.as_deref(), Some("c2"));
        assert!(state.tab(tab1).is_none(), "该连接的标签要一起关闭");
        assert!(state.tab(tab2).is_some());
        assert_eq!(state.active_tab, Some(tab2));
        assert!(!state.expanded.contains("conn:c1"));
        assert!(!state.expanded.contains("schema:c1:public"));
        assert!(state.expanded.contains("conn:c2"));
    }

    #[test]
    fn removing_other_connection_keeps_current_and_tabs() {
        let mut state = test_state(vec![conn("c1"), conn("c2")]);
        state.config.connections = state.conns.iter().map(|c| c.cfg.clone()).collect();
        let tab = add_tab(&mut state, "c1");

        let removed = state.detach_connection("c2");

        assert_eq!(removed.map(|cfg| cfg.id), Some("c2".to_string()));
        assert_eq!(state.current.as_deref(), Some("c1"));
        assert_eq!(state.active_tab, Some(tab));
        assert_eq!(state.conns.len(), 1);
    }

    #[test]
    fn opening_same_table_twice_reuses_the_existing_tab() {
        let mut state = test_state(vec![conn("c1")]);
        let table = TableRef::new(Some("db".into()), "users");
        let mut tab = QueryTab::new(1, "users".into(), Some("c1".into()), Some("db".into()));
        tab.table = Some(table.clone());
        state.tabs.push(tab);
        state.active_tab = Some(2);

        let id = state.open_table_tab("c1", table);

        assert_eq!(id, 1, "重复点击应复用原标签");
        assert_eq!(state.tabs.len(), 1, "不得新建重复标签");
        assert_eq!(state.active_tab, Some(1), "复用后应聚焦原标签");
    }

    #[test]
    fn structure_tab_is_independent_from_data_tab() {
        let mut state = test_state(vec![conn("c1")]);
        let table = TableRef::new(Some("db".into()), "users");
        let mut data_tab = QueryTab::new(1, "users".into(), Some("c1".into()), None);
        data_tab.table = Some(table.clone());
        let mut structure_tab = QueryTab::new(2, "结构".into(), Some("c1".into()), None);
        structure_tab.structure = Some(table.clone());
        state.tabs.extend([data_tab, structure_tab]);
        state.active_tab = Some(1);

        assert_eq!(state.open_structure_tab("c1", table.clone()), 2);
        assert_eq!(state.open_table_tab("c1", table), 1);
        assert_eq!(state.tabs.len(), 2, "数据标签与结构标签互不干扰");
        assert_eq!(state.active_tab, Some(1));
    }

    #[test]
    fn pending_cell_value_returns_latest_update() {
        let pending = vec![
            RowChange::Update {
                key: vec![("id".into(), Value::Int(1))],
                column: "name".into(),
                new_value: Value::Text("a".into()),
            },
            RowChange::Update {
                key: vec![("id".into(), Value::Int(1))],
                column: "name".into(),
                new_value: Value::Text("b".into()),
            },
        ];
        let value = pending_cell_value(&pending, &[("id".into(), Value::Int(1))], "name");
        assert_eq!(value, Some(Value::Text("b".into())));
        assert!(pending_cell_value(&pending, &[("id".into(), Value::Int(2))], "name").is_none());
    }

    #[test]
    fn stage_change_reverting_to_original_clears_pending() {
        let mut state = test_state(vec![conn("c1")]);
        let mut tab = QueryTab::new(1, "users".into(), Some("c1".into()), Some("db".into()));
        tab.table = Some(TableRef::new(Some("db".into()), "users"));
        state.tabs.push(tab);
        let target = EditTarget {
            row: 0,
            col: 1,
            column_name: "name".into(),
            type_name: "text".into(),
            original: Value::Text("old".into()),
            key: vec![("id".into(), Value::Int(1))],
        };
        state.stage_change(1, &target, Value::Text("new".into()));
        assert_eq!(state.tab(1).unwrap().pending_changes.len(), 1);
        state.stage_change(1, &target, Value::Text("old".into()));
        assert!(state.tab(1).unwrap().pending_changes.is_empty());
    }

    #[test]
    fn staged_delete_is_previewed_as_delete_sql() {
        let mut state = test_state(vec![conn("c1")]);
        let mut tab = QueryTab::new(1, "users".into(), Some("c1".into()), Some("db".into()));
        tab.table = Some(TableRef::new(Some("db".into()), "users"));
        tab.pending_changes.push(RowChange::Update {
            key: vec![("id".into(), Value::Int(3))],
            column: "name".into(),
            new_value: Value::Text("新值".into()),
        });
        state.tabs.push(tab);

        state.stage_delete(1, vec![("id".into(), Value::Int(3))]);

        let sqls = state.pending_sql(1);
        assert_eq!(sqls.len(), 1, "同一行的 UPDATE 应被 DELETE 取代");
        assert!(sqls[0].starts_with("DELETE FROM"), "实际: {}", sqls[0]);
        assert!(sqls[0].contains("`id` = 3"), "实际: {}", sqls[0]);
    }

    #[test]
    fn open_table_tab_uses_settings_page_limit() {
        let mut state = test_state(vec![conn("c1")]);
        state.config.settings.default_page_limit = 100;
        state.conns[0].status = ConnStatus::Idle;
        let id = state.open_table_tab("c1", TableRef::new(Some("db".into()), "users"));
        let tab = state.tab(id).expect("标签存在");
        assert!(
            tab.initial_sql.contains("LIMIT 100"),
            "应使用设置页大小，实际: {}",
            tab.initial_sql
        );
        assert_eq!(
            tab.last_query.as_ref().map(|q| q.page.limit),
            Some(100)
        );
    }

    #[test]
    fn refresh_table_data_refuses_when_pending() {
        let mut state = test_state(vec![conn("c1")]);
        let mut tab = QueryTab::new(1, "users".into(), Some("c1".into()), Some("db".into()));
        tab.table = Some(TableRef::new(Some("db".into()), "users"));
        tab.pending_changes.push(RowChange::Delete {
            key: vec![("id".into(), Value::Int(1))],
        });
        state.tabs.push(tab);

        state.refresh_table_data(1);

        let status = &state.tab(1).expect("标签存在").status;
        assert!(
            status.contains("提交") || status.contains("回滚"),
            "有 pending 时应拒绝刷新，实际: {status}"
        );
    }

    #[test]
    fn apply_table_filter_stores_filter_and_refuses_when_pending() {
        let mut state = test_state(vec![conn("c1")]);
        state.conns[0].status = ConnStatus::Idle;
        let mut tab = QueryTab::new(1, "users".into(), Some("c1".into()), Some("db".into()));
        tab.table = Some(TableRef::new(Some("db".into()), "users"));
        state.tabs.push(tab);

        state.apply_table_filter(1, "id > 10".into());
        assert_eq!(state.tab(1).unwrap().data_filter, "id > 10");

        state.tab_mut(1).unwrap().pending_changes.push(RowChange::Delete {
            key: vec![("id".into(), Value::Int(1))],
        });
        state.apply_table_filter(1, "id > 20".into());
        assert_eq!(
            state.tab(1).unwrap().data_filter,
            "id > 10",
            "有 pending 时不得改写筛选"
        );
        let status = &state.tab(1).unwrap().status;
        assert!(
            status.contains("提交") || status.contains("回滚") || status.contains("筛选"),
            "实际: {status}"
        );
    }

    #[test]
    fn row_pending_kind_prefers_delete_over_update() {
        use super::row_pending_kind;
        use super::RowPendingKind;
        let key = vec![("id".into(), Value::Int(1))];
        let pending = vec![
            RowChange::Update {
                key: key.clone(),
                column: "name".into(),
                new_value: Value::Text("x".into()),
            },
            RowChange::Delete { key: key.clone() },
        ];
        assert_eq!(
            row_pending_kind(&pending, &key),
            Some(RowPendingKind::Deleted)
        );
        assert_eq!(
            row_pending_kind(
                &[RowChange::Update {
                    key: key.clone(),
                    column: "name".into(),
                    new_value: Value::Text("x".into()),
                }],
                &key
            ),
            Some(RowPendingKind::Modified)
        );
        assert_eq!(
            row_pending_kind(&[], &key),
            None
        );
    }

    fn structure_tab_with_index(state: &mut AppState, name: &str) -> TabId {
        state.conns[0].cfg.read_only = false;
        let mut tab = QueryTab::new(1, "结构".into(), Some("c1".into()), Some("db".into()));
        tab.structure = Some(TableRef::new(Some("db".into()), "users"));
        tab.structure_indexes = vec![IndexMeta {
            name: name.into(),
            columns: vec!["id".into()],
            unique: false,
        }];
        tab.structure_specs.push(ColumnSpec {
            name: "id".into(),
            data_type: "int".into(),
            length: None,
            nullable: false,
            primary_key: true,
            default: None,
            comment: None,
            charset: None,
            collation: None,
            auto_increment: false,
        });
        state.tabs.push(tab);
        1
    }

    #[test]
    fn structure_update_index_drops_and_recreates_existing() {
        let mut state = test_state(vec![conn("c1")]);
        let tab = structure_tab_with_index(&mut state, "idx_a");

        state.structure_update_index(
            tab,
            "idx_a",
            "idx_b".into(),
            vec!["id".into()],
            true,
        );

        assert_eq!(
            state.tab(tab).unwrap().structure_changes.index_ops,
            vec![
                IndexOp::Drop {
                    name: "idx_a".into()
                },
                IndexOp::Create {
                    name: "idx_b".into(),
                    columns: vec!["id".into()],
                    unique: true,
                },
            ]
        );
    }

    #[test]
    fn structure_update_index_replaces_pending_create() {
        let mut state = test_state(vec![conn("c1")]);
        let tab = structure_tab_with_index(&mut state, "idx_a");
        state.structure_add_index(tab, "idx_new".into(), vec!["id".into()], false);

        state.structure_update_index(
            tab,
            "idx_new",
            "idx_new2".into(),
            vec!["id".into()],
            true,
        );

        assert_eq!(
            state.tab(tab).unwrap().structure_changes.index_ops,
            vec![IndexOp::Create {
                name: "idx_new2".into(),
                columns: vec!["id".into()],
                unique: true,
            }]
        );
    }

    #[test]
    fn structure_update_index_noops_when_unchanged() {
        let mut state = test_state(vec![conn("c1")]);
        let tab = structure_tab_with_index(&mut state, "idx_a");
        let rev = state.tab(tab).unwrap().structure_rev;

        state.structure_update_index(
            tab,
            "idx_a",
            "idx_a".into(),
            vec!["id".into()],
            false,
        );

        assert!(state.tab(tab).unwrap().structure_changes.index_ops.is_empty());
        assert_eq!(state.tab(tab).unwrap().structure_rev, rev);
    }
}
