---
name: fix-connection-dialog-buttons
overview: 修复新建连接弹窗缺少「保存 / 取消 / 测试连接」按钮与右上角关闭图标不可见的问题，并新增不落库的连接测试能力。
todos:
  - id: register-assets
    content: 在 main.rs 用 with_assets 注册 gpui_kit::assets::Assets，恢复关闭图标显示
    status: completed
  - id: dialog-footer
    content: 参考 [skill:gpui-kit] 用 DialogFooter 显式组装测试连接、取消、保存按钮并复用保存逻辑
    status: completed
  - id: test-connection
    content: 实现表单配置组装与后台测试连接，结果通过通知提示且不落盘
    status: completed
    dependencies:
      - dialog-footer
  - id: verify
    content: 运行 cargo build/test 并人工验证弹窗按钮、关闭图标与测试连接行为
    status: completed
    dependencies:
      - register-assets
      - dialog-footer
      - test-connection
---

## 用户需求

修复「新建连接」弹窗的功能与可见性问题：

- 弹窗底部缺少操作按钮，导致无法保存或取消；需要在底部提供「保存」「取消」「测试」三个按钮。
- 弹窗顶部右侧的关闭按钮看不到（实际已渲染，只是图标不显示），需要让关闭按钮可见可用。

## 产品概述

新建/编辑连接弹窗应是一个完整可用的表单弹窗：填写数据库类型、名称、主机、端口、用户名、密码、默认库后，可以测试连通性、保存配置并自动连接，或取消关闭；右上角关闭图标清晰可见，点击可关闭。

## 核心功能

- 底部操作区：左侧「测试连接」，右侧「取消」「保存」；保存后保持现有「保存 → 落盘 → 自动连接」行为。
- 测试连接：使用当前表单值建立一次临时连接并立即断开，成功/失败通过通知提示，不写配置、不写钥匙串。
- 取消：直接关闭弹窗，不保存任何内容。
- 关闭图标：恢复右上角 X 图标的正常绘制与点击关闭。
- 表单校验保持现状：端口必须 1-65535，名称不能为空，错误使用通知提示且弹窗不关闭。
- 编辑模式下密码留空时，测试连接复用钥匙串中已保存的密码。

## 技术栈

沿用现有技术栈，不新增依赖：

- Rust 2024 / `gpui-kit 0.9.1`（`gpui_kit::component` 即 `gpui-component 0.9.1`）
- 数据库访问：`thinsql-core` 的 `Registry` / `DatabaseProvider` / `session`
- 主题：现有 `crates/thinsql-app/src/theme.rs` 深色主题

## 根因确认

1. **按钮不显示**：`gpui-component-0.9.1/src/dialog/dialog.rs` 中普通 `Dialog` 的 `button_props` 仅用于 `AlertDialog` 的默认 footer；`Dialog` 渲染时只处理显式 `.footer(...)`（`.when_some(self.footer, ...)`），因此 `open_connection_dialog` 里的 `.button_props(...)` 实际不产生任何按钮。默认 footer 按钮生成方法 `render_ok/render_cancel` 是 `pub(crate)`，外部不可调用。
2. **关闭图标不可见**：`Dialog` 默认 `close_button: true`，渲染 `Button::icon(IconName::Close)`；`gpui-component/src/icon.rs` 明确要求图标 SVG 由应用的 `gpui::AssetSource` 加载。`crates/thinsql-app/src/main.rs` 当前是 `gpui_kit::application().run(...)`，未注册 asset source，因此所有 `IconName` 图标（包括关闭按钮）都画不出来。`gpui-kit` 默认 feature 已含 `assets`，无需改 Cargo.toml。

## 实现方案

### 1. 注册图标资源（修复关闭按钮）

在 `main.rs` 启动应用时链式注册内置资源：

```rust
gpui_kit::application()
    .with_assets(gpui_kit::assets::Assets)
    .run(|cx| { ... });
```

注册后 `Dialog` 默认右上角关闭按钮的 X 图标恢复绘制，点击行为沿用 `DialogClose` 的关闭语义。同时项目中其他 `IconName` 图标也会一并恢复，属于预期收益。

### 2. 显式组装弹窗 footer

在 `open_connection_dialog` 中移除仅对 `AlertDialog` 生效的 `.button_props(...)`，改为 `.footer(...)`：

