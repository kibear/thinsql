//! 按标签的结果视图：数据网格、语句执行摘要、单元格编辑与导出。
//!
//! 每个标签一个实例，持有自己的表格状态与编辑上下文；数据用 Arc 共享，
//! 版本号变化时才把结果交给表格代理，不在每帧克隆行数据。

use std::sync::Arc;

use gpui_kit::base::StyledExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use thinsql_core::Value;
use thinsql_core::edit::locate_key;
use thinsql_core::model::{QueryResult, ResultColumn};
use thinsql_core::value::value_from_input;

use crate::i18n::{t, tr};
use crate::state::{
    AppState, EditTarget, ExportFormat, RowPendingKind, StatementStatus, TabId, pending_cell_value,
    row_pending_kind,
};

pub struct ResultDelegate {
    state: Entity<AppState>,
    tab: TabId,
    result: Option<Arc<QueryResult>>,
    summary: String,
    editing_cell: Option<(usize, usize)>,
    cell_input: Option<Entity<InputState>>,
}

impl ResultDelegate {
    fn new(state: Entity<AppState>, tab: TabId) -> Self {
        Self {
            state,
            tab,
            result: None,
            summary: t("尚无结果").into(),
            editing_cell: None,
            cell_input: None,
        }
    }

    fn load(&mut self, result: Option<Arc<QueryResult>>) {
        self.summary = match result.as_ref() {
            None => t("尚无结果").into(),
            Some(result) => match result.affected_rows {
                Some(affected) => tr(
                    "影响 {affected} 行 · {ms}ms",
                    &[
                        ("affected", &affected.to_string()),
                        ("ms", &result.elapsed.as_millis().to_string()),
                    ],
                ),
                None => {
                    let more = if result.has_more { t("（还有更多）") } else { "" };
                    tr(
                        "{n} 行{more} · {ms}ms",
                        &[
                            ("n", &result.row_count().to_string()),
                            ("more", more),
                            ("ms", &result.elapsed.as_millis().to_string()),
                        ],
                    )
                }
            },
        };
        self.result = result;
    }

    fn result(&self) -> Option<&QueryResult> {
        self.result.as_deref()
    }

    fn pending_kind_for_row(&self, row_ix: usize, cx: &App) -> Option<RowPendingKind> {
        let state = self.state.read(cx);
        let tab = state.tab(self.tab)?;
        if tab.pending_changes.is_empty() || tab.current_pk.is_empty() {
            return None;
        }
        let result = self.result()?;
        let row = result.rows.get(row_ix)?;
        let key = locate_key(&tab.current_pk, &result.columns, row).ok()?;
        row_pending_kind(&tab.pending_changes, &key)
    }
}

impl TableDelegate for ResultDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.result().map(|r| r.columns.len()).unwrap_or(0)
    }

    fn rows_count(&self, _: &App) -> usize {
        self.result().map(|r| r.rows.len()).unwrap_or(0)
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let name = self
            .result()
            .and_then(|r| r.columns.get(col_ix))
            .map(|c: &ResultColumn| c.name.clone())
            .unwrap_or_default();
        Column::new(format!("c{col_ix}"), name)
            .width(px(160.))
            .resizable(true)
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let kind = self.pending_kind_for_row(row_ix, cx);
        let mut row = div().id(("row", row_ix));
        match kind {
            Some(RowPendingKind::Deleted) => {
                row = row
                    .bg(cx.theme().danger.opacity(0.12))
                    .text_color(cx.theme().muted_foreground);
            }
            Some(RowPendingKind::Modified) => {
                row = row.bg(cx.theme().warning.opacity(0.12));
            }
            None => {}
        }
        row
    }

    /// 数据网格行右键：删除整行。删除先进暂存区，与单元格修改共用
    /// 同一份 SQL 预览与单事务提交流程。
    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let state = self.state.clone();
        let tab = self.tab;
        menu.item(PopupMenuItem::new(t("删除行")).on_click(move |_, _, app| {
            // start_edit 只做前置校验与主键定位，这里借用它的 key，不进入编辑态
            let outcome = state.update(app, |state, cx| {
                let result = state.start_edit(tab, row_ix, 0).map(|target| target.key);
                cx.notify();
                result
            });
            match outcome {
                Ok(key) => state.update(app, |state, cx| {
                    state.stage_delete(tab, key);
                    cx.notify();
                }),
                Err(message) => state.update(app, |state, cx| {
                    state.set_tab_status(tab, message);
                    cx.notify();
                }),
            }
        }))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if self.editing_cell == Some((row_ix, col_ix))
            && let Some(input) = self.cell_input.as_ref()
        {
            return div()
                .px_1()
                .min_w_0()
                .child(Input::new(input).appearance(false))
                .into_any_element();
        }

        let value = self.display_value(row_ix, col_ix, cx);
        let is_null = value.is_null();
        let text = value.display();

        let mut cell = div().px_2().min_w_0().child(text);
        if is_null {
            // NULL 用弱化色区分，避免和字面量字符串 "NULL" 混淆
            cell = cell.text_color(cx.theme().muted_foreground);
        }
        cell.into_any_element()
    }
}

