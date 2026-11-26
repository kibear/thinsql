---
name: add-language-setting-i18n
overview: 为 ThinSQL 增加语言设置（中文/English）：thinsql-core 新增 LanguagePref 配置项（默认跟随系统 locale），thinsql-app 新建 i18n 文案查找模块，把全部约 316 处硬编码中文 UI 文案替换为双语表查找，设置对话框中新增"语言"下拉项，语言改动重启后生效。
todos:
  - id: core-language-pref
    content: 使用 [skill:rust-testing] 在 thinsql-core config.rs 新增 LanguagePref（System/Zh/En + resolve 跟随系统）与单元测试
    status: completed
  - id: app-i18n-module
    content: 使用 [mcp:codegraph] 新建 thinsql-app/src/i18n.rs：语言全局、tr/t 查找回退占位符替换，附单元测试
    status: completed
    dependencies:
      - core-language-pref
  - id: migrate-static-ui
    content: 迁移 shell.rs/sidebar.rs/tree_model.rs/workspace.rs 静态文案到 tr/t
    status: completed
    dependencies:
      - app-i18n-module
  - id: migrate-dialogs
    content: 迁移 connection_dialog.rs 与 settings_dialog.rs 文案，并新增「语言」下拉设置项
    status: completed
    dependencies:
      - app-i18n-module
  - id: migrate-state-results
    content: 迁移 state.rs/results.rs 动态状态与错误消息（占位符参数化）
    status: completed
    dependencies:
      - app-i18n-module
  - id: migrate-misc-and-boot
    content: 迁移 single_instance.rs/platform_x11.rs/perf.rs，main.rs 启动时初始化语言
    status: completed
    dependencies:
      - app-i18n-module
  - id: verify-build-tests
    content: 运行 cargo test 双 crate 与 cargo build -p thinsql-app 验证
    status: completed
    dependencies:
      - migrate-static-ui
      - migrate-dialogs
      - migrate-state-results
      - migrate-misc-and-boot
---

## 产品概述

在 ThinSQL 设置中新增「语言」选项，支持中文/英文切换。文案层从硬编码中文迁移为双语查找表。

## 核心需求（已澄清）

- **覆盖范围**：全量界面文案 —— 全部约 316 处中文文案均纳入双语表，包括工具条、侧栏、树节点、标签页状态、错误提示、通知、设置/连接对话框、单实例提示等
- **生效方式**：重启后生效（与日志级别一致），启动时按配置初始化，无需全局刷新
- **默认语言**：跟随系统 locale（`zh` 开头 → 中文，否则英文），用户可在设置中显式覆盖
- **设置入口**：在现有设置对话框（`settings_dialog.rs`）的分组表单中新增「语言」下拉项，保存后持久化到 config.json

## 视觉效果

设置对话框「常规/外观」分组内新增「语言」下拉选择（跟随系统 / 中文 / English），其余界面不变；切换语言后重启，全部界面文案与状态消息显示为对应语言。

## 技术栈

- Rust 1.99.1 / edition 2024，UI: gpui-kit 0.9.1，配置持久化: serde JSON（现有 `AppConfig`，`#[serde(default)]` 兼容旧配置）
- 不引入新依赖：locale 检测用 `std::env` 读 `LC_ALL`/`LC_MESSAGES`/`LANG`

## 实现方案

1. **配置层**（thinsql-core）：`config.rs` 新增 `LanguagePref` 枚举（`System`/`Zh`/`En`，serde lowercase），加入 `AppSettings.language` 字段（缺省 `System`）；提供 `resolve()`：`System` → 检测环境变量 locale 前缀，`Zh`/`En` 直接生效。沿用现有测试模式补单元测试（默认值、旧配置兼容、roundtrip、resolve 环境变量优先级）。
2. **i18n 层**（thinsql-app）：新建 `src/i18n.rs`：

- `static LANG: OnceLock<Lang>`，main.rs 启动时按 `settings.language` 初始化（重启生效，无需响应式刷新）
- 文案表以**中文原文为 key**（含 `{placeholder}` 占位符，如 `"已连接 {name}"`），英文表为对应模式串；`tr(key, args) -> String` 查找 → 未命中回退 key 本身 → 再做占位符替换。这样现有调用点可机械迁移：`format!("已连接 {name}")` → `tr("已连接 {name}", &[("name", &cfg.name)])`，纯静态文案 `t("新建连接") -> &'static str`
- 查找用 sorted 静态切片 + binary search，启动期 OnceLock 缓存语言，运行期 O(log n)，无性能热点

3. **文案迁移**：按文件分批替换硬编码中文（shell/sidebar/tree_model/workspace → dialogs → state.rs+results.rs → single_instance 等）；`state.rs` 中存入 `QueryTab.status` 的消息在生成时即按当前语言定稿（重启生效模型下天然一致）
4. **设置对话框**：`settings_dialog.rs` 在分组中新增「语言」`Dropdown` 项（跟随系统/中文/English），编辑副本 + 保存持久化，与现有字段模式一致

## 架构约束

- i18n 仅服务 UI 文案，放 thinsql-app，不进 thinsql-core（core 只含 `LanguagePref` 枚举与持久化）
- 数据库专有知识仍只在 drivers/ 下，不受影响

## 验证

- `cargo test -p thinsql-core`、`cargo test -p thinsql-app`（i18n 查找/回退/占位符/resolve 单测 + 现有测试回归）
- `cargo build -p thinsql-app` 编译通过

## Agent Extensions

### Skill

- **rust-testing**
- Purpose: 指导 LanguagePref resolve、旧配置兼容、i18n 查找/回退/占位符替换的单元测试编写
- Expected outcome: 配置层与 i18n 层均有完整单测并通过

### MCP

- **codegraph**
- Purpose: 精确定位各文件中文案调用点与 AppSettings/settings_dialog 的消费关系，避免遗漏 316 处文案
- Expected outcome: 迁移清单完整、无遗漏调用点