- 使用 `gpui_kit::component::dialog::DialogFooter`（内部为 `h_flex().gap_2().justify_end()`）。
- 布局：`[测试连接]` + `div().flex_1()` 占位 + `[取消]` + `[保存]`。
- 「保存」使用 primary 按钮并保留 `.on_ok(...)`：这样点击按钮与键盘确认走同一套保存逻辑；保存成功返回 `true` 由 Dialog 关闭。
- 「取消」直接 `window.close_dialog(cx)`，不触发保存；ESC/点击遮罩仍沿用默认关闭行为。
- 保存逻辑抽成闭包（如 `Rc<dyn Fn(&mut Window, &mut App) -> bool>`），同时供 `.on_ok` 与保存按钮使用，避免复制校验代码。
- 保留现有 `KindPicker` 与输入实体结构；所有实体仍在 `open_dialog` 之前创建，builder 内只 clone 句柄，遵守 gpui 的构建约束。

### 3. 测试连接（异步、不落盘）

新增纯逻辑 + 异步执行两段：

- 纯逻辑：从表单实体读取字段并复用 `parse_port`，组装 `ConnectionConfig`（编辑模式沿用原 id/read_only 等字段），返回 `Result<ConnectionConfig, String>`；同时返回密码：优先表单输入，编辑模式留空则 `thinsql_core::config::secret::load(&cfg)`。
- 异步执行：点击「测试连接」后先推送 `Notification::info("正在测试连接…")`，用 `app.background_executor().spawn(...)` 在后台线程执行 `Registry::create(kind)` → `provider.connect(&cfg, password)` → 成功后 `provider.disconnect()`，避免阻塞 UI 线程。
- 结果回传：用 `app.spawn(async move |cx| ...)` 等待后台任务完成，再通过 `window.window_handle().update(cx, |_, window, cx| ...)` 推送 `Notification::success` 或 `Notification::error`（错误附带 `err.message` 与 `err.hint()`，与 `state.rs` 现有用法一致）。
- 防重复：在 `open_dialog` 之前创建 `Rc<Cell<bool>>` 作为 in-flight 标记，任务完成或失败后复位，避免连续点击堆积连接尝试。
- 测试不修改 `AppConfig`、不写钥匙串、不影响当前连接状态；仅做一次临时握手。

### 4. 错误处理与边界

- 端口非法、名称为空：沿用现有通知提示，测试与保存都不继续。
- 主机/用户名为空：保持现状（不强制拦截），交由驱动返回明确错误并通知。
- 后台连接失败：只提示错误，弹窗保持打开，用户可修改后重试。
- 弹窗在测试进行中被关闭：异步回调仍会尝试通过窗口句柄推送通知，句柄失效时安静忽略错误（`let _ = handle.update(...)`）。

### 性能与可靠性

- 测试连接是一次性后台任务，不参与 80ms 事件泵，不增加每帧开销。
- 不在 render/builder 中创建持久实体，避免重复构建导致状态丢失。
- 不触碰 `thinsql-core` 驱动层，不引入 `if mysql` 之类分支，遵守 AGENTS.md 的边界约束。

## 目录结构（仅列出受影响文件）

```
crates/thinsql-app/
├── src/
│   ├── main.rs                         # [MODIFY] 注册 gpui_kit::assets::Assets，修复关闭图标及其他 IconName 图标不显示
│   └── panels/
│       └── connection_dialog.rs        # [MODIFY] 用 DialogFooter 显式组装 测试连接/取消/保存；抽出保存闭包；
│                                       #          新增表单→ConnectionConfig 组装与异步测试连接、结果通知
```

## 关键代码结构（仅签名示意）

```rust
/// 从弹窗表单值组装配置，保存与测试共用；错误直接用于通知提示。
fn assemble_config(
    base: Option<ConnectionConfig>,
    fields: FormFields,
) -> Result<ConnectionConfig, String>;

/// 后台执行一次临时连接（成功即断开），返回耗时或错误信息。
fn test_connection_task(
    cfg: ConnectionConfig,
    password: Option<String>,
) -> Result<std::time::Duration, thinsql_core::Error>;
```

## 验证方式

- `cargo build -p thinsql-app` 编译通过。
- `cargo test -p thinsql-app`（含现有 `parse_port` / id 唯一性测试）通过；为 `assemble_config` 补充端口/名称校验的单测。
- 有图形环境时人工验证：右上角 X 可见且可关闭；底部三按钮齐全；测试成功/失败均有通知；取消不保存；保存后自动连接。

## Agent Extensions

### Skill

- **gpui-kit**
- Purpose: 核对 gpui-kit 的 Dialog/footer 用法、AssetSource 注册方式与异步任务/通知范式，确保实现符合框架约定
- Expected outcome: 弹窗 footer 与关闭按钮按官方推荐方式实现，异步测试连接使用正确的主线程/后台执行器配合