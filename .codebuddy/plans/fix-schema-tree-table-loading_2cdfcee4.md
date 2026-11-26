---
name: fix-schema-tree-table-loading
overview: 修复左侧结构树无法展开库并加载表的核心可用性问题：占位子节点触发懒加载、空表结果正确记账，并校正多连接下表预览的连接归属。
todos:
  - id: reproduce-tree-block
    content: 用 [skill:systematic-debugging] 固化结构树展开死锁根因
    status: completed
  - id: add-regression-tests
    content: 用 [skill:test-driven-development] 编写占位节点、空表记账、连接解析测试
    status: completed
    dependencies:
      - reproduce-tree-block
  - id: fix-tree-lazy-loading
    content: 修改 schema_tree.rs 占位子节点与表节点 ID 解析
    status: completed
    dependencies:
      - add-regression-tests
  - id: fix-state-metadata-flow
    content: 修改 state.rs 表列表记账与按连接预览
    status: completed
    dependencies:
      - add-regression-tests
  - id: verify-core-flow
    content: 用 [skill:verification-before-completion] 运行测试、构建与冒烟
    status: completed
    dependencies:
      - fix-tree-lazy-loading
      - fix-state-metadata-flow
---

## 产品概述
桌面数据库客户端的骨架已完成，但核心使用链路尚未打通。以左侧结构树无法展开库、看不到表为首要修复点，让「连接 → 展开库 → 看到表 → 点击预览数据 → 执行 SQL」这条基本流程真正可用。

## 核心功能
- 左侧连接节点可点击展开或发起连接，连接中、失败、已连接状态有明确展示。
- 展开库节点后立即触发表列表加载，加载中有提示；空库显示「无表」且不重复请求。
- 点击表节点可预览数据；多连接场景下，数据来自该表所属连接而不是当前活动连接。
- 已展开的节点在列表刷新后保持展开状态，表加载完成后占位提示被真实表名替换。

## 技术栈与约束
- 沿用现有 Rust 1.99.1 / edition 2024、gpui-kit 0.9.1、`thinsql-core` 与 `thinsql-app` 分层，不新增依赖。
- 数据库专有逻辑仍只位于 `thinsql-core/src/drivers/`；本次修复限定在 `thinsql-app` 的 UI 状态与面板层，`thinsql-core` 不需改动。

## 实现方案
- 根因：gpui 0.9.1 的树控件仅对「已有子节点」的节点发出展开事件；本应用中 schema 节点在表列表返回前没有子节点，而 `ListTables` 又依赖该展开事件，形成死锁。连接根节点在未连接时同理，无法从结构树触发连接。
- 占位子节点：为未加载的连接根节点与库节点生成禁用、灰色的占位子节点，使节点成为可展开目录；展开事件继续走既有 `toggle_node` 懒加载链路，真实数据到达后替换占位。空表结果保留「无表」占位，避免反复请求与再次不可展开。
- 表列表记账：将 `Event::Tables` 改为按 schema 整体写入，空列表也记录为已加载；对表名去重并排序，避免重复响应追加重复表名。
- 多连接预览：从表节点 id 中保留连接 id，`AppState` 增加按连接预览入口，先切换活动连接再发送 `ListColumns` 与查询；`TableRef` 保持 core 中的定义不变。
- 数据流：用户点击结构树节点 → `TreeEvent::Expanded` → `AppState::toggle_node` → 会话发送元数据命令 → `drain_events` 接收事件 → `apply_event` 更新状态 → `StateEvent::Changed` → 结构树重建并把占位节点替换为真实节点。

## 实现注意
- 占位节点 id 使用 `placeholder:` 与 `empty:` 前缀，确保不会被表节点解析命中；占位项设为 disabled，避免误触。
- 不修改 gpui-kit 依赖或自行 fork 树控件，只适配其现有 `is_folder()` 与展开事件语义。
- 保持 `TreeState`、`Session`、驱动层命令协议不变； blast radius 控制在 `schema_tree.rs` 与 `state.rs`。
- 日志沿用现有 `tracing`，不输出密码或大结果集；表列表加载失败继续由状态栏提示，不改动错误分类。
- 新环境构建前仍需执行 `scripts/dev-setup-linux.sh`；本次不触碰该脚本。

## 性能与可靠性
- 每个未加载分支只增加 O(1) 个占位节点；元数据仍按需加载，每个库首次展开仅发送一次 `ListTables`。
- 表列表写入与排序为 O(n log n)，n 为单库表数，规模有限；树重建只在会话事件到达时发生，不在每帧渲染中重复遍历。
- 空列表与重复响应被显式记账，避免无限重试和重复行。

## 目录结构
项目根目录 `/data/rustwork/thinsql` 下仅修改两个文件，无新增文件。

```
crates/thinsql-app/
├── src/
│   ├── panels/
│   │   └── schema_tree.rs  # [MODIFY] 调整 build_items 为按连接与展开状态生成节点；为未加载连接根、未加载库、空库生成禁用占位子节点；表节点 id 解析保留连接 id；点击表时按所属连接调用预览；补充纯函数测试。
│   └── state.rs            # [MODIFY] 开放测试用 ConnEntry 构造；抽取按 schema 写入表列表的纯函数，空列表也记录已加载并去重排序；新增按连接预览入口；补充空 Tables 事件、多连接预览相关单元测试。
└── Cargo.toml              # 不修改
```

## Agent Extensions
### Skill
- **systematic-debugging**
  - Purpose: 在实现前固化「schema 节点无子节点导致展开事件不触发」的因果链，排除驱动层与会话层干扰。
  - Expected outcome: 明确根因、影响范围和最小复现路径，避免只修表层症状。
- **test-driven-development**
  - Purpose: 先为占位节点生成、空表列表记账、表节点连接 id 解析补充会失败的单元测试，再实现修复。
  - Expected outcome: 形成红到绿的回归保护，覆盖首次展开、空库、重复响应、多连接预览。
- **verification-before-completion**
  - Purpose: 完成后运行 `cargo test -p thinsql-core`、`cargo test -p thinsql-app`、`cargo build --release -p thinsql-app` 并做一次人工冒烟。
  - Expected outcome: 在声明完成前拿到测试与构建的真实通过结果，并确认连接、展开、预览链路可用。
