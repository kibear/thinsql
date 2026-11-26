//! 新建 / 编辑连接弹窗。
//!
//! 弹窗的构建函数可能被重复执行，输入实体必须在 `open_dialog` 之前创建，
//! 这是 gpui 的明确要求。密码只写系统钥匙串；编辑时留空表示不修改。

use gpui_kit::base::StyledExt;
use gpui_kit::component::Disableable;
use gpui_kit::component::Selectable;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::{DialogButtonProps, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::*;
use std::rc::Rc;
use std::time::{Duration, Instant};
use thinsql_core::config::secret;
use thinsql_core::model::{ConnectionConfig, DriverKind};
use thinsql_core::provider::Registry;

use crate::i18n::{t, tr};
use crate::state::AppState;

/// 端口必须是 1-65535。非法输入要明确拒绝，不能静默回退到 3306。
fn parse_port(value: &str) -> Result<u16, String> {
    let port: u16 = value.trim().parse().map_err(|_| {
        tr(
            "端口必须是 1-65535 的数字，当前是 {value}",
            &[("value", &format!("{value:?}"))],
        )
    })?;
    if port == 0 {
        return Err(t("端口必须在 1-65535 之间").into());
    }
    Ok(port)
}

/// 纳秒时间戳 + 进程内计数器，避免同一毫秒创建两个连接时 id 碰撞。
fn next_connection_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("c{nanos}-{seq}")
}

/// 弹窗表单的原始值。保存与测试连接共用同一条组装路径，校验只写一处。
struct FormValues {
    name: String,
    host: String,
    port: String,
    user: String,
    database: String,
    kind: DriverKind,
}

/// 把表单值组装成配置。错误信息直接用于通知提示，弹窗保持打开。
/// `default_read_only` 只影响新建连接（编辑模式沿用原配置的只读标记）。
fn assemble_config(
    base: Option<ConnectionConfig>,
    values: &FormValues,
    default_read_only: bool,
) -> Result<ConnectionConfig, String> {
    let port = parse_port(&values.port)?;
    if values.name.trim().is_empty() {
        return Err(t("连接名称不能为空").into());
    }
    let mut cfg = match base {
        Some(existing) => {
            let mut cfg = existing;
            cfg.name = values.name.clone();
            cfg.kind = values.kind;
            cfg
        }
        None => {
            let mut cfg =
                ConnectionConfig::new(next_connection_id(), values.name.clone(), values.kind);
            cfg.read_only = default_read_only;
            cfg
        }
    };
    cfg.host = values.host.clone();
    cfg.port = port;
    cfg.user = values.user.clone();
    cfg.database = Some(values.database.clone()).filter(|d| !d.is_empty());
    Ok(cfg)
}

/// 后台执行一次临时连接：成功立即断开，不落配置、不写钥匙串。
fn test_connection_task(
    cfg: ConnectionConfig,
    password: Option<String>,
) -> Result<Duration, thinsql_core::Error> {
    let started = Instant::now();
    let mut provider = Registry::create(cfg.kind)?;
    let outcome = provider.connect(&cfg, password.as_deref());
    let elapsed = started.elapsed();
    let _ = provider.disconnect();
    outcome.map(|()| elapsed)
}

/// 测试连接的进行中标记。用实体承载，按钮禁用态才能随重绘刷新。
struct TestState {
    running: bool,
}

/// 数据库类型选择。它自己是一个实体，这样切换时不需要重建成个弹窗。
struct KindPicker {
    kind: DriverKind,
    port: Entity<InputState>,
}

impl Render for KindPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .h_flex()
            .gap_1()
            .children(
                [DriverKind::MySql, DriverKind::Postgres]
                    .into_iter()
                    .map(|kind| {
                        let port = self.port.clone();
                        Button::new(format!("kind-{}", kind.display_name()))
                            .ghost()
                            .selected(self.kind == kind)
                            .label(kind.display_name())
                            .text_color(theme.foreground)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.kind = kind;
                                port.update(cx, |input, cx| {
                                    input.set_value(kind.default_port().to_string(), window, cx);
                                });
                                cx.notify();
                            }))
                    }),
            )
    }
}

