---
name: fix-review-findings
overview: 按代码审查结论修复：1) 将 single_instance 实现改为 OS 文件锁（flock）方案以匹配已重写的测试；2) 修复 disconnect 中 column/apply 请求清理失效的问题；3) 修复 set_tab_connection 的 query_in_flight 残留与同连接重复选择误清理；4) 增强子进程测试失败路径的健壮性。
todos:
  - id: rewrite-single-instance-impl
    content: 使用 [skill:rust-patterns] 重写 single_instance.rs 实现：flock 互斥、保留锁文件、创建父目录、fail-closed，引入区分「已在运行/锁不可用」的错误枚举，移除失效的 acquire_with/pid_alive
    status: completed
  - id: fix-disconnect-cleanup
    content: 修复 state.rs disconnect：为 column_requests/apply_requests 关联连接维度，使断开时能清理全部在途列/提交请求映射
    status: completed
  - id: fix-set-tab-connection-robustness
    content: 修复 set_tab_connection：同连接切换提前返回、重置 query_in_flight；修补子进程测试失败路径遗留子进程问题
    status: completed
    dependencies:
      - fix-disconnect-cleanup
  - id: verify-tests
    content: 使用 [skill:verification-before-completion] 运行 cargo test -p thinsql-app，确认全部测试通过且无新告警
    status: completed
    dependencies:
      - rewrite-single-instance-impl
      - fix-set-tab-connection-robustness
---

## 背景

用户要求 review 工作区两处未提交改动（`single_instance.rs`、`state.rs`）。审查已完成，发现以下问题，本计划用于修复：

## 审查结论

- **P0（阻塞）**：`single_instance.rs` 的测试已被重写为 flock 新设计（互斥只依赖 OS 文件锁、锁文件永不删除以防 inode 竞态、自动创建父目录、IO 失败拒绝启动），但实现（第 30-97 行）仍是旧的 "PID 判活 + 删锁文件 + IO 失败跳过互斥" 方案，7 个测试中约 5 个会失败。
- **P1（逻辑缺陷）**：`state.rs` 的 `disconnect()` 收集的 `request_ids` 仅来自 `in_flight`，而 `column_requests`（ListColumns 使用独立 column_id）与 `apply_requests`（Apply 使用独立 request_id）的 id 从不进入 `in_flight`，因此新增的两个 `retain` 是空操作，清理无效，会残留过期请求映射。
- **P2（健壮性）**：
- `set_tab_connection` 未重置 `tab.query_in_flight`，且 `old_conn_id == conn_id`（重新选择同一连接）时也会误清在途结果；
- 子进程测试 `child_process_contention_and_exit_release` 在 LOCK_READY 读取循环的失败路径上会遗留等待 stdin 的子进程。

## 验收标准

- `cargo test -p thinsql-app` 全部通过，包括新固化的单实例测试与 state 测试；
- 锁文件语义符合新测试契约：OS 锁为唯一互斥依据、释放后锁文件保留且 inode 不变、IO 失败 fail-closed；
- 断开连接时该连接的全部在途请求（查询 / 列读取 / 提交）映射均被清理。

## 技术方案

- 语言与工程：Rust 1.99.1 / edition 2024，沿用现有 crate 结构，不引入新依赖（`fs::File::try_lock` 已稳定可用）。
- **single_instance.rs 实现重写（对齐测试契约）**：
- `acquire`：`fs::create_dir_all` 创建父目录 → `OpenOptions::new().write(true).create(true).truncate(false)` 打开/创建锁文件 → `file.try_lock()`；
- `try_lock` 返回 `WouldBlock` → 返回 `AlreadyRunning { pid: read_pid().unwrap_or(0) }`（PID 仅作展示，advisory）；
- 获取锁成功后截断写入自身 PID；任何 IO 失败 → 返回错误拒绝启动（fail-closed）。错误类型需从单一的 `AlreadyRunning` 扩展为可区分「已有实例」与「锁不可用」的枚举（测试只断言 `is_err()`，语义上不应把 IO 错误伪装成“已在运行”）；
- `Drop`：只释放 OS 锁（关闭文件即可），**不再删除锁文件**；
- 删除不再使用的 `acquire_with`、`pid_alive`、`InstanceGuard.pid` 字段（避免 dead_code 告警），保留 `read_pid` 供错误信息使用。
- **state.rs disconnect 修复**：将 `column_requests: HashMap<RequestId, TabId>`、`apply_requests: HashMap<RequestId, TabId>` 的值改为 `(conn_id, TabId)`（或在 `ConnEntry` 中记录 pending 集合），使 `disconnect` 能按连接清理；同步修改所有插入点（第 838、984 行）与消费点（第 1405、1459、1463、1488 行）及 `close_tab`/`set_tab_connection` 的 retain 逻辑，保持行为不变、仅增加维度。
- **state.rs P2 修复**：`set_tab_connection` 中若 `old_conn_id == conn_id` 则提前返回不做清理；清理时同步重置 `tab.query_in_flight = false`。
- **测试健壮性**：`child_process_contention_and_exit_release` 中在读取循环失败/断言失败路径上先 `child.kill()` 再返回（可用 defer 式封装或调整结构），确保不遗留子进程。
- 性能与爆炸半径：均为局部修改，不触及 `thinsql-core`；事件处理路径仍为 HashMap O(1) 操作。

## Agent Extensions

### Skill

- **rust-patterns**
- Purpose: 指导 flock 锁实现、错误类型设计与所有权处理，保证实现符合惯用 Rust 模式
- Expected outcome: single_instance.rs 重写后具备清晰的错误枚举与正确的 Drop 语义
- **rust-testing**
- Purpose: 按测试契约驱动实现（测试已先行），并完善子进程测试的清理逻辑
- Expected outcome: 7 个单实例测试与新增 state 测试全部通过
- **verification-before-completion**
- Purpose: 修复完成后运行 `cargo test -p thinsql-app` 验证并以实际输出为准
- Expected outcome: 全部测试通过，无 dead_code 告警回归