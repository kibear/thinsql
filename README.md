# ThinSQL
**Lightweight desktop SQL client for MySQL & PostgreSQL** — fast startup, low memory, no Electron / WebView / JVM.
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg)](https://www.rust-lang.org/)
[![UI](https://img.shields.io/badge/UI-gpui--kit-0A7EA4.svg)](https://crates.io/crates/gpui-kit)
[![Status](https://img.shields.io/badge/status-early%20preview-yellow.svg)](#status)
> Native Rust database GUI · MySQL client · PostgreSQL client · SQL editor · table browser · schema editor
[English](#thinsql) · [中文](#thinsql-轻量数据库客户端)
---
## Why ThinSQL?
Most database IDEs ship a browser engine or a JVM. ThinSQL is a **single native process** built on [gpui-kit](https://crates.io/crates/gpui-kit) (Zed’s GPUI stack): open a connection, browse schemas, run SQL, edit rows, change table structure — without waiting for a heavy runtime to boot.
| | ThinSQL | Typical Electron / JVM clients |
|---|---|---|
| Runtime | Native Rust | WebView / JVM |
| Process model | One process | Often multi-process |
| Goal | Fast & thin | Feature-maximal |
## Features
- **MySQL** and **PostgreSQL** connections (Oracle planned)
- Connection tree: schemas, tables, views, and other objects
- Multi-tab SQL workspace with syntax-aware editing
- Table data preview with paging; cell edits with pending-change commit
- Table structure editor (columns / indexes / foreign keys)
- Query history; export results to **CSV** / **JSON**
- Read-only connection mode; passwords stored in the OS keyring
- UI language: **Chinese** / **English**
## Screenshot
<!-- Add a real screenshot when available, e.g.:
![ThinSQL main window](docs/images/screenshot.png)
-->
_Screenshot coming soon._
## Status
Early preview (`0.1.0`). APIs and UI can change. Linux (X11) is the primary development target today.
## Build from source (Linux)
Requirements: a recent Rust toolchain (edition 2024), and the usual X11 / fontconfig runtime libs.
```bash
git clone https://github.com/<your-org>/thinsql.git
cd thinsql
# First time only: create native/lib symlinks for linking (no root needed)
./scripts/dev-setup-linux.sh
cargo build --release -p thinsql-app
./target/release/thinsql-app
```
Tests:
```bash
cargo test -p thinsql-core
cargo test -p thinsql-app
```
Optional integration tests against a real server (isolated DB/schema only):
```bash
THINSQL_TEST_MYSQL=1 THINSQL_TEST_MYSQL_PASS=root \
  cargo test -p thinsql-core --test mysql_integration
THINSQL_TEST_PG=1 THINSQL_TEST_PG_PASS=root \
  cargo test -p thinsql-core --test postgres_integration
```
A `.desktop` entry lives under [`packaging/thinsql.desktop`](packaging/thinsql.desktop).
## Architecture (short)
```
thinsql-app   → gpui-kit UI (shell, sidebar, tabs, results)
thinsql-core  → drivers, Value model, config, session worker thread
```
UI never holds a DB handle: it sends commands to a per-connection worker thread and polls events. Dialect-specific SQL lives only under `thinsql-core/src/drivers/`.
Contributor notes for agents and humans: [`AGENTS.md`](AGENTS.md).
## License
[Apache License 2.0](LICENSE)
---
## GitHub Topics (suggested)
When publishing the repo, set **About** description to something like:
> Lightweight native desktop SQL client for MySQL and PostgreSQL, written in Rust (gpui). Fast startup, low memory, no Electron.
Suggested topics:
`sql` `mysql` `postgresql` `database` `database-client` `sql-client` `desktop` `gui` `rust` `gpui` `lightweight` `db-tool`
---
## ThinSQL 轻量数据库客户端
**面向 MySQL / PostgreSQL 的轻量桌面 SQL 客户端**：启动快、内存省，无 Electron / WebView / JVM。
用 Rust + [gpui-kit](https://crates.io/crates/gpui-kit) 做原生界面，单进程管理连接、浏览库表、执行 SQL、编辑行数据与表结构。
### 功能概览
- 支持 **MySQL**、**PostgreSQL**（Oracle 规划中）
- 连接树：库 / schema、表、视图等对象
- 多标签 SQL 工作区
- 表数据分页预览；单元格编辑后统一提交
- 表结构编辑（列 / 索引 / 外键）
- 查询历史；结果导出 **CSV** / **JSON**
- 只读连接；密码走系统钥匙串
- 界面语言：中文 / 英文
### 从源码构建（Linux）
```bash
./scripts/dev-setup-linux.sh   # 首次必须
cargo build --release -p thinsql-app
./target/release/thinsql-app
```
当前以 Linux（X11）为主要开发与验证环境；版本 `0.1.0`，仍属早期预览。
更细的工程约定见 [`AGENTS.md`](AGENTS.md)。

