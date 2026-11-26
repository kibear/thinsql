//! Phase 0 输入法探针：通过 XTEST 向指定窗口注入按键，用来验证
//! gpui + XIM 在 fcitx5 下的中文输入是否真的可用。
//!
//! 用法：ime-probe <窗口ID(十六进制或十进制)> [要输入的拼音]
//!
//! 这是验证工具，不属于产品代码。

use std::thread::sleep;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, InputFocus};
use x11rb::protocol::xtest::ConnectionExt as _;

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const MOTION: u8 = 6;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;

/// 发送组合键，目前支持 ctrl-return / ctrl-enter。
/// 先显式设置输入焦点，键盘事件不受窗口叠放顺序影响。
fn send_combo(win: u32, combo: &str) {
    let (conn, _screen) = x11rb::connect(None).expect("连接 X 失败");
    let setup = conn.setup();
    let min = setup.min_keycode;
    let count = setup.max_keycode - min + 1;
    let map = conn
        .get_keyboard_mapping(min, count)
        .expect("get_keyboard_mapping")
        .reply()
        .expect("reply");
    let per = map.keysyms_per_keycode as usize;
    let keycode_of = |keysym: u32| -> u8 {
        for i in 0..count as usize {
            for j in 0..per {
                if map.keysyms[i * per + j] == keysym {
                    return min + i as u8;
                }
            }
        }
        panic!("找不到 keysym {keysym:#x}");
    };

    let ctrl = keycode_of(0xffe3);
    let main = match combo.split('-').last().unwrap_or("return") {
        "return" | "enter" => keycode_of(0xff0d),
        other => panic!("不支持的主键 {other}"),
    };

    conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
        .expect("set_input_focus");
    let _ = conn.flush();
    sleep(Duration::from_millis(200));

    let _ = conn.xtest_fake_input(KEY_PRESS, ctrl, 0, x11rb::NONE, 0, 0, 0);
    let _ = conn.xtest_fake_input(KEY_PRESS, main, 0, x11rb::NONE, 0, 0, 0);
    let _ = conn.flush();
    sleep(Duration::from_millis(60));
    let _ = conn.xtest_fake_input(KEY_RELEASE, main, 0, x11rb::NONE, 0, 0, 0);
    let _ = conn.xtest_fake_input(KEY_RELEASE, ctrl, 0, x11rb::NONE, 0, 0, 0);
    let _ = conn.flush();
    println!("sent {combo}");
    sleep(Duration::from_millis(300));
}

/// 在窗口内相对坐标处点一下左键。
/// 先把窗口坐标翻译成根坐标，避免窗口位置变化导致点偏。
fn click(win: u32, x: i16, y: i16) {
    let (conn, screen) = x11rb::connect(None).expect("连接 X 失败");
    let root = conn.setup().roots[screen].root;

    let translated = conn
        .translate_coordinates(win, root, x, y)
        .expect("translate_coordinates")
        .reply()
        .expect("reply");

    conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
        .ok();
    let _ = conn.xtest_fake_input(MOTION, 0, 0, root, translated.dst_x, translated.dst_y, 0);
    let _ = conn.flush();
    sleep(Duration::from_millis(120));
    let _ = conn.xtest_fake_input(BUTTON_PRESS, 1, 0, root, 0, 0, 0);
    let _ = conn.flush();
    sleep(Duration::from_millis(60));
    let _ = conn.xtest_fake_input(BUTTON_RELEASE, 1, 0, root, 0, 0, 0);
    let _ = conn.flush();
    println!(
        "clicked window({x},{y}) -> root({},{})",
        translated.dst_x, translated.dst_y
    );
    sleep(Duration::from_millis(400));
}

fn parse_window(arg: &str) -> u32 {
    let arg = arg.trim();
    let stripped = arg.strip_prefix("0x").unwrap_or(arg);
    u32::from_str_radix(stripped, 16).unwrap_or_else(|_| arg.parse().expect("无效的窗口 ID"))
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let win = parse_window(argv.first().expect("需要窗口 ID"));

    // 子命令 click：`ime-probe <win> click <x> <y>`，坐标相对窗口左上角
    if argv.get(1).map(String::as_str) == Some("click") {
        let x: i16 = argv.get(2).expect("需要 x").parse().expect("x 非法");
        let y: i16 = argv.get(3).expect("需要 y").parse().expect("y 非法");
        click(win, x, y);
        return;
    }

    // 子命令 key：`ime-probe <win> key ctrl-return`，用于触发快捷键
    if argv.get(1).map(String::as_str) == Some("key") {
        let combo = argv.get(2).cloned().unwrap_or_else(|| "ctrl-return".into());
        send_combo(win, &combo);
        return;
    }

    let text = argv.get(1).cloned().unwrap_or_else(|| "nihao".into());
    let commit = argv.get(2).map(String::as_str) == Some("commit");

    let (conn, _screen) = x11rb::connect(None).expect("连接 X 失败");
    let setup = conn.setup();
    let min = setup.min_keycode;
    let count = setup.max_keycode - min + 1;

    let map = conn
        .get_keyboard_mapping(min, count)
        .expect("get_keyboard_mapping")
        .reply()
        .expect("reply");
    let per = map.keysyms_per_keycode as usize;
    let keycode_of = |keysym: u32| -> u8 {
        for i in 0..count as usize {
            for j in 0..per {
                if map.keysyms[i * per + j] == keysym {
                    return min + i as u8;
                }
            }
        }
        panic!("找不到 keysym {keysym:#x} 对应的 keycode");
    };

    // 显式把焦点给目标窗口，避免按键打进其他应用
    conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
        .expect("set_input_focus");
    conn.flush().expect("flush");
    sleep(Duration::from_millis(300));

    let tap = |code: u8| {
        let _ = conn.xtest_fake_input(KEY_PRESS, code, 0, x11rb::NONE, 0, 0, 0);
        let _ = conn.flush();
        sleep(Duration::from_millis(30));
        let _ = conn.xtest_fake_input(KEY_RELEASE, code, 0, x11rb::NONE, 0, 0, 0);
        let _ = conn.flush();
        sleep(Duration::from_millis(80));
    };

    // Ctrl+Space：fcitx5 默认的激活/切换输入法快捷键
    let ctrl = keycode_of(0xffe3);
    let space = keycode_of(0x0020);
    let _ = conn.xtest_fake_input(KEY_PRESS, ctrl, 0, x11rb::NONE, 0, 0, 0);
    let _ = conn.flush();
    sleep(Duration::from_millis(50));
    tap(space);
    let _ = conn.xtest_fake_input(KEY_RELEASE, ctrl, 0, x11rb::NONE, 0, 0, 0);
    let _ = conn.flush();
    sleep(Duration::from_millis(600));

    for ch in text.chars() {
        tap(keycode_of(ch as u32));
    }

    if commit {
        tap(space);
    }

    println!("typed: {text} commit={commit}");
    sleep(Duration::from_millis(1500));
}