impl ResultDelegate {
    fn display_value(&self, row_ix: usize, col_ix: usize, cx: &App) -> Value {
        let result = match self.result() {
            Some(result) => result,
            None => return Value::Null,
        };
        let original = result
            .rows
            .get(row_ix)
            .and_then(|row| row.get(col_ix))
            .cloned()
            .unwrap_or(Value::Null);
        let state = self.state.read(cx);
        let Some(tab) = state.tab(self.tab) else {
            return original;
        };
        if tab.pending_changes.is_empty() || tab.current_pk.is_empty() {
            return original;
        }
        let Some(column) = result.columns.get(col_ix) else {
            return original;
        };
        let Some(row) = result.rows.get(row_ix) else {
            return original;
        };
        let Ok(key) = locate_key(&tab.current_pk, &result.columns, row) else {
            return original;
        };
        pending_cell_value(&tab.pending_changes, &key, &column.name).unwrap_or(original)
    }
}

pub struct ResultsView {
    state: Entity<AppState>,
    tab: TabId,
    table: Entity<TableState<ResultDelegate>>,
    loaded_rev: u64,
    loaded_pending: usize,
    /// 双击事件里没有 window，先记下请求，等下一次 render 再处理
    edit_request: Option<(usize, usize)>,
    editing: Option<EditTarget>,
    /// 收下编辑后卸掉单元格 Input 会触发 Blur，用它避免二次提交。
    suppress_cell_blur: bool,
    cell_input: Entity<InputState>,
    filter_input: Entity<InputState>,
    show_preview: bool,
    _subs: Vec<Subscription>,
}

impl ResultsView {
    pub fn new(
        state: Entity<AppState>,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| {
            TableState::new(ResultDelegate::new(state.clone(), tab), window, cx)
                .cell_selectable(true)
        });
        let sub_state = cx.observe(&state, |this, _state, cx| {
            this.sync(cx);
            cx.notify();
        });

