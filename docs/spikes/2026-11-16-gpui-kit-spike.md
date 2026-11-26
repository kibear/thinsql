# Phase 0：gpui-kit 可行性 Spike

日期：2026-11-16　结论：**通过（附条件）**，继续使用 gpui-kit 0.9.1。

## 1. 验证目标

在正式开工前，用最小可运行程序回答四个问题：

1. 在**没有 root、缺开发包**的这台 Linux 机器上能否编译并运行 gpui-kit？
2. 冷启动到首帧要多久？空载内存多少？有没有子进程？
3. 中文输入法（fcitx5 + XIM）能不能用？
4. 10 万行 × 50 列的表格能否虚拟化流畅渲染？

## 2. 测试环境

| 项 | 值 |
|---|---|
| 系统 | Linux，X11 会话（`DISPLAY=:0`，X.Org 21.1.11），XTEST 已启用 |
| 工具链 | rustc/cargo 1.99.1，edition 2024 |
| CPU | 16 核 |
| 依赖版本 | `gpui-kit = "=0.9.1"`（2026-11-09 发布） |
| 输入法 | fcitx5（`/usr/bin/fcitx5 -d`，进程已在运行） |
| 权限 | 普通用户，**无 sudo**，无法 `apt-get install` 任何 `-dev` 包 |

## 3. 量化结果

| 指标 | 计划目标 | 实测 | 结论 |
|---|---|---|---|
| 冷启动到首帧 | < 1000ms | **130ms** | 远超预期 |
| 首帧时 RSS | < 150MB | **122MB** | 达标 |
| 空载 7s 后 RSS（含 10 万×50 虚拟表格） | < 150MB | **146MB** | 达标但临界 |
| 子进程数 | 0 | **0** | 达标 |
| 线程数 | — | 41 | 记录 |
| release 二进制 | < 40MB | **41MB**（lto=thin, strip） | 略超 1MB |
| 10 万行 × 50 列渲染 | 可滚动 | 正常，表头粘性、横向滚动、行虚拟化生效 | 达标 |
| 中文字形渲染 | 正常 | 正常（思源/Noto 回退生效） | 达标 |

对比参照：Electron 系的 DBeaver 官方对比数据是冷启动 5.2s / 空载 606MB；同为 Rust+GPUI 的 DBDelve 是 1.13s / 133MB。本项目的 130ms 是**空载首帧**，未连接数据库、未加载真实数据，属于下限值，正式版加上连接与元数据后会上升，但量级已经确定。

## 4. 构建环境踩坑（已解决，无需 root）

这台机器只有运行时库，没有开发包，直接 `cargo build` 会失败两次：

1. `yeslogic-fontconfig-sys` 构建脚本 panic —— 找不到 `fontconfig.pc`。
   解法：`.cargo/config.toml` 里设 `RUST_FONTCONFIG_DLOPEN=1`，改走运行期 dlopen。
2. 链接期 `unable to find library -lxkbcommon/-lxcb` —— 缺 `libfoo.so` 软链。
   解法：`scripts/dev-setup-linux.sh` 在 `native/lib/` 下建指向 `libfoo.so.N` 的软链，并在 `[build] rustflags` 里加 `-L native/lib`。链接器会从库文件读 SONAME，运行期仍解析到系统库。

**含义**：这台机器可以直接开发，但新环境（CI、同事机器）必须先跑 `scripts/dev-setup-linux.sh`，否则链接失败。这一点要写进 README。

## 5. 中文输入法：部分通过，需要人工复核

X11 后端由 `gpui-pre-linux` 通过 `xim` crate 接入 XIM 协议（源码确认：`src/linux/x11/client.rs` 使用 `xim::{Client, InputStyle, X11rbClient}`），不走 GTK IM。

用 XTEST 注入按键 + fcitx5 的实测结果：

- 注入 `nihao`、`zhongguo` 等拼音后，输入框里出现了**正确的中文**（截图中可见「你好」「中国人」「智能」），说明 XIM 提交链路是通的，不存在此前担心的 GB2312 乱码。
- 但同一次输入里也混入了未被合成处理的拉丁字符残留。目前无法区分是「预编辑状态处理不完整」还是「多次实验叠加造成的污染」。
- **未能完成干净的对照实验**：gpui 的窗口没有设置 `WM_CLASS`/`WM_NAME`（`WindowOptions::default()` 下为空），窗口管理器无法可靠激活它；`wmctrl -a` 无效，改用 XTEST 盲注有把按键打进用户其他窗口的风险，因此中止了后续注入实验。

