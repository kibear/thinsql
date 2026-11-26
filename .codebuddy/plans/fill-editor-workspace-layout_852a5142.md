---
name: fill-editor-workspace-layout
overview: 修复查询工作区布局：让 SQL 编辑器填满整个标签工作区（根因是 gpui-kit 多行 Input 默认 h_auto 只按内容高度渲染小框），结果区改为按需出现在下方，编辑器与结果之间用可拖拽的垂直分栏，所有标签统一。
todos:
  - id: read-gpui-kit-guides
    content: 使用 [skill:gpui-kit] 阅读布局与 resizable 组件规范，确认 v_resizable/resizable_panel/Editor.h 的正确用法
    status: completed
  - id: refactor-tab-body-layout
    content: 重构 workspace.rs render_tab_body：Editor 填满、action_bar 上移、v_resizable 上下分栏、结果区按需出现
    status: completed
    dependencies:
      - read-gpui-kit-guides
  - id: verify-build-and-tests
    content: 使用 [skill:verification-before-completion] 运行 cargo build 与 cargo test -p thinsql-app，确认无回归无新告警
    status: completed
    dependencies:
      - refactor-tab-body-layout
---

## 需求概述

修复查询标签页中 SQL 编辑区的布局问题，并重新组织工作区结构。

## 现状问题（已定位根因）

- `workspace.rs` 的 `render_tab_body` 中编辑器与结果区各占一半（各 `flex_1`），且 gpui-component 的多行 `Input` 默认 `h_auto()`（内容多高框就多高），导致编辑器只渲染一个高度约 3 行的小边框盒，嵌在大片空白区域里——“两层”感与“实际可操作区域很小”。
- SQL 超出时没有可用的整页滚动体验。

## 目标行为（用户已确认）

1. **编辑器填满**：打开任意标签后，SQL 编辑区占据工作区主体，内容超出时由编辑器自身滚动（gpui 多行输入在高度确定后自带滚动）。
2. **结果区按需出现**：默认不显示结果区（编辑器占满）；执行查询后结果在下方出现，编辑器与结果之间的分界线可拖拽调整高度（接近 DataGrip 习惯）。
3. **所有标签统一**：查询标签与点表打开的预览标签使用同一布局逻辑。
4. 操作栏（执行/执行所有/取消/状态）移到编辑器上方，避免卡在编辑器与结果区中间。

## 技术方案

### 根因与修复依据（已核实源码）

- `gpui-component-0.9.1/src/input/input.rs:795-798`：多行模式 `.h_auto()`，仅显式 `.h()`/`.h_full()` 才有确定高度。
- `gpui-component-0.9.1/src/input/editor.rs:58-61`：`Editor::h(height: impl Into<DefiniteLength>)` 转发给 Input → 用 `Editor::new(&view.editor).h(relative(1.))` 填满容器。
- `gpui-component-0.9.1/src/resizable.rs:34-36`：`v_resizable(id)` 垂直分栏可用；`gpui-base` 的 `resizable_panel()` 支持 `.size(px)`（初始高度）与 `.size_range(range)`（默认 `PANEL_MIN_SIZE..Pixels::MAX`）；`ResultsView::render` 已是 `.size_full()`，可直接放入面板。

### 修改点（全部集中在 `crates/thinsql-app/src/panels/workspace.rs` 的 `render_tab_body`）

1. **编辑器**：`Editor::new(&view.editor).h(relative(1.))`（`workspace.rs` 已 `use gpui_kit::*`，`relative` 可用），容器 `div().flex_1().min_h_0()` 保留。
2. **操作栏上移**：`action_bar` 移到 `context_row` 之后、分栏区之前（与 context_row 相邻，均在 `v_resizable` 外）。
3. **结果区按需出现**：新增判定（QueryTab 已有字段）：

```rust
let show_results = tab.result.is_some()
|| tab.is_running()
|| !tab.statements.is_empty()
|| !tab.pending_changes.is_empty();
```

- `show_results == false`：编辑器直接占满剩余空间，不渲染分栏（无拖拽手柄）。
- `show_results == true`：`v_resizable(("editor-results", id))` 上下分栏——上：编辑器面板（`size(px(420.))`，编辑器占大头）；下：结果面板（`resizable_panel().child(view.results.clone())`）。id 按标签区分，避免切换标签串分栏记忆。

4. 无 `state.rs` / `results.rs` 改动（ResultsView 已 `size_full`，状态字段齐全）。

### 关键取舍

- 用现成 `v_resizable` 而非手写拖拽，复用 shell.rs 已验证的分栏模式，零新依赖。
- 结果区按条件渲染而非折叠隐藏：无结果时不留白，且拖拽手柄只在有结果时出现。
- 分栏高度记忆由 `ResizableState` 全局状态按 id 持有，切标签自动恢复。

### 验证

- `cargo build -p thinsql-app` 编译通过；
- `cargo test -p thinsql-app --lib` 现有 41 个测试不回归（本次为纯布局改动，无状态层变化）；
- UI 效果由用户运行 `cargo run --release -p thinsql-app` 目检：打开查询标签 → 编辑器占满；执行后结果出现在下方且分界线可拖拽。

## Agent Extensions

### Skill

- **gpui-kit**
- Purpose: 执行前查阅 GPUI Kit 组件规范（v_resizable / resizable_panel / Editor 尺寸 API 的正确用法与布局约束）
- Expected outcome: 布局改动遵循 gpui-kit 设计规范，不与组件内部布局管理冲突（如 resizable panel 的保留样式约束）
- **verification-before-completion**
- Purpose: 完成后运行构建与测试命令，以实际输出为依据声称完成
- Expected outcome: cargo build 通过、41 个测试全绿、无新增 clippy 告警