        let cell_input = cx.new(|cx| InputState::new(window, cx).placeholder(t("新值")));
        let initial_filter = state
            .read(cx)
            .tab(tab)
            .map(|t| t.data_filter.clone())
            .unwrap_or_default();
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t("筛选条件，例如 id > 10"))
                .default_value(initial_filter)
        });
        let sub_table = cx.subscribe(&table, |this, _table, event: &TableEvent, cx| {
            if let TableEvent::DoubleClickedCell(row, col) = event {
                this.edit_request = Some((*row, *col));
                cx.notify();
            }
        });
        let sub_cell = cx.subscribe(&cell_input, |this, _input, event: &InputEvent, cx| {
            match event {
                InputEvent::PressEnter { .. } => this.accept_edit(cx),
                InputEvent::Blur => {
                    if this.suppress_cell_blur {
                        this.suppress_cell_blur = false;
                        return;
                    }
                    this.accept_edit(cx);
                }
                _ => {}
            }
        });

        Self {
            state,
            tab,
            table,
            loaded_rev: 0,
            loaded_pending: 0,
            edit_request: None,
            editing: None,
            suppress_cell_blur: false,
            cell_input,
            filter_input,
            show_preview: false,
            _subs: vec![sub_state, sub_table, sub_cell],
        }
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let (rev, result, pending) = {
            let state = self.state.read(cx);
            let Some(tab) = state.tab(self.tab) else {
                return;
            };
            (tab.result_rev, tab.result.clone(), tab.pending_changes.len())
        };
        if rev == self.loaded_rev && pending == self.loaded_pending {
            return;
        }
        let result_changed = rev != self.loaded_rev;
        self.loaded_rev = rev;
        self.loaded_pending = pending;
        if result_changed {
            self.editing = None;
        }
        let editing_cell = self.editing.as_ref().map(|t| (t.row, t.col));
        let cell_input = editing_cell.map(|_| self.cell_input.clone());
        self.table.update(cx, |table, cx| {
            if result_changed {
                table.delegate_mut().load(result);
            }
            let delegate = table.delegate_mut();
            delegate.editing_cell = editing_cell;
            delegate.cell_input = cell_input;
            table.refresh(cx);
        });
    }

    /// 处理挂起的双击请求：这里才有 window 可以设置输入框内容与焦点。
    fn tick_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, col)) = self.edit_request.take() else {
            return;
        };
        // 换格编辑前先收下当前格，避免丢改。
        if self.editing.is_some() {
            self.accept_edit(cx);
        }

        let outcome = self.state.update(cx, |state, cx| {
            let result = state.start_edit(self.tab, row, col);
            cx.notify();
            result
        });

        match outcome {
            Ok(target) => {
                let text = {
                    let state = self.state.read(cx);
                    state
                        .tab(self.tab)
                        .and_then(|tab| {
                            pending_cell_value(
                                &tab.pending_changes,
                                &target.key,
                                &target.column_name,
                            )
                        })
                        .map(|v| v.display())
                        .unwrap_or_else(|| target.original.display())
                };
                self.cell_input.update(cx, |input, cx| {
                    input.set_value(text, window, cx);
                    input.focus(window, cx);
                });
                self.editing = Some(target.clone());
                let cell_input = self.cell_input.clone();
                self.table.update(cx, |table, cx| {
                    let delegate = table.delegate_mut();
                    delegate.editing_cell = Some((target.row, target.col));
                    delegate.cell_input = Some(cell_input);
                    table.refresh(cx);
                });
            }
            Err(message) => {
                self.state.update(cx, |state, cx| {
                    state.set_tab_status(self.tab, message);
                    cx.notify();
                });
            }
        }
    }

    fn accept_edit(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.editing.take() else {
            return;
        };
        self.suppress_cell_blur = true;
        let text = self.cell_input.read(cx).value().to_string();
        let value = value_from_input(&target.type_name, &text);
        self.state.update(cx, |state, cx| {
            state.stage_change(self.tab, &target, value);
            cx.notify();
        });
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.editing_cell = None;
            delegate.cell_input = None;
            table.refresh(cx);
        });
        cx.notify();
    }

    fn export_menu(
        state: Entity<AppState>,
        tab: TabId,
    ) -> impl Fn(
        gpui_kit::component::menu::PopupMenu,
        &mut Window,
        &mut Context<gpui_kit::component::menu::PopupMenu>,
    ) -> gpui_kit::component::menu::PopupMenu {
        move |menu, _, _| {
            let mut menu = menu;
            for (label, format) in [
                ("导出 CSV", ExportFormat::Csv),
                ("导出 JSON", ExportFormat::Json),
                ("导出 INSERT", ExportFormat::Insert),
            ] {
                let label = t(label);
                let state = state.clone();
                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, app| {
                    state.update(app, |state, cx| {
                        state.export(tab, format, cx);
                        cx.notify();
                    });
                }));
            }
            menu
        }
    }
}

impl Render for ResultsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick_edit(window, cx);
        let theme = cx.theme();
        let summary = self.table.read(cx).delegate().summary.clone();
        let has_more = self
            .table
            .read(cx)
            .delegate()
            .result()
            .map(|r| r.has_more)
            .unwrap_or(false);
        let is_table_tab = self
            .state
            .read(cx)
            .tab(self.tab)
            .is_some_and(|tab| tab.is_table_tab());

        div()
            .v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .when(is_table_tab, |bar| {
                        bar.child(
                            div()
                                .w(px(280.))
                                .min_w_0()
                                .child(Input::new(&self.filter_input)),
                        )
                        .child(
                            Button::new(("filter-apply", self.tab))
                                .label(t("应用"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let filter = this.filter_input.read(cx).value().to_string();
                                    this.state.update(cx, |state, cx| {
                                        state.apply_table_filter(this.tab, filter);
                                        cx.notify();
                                    });
                                })),
                        )
                        .child(
                            Button::new(("filter-clear", self.tab))
                                .label(t("清除"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let pending = this.state.read(cx).tab(this.tab).is_some_and(
                                        |tab| !tab.pending_changes.is_empty(),
                                    );
                                    this.state.update(cx, |state, cx| {
                                        state.apply_table_filter(this.tab, String::new());
                                        cx.notify();
                                    });
                                    if !pending {
                                        this.filter_input.update(cx, |input, cx| {
                                            input.set_value("", window, cx);
                                        });
                                    }
                                })),
                        )
                        .child(
                            Button::new(("table-refresh", self.tab))
                                .label(t("刷新"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.state.update(cx, |state, cx| {
                                        state.refresh_table_data(this.tab);
                                        cx.notify();
                                    });
                                })),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(summary),
                    )
                    .when(has_more, |bar| {
                        let state = self.state.clone();
                        let tab = self.tab;
                        bar.child(Button::new(("load-more", tab)).label(t("加载更多")).on_click(
                            move |_, _, app| {
                                state.update(app, |s, cx| {
                                    s.load_more(tab);
                                    cx.notify();
                                });
                            },
                        ))
                    })
                    .child(
                        Button::new(("export", self.tab))
                            .label(t("导出"))
                            .dropdown_menu(Self::export_menu(self.state.clone(), self.tab)),
                    ),
            )
            .child(self.render_pending_bar(cx))
            .child(self.render_statements(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.table).stripe(true)),
            )
    }
}

