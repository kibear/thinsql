//! 启动性能探针（默认关闭）。
//!
//! `THINSQL_PROBE=1` 时在首帧打印"进程启动 → 首帧渲染完成"的耗时与当前 RSS。
//! 保留在代码里而不是一次性脚本，是因为这个数字会随依赖和功能变化而漂移，
//! 需要能随时复查——"轻量"是这个产品的前提，不能只靠一次测量。
//!
//! 注意：这里测的是空载首帧，不含连接数据库与加载元数据，是性能下限。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
static LOGGED: AtomicBool = AtomicBool::new(false);

pub fn enabled() -> bool {
    std::env::var("THINSQL_PROBE").as_deref() == Ok("1")
}

/// 在 main 的最早期调用。
pub fn mark_start() {
    let _ = START.set(Instant::now());
}

/// 首帧渲染时调用，只生效一次。
pub fn log_first_frame() {
    if !enabled() || LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    let ms = START.get().map(|t| t.elapsed().as_millis()).unwrap_or(0);

    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let pick = |key: &str| {
        status
            .lines()
            .find(|line| line.starts_with(key))
            .map(|line| line.trim_start_matches(key).trim().to_string())
            .unwrap_or_else(|| "?".into())
    };

    println!(
        "PROBE first_frame_ms={ms} rss={} hwm={} threads={}",
        pick("VmRSS:"),
        pick("VmHWM:"),
        pick("Threads:"),
    );
}
