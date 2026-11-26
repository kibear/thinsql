# ThinSQL 正确性与健壮性审查报告

日期：2026-11-18
范围：`crates/thinsql-core`、`crates/thinsql-app` 全部模块（只读审查）
结论：发现 **8 项 P0**（会静默产生错误结果或丢数据）、**10 项 P1**（会话/安全/可观测性/UI 阻塞）、若干 P2。建议按 P0 → P1 顺序分批修复。

> 本报告只关注正确性与健壮性。结果集三份拷贝、每帧字符串分配、80ms 轮询等纯性能问题不纳入本次实施，仅在 §6 作为相关建议记录。

## 1. 审查目标与方法

- 目标：找出会在异常、并发、数据损坏场景下**静默**产生错误结果、丢数据、卡死 UI 或埋下安全风险的问题。
- 方法：逐文件阅读 + 针对关键路径核对源码；所有发现均带 `文件:行号` 证据。
- 约束：数据库专有知识只能出现在 `thinsql-core/src/drivers/` 下；修复不得改变正常路径的产品行为。

## 2. 结论摘要

| 级别 | 数量 | 主题 |
|---|---|---|
| P0 | 8 | 会话变量泄漏、配置/历史损坏覆盖、类型解码与元数据吞错、结果串号、并发状态覆盖、结果无界增长 |
| P1 | 10 | 会话关闭阻塞、线程创建 panic、TLS 被忽略、RETURNING 误判、输入校验缺失、无效状态残留、吞错不可观测、UI 线程同步 I/O、PID 复用误判 |
| P2 | 4+ | 依赖语义待复核、非法 UTF-8 静默替换、环境变量重复读取、日志分配 |

## 3. P0 发现

### P0-1 MySQL 会话级 `sql_select_limit` 泄漏

- 证据：`crates/thinsql-core/src/drivers/mysql.rs:254-259` 每次 `query()` 执行 `SET SESSION sql_select_limit = page.probe_limit()`（=501）；查询流程 `:261-313` 结束后**从不恢复**。同连接的元数据查询 `list_schemas` `:316-325`、`list_tables`/`list_columns`/`list_indexes`/`list_foreign_keys` 直接 `conn.query(...)`，会继承该会话变量。
- 症状：超过 501 行的元数据（表、列、索引、外键）被静默截断，界面表现为“树里少表 / 少列 / 少约束”；取消或错误提前返回时同样留下被修改的会话状态。
- 根因：会话状态被修改但缺少统一恢复路径（所有 return/`?`/cancel 分支都跳过恢复）。
- 修复方向：把查询主体放入内层函数，外层无论成功/失败/取消都执行 `SET SESSION sql_select_limit = DEFAULT` 恢复；恢复失败记 `warn`。若需保留用户自定义值，可先读 `@@SESSION.sql_select_limit` 再恢复。
- 验证：集成测试先执行一次分页查询，再查 >501 行的元数据，断言结果未被截断；并覆盖取消路径。

### P0-2 配置文件损坏时静默降级为空配置

- 证据：`crates/thinsql-core/src/config.rs:62-77` 读取/解析失败直接返回 `AppConfig::default()`；`:80-93` `save()` 以临时文件 + rename 覆盖原文件。
- 症状：用户看到“没有连接”；一旦再次保存，损坏前的配置被永久覆盖，造成不可逆数据丢失。
- 根因：加载失败与“确实没有配置”不可区分，且损坏文件无备份。
- 修复方向：回退默认值前先把损坏文件重命名为 `config.json.corrupt-<unix_ms>`；新增 `load_from_with_warning` 返回 `LoadOutcome { value, warning }`，由 UI 明确提示；保留原 `load_from` 签名以兼容现有测试。
- 验证：单测写入非法 JSON，断言返回默认值、生成备份文件、`warning` 非空。

### P0-3 历史文件损坏静默清空

- 证据：`crates/thinsql-core/src/history.rs:44-51` 解析失败 `Self::default()`；`:58-67` 保存覆盖。
- 症状：历史记录无声消失，且下一次记录会覆盖原文件。
- 根因：同 P0-2。
- 修复方向：与 config 一致：先备份 `history.json.corrupt-<ts>` 再重置，并通过 warning 透传。
- 验证：单测覆盖损坏备份与 warning。

### P0-4 PostgreSQL NUMERIC 解码失败静默变空串

