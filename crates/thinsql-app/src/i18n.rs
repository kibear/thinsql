//! 界面文案的双语查找层。
//!
//! 设计约束：
//! - 文案表以**中文原文为 key**（可含 `{placeholder}` 占位符），英文表给出对应模式串。
//!   现有调用点可机械迁移：`format!("已连接 {name}")` → `tr("已连接 {name}", &[("name", &name)])`，
//!   纯静态文案 `Button::new("新建连接")` → `t("新建连接")`。
//! - 语言在启动时由 main.rs 按 `settings.language` 初始化一次，运行中不热切换
//!   （与日志级别一致，重启后生效），因此全局状态用 `OnceLock` 即可，无需响应式刷新。
//! - 英文表未覆盖的 key 回退返回中文原文：迁移可以分批进行，缺一条不至于崩 UI。
//!
//! 验证逻辑（lookup/render）抽成纯函数，单测可覆盖两种语言而不碰进程全局状态。

use std::collections::HashMap;
use std::sync::OnceLock;

use thinsql_core::config::Language;

/// 英文文案表：中文原文（key）→ 英文模式串。
/// 由各文件迁移时逐步扩充；顺序无所谓，首次查找时构建一次 HashMap。
static EN: &[(&str, &str)] = &[
    // shell.rs 工具条与状态栏
    ("未连接", "Not connected"),
    ("新建连接", "New Connection"),
    ("新建查询", "New Query"),
    ("刷新", "Refresh"),
    ("设置", "Settings"),
    ("影响 {affected} 行", "{affected} rows affected"),
    ("{n} 行", "{n} rows"),
    ("默认库", "default"),
    ("未选择库", "none"),
    // sidebar.rs
    ("搜索连接或表", "Search connections or tables"),
    ("还没有执行过查询", "No queries executed yet"),
    ("失败", "Failed"),
    ("{n} 行 · {ms}ms", "{n} rows · {ms}ms"),
    ("点击一条复制 SQL", "Click an entry to copy SQL"),
    ("结构", "Schema"),
    ("历史", "History"),
    ("断开连接", "Disconnect"),
    ("连接", "Connect"),
    ("编辑连接…", "Edit Connection…"),
    ("只读模式", "Read-only mode"),
    ("删除连接…", "Delete Connection…"),
    ("刷新库列表", "Refresh databases"),
    ("打开数据", "Open Data"),
    ("修改表结构", "Edit Table Structure"),
    // tree_model.rs 树占位节点
    ("加载中…", "Loading…"),
    ("（无表）", "(no tables)"),
    ("（无库）", "(no databases)"),
    ("连接中…", "Connecting…"),
    ("展开以连接并加载库", "Expand to connect and load databases"),
    ("展开以加载", "Expand to load"),
    // workspace.rs
    ("还没有打开的标签", "No tabs open"),
    ("未选择连接", "No connection selected"),
    ("暂无连接", "No connections"),
    ("暂无库列表", "No databases loaded"),
    ("只读", "Read-only"),
    ("执行", "Run"),
    ("执行所有", "Run All"),
    ("取消", "Cancel"),
    // connection_dialog.rs
    (
        "端口必须是 1-65535 的数字，当前是 {value}",
        "Port must be a number between 1-65535, got {value}",
    ),
    ("端口必须在 1-65535 之间", "Port must be between 1 and 65535"),
    ("连接名称不能为空", "Connection name cannot be empty"),
    ("连接名称", "Connection name"),
    ("本地 MySQL", "Local MySQL"),
    ("主机", "Host"),
    ("端口", "Port"),
    ("用户名", "Username"),
    ("密码（留空则不修改）", "Password (leave empty to keep current)"),
    ("密码", "Password"),
    ("默认库（可空）", "Default database (optional)"),
    ("测试连接", "Test Connection"),
    ("正在测试连接…", "Testing connection…"),
    ("连接成功（{ms}ms）", "Connected ({ms}ms)"),
    ("连接失败：{message}（{hint}）", "Connection failed: {message} ({hint})"),
    ("编辑连接", "Edit Connection"),
    ("保存", "Save"),
    ("数据库类型", "Database type"),
    ("名称", "Name"),
    ("默认库", "Default database"),
    ("未选择库", "No database selected"),
    ("删除连接「{name}」？", "Delete connection \"{name}\"?"),
    (
        "配置与已保存的密码会一起删除，无法撤销。",
        "The configuration and saved password will be deleted together. This cannot be undone.",
    ),
    ("删除", "Delete"),
    // settings_dialog.rs
    (
        "{label}必须是 {min}-{max} 的整数，当前是 {value}",
        "{label} must be an integer between {min}-{max}, got {value}",
    ),
    ("{label}必须在 {min}-{max} 之间", "{label} must be between {min}-{max}"),
    (
        "{label}必须是 {min}-{max} 的数字，当前是 {value}",
        "{label} must be a number between {min}-{max}, got {value}",
    ),
    ("外观", "Appearance"),
    ("数据", "Data"),
    ("历史", "History"),
    ("连接", "Connection"),
    ("日志", "Logging"),
    ("主题模式", "Theme mode"),
    ("品牌色保持不变", "Brand colors stay unchanged"),
    ("语言", "Language"),
    ("重启后生效", "Takes effect after restart"),
    ("跟随系统", "Follow System"),
    ("中文", "中文"),
    ("English", "English"),
    ("界面字体", "UI font"),
    ("留空跟随系统默认", "Leave empty to follow system default"),
    ("界面字号", "UI font size"),
    ("等宽字体", "Monospace font"),
    ("SQL 编辑器与结果表格", "SQL editor and result tables"),
    ("等宽字号", "Monospace font size"),
    ("每页条数", "Rows per page"),
    ("点表预览时每页取多少行", "Rows fetched per page when previewing a table"),
    ("结果最大加载行数", "Max loaded rows"),
    (
        "「加载更多」累计到该行数后停止",
        "\"Load more\" stops after reaching this many rows",
    ),
    ("历史记录条数", "History entries"),
    ("超出后丢弃最旧的记录", "Oldest entries are dropped beyond this limit"),
    ("新建连接默认只读", "New connections read-only by default"),
    ("只影响新建连接的初始勾选", "Only affects the initial checkbox for new connections"),
    ("日志级别", "Log level"),
    ("下次启动生效", "Takes effect on next start"),
    ("深色", "Dark"),
    ("浅色", "Light"),
    ("错误", "Error"),
    ("警告", "Warn"),
    ("信息", "Info"),
    ("调试", "Debug"),
    ("跟踪", "Trace"),
    // state.rs 状态与错误消息
    ("连接中", "Connecting"),
    ("已连接", "Connected"),
    ("连接失败", "Connection failed"),
    ("失败: {msg}", "Failed: {msg}"),
    ("等待", "Pending"),
    ("成功", "Succeeded"),
    ("已跳过", "Skipped"),
    ("就绪", "Ready"),
    ("表", "Tables"),
    ("视图", "Views"),
    ("物化视图", "Materialized Views"),
    ("存储过程", "Procedures"),
    ("函数", "Functions"),
    ("序列", "Sequences"),
    ("触发器", "Triggers"),
    ("事件", "Events"),
    ("（空）", "(empty)"),
    ("复制名称", "Copy name"),
    ("已切换到库 {schema}", "Switched to {schema}"),
    ("会话已切换到 {schema}", "Session switched to {schema}"),

    ("查询 {no}", "Query {no}"),
    ("已切换连接，结果已清空", "Connection switched, results cleared"),
    ("密码保存失败：{message}", "Failed to save password: {message}"),
    ("正在连接 {name}…", "Connecting to {name}…"),
    ("已删除连接 {name}", "Deleted connection {name}"),
    ("正在刷新库列表…", "Refreshing databases…"),
    ("没有可执行的语句", "No statements to execute"),
    ("共 {total} 条语句（已截断到 {max} 条）", "{total} statements (truncated to {max})"),
    ("共 {total} 条语句", "{total} statements"),
    ("执行中…（语句已截断到 {max} 条）", "Running… (statements truncated to {max})"),
    ("没有可继续加载的结果", "Nothing left to load"),
    ("已经是最后一页", "Already on the last page"),
    ("多语句执行的结果不支持继续加载", "Multi-statement results do not support loading more"),
    ("请先连接一个数据库", "Connect to a database first"),
    ("连接未建立", "Connection not established"),
    ("标签已关闭", "Tab is closed"),
    ("当前连接是只读模式，请先关闭只读再编辑", "Connection is read-only. Turn off read-only before editing"),
    ("该表没有主键，无法安全定位要修改的行", "This table has no primary key; rows cannot be located safely"),
    ("还没有结果集", "No result set yet"),
    ("列不存在", "Column does not exist"),
    ("行不存在", "Row does not exist"),
    ("已放弃改动", "Changes discarded"),
    ("没有可提交的表", "No table to commit"),
    ("没有待提交的改动", "No pending changes to commit"),
    ("提交中…", "Committing…"),
    ("没有可导出的结果", "No results to export"),
    ("正在导出：{path}", "Exporting: {path}"),
    ("结果为空，没有可导出的内容", "Result is empty, nothing to export"),
    ("已导出：{path}", "Exported: {path}"),
    ("导出失败：{error}", "Export failed: {error}"),
    ("已请求取消（作用于该连接当前语句）", "Cancellation requested (applies to the connection's current statement)"),
    ("连接已断开，未能执行", "Connection closed before execution"),
    ("已断开", "Disconnected"),
    ("已加载 {n} 行，达到上限 {max} 行", "Loaded {n} rows, reached the limit of {max} rows"),
    ("已加载 {n} 行{more}", "Loaded {n} rows{more}"),
    ("（还有更多）", " (more available)"),
    ("完成 · {n} 行 · {ms}ms", "Done · {n} rows · {ms}ms"),
    ("已提交，影响 {n} 行", "Committed, {n} rows affected"),
    ("{message}（{hint}）", "{message} ({hint})"),
    ("读取表结构失败：{message}", "Failed to load table schema: {message}"),
    ("提交失败：{message}（{hint}）", "Commit failed: {message} ({hint})"),
    // results.rs
    ("尚无结果", "No results yet"),
    ("影响 {affected} 行 · {ms}ms", "{affected} rows affected · {ms}ms"),
    ("{n} 行{more} · {ms}ms", "{n} rows{more} · {ms}ms"),
    ("新值", "New value"),
    ("导出 CSV", "Export CSV"),
    ("导出 JSON", "Export JSON"),
    ("导出 INSERT", "Export INSERT"),
    ("加载更多", "Load More"),
    ("清除", "Clear"),
    ("筛选条件，例如 id > 10", "Filter, e.g. id > 10"),
    ("请先提交或回滚改动后再刷新", "Commit or discard changes before refreshing"),
    ("请先提交或回滚改动后再筛选", "Commit or discard changes before filtering"),
    ("只有表数据标签可以刷新", "Only table data tabs can refresh"),
    ("只有表数据标签可以筛选", "Only table data tabs can filter"),
    ("导出", "Export"),
    ("状态", "Status"),
    ("返回行", "Rows"),
    ("影响行", "Affected"),
    ("耗时", "Time"),
    ("未返回结果集", "no result set"),
    ("第 {row} 行 · {column} · {type}", "Row {row} · {column} · {type}"),
    ("待提交 {n} 处改动", "{n} pending change(s)"),
    ("提交", "Commit"),
    ("回滚", "Rollback"),
    // single_instance.rs stderr 错误信息
    (
        "ThinSQL 已在运行（进程 {pid}），请切换到已有窗口",
        "ThinSQL is already running (PID {pid}). Switch to the existing window",
    ),
    (
        "ThinSQL 已在运行，请切换到已有窗口",
        "ThinSQL is already running. Switch to the existing window",
    ),
    (
        "无法获取单实例锁：{error}",
        "Failed to acquire the single-instance lock: {error}",
    ),
    // structure.rs 表结构编辑面板（页签、表头、按钮、状态）
    ("DDL", "DDL"),
    ("Columns", "Columns"),
    ("Indexes", "Indexes"),
    ("#", "#"),
    ("Column", "Column"),
    ("Type", "Type"),
    ("Length", "Length"),
    ("Nullable", "Nullable"),
    ("Primary Key", "Primary Key"),
    ("Default", "Default"),
    ("Comment", "Comment"),
    ("Charset", "Charset"),
    ("Collation", "Collation"),
    ("Auto Inc", "Auto Inc"),
    ("Name", "Name"),
    ("UNIQUE", "UNIQUE"),
    ("INDEX", "INDEX"),
    ("删除列", "Delete column"),
    ("列名", "Column name"),
    ("类型", "Type"),
    ("长度", "Length"),
    ("默认值", "Default"),
    ("注释", "Comment"),
    ("字符集", "Charset"),
    ("排序规则", "Collation"),
    ("索引名", "Index name"),
    ("列名，逗号分隔", "Columns, comma-separated"),
    ("列名不能为空", "Column name cannot be empty"),
    ("添加列", "Add column"),
    ("编辑选中列", "Edit selected column"),
    ("删除选中列", "Delete selected column"),
    ("编辑第 {row} 列", "Edit column {row}"),
    ("新增列", "New column"),
    ("可空", "Nullable"),
    ("主键", "Primary key"),
    ("自增", "Auto increment"),
    ("（自动命名）", "(auto-named)"),
    ("新建索引", "New index"),
    ("编辑索引", "Edit index"),
    ("无索引", "No indexes"),
    ("添加字段", "Add field"),
    ("删除字段", "Remove field"),
    ("选择列", "Select column"),
    ("索引字段", "Index column"),
    ("无可用字段", "No columns available"),
    ("索引至少需要一个字段", "An index needs at least one column"),
    ("索引字段不能重复", "Index columns cannot be duplicated"),
    ("唯一", "Unique"),
    ("暂无 DDL", "No DDL available"),
    ("待提交 {n} 项结构变更", "{n} pending structure change(s)"),
    ("SQL", "SQL"),
    ("应用", "Apply"),
    ("放弃", "Discard"),
    ("当前驱动不支持修改表结构", "This driver does not support editing table structure"),
    ("无字段", "No columns"),
    ("加载表结构…", "Loading table structure…"),
    ("表结构已加载", "Table structure loaded"),
    ("表结构已加载，但部分信息读取失败：{message}", "Table structure loaded, but some information failed to load: {message}"),
    ("已放弃结构改动", "Structure changes discarded"),
    ("正在应用结构变更…", "Applying structure changes…"),
    ("没有待提交的结构改动", "No pending structure changes"),
    ("结构 · {name}", "Structure · {name}"),
    ("结构已应用", "Structure applied"),
];