fn field(label: &str, input: &Entity<InputState>) -> impl IntoElement {
    div()
        .v_flex()
        .gap_1()
        .child(div().text_xs().child(label.to_string()))
        .child(Input::new(input))
}

/// 打开新建或编辑连接弹窗。`existing` 为连接 id 时进入编辑模式。
pub fn open_connection_dialog(
    state: Entity<AppState>,
    existing: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let existing_cfg = existing
        .as_deref()
        .and_then(|id| state.read(cx).entry(id))
        .map(|entry| entry.cfg.clone());
    let is_edit = existing_cfg.is_some();
    let kind = existing_cfg
        .as_ref()
        .map(|cfg| cfg.kind)
        .unwrap_or(DriverKind::MySql);

    let name = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t("连接名称"))
            .default_value(
                existing_cfg
                    .as_ref()
                    .map(|cfg| cfg.name.clone())
                    .unwrap_or_else(|| t("本地 MySQL").into()),
            )
    });
    let host = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t("主机"))
            .default_value(
                existing_cfg
                    .as_ref()
                    .map(|cfg| cfg.host.clone())
                    .unwrap_or_else(|| "127.0.0.1".into()),
            )
    });
    let port = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t("端口"))
            .default_value(
                existing_cfg
                    .as_ref()
                    .map(|cfg| cfg.port.to_string())
                    .unwrap_or_else(|| kind.default_port().to_string()),
            )
    });
    let user = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t("用户名"))
            .default_value(
                existing_cfg
                    .as_ref()
                    .map(|cfg| cfg.user.clone())
                    .unwrap_or_else(|| "root".into()),
            )
    });
    let password = cx.new(|cx| {
        InputState::new(window, cx).placeholder(if is_edit {
            t("密码（留空则不修改）")
        } else {
            t("密码")
        })
    });
    let database = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t("默认库（可空）"))
            .default_value(
                existing_cfg
                    .as_ref()
                    .and_then(|cfg| cfg.database.clone())
                    .unwrap_or_default(),
            )
    });

    let kind_picker = cx.new(|_| KindPicker {
        kind,
        port: port.clone(),
    });

    let test_state = cx.new(|_| TestState { running: false });

    let save_name = name.clone();
    let save_host = host.clone();
    let save_port = port.clone();
    let save_user = user.clone();
    let save_password = password.clone();
    let save_database = database.clone();
    let save_kind = kind_picker.clone();
    let save_test_state = test_state.clone();

    window.open_dialog(cx, move |dialog, _, app| {
        let state = state.clone();
        let base_cfg = existing_cfg.clone();
        let default_read_only = state.read(app).config.settings.default_read_only;
        let name = save_name.clone();
        let host = save_host.clone();
        let port = save_port.clone();
        let user = save_user.clone();
        let password = save_password.clone();
        let database = save_database.clone();
        let kind_picker = save_kind.clone();
        let test_state = save_test_state.clone();

        // 测试连接：用当前表单值做一次临时握手，不保存配置、不写钥匙串。
        let test_button = {
            let name = name.clone();
            let host = host.clone();
            let port = port.clone();
            let user = user.clone();
            let database = database.clone();
            let kind_picker = kind_picker.clone();
            let password = password.clone();
            let base_cfg = base_cfg.clone();
            let test_state = test_state.clone();
            Button::new("test-connection")
                .label(t("测试连接"))
                .disabled(test_state.read(app).running)
                .on_click(move |_, window, app| {
                    if test_state.read(app).running {
                        return;
                    }
                    let values = FormValues {
                        name: name.read(app).value().to_string(),
                        host: host.read(app).value().to_string(),
                        port: port.read(app).value().to_string(),
                        user: user.read(app).value().to_string(),
                        database: database.read(app).value().to_string(),
                        kind: kind_picker.read(app).kind,
                    };
                    let cfg = match assemble_config(base_cfg.clone(), &values, default_read_only) {
                        Ok(cfg) => cfg,
                        Err(message) => {
                            window.push_notification(Notification::error(message), app);
                            return;
                        }
                    };
                    // 新建时用输入的密码；编辑模式留空则沿用钥匙串里已保存的密码
                    let password = password.read(app).value().to_string();
                    let password = if password.is_empty() {
                        secret::load(&cfg)
                    } else {
                        Some(password)
                    };

                    test_state.update(app, |state, cx| {
                        state.running = true;
                        cx.notify();
                    });
                    window.push_notification(Notification::info(t("正在测试连接…")), app);

                    let handle = window.window_handle();
                    let task = app
                        .background_executor()
                        .spawn(async move { test_connection_task(cfg, password) });
                    // on_click 是 Fn 闭包，会被多次调用；异步回调必须持有自己的句柄
                    let test_state = test_state.clone();
                    app.spawn(async move |cx| {
                        let outcome = task.await;
                        let _ = handle.update(cx, |_, window, cx| match outcome {
                            Ok(elapsed) => window.push_notification(
                                Notification::success(tr(
                                    "连接成功（{ms}ms）",
                                    &[("ms", &elapsed.as_millis().to_string())],
                                )),
                                cx,
                            ),
                            Err(err) => window.push_notification(
                                Notification::error(tr(
                                    "连接失败：{message}（{hint}）",
                                    &[("message", &err.message), ("hint", &err.hint())],
                                )),
                                cx,
                            ),
                        });
                        test_state.update(cx, |state, cx| {
                            state.running = false;
                            cx.notify();
                        });
                    })
                    .detach();
                })
        };

        // 保存：校验表单 → 写配置并自动连接。回车确认与「保存」按钮共用。
        // 实体句柄在这里克隆一份：闭包 move 走之后，表单区域还要继续用。
        let save: Rc<dyn Fn(&mut Window, &mut App) -> bool> = {
            let name = name.clone();
            let host = host.clone();
            let port = port.clone();
            let user = user.clone();
            let database = database.clone();
            let kind_picker = kind_picker.clone();
            let password = password.clone();
            let base_cfg = base_cfg.clone();
            let state = state.clone();
            Rc::new(move |window, app| {
                let values = FormValues {
                    name: name.read(app).value().to_string(),
                    host: host.read(app).value().to_string(),
                    port: port.read(app).value().to_string(),
                    user: user.read(app).value().to_string(),
                    database: database.read(app).value().to_string(),
                    kind: kind_picker.read(app).kind,
                };
                let cfg = match assemble_config(base_cfg.clone(), &values, default_read_only) {
                    Ok(cfg) => cfg,
                    Err(message) => {
                        window.push_notification(Notification::error(message), app);
                        return false;
                    }
                };
                let password = password.read(app).value().to_string();
                state.update(app, |state, cx| {
                    state.save_connection(cfg, Some(&password));
                    cx.notify();
                });
                true
            })
        };

        dialog
            .title(if is_edit { t("编辑连接") } else { t("新建连接") })
            .w(px(420.))
            .footer(
                DialogFooter::new()
                    .child(test_button)
                    .child(div().flex_1())
                    .child(
                        Button::new("cancel-connection")
                            .label(t("取消"))
                            .on_click(|_, window, app| window.close_dialog(app)),
                    )
                    .child(
                        Button::new("save-connection")
                            .primary()
                            .label(t("保存"))
                            .on_click({
                                let save = save.clone();
                                move |_, window, app| {
                                    if save(window, app) {
                                        window.close_dialog(app);
                                    }
                                }
                            }),
                    ),
            )
            .on_ok(move |_, window, app| save(window, app))
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(div().text_xs().child(t("数据库类型")))
                            .child(kind_picker.clone()),
                    )
                    .child(field(t("名称"), &name))
                    .child(field(t("主机"), &host))
                    .child(field(t("端口"), &port))
                    .child(field(t("用户名"), &user))
                    .child(field(t("密码"), &password))
                    .child(field(t("默认库"), &database)),
            )
    });
}