- 证据：`crates/thinsql-core/src/drivers/postgres.rs:51-59`，`:53` 为 `decode_numeric(raw).unwrap_or_default()`。
- 症状：解码失败显示为空串，用户无法区分“真 NULL / 空值 / 解码失败”，与项目“不要静默变 NULL”的设计原则冲突。
- 根因：`FromSql::from_sql` 把错误吞成默认值。
- 修复方向：解码失败返回可辨识文本（如原始 hex 前缀 `\\x...`）并 `warn`；或在 `convert` 中对 `Type::NUMERIC` 显式处理并返回 `Err` 语义。
- 验证：构造非法 NUMERIC 字节的单测；集成测试覆盖正常 NUMERIC 精度与 scale。

### P0-5 PostgreSQL 元数据查询静默丢行 / 吞错

- 证据：`drivers/postgres.rs:305-309`（schema 丢行）、`:326-343`（表丢行）、`:369-390`（列丢行）、`:412-429`（索引丢行）、`:453-466`（外键 `continue` 丢行）；版本查询 `:186-190` `query_opt(...).ok().flatten()` 吞错。
- 症状：类型不符或异常行被静默跳过，表现为“表结构少列/少约束”；版本信息缺失无从排查。
- 根因：`filter_map(|r| ... .ok()?)` / `continue` 把错误当控制流。
- 修复方向：改为显式 `match`，单行失败 `warn` 并继续、计数汇总；全量失败返回 `Err`；版本查询保留错误日志。
- 验证：集成测试创建含特殊类型的表，断言列/约束数量完整；单测覆盖错误分支。

### P0-6 结果事件无请求 ID，靠可变 UI 状态关联

- 证据：`crates/thinsql-core/src/session.rs:20-41`（`Command` 无 id）、`:43-54`（`Event` 无 id）；`crates/thinsql-app/src/state.rs:665-686` 按连接 drain，`:747-777` 的 `append` 依赖 `self.last_query`。
- 症状：迟到/乱序事件（取消后到达、切换连接/表后到达、多语句）可能被错误 append 到新结果或覆盖，出现“串数据”。
- 根因：命令与事件没有唯一关联键，响应归属依赖 UI 当前可变状态。
- 修复方向：`Command::Query` 与 `Event::Result` 增加 `request_id: u64`；`Event::Failed`/`Event::Cancelled` 携带 `Option<RequestId>`；`AppState` 为每条连接维护在途请求集合，只接受匹配的响应，过期事件丢弃。
- 验证：单测模拟乱序/过期事件，断言不会污染当前结果。

### P0-7 `pending_tables` 单一全局槽位

- 证据：`crates/thinsql-app/src/state.rs:140`（`Option<(String,String)>`）、`:711`、`:719-724`、`:284`、`:298-304`。
- 症状：两个连接或两个 schema 并发加载表列表时互相覆盖，空列表事件被记到错误 schema 或直接丢弃。
- 根因：全局只保留一个在途加载标记。
- 修复方向：改为 `HashSet<(conn_id, schema)>`，或在 `ConnEntry` 内按 schema 记录在途加载。
- 验证：单测模拟两条并发 `ListTables` 响应，断言各自落到正确 schema。

### P0-8 “加载更多”结果无上限增长

- 证据：`crates/thinsql-app/src/state.rs:756-768`，`:758` `existing.rows.append(&mut incoming.rows)`。
- 症状：反复 Load more 使内存无上限增长，存在 OOM 风险。
- 根因：追加路径没有行数上限。
- 修复方向：设置 `MAX_LOADED_ROWS`（建议 50_000）硬上限，达到后停止追加并在状态栏提示；或改为窗口化分页只保留当前页附近数据。
- 验证：单测构造多页追加，断言行数被截断且给出提示。

## 4. P1 发现

### P1-1 `Session::drop` 可能无限阻塞 UI 线程

- 证据：`session.rs:100-106` 发送 `Shutdown` 后 `join()`；`run` `:119-161` 顺序处理命令，`Shutdown` 排在在途查询之后。
- 症状：查询卡在网络或大结果时，断开/退出会卡死 UI。
- 修复方向：先 `cancel.store(true)` 再发 `Shutdown`；`join` 放后台线程或加超时，避免 UI 线程阻塞。
- 验证：单测/手动模拟长查询期间断开，断言 UI 不被阻塞。

### P1-2 PostgreSQL 始终使用 `NoTls`，忽略 `SslMode::Prefer/Required`

