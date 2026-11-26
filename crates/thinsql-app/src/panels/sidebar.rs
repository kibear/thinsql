//! 左侧单栏：搜索框 + 连接树 + 历史切换。
//!
//! 树点击表节点直接在工作区开标签；连接管理全部走右键菜单，
//! 不再占用独立的右侧面板。

use gpui_kit::base::StyledExt;
use gpui_kit::component::Selectable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::tree::{Tree, TreeEvent, TreeItem, TreeState};
use gpui_kit::*;

use crate::i18n::{t, tr};
use crate::panels::connection_dialog::{open_connection_dialog, open_delete_connection_dialog};
use crate::panels::tree_model::{NodeRef, build_items, object_node_id, parse_node};
use crate::panels::workspace::WorkspacePanel;
use crate::state::{AppState, ConnStatus, QueryTab};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarMode {
    Schema,
    History,
}

pub struct SidebarPanel {
    state: Entity<AppState>,
    workspace: Entity<WorkspacePanel>,
    tree: Entity<TreeState>,
    search: Entity<InputState>,
    mode: SidebarMode,
    _subs: Vec<Subscription>,
}

impl SidebarPanel {
    pub fn new(
        state: Entity<AppState>,
        workspace: Entity<WorkspacePanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx).items(Vec::<TreeItem>::new()));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t("搜索连接或表")));

        let sub_tree = cx.subscribe(&tree, |this, _tree, event: &TreeEvent, cx| {
            let (id, expanded) = match event {
                TreeEvent::Expanded(id) => (id.to_string(), true),
                TreeEvent::Collapsed(id) => (id.to_string(), false),
            };
            this.state.update(cx, |state, cx| {
                state.toggle_node(&id, expanded);
                cx.notify();
            });
        });

        let sub_state = cx.observe(&state, |this, _state, cx| {
            this.sync(cx);
            cx.notify();
        });

        let sub_search = cx.subscribe(&search, |this, _search, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.sync(cx);
                cx.notify();
            }
        });

        let mut this = Self {
            state,
            workspace,
            tree,
            search,
            mode: SidebarMode::Schema,
            _subs: vec![sub_tree, sub_state, sub_search],
        };
        this.sync(cx);
        this
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let (items, select_id) = {
            let state = self.state.read(cx);
            let query = self.search.read(cx).value().to_string();
            let items = build_items(&state.conns, &state.expanded, &query);
            // 优先树点击选中；否则回退到活动表标签
            let select_id = state
                .tree_selection
                .clone()
                .or_else(|| state.active().and_then(active_table_node_id));
            (items, select_id)
        };
        self.tree.update(cx, |tree, cx| {
            tree.set_items(items, cx);
            // set_items 会清空选中；按 tree_selection / 活动表恢复
            if let Some(id) = select_id {
                let id: SharedString = id.into();
                if let Some(ix) = tree.index_of(&id) {
                    tree.set_selected_index(Some(ix), cx);
                }
            }
        });
    }

    fn conn_status_color(
        status: &ConnStatus,
        theme: &gpui_kit::component::theme::Theme,
    ) -> gpui_kit::Hsla {
        match status {
            ConnStatus::Connected => theme.success,
            ConnStatus::Connecting => theme.warning,
            ConnStatus::Failed(_) => theme.danger,
            ConnStatus::Idle => theme.muted_foreground,
        }
    }

    fn render_tree(&mut self, _cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.clone();
        let workspace = self.workspace.clone();
        let menu_state = self.state.clone();
        let menu_workspace = self.workspace.clone();

        Tree::new(&self.tree, move |_ix, entry, _selected, _window, cx| {
            let id = entry.item().id.to_string();
            let node = parse_node(&id);
            let workspace = workspace.clone();
            let theme = cx.theme();
            let app = state.read(cx);
            let conn_status = match &node {
                Some(NodeRef::Conn(conn_id)) => app.entry(conn_id).map(|entry| entry.status.clone()),
                _ => None,
            };
            let (active_conn, active_schema) = active_schema_marker(app.active());

            let mut label = div().text_sm().child(entry.item().label.clone());
            if !matches!(node, Some(NodeRef::Conn(_))) {
                label = label.text_color(theme.muted_foreground);
            }

            // 树只提供扁平行，缩进与展开指示由渲染方负责
            let chevron = if entry.is_folder() {
                if entry.is_expanded() { "▾" } else { "▸" }
            } else {
                ""
            };
            let mut row = div()
                .h_flex()
                .items_center()
                .gap_2()
                .pl(px(12. * entry.depth() as f32))
                .child(
                    div()
                        .w_3()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(chevron),
                );
            if let Some(status) = conn_status {
                let color = Self::conn_status_color(&status, theme);
                row = row
                    .child(div().w_2().h_2().rounded_full().bg(color))
                    .child(label);
            } else if matches!(
                &node,
                Some(NodeRef::Schema { conn, schema })
                    if active_conn.as_deref() == Some(conn.as_str())
                        && active_schema.as_deref() == Some(schema.as_str())
            ) {
                // 当前库：与已连接相同的绿色点
                row = row
                    .child(div().w_2().h_2().rounded_full().bg(theme.success))
                    .child(label);
            } else {
                row = row.child(label);
            }

            let mut item = ListItem::new(format!("sidebar-node-{id}"));
            item = item.child(row);

            let state_click = state.clone();
            item.on_click(move |_, window, app| {
                let clicked = id.clone();
                state_click.update(app, |s, cx| {
                    s.set_tree_selection(Some(clicked));
                    cx.notify();
                });
                match node.clone() {
                    Some(NodeRef::Object {
                        conn,
                        schema,
                        kind,
                        name,
                    }) if kind.is_relation() => {
                        let table = thinsql_core::model::TableRef::new(Some(schema), name);
                        workspace.update(app, |ws, cx| {
                            ws.open_table_tab(&conn, table, window, cx);
                        });
                    }
                    _ => {}
                }
            })
        })
        .context_menu(move |_ix, entry, menu, window, cx| {
            let id = entry.item().id.to_string();
            let Some(node) = parse_node(&id) else {
                return menu;
            };
            build_context_menu(
                node,
                menu,
                menu_state.clone(),
                menu_workspace.clone(),
                window,
                cx,
            )
        })
    }

    fn render_history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let history = self
            .state
            .read(cx)
            .history
            .recent(200)
            .cloned()
            .collect::<Vec<_>>();

        let mut list = div().id("history-list").v_flex().gap_0p5().px_2().py_1();

        if history.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t("还没有执行过查询")),
            );
        }

        for (ix, entry) in history.into_iter().enumerate() {
            // 单行摘要：把换行压成空格，避免一条历史占好几行
            let one_line = entry.sql.split_whitespace().collect::<Vec<_>>().join(" ");
            let preview = if one_line.chars().count() > 60 {
                format!("{}…", one_line.chars().take(60).collect::<String>())
            } else {
                one_line
            };
            let meta = if entry.failed {
                t("失败").to_string()
            } else {
                tr(
                    "{n} 行 · {ms}ms",
                    &[("n", &entry.row_count.to_string()), ("ms", &entry.elapsed_ms.to_string())],
                )
            };
            let sql = entry.sql.clone();

            list = list.child(
                div()
                    // on_click 属于 StatefulInteractiveElement，必须先给元素一个 id
                    .id(("history-entry", ix))
                    .v_flex()
                    .gap_0p5()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .hover(|style| style.bg(theme.muted))
                    .child(div().text_xs().child(preview))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(meta),
                    )
                    .on_click(move |_, _, app| {
                        app.write_to_clipboard(ClipboardItem::new_string(sql.clone()));
                    }),
            );
        }

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t("点击一条复制 SQL")),
            )
            .child(
                div()
                    .id("history-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
    }
}

