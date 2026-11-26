---
name: add-settings-dialog
overview: 为 ThinSQL 新增设置界面：顶部工具条加"设置"按钮，弹出模态设置对话框，包含主题（深/浅）、字体家族与字号、默认每页条数、历史记录条数上限、连接默认只读、结果最大加载行数、日志级别等可配置项；点"保存/应用"后统一生效并异步持久化到 config.json。
design:
  styleKeywords:
    - 桌面原生感
    - 分组表单
    - 简洁克制
  fontSystem:
    fontFamily: SystemUIFont / DejaVu Sans Mono
    heading:
      size: 16px
      weight: 600
    subheading:
      size: 14px
      weight: 500
    body:
      size: 13px
      weight: 400
  colorSystem:
    primary:
      - "#4C8DFF"
      - "#3B7BEA"
    background:
      - 使用 cx.theme() 背景/表面色（Dark/Light 随主题切换）
    text:
      - 使用 cx.theme() 前景色/次要前景色（muted_foreground）
    functional:
      - success/warning/danger 取自 cx.theme()
todos:
  - id: core-app-settings
    content: 使用 [skill:rust-testing] 扩展 thinsql-core 配置：新增 AppSettings 结构与字段、serde 默认值、单元测试（含旧配置兼容）
    status: completed
  - id: core-consumers
    content: 使用 [mcp:codegraph] 定位调用点并接入核心消费：Page 分页 limit、MAX_LOADED_ROWS、history 截断、新连接默认只读
    status: completed
    dependencies:
      - core-app-settings
  - id: theme-apply-settings
    content: theme.rs 新增 apply_settings()，支持 Dark/Light 与字体字号应用并保留品牌色
    status: completed
    dependencies:
      - core-app-settings
  - id: settings-dialog-ui
    content: 使用 [skill:gpui-kit] 与 [skill:gpui-kit-design-guides] 新建 panels/settings_dialog.rs：分组表单、编辑副本、取消/保存
    status: completed
    dependencies:
      - core-app-settings
      - theme-apply-settings
  - id: shell-entry-save
    content: shell.rs 工具条加“设置”按钮打开对话框；保存后应用主题、save_async 持久化并 emit Changed
    status: completed
    dependencies:
      - settings-dialog-ui
  - id: startup-log-level
    content: main.rs 按配置 log_level 初始化 tracing EnvFilter
    status: completed
    dependencies:
      - core-app-settings
  - id: verify-build
    content: 验证：cargo test -p thinsql-core 与 cargo build -p thinsql-app 通过，人工核对对话框交互
    status: completed
    dependencies:
      - settings-dialog-ui
      - startup-log-level
---

## Product Overview

为 ThinSQL 数据库桌面客户端增加一个设置界面：顶部工具条新增“设置”按钮，点击弹出模态设置对话框（参考现有连接对话框的实现方式），集中管理常用功能的定制项，点“保存”后生效并持久化。

## Core Features

- **外观设置**：主题模式 Dark/Light 切换（保留品牌色不变）
- **字体设置**：界面字体家族、界面字号、等宽字体家族、等宽字号（SQL 编辑器/结果表格用）
- **数据设置**：默认每页条数（替换 `Page::DEFAULT_LIMIT=500` 常量）、结果最大加载行数（替换 `MAX_LOADED_ROWS=50_000` 常量）
- **历史设置**：SQL 历史记录条数上限
- **连接设置**：新建连接的默认只读开关（影响新连接对话框的初始值）
- **日志设置**：日志级别选择（下次启动生效，对话框内说明）
- 生效方式：对话框内编辑副本暂存，点“保存”后一次性应用（主题/字体即时切换）并异步写入 config.json；点“取消”丢弃改动
- 所有设置项带默认值，旧 config.json 缺字段时通过 `serde(default)` 无痛兼容

## Tech Stack

- Rust 1.99.1 / edition 2024（既有工程约束，不引入新依赖）
- UI：gpui-kit 0.9.1（`component::setting` / Dialog / Input / Switch / Dropdown / Button / Notification）
- 持久化：扩展现有 `thinsql-core/src/config.rs` 的 JSON 配置（原子写 + 专用写线程 `save_async`）

## Implementation Approach

1. **配置层（thinsql-core）**：在 `config.rs` 新增 `AppSettings` 结构（theme_mode、font_family、mono_font_family、font_size、mono_font_size、default_page_limit、max_loaded_rows、history_limit、default_read_only、log_level），全部 `#[serde(default)]` 挂到 `AppConfig.settings`；为新结构补充单元测试（默认值、序列化/反序列化、向后兼容旧文件）。
2. **消费点接入**：

