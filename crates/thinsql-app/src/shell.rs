//! 应用主视图：顶部工具条 + 左侧连接树 + 多标签工作区 + 底部状态栏。
//!
//! 单独抽出来是为了让测试能构造真实的窗口，而不必复制一份布局代码。

use gpui_kit::base::StyledExt;
use gpui_kit::component::Disableable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::*;

use crate::i18n::{t, tr};
use crate::panels::connection_dialog::open_connection_dialog;
use crate::panels::settings_dialog::open_settings_dialog;
use crate::panels::sidebar::SidebarPanel;
use crate::panels::workspace::WorkspacePanel;
use crate::state::AppState;

pub struct Shell {
    state: Entity<AppState>,
    workspace: Entity<WorkspacePanel>,
    sidebar: Entity<SidebarPanel>,
    _sub: Subscription,
}

impl Shell {
    pub fn build(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let state = cx.new(AppState::new);
            let workspace = cx.new(|cx| WorkspacePanel::new(state.clone(), window, cx));
            let sidebar =
                cx.new(|cx| SidebarPanel::new(state.clone(), workspace.clone(), window, cx));
            let sub = cx.observe(&state, |_, _, cx| cx.notify());

            Self {
                state,
                workspace,
                sidebar,
                _sub: sub,
            }
        })
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (border, muted_foreground) = {
            let theme = cx.theme();
            (theme.border, theme.muted_foreground)
        };
        let (conn_name, can_refresh, current) = {
            let state = self.state.read(cx);
            let name = state
                .current
                .as_deref()
                .and_then(|id| state.entry(id))
                .map(|entry| entry.cfg.name.clone())
                .unwrap_or_else(|| t("未连接").into());
            (name, state.current.is_some(), state.current.clone())
        };
        let dialog_state = self.state.clone();
        let settings_state = self.state.clone();

        div()
            .h_flex()
            .items_center()
            .gap_2()
            .px_2()
            .h_10()
            .border_b_1()
            .border_color(border)
            .child(Button::new("new-connection").label(t("新建连接")).on_click(
                move |_, window, app| {
                    open_connection_dialog(dialog_state.clone(), None, window, app);
                },
            ))
            .child(
                Button::new("new-query")
                    .label(t("新建查询"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.workspace.update(cx, |workspace, cx| {
                            workspace.open_query_tab(window, cx);
                        });
                    })),
            )
            .child(
                Button::new("refresh-connection")
                    .ghost()
                    .label(t("刷新"))
                    .disabled(!can_refresh)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let Some(id) = current.clone() else {
                            return;
                        };
                        this.state.update(cx, |state, cx| {
                            state.refresh_connection(&id);
                            cx.notify();
                        });
                    })),
            )
            .child(div().flex_1())
            .child(Button::new("open-settings").ghost().label(t("设置")).on_click(
                move |_, window, app| {
                    open_settings_dialog(settings_state.clone(), window, app);
                },
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(muted_foreground)
                    .child(conn_name),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (message, meta) = {
            let state = self.state.read(cx);
            match state.active() {
                Some(tab) => {
                    let conn = tab
                        .conn_id
                        .as_deref()
                        .and_then(|id| state.entry(id))
                        .map(|entry| entry.cfg.name.clone())
                        .unwrap_or_else(|| "-".into());
                    let rows = tab
                        .result
                        .as_ref()
                        .map(|result| match result.affected_rows {
                            Some(affected) => tr("影响 {affected} 行", &[("affected", &affected.to_string())]),
                            None => tr("{n} 行", &[("n", &result.row_count().to_string())]),
                        })
                        .unwrap_or_else(|| "-".into());
                    let elapsed = tab
                        .result
                        .as_ref()
                        .map(|result| format!("{}ms", result.elapsed.as_millis()))
                        .unwrap_or_else(|| "-".into());
                    (
                        state.status.clone(),
                        format!(
                            "{} · {} · {} · {}",
                            conn,
                            tab.schema
                                .as_deref()
                                .or_else(|| {
                                    tab.conn_id
                                        .as_deref()
                                        .and_then(|id| state.entry(id))
                                        .and_then(|e| e.cfg.database.as_deref())
                                })
                                .unwrap_or_else(|| t("未选择库")),
                            rows,
                            elapsed
                        ),
                    )
                }
                None => (state.status.clone(), String::new()),
            }
        };

        div()
            .h_flex()
            .items_center()
            .gap_2()
            .px_2()
            .h_6()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(div().flex_1().min_w_0().child(message))
            .child(meta)
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::log_first_frame();
        let (background, foreground) = {
            let theme = cx.theme();
            (theme.background, theme.foreground)
        };
        div()
            .v_flex()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(self.render_toolbar(cx))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("main-layout")
                        // 分栏宽度是窗口几何约束，随窗口宽度调整
                        .child(
                            resizable_panel()
                                .size(px(280.))
                                .size_range(px(220.)..px(420.))
                                .child(self.sidebar.clone()),
                        )
                        .child(resizable_panel().child(self.workspace.clone())),
                ),
            )
            .child(self.render_status_bar(cx))
    }
}
