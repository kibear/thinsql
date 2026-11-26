//! 主工作区：标签条 + 每个标签独立的编辑器与结果视图。
//!
//! 编辑器与结果视图依赖 `Window`，只能由这一层持有；`AppState` 只保存
//! 标签元数据与数据。标签的创建与关闭都必须经过本面板，保证 UI 实体与
//! 状态层的标签一一对应，关闭时订阅随之释放。

use std::collections::{HashMap, HashSet};

use gpui_kit::base::StyledExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::resizable::{resizable_panel, v_resizable};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::base::Disableable;
use gpui_kit::*;
use thinsql_core::model::TableRef;

use crate::i18n::t;
use crate::panels::editor::{create_editor, full_sql, sql_to_run};
use crate::panels::results::ResultsView;
use crate::panels::structure::StructureView;
use crate::state::{AppState, TabId};

struct TabView {
    /// 表数据标签不显示 SQL 编辑器，编辑器实体一并省去；
    /// 查询标签持有编辑器，Ctrl+Enter 执行依赖它。
    editor: Option<Entity<EditorState>>,
    results: Entity<ResultsView>,
    /// 结构编辑标签专用的面板，与编辑器/结果视图互斥
    structure: Option<Entity<StructureView>>,
    /// Ctrl+Enter 的订阅必须随标签一起释放，否则关掉的标签仍会响应回车。
    /// 仅查询标签存在（订阅挂在编辑器事件上）。
    _enter: Option<Subscription>,
}

pub struct WorkspacePanel {
    state: Entity<AppState>,
    views: HashMap<TabId, TabView>,
    _sub: Subscription,
}

impl WorkspacePanel {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // observe 同时覆盖 cx.emit(StateEvent) 与 UI 侧 cx.notify()，
        // 否则本地操作（增删标签、切换连接）不会触发视图对账。
        let sub = cx.observe(&state, |this, _state, cx| {
            this.prune_views(cx);
            cx.notify();
        });