**待办（Phase 1 人工验收项）**：

1. 给窗口设置正式标题与 app id（`WindowOptions` 显式 `.with_title(...)`），让 WM 能正常激活、Alt-Tab 可见。
2. 人工在 SQL 编辑器里输入中文注释、删除、取消预编辑（Esc）、候选翻页，确认无残留、无丢字。
3. 若确认存在预编辑残留，优先评估 gpui 上游 issue，而不是自己写输入法层。

## 6. 附带发现

- `gpui_kit::base::StyledExt` 是 `.v_flex()/.h_flex()` 的来源，需要显式 `use`；`gpui_kit::*` 不含它。
- 表格 API 与文档一致：`TableDelegate`（`columns_count`/`rows_count`/`column`/`render_td`）+ `TableState::new(delegate, window, cx)` + `DataTable::new(&entity)`，`.stripe(true)` 开斑马纹。
- 依赖树里已包含 `tree-sitter-sql`、`dock`、`input/editor`、`tree`、`tab`，后续 Phase 2/3 需要的控件都在，不需要额外引入 UI 框架。
- 41 个线程偏多（GPU 渲染线程池 + 输入法 + accesskit），但仍是单进程，满足「不拉起额外进程」的核心诉求。

## 7. 对计划的影响

- 框架**不变**：继续 gpui-kit 0.9.1，`egui` 备选方案不启用。
- 内存目标从「空载 < 150MB」调整为「空载 < 180MB，含 1 个连接与 500 行结果 < 260MB」，因为 146MB 已经是纯框架基线，没有余量。
- 二进制目标从 < 40MB 调整为 < 45MB；若要压回 40MB，Phase 7 再评估裁掉未用的组件 feature。
- 新增 Phase 1 验收项：窗口标题/WM 可识别、中文输入人工确认。

## 8. 复现方式

```bash
./scripts/dev-setup-linux.sh          # 建 native/lib 软链（一次性）
cargo build --release -p thinsql-app
DISPLAY=:0 ./target/release/thinsql-app
# 首帧会打印：PROBE first_frame_ms=... rss=... hwm=...
```

输入法探针（验证工具，非产品代码）：

```bash
# 先确认窗口归属，避免把按键打进其他应用
DISPLAY=:0 xprop -id <win> _NET_WM_PID
cargo build --release -p ime-probe
DISPLAY=:0 ./target/release/ime-probe <win> nihao commit
```

---

# 追加：功能落地后的指标复核

上面是 Phase 0 空壳阶段的数字。加入 MySQL/PostgreSQL 两个驱动、tree-sitter SQL 高亮、
Dock 三栏与全部面板之后，用 `THINSQL_PROBE=1` 重新测量（同一台机器，X11 会话）。

| 指标 | Phase 0（空壳） | 当前（完整应用） | 说明 |
|---|---|---|---|
| 冷启动到首帧（首次运行） | 130ms | **248ms** | 含页面缓存未预热 |
| 冷启动到首帧（预热后） | — | **99 / 106 / 118ms** | 连跑三次 |
| 首帧时 RSS | 122MB | **119MB** | 差值在噪声内 |
| 空载 8s 后 RSS | 146MB | **151MB** | 两个驱动 + 高亮引擎 |
| 线程数 | 41 | 41–42 | 单进程 |
| 子进程 | 0 | 0 | — |
| 二进制 | 41MB | **52MB** | fat LTO 后（thin 时 61MB） |

## 体积的构成与优化

- `[profile.release]` 从 `lto = "thin"` 改为 `lto = "fat" + codegen-units = 1`，
  体积 61MB → 52MB，代价是构建时间约 6–7 分钟。
- 语法包不是主因：依赖树里只有 `tree-sitter-sequel` 与 `tree-sitter-json`，
  体积主要来自 mysql + postgres 两个驱动与 rustls。
- 还可以压但没做：`panic = "abort"`（省异常表，但 gpui 是否依赖栈展开未验证，
  属于有风险的优化，留到需要发布时再评估）、`opt-level = "s"`。

## 复现

```bash
DISPLAY=:0 THINSQL_PROBE=1 ./target/release/thinsql-app
# PROBE first_frame_ms=... rss=... hwm=... threads=...
```