/// 删除连接的二次确认。
pub fn open_delete_connection_dialog(
    state: Entity<AppState>,
    conn_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let name = state
        .read(cx)
        .entry(&conn_id)
        .map(|entry| entry.cfg.name.clone())
        .unwrap_or_else(|| conn_id.clone());
    window.open_alert_dialog(cx, move |alert, _, _| {
        let state = state.clone();
        let conn_id = conn_id.clone();
        alert
            .title(tr("删除连接「{name}」？", &[("name", &name)]))
            .description(t("配置与已保存的密码会一起删除，无法撤销。"))
            .confirm()
            .button_props(
                DialogButtonProps::default()
                    .ok_text(t("删除"))
                    .ok_variant(ButtonVariant::Danger)
                    .cancel_text(t("取消")),
            )
            .on_ok(move |_, _, app| {
                let conn_id = conn_id.clone();
                state.update(app, |state, cx| {
                    state.remove_connection(&conn_id);
                    cx.notify();
                });
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use super::{FormValues, assemble_config, next_connection_id, parse_port};
    use std::collections::HashSet;
    use thinsql_core::model::{ConnectionConfig, DriverKind};

    #[test]
    fn port_rejects_invalid_and_out_of_range_values() {
        assert_eq!(parse_port("3306"), Ok(3306));
        assert_eq!(parse_port(" 5432 "), Ok(5432));
        assert!(parse_port("abc").is_err());
        assert!(parse_port("0").is_err(), "0 不是合法客户端端口");
        assert!(parse_port("70000").is_err(), "u16 溢出必须被拒绝");
    }

    #[test]
    fn connection_ids_are_unique_even_within_the_same_millisecond() {
        let ids: HashSet<String> = (0..1000).map(|_| next_connection_id()).collect();
        assert_eq!(ids.len(), 1000, "连接 id 不允许碰撞");
    }

    fn form_values(name: &str, port: &str) -> FormValues {
        FormValues {
            name: name.into(),
            host: "127.0.0.1".into(),
            port: port.into(),
            user: "root".into(),
            database: String::new(),
            kind: DriverKind::MySql,
        }
    }

    #[test]
    fn assemble_config_rejects_invalid_port_and_blank_name() {
        assert!(assemble_config(None, &form_values("本地", "abc"), true).is_err());
        assert!(assemble_config(None, &form_values("本地", "0"), true).is_err());
        assert!(assemble_config(None, &form_values("   ", "3306"), true).is_err());
    }

    #[test]
    fn assemble_config_builds_new_connection_with_empty_database_cleared() {
        let mut values = form_values("本地 MySQL", "3307");
        values.database = String::new();
        let cfg = assemble_config(None, &values, true).expect("合法表单应能组装出配置");
        assert_eq!(cfg.port, 3307);
        assert_eq!(cfg.kind, DriverKind::MySql);
        assert_eq!(cfg.database, None, "空库名按未填处理");
    }

    #[test]
    fn assemble_config_new_connection_honors_default_read_only() {
        let values = form_values("本地", "3306");
        assert!(
            assemble_config(None, &values, false)
                .expect("合法表单应能组装出配置")
                .read_only
                == false,
            "设置关闭只读时新建连接不应只读"
        );
        assert!(
            assemble_config(None, &values, true)
                .expect("合法表单应能组装出配置")
                .read_only,
            "设置开启只读时新建连接应只读"
        );
    }

    #[test]
    fn assemble_config_keeps_existing_identity_when_editing() {
        let mut base = ConnectionConfig::new("c42", "旧名字", DriverKind::Postgres);
        base.port = 5432;
        base.read_only = true;
        let values = form_values("新名字", "6543");
        let cfg = assemble_config(Some(base), &values, false).expect("编辑模式应沿用原配置");
        assert_eq!(cfg.id, "c42", "编辑不能生成新 id");
        assert_eq!(cfg.name, "新名字");
        assert_eq!(cfg.kind, DriverKind::MySql);
        assert_eq!(cfg.port, 6543);
        assert!(cfg.read_only, "只读标记不能被表单丢失");
    }
}