        let mut this = Self {
            state,
            views: HashMap::new(),
            _sub: sub,
        };
        let existing: Vec<(TabId, String)> = this
            .state
            .read(cx)
            .tabs
            .iter()
            .map(|tab| (tab.id, tab.initial_sql.clone()))
            .collect();
        for (id, sql) in existing {
            this.ensure_view(id, &sql, window, cx);
        }
        if let Some(active) = this.state.read(cx).active_tab_id() {
            this.focus_tab(active, window, cx);
        }
        this
    }

    fn ensure_view(&mut self, id: TabId, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.views.contains_key(&id) {
            return;
        }
        let state = self.state.clone();
        let results = cx.new(|cx| ResultsView::new(state.clone(), id, window, cx));
        let is_structure_tab = self
            .state
            .read(cx)
            .tab(id)
            .is_some_and(|tab| tab.is_structure_tab());
        let structure = is_structure_tab
            .then(|| cx.new(|cx| StructureView::new(state.clone(), id, window, cx)));

        // 表数据/结构标签整区渲染专属面板，不建编辑器；编辑器只在查询标签创建
        let (editor, enter) = {
            let is_table_tab = self
                .state
                .read(cx)
                .tab(id)
                .is_some_and(|tab| tab.is_table_tab());
            if is_table_tab || is_structure_tab {
                (None, None)
            } else {
                let editor = create_editor(sql, window, cx);

                // Ctrl+Enter 走输入框自身事件：动作分发依赖焦点链，这条路径更可靠
                let enter = cx.subscribe(&editor, move |this, editor, event: &InputEvent, cx| {
                    if let InputEvent::PressEnter {
                        secondary: true, ..
                    } = event
                    {
                        let sql = sql_to_run(&editor, cx);
                        this.state.update(cx, |state, cx| {
                            state.execute(id, sql);
                            cx.notify();
                        });
                    }
                });
                (Some(editor), Some(enter))
            }
        };

        self.views.insert(
            id,
            TabView {
                editor,
                results,
                structure,
                _enter: enter,
            },
        );
    }

    /// 状态层删除标签后（例如删除连接），把对应的 UI 实体一并释放。
    fn prune_views(&mut self, cx: &mut Context<Self>) {
        let ids: HashSet<TabId> = self.state.read(cx).tabs.iter().map(|t| t.id).collect();
        self.views.retain(|id, _| ids.contains(id));
    }

    fn focus_tab(&self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.views.get(&id)
            && let Some(editor) = &view.editor
        {
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
    }

    pub fn open_query_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.state.update(cx, |state, cx| {
            let id = state.new_query_tab();
            cx.notify();
            id
        });
        let sql = self
            .state
            .read(cx)
            .tab(id)
            .map(|tab| tab.initial_sql.clone())
            .unwrap_or_default();
        self.ensure_view(id, &sql, window, cx);
        self.focus_tab(id, window, cx);
    }

    pub fn open_query_tab_on(
        &mut self,
        conn_id: String,
        schema: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.state.update(cx, |state, cx| {
            let id = state.new_query_tab_on(Some(conn_id), schema, String::new());
            cx.notify();
            id
        });
        self.ensure_view(id, "", window, cx);
        self.focus_tab(id, window, cx);
    }

    /// 点结构树里的表：开标签、填预览 SQL、立即执行。
    pub fn open_table_tab(
        &mut self,
        conn_id: &str,
        table: TableRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.state.update(cx, |state, cx| {
            let id = state.open_table_tab(conn_id, table);
            cx.notify();
            id
        });
        let sql = self
            .state
            .read(cx)
            .tab(id)
            .map(|tab| tab.initial_sql.clone())
            .unwrap_or_default();
        self.ensure_view(id, &sql, window, cx);
        self.focus_tab(id, window, cx);
    }

    /// 表节点右键"修改表结构"：复用已有结构标签或新建，并置为焦点。
    pub fn open_structure_tab(
        &mut self,
        conn_id: &str,
        table: TableRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.state.update(cx, |state, cx| {
            let id = state.open_structure_tab(conn_id, table);
            cx.notify();
            id
        });
        self.ensure_view(id, "", window, cx);
        self.focus_tab(id, window, cx);
    }

    fn activate(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.activate_tab(id);
            cx.notify();
        });
        self.focus_tab(id, window, cx);
    }

    fn close_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.close_tab(id);
            cx.notify();
        });
        self.views.remove(&id);
        if let Some(active) = self.state.read(cx).active_tab_id() {
            self.focus_tab(active, window, cx);
        }
    }

    fn execute_tab(&mut self, id: TabId, all: bool, cx: &mut Context<Self>) {
        let Some(view) = self.views.get(&id) else {
            return;
        };
        let Some(editor) = view.editor.as_ref() else {
            // 表数据标签没有编辑器，执行按钮也不会渲染
            return;
        };
        let sql = if all {
            full_sql(editor, cx)
        } else {
            sql_to_run(editor, cx)
        };
        if sql.trim().is_empty() {
            return;
        }
        self.state.update(cx, |state, cx| {
            if all {
                state.execute_all(id, sql);
            } else {
                state.execute(id, sql);
            }
            cx.notify();
        });
    }

    fn cancel_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.cancel(id);
            cx.notify();
        });
    }

    fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (border, danger, foreground, muted_foreground, muted, primary) = {
            let theme = cx.theme();
            (
                theme.border,
                theme.danger,
                theme.foreground,
                theme.muted_foreground,
                theme.muted,
                theme.primary,
            )
        };
        let chips: Vec<(TabId, String, bool, bool)> = self
            .state
            .read(cx)
            .tabs
            .iter()
            .map(|tab| {
                (
                    tab.id,
                    tab.title.clone(),
                    tab.is_running(),
                    tab.has_failed_statement(),
                )
            })
            .collect();
        let active = self.state.read(cx).active_tab_id();

        let mut strip = div()
            .h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(border);

        for (id, title, running, failed) in chips {
            let is_active = Some(id) == active;
            let mut label = div().text_xs().child(title);
            label = if failed {
                label.text_color(danger)
            } else if is_active {
                label.text_color(foreground)
            } else {
                label.text_color(muted_foreground)
            };

            let mut chip = div()
                .id(("tab", id))
                .h_flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_md()
                .child(label);
            if is_active {
                chip = chip.bg(muted);
            }
            if running {
                chip = chip.child(div().w_1().h_1().rounded_full().bg(primary));
            }
            chip = chip.child(
                div()
                    .id(("close-tab", id))
                    .px_1()
                    .text_xs()
                    .text_color(muted_foreground)
                    .child("×")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_tab(id, window, cx);
                    })),
            );
            chip = chip.on_click(cx.listener(move |this, _, window, cx| {
                this.activate(id, window, cx);
            }));
            strip = strip.child(chip);
        }

        strip.child(
            Button::new("new-query-tab")
                .ghost()
                .label("＋")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_query_tab(window, cx);
                })),
        )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_foreground = cx.theme().muted_foreground;
        div()
            .v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(muted_foreground)
                    .child(t("还没有打开的标签")),
            )
            .child(
                Button::new("empty-new-query")
                    .label(t("新建查询"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_query_tab(window, cx);
                    })),
            )
    }

    fn render_tab_body(
        &self,
        id: TabId,
        view: &TabView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (border, muted_foreground, warning) = {
            let theme = cx.theme();
            (theme.border, theme.muted_foreground, theme.warning)
        };
        let (conn_name, schema_label, schemas, has_conn, read_only, status, show_results, is_table_tab, is_structure_tab) = {
            let state = self.state.read(cx);
            let tab = state.tab(id);
            let conn = tab
                .and_then(|t| t.conn_id.as_deref())
                .and_then(|c| state.entry(c));
            let show_results = tab.is_some_and(|t| {
                t.result.is_some()
                    || t.is_running()
                    || !t.statements.is_empty()
                    || !t.pending_changes.is_empty()
            });
            let schemas = conn.map(|c| c.schemas.clone()).unwrap_or_default();
            (
                conn.map(|c| c.cfg.name.clone())
                    .unwrap_or_else(|| t("未选择连接").into()),
                tab.and_then(|t| t.schema.clone())
                    .or_else(|| conn.and_then(|c| c.cfg.database.clone()))
                    .unwrap_or_else(|| t("未选择库").into()),
                schemas,
                conn.is_some(),
                conn.map(|c| c.cfg.read_only).unwrap_or(false),
                tab.map(|t| t.status.clone()).unwrap_or_default(),
                show_results,
                tab.is_some_and(|t| t.is_table_tab()),
                tab.is_some_and(|t| t.is_structure_tab()),
            )
        };

        let context_row = div()
            .h_flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(border)
            .child(
                Button::new(("tab-conn", id))
                    .label(conn_name)
                    .dropdown_menu({
                        let state = self.state.clone();
                        move |menu, _, cx| {
                            let conns: Vec<(String, String)> = state
                                .read(cx)
                                .conns
                                .iter()
                                .map(|c| (c.cfg.id.clone(), c.cfg.name.clone()))
                                .collect();
                            let mut menu = menu;
                            if conns.is_empty() {
                                menu = menu.item(PopupMenuItem::new(t("暂无连接")).disabled(true));
                            }
                            for (conn_id, name) in conns {
                                let state = state.clone();
                                menu = menu.item(PopupMenuItem::new(name).on_click(
                                    move |_, _, app| {
                                        let conn_id = conn_id.clone();
                                        state.update(app, |s, cx| {
                                            s.set_tab_connection(id, conn_id);
                                            cx.notify();
                                        });
                                    },
                                ));
                            }
                            menu
                        }
                    }),
            )
            .child(
                Button::new(("tab-schema", id))
                    .ghost()
                    .label(schema_label)
                    .disabled(!has_conn || schemas.is_empty())
                    .dropdown_menu({
                        let state = self.state.clone();
                        let schemas = schemas.clone();
                        move |menu, _, _cx| {
                            let mut menu = menu;
                            if schemas.is_empty() {
                                menu = menu.item(
                                    PopupMenuItem::new(t("暂无库列表")).disabled(true),
                                );
                            }
                            for schema in schemas.clone() {
                                let state = state.clone();
                                menu = menu.item(PopupMenuItem::new(schema.clone()).on_click(
                                    move |_, _, app| {
                                        let schema = schema.clone();
                                        state.update(app, |s, cx| {
                                            s.set_tab_schema(id, schema);
                                            cx.notify();
                                        });
                                    },
                                ));
                            }
                            menu
                        }
                    }),
            )
            .when(read_only, |row| {
                row.child(div().text_xs().text_color(warning).child(t("只读")))
            });

        // 结构标签：整区交给结构面板，状态条贴在上下文行。
        if is_structure_tab {
            return div()
                .v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    context_row.child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(muted_foreground)
                            .truncate()
                            .child(status),
                    ),
                )
                .child(
                    div().flex_1().min_h_0().child(
                        view.structure
                            .clone()
                            .map(|structure| structure.into_any_element())
                            .unwrap_or_else(|| div().into_any_element()),
                    ),
                );
        }

        // 表数据标签：整个区域就是数据表格，执行状态并入上下文行，
        // 没有编辑器与执行按钮；查询标签才走编辑器 + 结果分栏。
        if is_table_tab {
            return div()
                .v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    context_row.child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(muted_foreground)
                            .truncate()
                            .child(status),
                    ),
                )
                .child(div().flex_1().min_h_0().child(view.results.clone()));
        }

        let action_bar = div()
            .h_flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(border)
            .child(
                Button::new(("run", id))
                    .primary()
                    .label(t("执行"))
                    .on_click(cx.listener(move |this, _, _, cx| this.execute_tab(id, false, cx))),
            )
            .child(
                Button::new(("run-all", id))
                    .label(t("执行所有"))
                    .on_click(cx.listener(move |this, _, _, cx| this.execute_tab(id, true, cx))),
            )
            .child(
                Button::new(("cancel", id))
                    .label(t("取消"))
                    .on_click(cx.listener(move |this, _, _, cx| this.cancel_tab(id, cx))),
            )
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(muted_foreground)
                    .child(status),
            );

        // 编辑器填满所在面板；多行 Input 默认 h_auto，必须显式给高度才有内部滚动。
        let editor = match view.editor.as_ref() {
            Some(editor) => div().size_full().child(Editor::new(editor).h(relative(1.))),
            None => div(),
        };

        let body: AnyElement = if show_results {
            // 执行过查询后才出现结果区，编辑器与结果上下分栏、分界线可拖拽；
            // 分栏记忆按标签区分，避免切换标签串尺寸。
            v_resizable(("editor-results", id))
                .child(resizable_panel().size(px(420.)).child(editor))
                .child(resizable_panel().child(view.results.clone()))
                .into_any_element()
        } else {
            div().flex_1().min_h_0().child(editor).into_any_element()
        };

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .child(context_row)
            .child(action_bar)
            .child(body)
    }
}

impl Render for WorkspacePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strip = self.render_tab_strip(cx).into_any_element();
        let active = self.state.read(cx).active_tab_id();
        let body = match active.and_then(|id| self.views.get(&id).map(|view| (id, view))) {
            Some((id, view)) => self.render_tab_body(id, view, cx).into_any_element(),
            None => self.render_empty(cx).into_any_element(),
        };
        let (background, foreground) = {
            let theme = cx.theme();
            (theme.background, theme.foreground)
        };

        div()
            .v_flex()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(strip)
            .child(body)
    }
}