- `model.rs:211` `Page::DEFAULT_LIMIT` → `Page::first_with(limit)`，调用点 `state.rs:422/691/746` 改读 `config.settings.default_page_limit`
- `state.rs:28-29` `MAX_LOADED_ROWS` 常量 → 读配置值（`load_more` 761-793）
- `history.rs` 保存/加载时按 `history_limit` 截断
- `connection_dialog.rs` 新建连接时 `read_only` 初始值取 `settings.default_read_only`
- `main.rs` 启动时用配置的 `log_level` 构建 tracing `EnvFilter`（运行中不热切换，对话框内注明下次启动生效）

3. **主题层**：`theme.rs` 新增 `apply_settings(&AppSettings, cx)`——`Theme::change(mode, ...)` + `Theme::update` 重设品牌色（PRIMARY 0x4c8dff / ACCENT 0x3b7bea）、radius、`font_family`/`mono_font_family`/`font_size`/`mono_font_size`，与现有初始化写法保持一致。
4. **UI 层**：新增 `panels/settings_dialog.rs`，仿照 `connection_dialog.rs` 的 Dialog 模式（`window.open_dialog` / `DialogFooter` / `close_dialog`）；内部持有 `AppSettings` 编辑副本，分组表单（外观/字体/数据/历史/连接/日志），底部“取消/保存”；保存时更新 `AppState.config.settings` → `apply_settings` 即时生效 → `save_async()` 持久化 → `emit(StateEvent::Changed)`。
5. **入口**：`shell.rs` 工具条（render_toolbar 42-103）加“设置”按钮，点击 `open_settings_dialog`（与“新建连接” 67-71 行同模式）。

## Performance & Reliability

- 配置写入复用现有专用写线程与合并队列，设置保存无阻塞；字体家族需校验系统可用字体（gpui-kit `system_font.rs` 的探测逻辑说明 Linux 缺字体会崩溃，故提供下拉候选+回退默认值）
- 分页/历史截断均为 O(1)/O(n) 简单操作，无性能风险
- 旧配置文件完全向后兼容；日志级别不热切换避免运行时行为突变

## Directory Structure Summary

```
crates/thinsql-core/
├── src/config.rs        # [MODIFY] 新增 AppSettings 结构、AppConfig.settings 字段、默认值与单元测试
├── src/model.rs         # [MODIFY] Page 增加 first_with(limit)；保留 DEFAULT_LIMIT 作默认值
├── src/history.rs       # [MODIFY] 按 history_limit 截断历史
crates/thinsql-app/
├── src/main.rs          # [MODIFY] 启动时按 settings.log_level 初始化 tracing filter
├── src/theme.rs         # [MODIFY] 新增 apply_settings()：Dark/Light + 字体/字号应用
├── src/shell.rs         # [MODIFY] 工具条加“设置”按钮并接线
├── src/state.rs         # [MODIFY] 分页 limit、MAX_LOADED_ROWS、只读默认值改读配置；保存入口
├── src/panels/settings_dialog.rs  # [NEW] 设置对话框：分组表单、编辑副本、保存/取消
```

## Design Style

遵循项目现有 gpui-kit 设计语言：模态设置对话框复用 `Dialog` 组件骨架（与连接对话框一致的头部/底部布局），内容区垂直分组，每组为“组标题 + 若干设置行”。设置行采用左标签右控件的表单布局：Switch 用于只读开关，Dropdown 用于主题模式/日志级别/字体家族（字体家族下拉列出系统常用候选），数字输入用 Input（限定数值范围，如每页条数 10–5000）。底部固定操作条：左侧提示“日志级别下次启动生效”，右侧“取消”（ghost）与“保存”（primary）按钮。保存后主题/字体立即切换并弹出保存成功 Notification，对话框关闭。

## Agent Extensions

### Skill

- **gpui-kit**
- Purpose：实现设置对话框前查阅 gpui-kit 组件用法（Dialog、setting、Switch、Dropdown、Input、Button）与 GPUI 状态/实体机制，确保遵循规范编码指南
- Expected outcome：设置对话框按 gpui-kit 正确模式实现（实体状态、Dialog 打开/关闭、组件 id 规范）
- **gpui-kit-design-guides**
- Purpose：设计设置界面布局/间距/视觉层级/文案时遵循 Design Guides 规范
- Expected outcome：设置界面视觉与交互符合规范（分组、行布局、按钮层级）
- **rust-testing**
- Purpose：为 AppSettings 配置结构编写单元测试（默认值、序列化兼容性、历史截断）
- Expected outcome：`cargo test -p thinsql-core` 通过，覆盖旧配置向后兼容场景

### MCP

- **codegraph**
- Purpose：实现过程中快速定位符号与调用链（如 Page::first 调用点、AppConfig 字段引用）
- Expected outcome：准确掌握改动影响面，避免遗漏调用点