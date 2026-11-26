//! ThinSQL 桌面端。
//!
//! 拆成 lib + bin 两个目标，目的是让集成测试能构造真实的窗口与面板，
//! 而不是只能靠肉眼截图验证。

pub mod i18n;
pub mod panels;
pub mod perf;
#[cfg(target_os = "linux")]
pub mod platform_x11;
pub mod shell;
pub mod single_instance;
pub mod state;
pub mod theme;
