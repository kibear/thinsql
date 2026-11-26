# ThinSQL — Agent 开发指南

轻量级数据库桌面客户端：管理连接、预览表、执行 SQL。
单进程、无 WebView/JVM；目标是启动快、内存省。

## 技术栈

以根目录 `Cargo.toml` / workspace 为准，勿抄过期文档数字：

- Rust edition **2024**（workspace）
- UI：`gpui-kit = "=0.11.0"`（Zed gpui，非 WebView）
- DB：MySQL（`mysql` + rustls）、PostgreSQL（`postgres`）；Oracle 规划中

## 目录职责

```
crates/thinsql-core/          # 与 UI 解耦：驱动、值模型、配置、会话线程
  src/drivers/                # 唯一允许出现方言知识的地方
  src/provider.rs             # DatabaseProvider trait / Capabilities
  src/session.rs              # 一连接 = 一工作线程 + Command/Event channel
  src/model.rs / value.rs     # 公共模型与统一 Value
  src/edit.rs / sql.rs        # 行编辑 SQL、方言无关 SQL 辅助
  src/config.rs / history.rs  # 配置、钥匙串密码、历史
crates/thinsql-app/           # gpui-kit UI
  src/state.rs                # AppState：发命令 → 轮询事件 → 更新视图模型
  src/shell.rs                # 顶栏 + 侧栏 + 工作区壳
  src/panels/                 # sidebar / workspace / editor / results / structure / dialogs
  src/i18n.rs                 # 界面文案（中文 key → 英文）
  src/platform_x11.rs         # X11 WM_CLASS 补设
tools/ime-probe/              # X11 注入验证工具（非产品代码）
scripts/dev-setup-linux.sh    # 首次构建必跑：native/lib 软链
```

## 硬性约束

1. **方言隔离**：分页语法、元数据 SQL、类型映射、转义、引号风格等，只能写在 `thinsql-core/src/drivers/`。公共层禁止 `if mysql` / `match DriverKind` 做方言分支（`drivers/mod.rs` 的工厂与类型表除外）。
2. **同步 + 会话线程**：接口同步，无 async 运行时。UI 只 `Session::send` / `try_recv`，数据库 I/O 不得阻塞渲染线程。
3. **统一 Value**：UI 与公共层只认 `thinsql_core::Value`，不暴露驱动原生类型。
4. **UI 不持 DB 句柄**：连接状态、结果、待提交改动都在 `AppState` / `QueryTab`；面板只读实体、发操作。
5. **多标签归属**：请求带 `RequestId`，结果必须写回发起它的标签，禁止按「当前连接」丢弃非活动标签结果。
6. **文案走 i18n**：用户可见字符串用 `t` / `tr`（中文原文为 key）；语言启动时定，不热切换。
7. **新增数据库**：只加 `drivers/<name>.rs` + `create` 一行，不向公共层扩散分支。

## 功能 → 入口

| 要改什么 | 优先看 |
|---|---|
| 连接增删改 / 测试连接 | `panels/connection_dialog.rs`、`state.rs` connect 路径、`config.rs` |
| 侧栏树 / 对象列表 / 右键菜单 | `panels/sidebar.rs`、`tree_model.rs`、`state` 的 list_* |
| SQL 编辑器 / 多标签工作区 | `panels/workspace.rs`、`editor.rs` |
| 执行查询 / 分页 / 取消 | `state.rs` → `session.rs` → `drivers/*` |
| 结果网格 / 单元格编辑提交 | `panels/results.rs`、`edit.rs`、`session` Apply |
| 表结构查看与 DDL 编辑 | `panels/structure.rs`、`session` Load/ApplyStructure、驱动元数据 |
| 设置 / 主题 / 语言 | `panels/settings_dialog.rs`、`theme.rs`、`i18n.rs`、`config.rs` |
| 预览 SQL / 保留字引号 | `model::table_preview_sql`、`sql.rs` |
| X11 窗口类 / 输入法 | `platform_x11.rs`；IME 探针见 `tools/ime-probe` |

数据流一句话：`面板 → AppState.send(Command) → Session 工作线程 → DatabaseProvider → Event → AppState.apply_event → 面板重绘`。

## 构建与测试（Linux，无需 root）

```bash
./scripts/dev-setup-linux.sh          # 首次必须：native/lib 软链
cargo test -p thinsql-core            # 单元测试（含 session 假驱动）
cargo test -p thinsql-app             # UI/状态相关单测
cargo build --release -p thinsql-app
```

缺 `-dev` 包时：`fontconfig` 走 dlopen（`.cargo/config.toml`），`libxkbcommon`/`libxcb` 靠 `native/lib` 软链。**漏跑 setup 会直接链接失败。**

集成测试（真实库，自建隔离对象，不碰既有数据）：

```bash
THINSQL_TEST_MYSQL=1 THINSQL_TEST_MYSQL_PASS=root \
  cargo test -p thinsql-core --test mysql_integration

THINSQL_TEST_PG=1 THINSQL_TEST_PG_PASS=root \
  cargo test -p thinsql-core --test postgres_integration
```

- MySQL：独立库 `thinsql_it` / `thinsql_it_edit`，测完删除  
- PostgreSQL：独立 schema `thinsql_it`，`DROP CASCADE`

## Agent 工作方式

- 仓库有 `.codegraph/`：查代码、摸调用链优先 `codegraph_explore`（或 `codegraph explore "..."`），再按需读文件。
- UI / 布局 / 组件：先读 `.cursor/skills/gpui-kit` 与 `gpui-kit-design-guides`；Rust 惯用法与测试见同目录 `rust-patterns`、`rust-testing`。
- 改动保持最小：只动完成任务所需的文件；未明确要求不要 commit / push。
- 文档与用户沟通默认中文；代码标识符、命令、错误信息保持原文。

## 已知坑

- gpui X11 窗口默认不设 `WM_CLASS`：启动后由 `platform_x11.rs` 按 PID 递归查找补设（WM 会重父化）。
- 中文输入走 XIM：可提交中文，预编辑残留尚未人工复核完毕。
- 密码只经 `Command::Connect` 明文一次，不进配置/日志；持久化走系统钥匙串。

## 延伸阅读

- `docs/spikes/2026-11-16-gpui-kit-spike.md` — gpui-kit 选型与平台笔记  
- `docs/reviews/2026-11-18-correctness-audit.md` — 正确性审计与驱动/会话风险点  