impl ResultsView {
    fn render_statements(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let logs = {
            let state = self.state.read(cx);
            match state.tab(self.tab) {
                Some(tab) if tab.summary_mode && !tab.statements.is_empty() => {
                    tab.statements.clone()
                }
                _ => Vec::new(),
            }
        };
        if logs.is_empty() {
            return div().into_any_element();
        }

        let mut list = div().v_flex();
        list = list.child(statement_row(
            "#".into(),
            t("状态").into(),
            t("返回行").into(),
            t("影响行").into(),
            t("耗时").into(),
            theme.muted_foreground,
        ));
        for log in logs {
            let color = match log.status {
                StatementStatus::Failed(_) => theme.danger,
                StatementStatus::Running => theme.primary,
                StatementStatus::Succeeded => theme.foreground,
                _ => theme.muted_foreground,
            };
            let rows = match log.affected {
                Some(_) => t("未返回结果集").to_string(),
                None => log.rows.to_string(),
            };
            let affected = log
                .affected
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into());
            let elapsed = if matches!(
                log.status,
                StatementStatus::Pending | StatementStatus::Skipped
            ) {
                "-".into()
            } else {
                format!("{}ms", log.elapsed_ms)
            };
            list = list.child(statement_row(
                format!("#{}", log.index + 1),
                log.status.label().to_string(),
                rows,
                affected,
                elapsed,
                color,
            ));
        }

        div()
            .id("statement-list")
            .v_flex()
            .border_b_1()
            .border_color(theme.border)
            .max_h_40()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }

    fn render_pending_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (count, sqls) = {
            let state = self.state.read(cx);
            match state.tab(self.tab) {
                Some(tab) => (tab.pending_changes.len(), state.pending_sql(self.tab)),
                None => (0, Vec::new()),
            }
        };
        if count == 0 {
            return div().into_any_element();
        }

        let commit_state = self.state.clone();
        let discard_state = self.state.clone();
        let tab = self.tab;
        let mut bar = div()
            .v_flex()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .child(tr("待提交 {n} 处改动", &[("n", &count.to_string())])),
                    )
                    .child(
                        Button::new("toggle-preview")
                            .label("SQL")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_preview = !this.show_preview;
                                cx.notify();
                            })),
                    )
                    .child(Button::new("commit").primary().label(t("提交")).on_click(
                        move |_, _, app| {
                            commit_state.update(app, |state, cx| {
                                state.commit_changes(tab);
                                cx.notify();
                            });
                        },
                    ))
                    .child(
                        Button::new("discard")
                            .label(t("回滚"))
                            .on_click(move |_, _, app| {
                                discard_state.update(app, |state, cx| {
                                    state.discard_changes(tab);
                                    cx.notify();
                                });
                            }),
                    ),
            );

        if self.show_preview {
            let text = sqls.join("\n");
            bar = bar.child(
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(text),
            );
        }
        bar.into_any_element()
    }
}

/// 语句摘要的一行：#、状态、返回行、影响行、耗时共用固定列宽，
/// 让多行之间可以纵向对比。
fn statement_row(
    index: String,
    status: String,
    rows: String,
    affected: String,
    elapsed: String,
    color: gpui_kit::Hsla,
) -> impl IntoElement {
    div()
        .h_flex()
        .gap_2()
        .px_2()
        .py_0p5()
        .text_xs()
        .text_color(color)
        .child(div().w_8().child(index))
        .child(div().w_16().child(status))
        .child(div().flex_1().child(rows))
        .child(div().w_16().child(affected))
        .child(div().w_16().text_right().child(elapsed))
}
