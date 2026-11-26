//! 补设 X11 窗口属性（仅 Linux）。
//!
//! gpui 在 X11 下不设置 `WM_CLASS`，导致：
//! - 任务栏无法分组、图标缺失；
//! - 部分窗口管理器拒绝激活窗口，Alt-Tab 切不过去；
//! - 依赖 WM_CLASS 的自动化工具按类查找会失效。
//!
//! 这里在窗口创建后按 PID 找到本进程的窗口，补上 `WM_CLASS`、
//! `WM_NAME`/`_NET_WM_NAME` 与 `_NET_WM_WINDOW_TYPE`。
//! 所有失败都静默忽略——这只是锦上添花，不能因为补属性失败而影响启动。

use std::thread;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
use x11rb::wrapper::ConnectionExt as _;

const APP_ID: &str = "thinsql";
const APP_NAME: &str = "ThinSQL";

/// 在后台线程里补属性：窗口可能还没映射完，所以重试若干次。
pub fn install_window_properties() {
    thread::spawn(|| {
        for attempt in 0..20 {
            match try_install() {
                Ok(true) => {
                    tracing::info!("已补设 WM_CLASS = {APP_ID}");
                    return;
                }
                Ok(false) => {
                    let wait = if attempt < 5 { 50 } else { 200 };
                    thread::sleep(Duration::from_millis(wait));
                }
                Err(err) => {
                    tracing::debug!(error = %err, "补设窗口属性失败，忽略");
                    return;
                }
            }
        }
        tracing::debug!("未定位到本进程的窗口，跳过 WM_CLASS 设置");
    });
}

fn try_install() -> Result<bool, Box<dyn std::error::Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let pid = std::process::id();

    let atom_pid = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
    let atom_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
    let atom_utf8 = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
    let atom_type = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE")?
        .reply()?
        .atom;
    let atom_type_normal = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE_NORMAL")?
        .reply()?
        .atom;

    let Some(window) = find_window(&conn, root, atom_pid, pid, 4) else {
        return Ok(false);
    };

    // WM_CLASS 的格式是 "instance\0class\0"
    let class = format!("{APP_ID}\0{APP_NAME}\0");
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        class.as_bytes(),
    )?;

    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        APP_NAME.as_bytes(),
    )?;

    conn.change_property8(
        PropMode::REPLACE,
        window,
        atom_name,
        atom_utf8,
        APP_NAME.as_bytes(),
    )?;

    conn.change_property32(
        PropMode::REPLACE,
        window,
        atom_type,
        AtomEnum::ATOM,
        &[atom_type_normal],
    )?;

    conn.flush()?;
    Ok(true)
}

/// 窗口管理器会把自己的窗口重父化到 frame 窗口下，所以不能只看 root 的直接子节点。
fn find_window<C: Connection>(
    conn: &C,
    node: u32,
    atom_pid: u32,
    pid: u32,
    depth: u32,
) -> Option<u32> {
    if depth == 0 {
        return None;
    }
    let tree = conn.query_tree(node).ok()?.reply().ok()?;
    for child in tree.children {
        let owned = conn
            .get_property(false, child, atom_pid, AtomEnum::CARDINAL, 0, 1)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32().and_then(|mut v| v.next()))
            == Some(pid);
        if owned {
            return Some(child);
        }
        if let Some(found) = find_window(conn, child, atom_pid, pid, depth - 1) {
            return Some(found);
        }
    }
    None
}