impl Render for SidebarPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode_switch = div()
            .h_flex()
            .gap_1()
            .child(
                Button::new("mode-schema")
                    .ghost()
                    .selected(self.mode == SidebarMode::Schema)
                    .label(t("结构"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mode = SidebarMode::Schema;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("mode-history")
                    .ghost()
                    .selected(self.mode == SidebarMode::History)
                    .label(t("历史"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mode = SidebarMode::History;
                        cx.notify();
                    })),
            );

        let content = match self.mode {
            SidebarMode::Schema => div()
                .flex_1()
                .min_h_0()
                .child(self.render_tree(cx))
                .into_any_element(),
            SidebarMode::History => self.render_history(cx).into_any_element(),
        };
        let (background, foreground, border) = {
            let theme = cx.theme();
            (theme.background, theme.foreground, theme.border)
        };

        div()
            .v_flex()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .px_2()
                    .py_2()
                    .border_b_1()
                    .border_color(border)
                    .child(Input::new(&self.search))
                    .child(mode_switch),
            )
            .child(content)
    }
}


/// 活动标签对应的表节点 id（数据表或结构表）。
fn active_table_node_id(tab: &QueryTab) -> Option<String> {
    let conn = tab.conn_id.as_deref()?;
    let table = tab.table.as_ref().or(tab.structure.as_ref())?;
    let schema = table.schema.as_deref().unwrap_or_default();
    Some(object_node_id(
        conn,
        schema,
        thinsql_core::model::ObjectKind::Table,
        &table.name,
    ))
}

/// 活动标签的当前库：(conn_id, schema)。
fn active_schema_marker(tab: Option<&QueryTab>) -> (Option<String>, Option<String>) {
    let Some(tab) = tab else {
        return (None, None);
    };
    let schema = tab
        .table
        .as_ref()
        .and_then(|t| t.schema.clone())
        .or_else(|| tab.structure.as_ref().and_then(|t| t.schema.clone()))
        .or_else(|| tab.schema.clone());
    (tab.conn_id.clone(), schema)
}