- 证据：`drivers/postgres.rs:181-185`，仅 `Disabled` 设置 `ssl_mode(Disable)`，其余一律 `connect(NoTls)`。
- 症状：用户配置 SSL 被静默忽略，存在明文传输风险。
- 修复方向：按 `SslMode` 配置 TLS connector；若暂不接入 TLS，`Required` 必须返回明确错误，`Prefer` 至少记录“已降级为 NoTls”告警。
- 验证：单测/集成测试断言 `Required` 不会静默成功。

### P1-3 `returns_rows` 用 `contains("returning")` 误判

- 证据：`drivers/postgres.rs:151-162`，`:161` `sql.to_ascii_lowercase().contains("returning")`。
- 症状：字符串字面量/注释中出现 `returning` 会误判;整串小写拷贝也有额外开销。
- 修复方向：顶层 token 扫描（忽略字符串/注释）判断 `RETURNING`；或 `prepare` 后以 `columns().is_empty()` 判定是否返回行。
- 验证：单测覆盖含字面量/注释的 `INSERT ... RETURNING` 与普通 `INSERT`。

### P1-4 端口解析失败静默回退 3306

- 证据：`crates/thinsql-app/src/panels/connections.rs:300-304` `.parse::<u16>().unwrap_or(3306)`。
- 症状：输入非法端口被静默改成 3306，可能连错数据库。
- 修复方向：校验并内联报错，阻止保存。
- 验证：单测校验端口解析函数。

### P1-5 连接 ID 用毫秒时间戳，存在碰撞风险

- 证据：`connections.rs:309-315`。
- 症状：同一毫秒内创建两个连接会得到相同 id，导致配置/状态互相覆盖。
- 修复方向：纳秒时间戳 + 进程内 `AtomicU64` 计数，避免新增依赖。
- 验证：单测并发生成 ID，断言唯一。

### P1-6 生产路径 `.expect` panic

- 证据：`crates/thinsql-app/src/main.rs:51`（窗口创建）、`crates/thinsql-core/src/session.rs:73`（线程创建）。
- 症状：资源失败直接 panic，无友好提示。
- 修复方向：`main.rs` 可保留（启动致命）；`Session::spawn` 改为返回 `Result`，由 UI 显示错误。
- 验证：单测注入失败路径（如可注入的线程创建器）或至少编译期签名调整。

### P1-7 `expanded` 集合只在删除连接时清理

- 证据：`state.rs:294-297`；schema/表刷新时未清理。
- 症状：schema/表被删除后残留节点 id，长期运行会积累无效条目。
- 修复方向：`Schemas`/`Tables` 事件刷新时 `retain` 仍存在的节点。
- 验证：单测模拟删除 schema 后断言 `expanded` 不含其节点。

### P1-8 `disconnect` 与事件发送吞错

- 证据：`drivers/mysql.rs:221-227`（`let _ = conn.query_drop("DO 1")`）、`session.rs:115-117`（`let _ = event_tx.send(...)`）。
- 症状：断开失败或事件通道关闭无任何日志，排查困难。
- 修复方向：补 `tracing::warn!`，不改变控制流。
- 验证：日志断言或人工检查。

### P1-9 同步磁盘/钥匙串 I/O 在 UI 线程

- 证据：`state.rs:157`（config load）、`:192`（history load）、`:327`（save）、`:548`（history save）、`:554-594`（export 写文件）；`config.rs` 的 keyring 调用为阻塞 IPC。
- 症状：慢磁盘/钥匙串超时会让 UI 卡顿。
- 修复方向：移入后台线程/异步，通过事件回传结果与错误。
- 验证：手动冒烟；单测覆盖后台任务的成功/失败回调。

### P1-10 `single_instance` PID 复用误判

- 证据：`single_instance.rs:92-95`，`/proc/<pid>` 存在即认为存活。
- 症状：PID 被系统复用时误判“已在运行”。
- 修复方向：校验 cmdline/启动时间，或改用 `flock` 锁文件。
- 验证：单测注入存活判断（现有 `acquire_with` 已支持）。

## 5. P2 发现（仅记录）

- `drivers/mysql.rs:162` `row.unwrap()`：需随 `mysql` crate 升级复核语义，避免未来版本变为可失败。
- `drivers/postgres.rs:71` `String::from_utf8_lossy` 会静默替换非法 UTF-8，可考虑返回错误或保留原始字节标记。
- `config.rs:40-49` `AppConfig::dir()` 每次调用重读环境变量，可缓存。
- `error.rs:113-121` `log_summary()` 每次分配 `String`，热路径可优化。

## 6. 相关建议（性能，非本次范围）