/// 启动时调用一次：按配置解析出的语言初始化全局文案语言。
/// 运行中不热切换，重复初始化以第一次为准（OnceLock 语义）。
pub fn init(lang: Language) {
    let _ = LANG.set(lang);
}

/// 当前生效的界面语言；未初始化时默认中文（与现有硬编码文案一致，保证测试/工具可用）。
pub fn lang() -> Language {
    *LANG.get_or_init(|| Language::Zh)
}

static LANG: OnceLock<Language> = OnceLock::new();

/// 静态文案查找：返回 `&'static str`，可直接用于组件标签。
/// 中文模式原样返回 key（零拷贝）；英文查表，未命中回退 key。
pub fn t(key: &'static str) -> &'static str {
    lookup(lang(), key)
}

/// 带占位符的文案：先按语言查找模式串，再替换 `{name}` 占位符。
/// args 里的 `name` 对应模式串中的 `{name}`；模式串里出现但 args 未提供的占位符原样保留。
pub fn tr(key: &'static str, args: &[(&str, &str)]) -> String {
    render(lookup(lang(), key), args)
}

/// 纯函数：按语言查文案。Zh 恒等于 key；En 查英文表，未命中回退 key（中文）。
fn lookup(lang: Language, key: &str) -> &str {
    match lang {
        Language::Zh => key,
        Language::En => en_table().get(key).copied().unwrap_or(key),
    }
}

fn en_table() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| EN.iter().copied().collect())
}