/// 连接树右键菜单：按节点类型给出不同命令。
fn build_context_menu(
    node: NodeRef,
    mut menu: PopupMenu,
    state: Entity<AppState>,
    workspace: Entity<WorkspacePanel>,
    _window: &mut Window,
    cx: &mut Context<TreeState>,
) -> PopupMenu {
    match node {
        NodeRef::Conn(conn_id) => {
            let (connected, read_only) = state
                .read(cx)
                .entry(&conn_id)
                .map(|entry| {
                    (
                        matches!(entry.status, ConnStatus::Connected),
                        entry.cfg.read_only,
                    )
                })
                .unwrap_or((false, false));

            menu = menu.item(
                PopupMenuItem::new(if connected { t("断开连接") } else { t("连接") }).on_click({
                    let state = state.clone();
                    let id = conn_id.clone();
                    move |_, _, app| {
                        let id = id.clone();
                        state.update(app, |state, cx| {
                            if connected {
                                state.disconnect(&id);
                            } else {
                                state.connect(&id);
                            }
                            cx.notify();
                        });
                    }
                }),
            );
            menu = menu.item(PopupMenuItem::new(t("新建查询")).on_click({
                let workspace = workspace.clone();
                let id = conn_id.clone();
                move |_, window, app| {
                    let id = id.clone();
                    workspace.update(app, |ws, cx| ws.open_query_tab_on(id, None, window, cx));
                }
            }));
            menu = menu.item(PopupMenuItem::new(t("刷新")).on_click({
                let state = state.clone();
                let id = conn_id.clone();
                move |_, _, app| {
                    let id = id.clone();
                    state.update(app, |state, cx| {
                        state.refresh_connection(&id);
                        cx.notify();
                    });
                }
            }));
            menu = menu.item(PopupMenuItem::separator());
            menu = menu.item(PopupMenuItem::new(t("编辑连接…")).on_click({
                let state = state.clone();
                let id = conn_id.clone();
                move |_, window, app| {
                    open_connection_dialog(state.clone(), Some(id.clone()), window, app);
                }
            }));
            menu = menu.item(PopupMenuItem::new(t("只读模式")).checked(read_only).on_click({
                let state = state.clone();
                let id = conn_id.clone();
                move |_, _, app| {
                    let id = id.clone();
                    state.update(app, |state, cx| {
                        state.toggle_read_only(&id);
                        cx.notify();
                    });
                }
            }));
            menu = menu.item(PopupMenuItem::separator());
            menu = menu.item(PopupMenuItem::new(t("删除连接…")).on_click({
                let state = state.clone();
                let id = conn_id.clone();
                move |_, window, app| {
                    open_delete_connection_dialog(state.clone(), id.clone(), window, app);
                }
            }));
            menu
        }
        NodeRef::Schema { conn, schema } => {
            menu = menu.item(PopupMenuItem::new(t("新建查询")).on_click({
                let workspace = workspace.clone();
                let conn = conn.clone();
                let schema = schema.clone();
                move |_, window, app| {
                    let conn = conn.clone();
                    let schema = schema.clone();
                    workspace.update(app, |ws, cx| {
                        ws.open_query_tab_on(conn, Some(schema), window, cx)
                    });
                }
            }));
            menu = menu.item(PopupMenuItem::new(t("刷新库列表")).on_click({
                let state = state.clone();
                move |_, _, app| {
                    let conn = conn.clone();
                    state.update(app, |state, cx| {
                        state.refresh_connection(&conn);
                        cx.notify();
                    });
                }
            }));
            menu
        }
        NodeRef::Kind { .. } => menu,
        NodeRef::Object {
            conn,
            schema,
            kind,
            name,
        } => {
            let display = name.clone();
            menu = menu.item(PopupMenuItem::new(t("复制名称")).on_click({
                let display = display.clone();
                move |_, _, app| {
                    app.write_to_clipboard(ClipboardItem::new_string(display.clone()));
                }
            }));
            if kind.is_relation() {
                let table = thinsql_core::model::TableRef::new(Some(schema.clone()), name.clone());
                menu = menu.item(PopupMenuItem::new(t("打开数据")).on_click({
                    let workspace = workspace.clone();
                    let conn = conn.clone();
                    let table = table.clone();
                    move |_, window, app| {
                        let conn = conn.clone();
                        let table = table.clone();
                        workspace.update(app, |ws, cx| {
                            ws.open_table_tab(&conn, table, window, cx)
                        });
                    }
                }));
                if kind == thinsql_core::model::ObjectKind::Table {
                    menu = menu.item(PopupMenuItem::new(t("修改表结构")).on_click({
                        let workspace = workspace.clone();
                        let conn = conn.clone();
                        let table = table.clone();
                        move |_, window, app| {
                            let conn = conn.clone();
                            let table = table.clone();
                            workspace.update(app, |ws, cx| {
                                ws.open_structure_tab(&conn, table, window, cx);
                            });
                        }
                    }));
                }
            }
            menu
        }
    }
}