- 结果数据在 `AppState` / `ResultsPanel::sync` / `ResultDelegate` 之间重复 2–3 份；`value.display()` 每可见单元格每帧分配。
- `history.rs` 面板每帧克隆最多 200 条历史；`schema_tree` 每次 `Changed` 重建整棵树。
- 会话层两个 `unbounded` channel 无背压；80ms 轮询即使空闲也持续唤醒。
- 以上单独排期，不混入本次正确性修复。

## 7. 建议实施批次

| 批次 | 内容 | 说明 |
|---|---|---|
| A | P0-1、P0-2、P0-3、P0-4、P0-5 | 状态泄漏与静默吞错 |
| B | P0-6、P0-7、P0-8 | 并发正确性 |
| C | P1-1、P1-2、P1-3、P1-6 | 会话生命周期与安全 |
| D | P1-4、P1-5、P1-7、P1-8、P1-9、P1-10 | UI 健壮性与可观测性 |
| E | P2 | 仅报告，按需再排期 |

## 8. 验证计划

1. `cargo test -p thinsql-core`：config/history 损坏备份、NUMERIC 解码、request_id/上限/端口/ID 等单测。
2. `THINSQL_TEST_MYSQL=1 THINSQL_TEST_MYSQL_PASS=root cargo test -p thinsql-core --test mysql_integration`：分页后元数据不被截断。
3. `THINSQL_TEST_PG=1 THINSQL_TEST_PG_PASS=root cargo test -p thinsql-core --test postgres_integration`：NUMERIC 精度与元数据完整性。
4. 手动冒烟：长查询期间断开/退出、非法端口保存、配置损坏启动提示。

## 9. 修复状态（2026-11-18）

| 编号 | 状态 | 说明 |
|---|---|---|
| P0-1 | 已修复 | 查询前后保存/恢复 `sql_select_limit`；新增 `paginated_query_does_not_truncate_later_metadata` 集成测试 |
| P0-2 | 已修复 | 损坏配置备份为 `config.json.corrupt-<nanos>`，`load_with_warning` 透传提示 |
| P0-3 | 已修复 | 损坏历史备份为 `history.json.corrupt-<nanos>`，启动 warning 合并展示 |
| P0-4 | 已修复 | NUMERIC 解码失败保留原始字节并以 hex 展示；新增 3 个单测 |
| P0-5 | 已修复 | 元数据查询改为 `collect_meta`：逐行 warn，全量失败返回 `Err` |
| P0-6 | 已修复 | `Command::Query`/`Event::Result` 增加 `request_id`；过期/非当前连接结果丢弃 |
| P0-7 | 已修复 | `pending_tables` 改为 `HashSet`；`Event::Tables` 回显 schema，空结果也能落到正确 schema |
| P0-8 | 已修复 | `MAX_LOADED_ROWS = 50_000`，达到上限停止追加并提示 |
| P1-1 | 已修复 | `Session::Drop` 先取消再后台 join，不再阻塞 UI |
| P1-2 | 已修复 | PostgreSQL `Required` 返回明确错误；`Prefer` 告警并降级明文 |
| P1-3 | 已修复 | 移除 `returns_rows` 启发式，改用 `prepare` 后 `columns().is_empty()` 判定 |
| P1-4 | 已修复 | 端口非法时阻止保存并在状态栏提示 |
| P1-5 | 已修复 | 连接 ID 改为纳秒 + 原子计数 |
| P1-6 | 已修复 | `Session::spawn` 返回 `Result`，UI 显示线程创建失败 |
| P1-7 | 已修复 | schema 刷新时清理失效的 `expanded` 节点 |
| P1-8 | 已修复 | 断开/事件发送/单实例锁失败补日志 |
| P1-9 | 部分修复 | config/history 改为后台合并写入；导出格式化与写盘移入后台执行器；keyring IPC 仍在 UI 线程 |
| P1-10 | 未改动 | `/proc/<pid>` 存活判断仍可能受 PID 复用影响，列为后续改进 |
| P2 | 未改动 | 仅记录 |

### 验证结果

- `cargo test -p thinsql-core`：通过（74 单测 + 集成用例；未启动数据库时集成用例自动跳过）。
- `cargo test -p thinsql-app --lib`：通过（22 单测）。
- 真实 MySQL 集成：**4/4 通过**（含新增的元数据截断回归用例）。
- 真实 PostgreSQL 集成：**未执行成功**，本机 `127.0.0.1:5432` 连接被拒（`ConnectionRefused`），属环境原因而非代码回归；需在可用 PG 环境复跑。
- `cargo build -p thinsql-app`：通过。