/// 纯函数：把模式串里的 `{name}` 占位符替换成 args 对应的值。
/// 未知占位符（args 里没有）保持原样，方便发现漏传参数。
fn render(pattern: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match args.iter().find(|(k, _)| *k == name) {
                    Some((_, value)) => out.push_str(value),
                    // 漏传参数：连花括号原样保留，便于发现缺失
                    None => out.push_str(&rest[start..=start + end + 1]),
                }
                rest = &after[end + 1..];
            }
            None => {
                // 没有配对右括号：把剩余内容原样吐回去，避免吞掉用户可见文本
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zh_lookup_returns_key_itself() {
        assert_eq!(lookup(Language::Zh, "新建连接"), "新建连接");
        assert_eq!(lookup(Language::Zh, "已连接 {name}"), "已连接 {name}");
    }

    #[test]
    fn en_lookup_falls_back_to_key_when_missing() {
        assert_eq!(lookup(Language::En, "不在英文表里的文案"), "不在英文表里的文案");
    }

    #[test]
    fn structure_menu_is_translated_in_english() {
        assert_eq!(lookup(Language::En, "打开数据"), "Open Data");
        assert_eq!(lookup(Language::En, "修改表结构"), "Edit Table Structure");
    }

    #[test]
    fn index_select_copy_is_translated_in_english() {
        assert_eq!(lookup(Language::En, "选择列"), "Select column");
        assert_eq!(lookup(Language::En, "索引字段"), "Index column");
        assert_eq!(lookup(Language::En, "无可用字段"), "No columns available");
        assert_eq!(lookup(Language::En, "编辑索引"), "Edit index");
        assert_eq!(lookup(Language::En, "无索引"), "No indexes");
        assert_eq!(
            lookup(Language::En, "索引字段不能重复"),
            "Index columns cannot be duplicated"
        );
    }

    #[test]
    fn render_replaces_known_placeholders() {
        let out = render("已连接 {name}（{host}）", &[("name", "本地"), ("host", "127.0.0.1")]);
        assert_eq!(out, "已连接 本地（127.0.0.1）");
    }

    #[test]
    fn render_keeps_unknown_placeholders_verbatim() {
        let out = render("已加载 {n} 行", &[]);
        assert_eq!(out, "已加载 {n} 行", "漏传参数应原样保留占位符便于排查");
    }

    #[test]
    fn render_handles_unbalanced_braces_without_losing_text() {
        assert_eq!(render("奇怪的 {文本", &[]), "奇怪的 {文本");
        assert_eq!(render("奇怪的 文本}", &[]), "奇怪的 文本}");
        assert_eq!(render("{{}}", &[]), "{{}}");
    }

    #[test]
    fn render_supports_repeated_and_adjacent_placeholders() {
        let out = render("{a}/{a}/{b}", &[("a", "x"), ("b", "y")]);
        assert_eq!(out, "x/x/y");
    }

    #[test]
    fn default_language_is_zh() {
        // lang() 的 OnceLock 可能已被其他测试初始化；这里只验证未初始化语义不 panic，
        // 以及 t() 对中文模式恒等返回（两种语言下迁移期都安全）。
        assert_eq!(t("新建连接"), "新建连接");
    }
}
