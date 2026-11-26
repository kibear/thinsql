//! 表结构编辑视图：Columns / Indexes / DDL 三页签。
//!
//! 所有改动先在 AppState 里暂存成 StructureChange，底部预览由驱动的
//! ALTER 生成器提供；真正执行仍走会话线程，与预览共用同一份 SQL。

use gpui_kit::base::StyledExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::Disableable;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::IndexPath;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::*;
use thinsql_core::{
    common_charsets, common_collations, common_column_types, default_column_type,
    default_length_for_type, type_allows_auto_increment, type_uses_charset, type_uses_length,
};
use thinsql_core::model::{ColumnSpec, DriverKind, IndexMeta};

use crate::i18n::{t, tr};
use crate::state::{AppState, TabId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Ddl,
    Columns,
    Indexes,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Self::Ddl => "DDL",
            Self::Columns => "Columns",
            Self::Indexes => "Indexes",
        }
    }
}

fn column_header(col_ix: usize) -> &'static str {
    match col_ix {
        0 => "#",
        1 => "Column",
        2 => "Type",
        3 => "Length",
        4 => "Nullable",
        5 => "Primary Key",
        6 => "Default",
        7 => "Comment",
        8 => "Charset",
        9 => "Collation",
        _ => "Auto Inc",
    }
}

fn has_duplicate_column(columns: &[String]) -> bool {
    columns
        .iter()
        .enumerate()
        .any(|(ix, column)| columns[..ix].contains(column))
}

/// 列网格代理：只负责展示工作副本，编辑仍由 StructureView 的输入条完成。
struct StructureDelegate {
    state: Entity<AppState>,
    tab: TabId,
    specs: Vec<ColumnSpec>,
}

impl TableDelegate for StructureDelegate {
    fn columns_count(&self, _: &App) -> usize {
        11
    }

    fn rows_count(&self, _: &App) -> usize {
        self.specs.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let (id, width) = match col_ix {
            0 => ("no", px(44.)),
            1 => ("name", px(160.)),
            2 => ("type", px(110.)),
            3 => ("length", px(80.)),
            4 => ("nullable", px(80.)),
            5 => ("pk", px(96.)),
            6 => ("default", px(120.)),
            7 => ("comment", px(180.)),
            8 => ("charset", px(110.)),
            9 => ("collation", px(150.)),
            _ => ("auto", px(80.)),
        };
        Column::new(id, t(column_header(col_ix)))
            .width(width)
            .resizable(true)
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let state = self.state.clone();
        let tab = self.tab;
        menu.item(
            PopupMenuItem::new(t("删除列")).on_click(move |_, _, app| {
                state.update(app, |state, cx| {
                    state.structure_remove_column(tab, row_ix);
                    cx.notify();
                });
            }),
        )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let text = self
            .specs
            .get(row_ix)
            .map(|spec| match col_ix {
                0 => (row_ix + 1).to_string(),
                1 => spec.name.clone(),
                2 => spec.data_type.clone(),
                3 => spec.length.clone().unwrap_or_default(),
                4 => if spec.nullable { "YES".into() } else { "NO".into() },
                5 => if spec.primary_key { "YES".into() } else { "NO".into() },
                6 => spec.default.clone().unwrap_or_default(),
                7 => spec.comment.clone().unwrap_or_default(),
                8 => spec.charset.clone().unwrap_or_default(),
                9 => spec.collation.clone().unwrap_or_default(),
                _ => if spec.auto_increment { "YES".into() } else { "NO".into() },
            })
            .unwrap_or_default();
        div().px_2().min_w_0().child(text)
    }
}

/// 索引字段选择行。`id` 是稳定身份，删除中间行不会改变其他行的 ElementId。
struct IndexColumnField {
    id: u64,
    select: Entity<SelectState<Vec<String>>>,
}

pub struct StructureView {
    state: Entity<AppState>,
    tab: TabId,
    section: Section,
    table: Entity<TableState<StructureDelegate>>,
    loaded_rev: u64,
    selected: Option<usize>,
    editing: bool,
    edit_row: Option<usize>,
    edit_request: Option<Option<usize>>,
    name_input: Entity<InputState>,
    type_select: Entity<SelectState<Vec<String>>>,
    length_input: Entity<InputState>,
    default_input: Entity<InputState>,
    comment_input: Entity<InputState>,
    charset_select: Entity<SelectState<Vec<String>>>,
    collation_select: Entity<SelectState<Vec<String>>>,
    /// 类型切换后需要在有 window 时写入 Length。
    pending_length: Option<String>,
    pending_clear_charset: bool,
    pending_refresh_collation: bool,
    nullable: bool,
    primary_key: bool,
    auto_increment: bool,
    index_name: Entity<InputState>,
    index_columns: Vec<IndexColumnField>,
    next_index_field_id: u64,
    index_column_options: Vec<String>,
    index_unique: bool,
    show_index_form: bool,
    /// None = 新建；Some(name) = 打开已有/未提交索引（真实名，可为空串）
    index_open_name: Option<String>,
    selected_index: Option<usize>,
    show_preview: bool,
    _subs: Vec<Subscription>,
}

impl StructureView {
    pub fn new(
        state: Entity<AppState>,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| {
            TableState::new(
                StructureDelegate {
                    state: state.clone(),
                    tab,
                    specs: Vec::new(),
                },
                window,
                cx,
            )
            .cell_selectable(true)
        });
        let sub_state = cx.observe(&state, |this, _, cx| {
            this.sync(cx);
            cx.notify();
        });
        let sub_table = cx.subscribe(&table, |this, _, event: &TableEvent, cx| match event {
            TableEvent::SelectRow(row) | TableEvent::SelectCell(row, _) => {
                this.selected = Some(*row);
                cx.notify();
            }
            TableEvent::DoubleClickedCell(row, _) => {
                this.edit_request = Some(Some(*row));
                cx.notify();
            }
            _ => {}
        });

        let type_select = cx.new(|cx| {
            SelectState::new(vec!["VARCHAR".into()], Some(IndexPath::new(0)), window, cx)
        });
        let charset_select =
            cx.new(|cx| SelectState::new(vec![String::new()], Some(IndexPath::new(0)), window, cx));
        let collation_select =
            cx.new(|cx| SelectState::new(vec![String::new()], Some(IndexPath::new(0)), window, cx));
        let sub_type = cx.subscribe(
            &type_select,
            |this, _, event: &SelectEvent<Vec<String>>, cx| {
                if matches!(event, SelectEvent::Confirm(_)) {
                    this.on_type_changed(cx);
                }
            },
        );
        let sub_charset = cx.subscribe(
            &charset_select,
            |this, _, event: &SelectEvent<Vec<String>>, cx| {
                if matches!(event, SelectEvent::Confirm(_)) {
                    this.on_charset_changed(cx);
                }
            },
        );

        let mut this = Self {
            state,
            tab,
            section: Section::Columns,
            table,
            loaded_rev: u64::MAX,
            selected: None,
            editing: false,
            edit_row: None,
            edit_request: None,
            name_input: cx.new(|cx| InputState::new(window, cx).placeholder(t("列名"))),
            type_select,
            length_input: cx.new(|cx| InputState::new(window, cx).placeholder(t("长度"))),
            default_input: cx.new(|cx| InputState::new(window, cx).placeholder(t("默认值"))),
            comment_input: cx.new(|cx| InputState::new(window, cx).placeholder(t("注释"))),
            charset_select,
            collation_select,
            pending_length: None,
            pending_clear_charset: false,
            pending_refresh_collation: false,
            nullable: true,
            primary_key: false,
            auto_increment: false,
            index_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("索引名"))),
            index_columns: Vec::new(),
            next_index_field_id: 0,
            index_column_options: Vec::new(),
            index_unique: false,
            show_index_form: false,
            index_open_name: None,
            selected_index: None,
            show_preview: false,
            _subs: vec![sub_state, sub_table, sub_type, sub_charset],
        };
        this.reset_index_fields(window, cx);
        this
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let (rev, specs) = {
            let state = self.state.read(cx);
            let Some(tab) = state.tab(self.tab) else { return };
            (tab.structure_rev, tab.structure_specs.clone())
        };
        if rev == self.loaded_rev {
            return;
        }
        self.loaded_rev = rev;
        if self.selected.is_some_and(|row| row >= specs.len()) {
            self.selected = None;
        }
        self.table.update(cx, |table, cx| {
            table.delegate_mut().specs = specs;
            table.refresh(cx);
        });
    }

    fn driver_kind(&self, cx: &App) -> DriverKind {
        self.state
            .read(cx)
            .tab_driver_kind(self.tab)
            .unwrap_or(DriverKind::MySql)
    }

    fn selected_type(&self, cx: &App) -> String {
        self.type_select
            .read(cx)
            .selected_value()
            .cloned()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| default_column_type(self.driver_kind(cx)).to_string())
    }

    fn selected_optional(select: &Entity<SelectState<Vec<String>>>, cx: &App) -> Option<String> {
        select
            .read(cx)
            .selected_value()
            .cloned()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    fn fill_select(
        select: &Entity<SelectState<Vec<String>>>,
        options: Vec<String>,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut options = options;
        if !current.is_empty()
            && !options
                .iter()
                .any(|item| item.eq_ignore_ascii_case(current))
        {
            options.insert(1.min(options.len()), current.to_string());
        }
        let selected = options
            .iter()
            .position(|item| item.eq_ignore_ascii_case(current))
            .unwrap_or(0);
        select.update(cx, |select, cx| {
            select.set_items(options, window, cx);
            select.set_selected_index(Some(IndexPath::new(selected)), window, cx);
        });
    }

    fn charset_options(kind: DriverKind) -> Vec<String> {
        std::iter::once(String::new())
            .chain(common_charsets(kind).iter().map(|s| (*s).to_string()))
            .collect()
    }

    fn collation_options(kind: DriverKind, charset: &str) -> Vec<String> {
        std::iter::once(String::new())
            .chain(
                common_collations(kind, charset)
                    .iter()
                    .map(|s| (*s).to_string()),
            )
            .collect()
    }

    fn refresh_charset_collation(
        &mut self,
        charset: &str,
        collation: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = self.driver_kind(cx);
        Self::fill_select(
            &self.charset_select,
            Self::charset_options(kind),
            charset,
            window,
            cx,
        );
        Self::fill_select(
            &self.collation_select,
            Self::collation_options(kind, charset),
            collation,
            window,
            cx,
        );
    }

    fn on_type_changed(&mut self, cx: &mut Context<Self>) {
        let kind = self.driver_kind(cx);
        let data_type = self.selected_type(cx);
        let length_now = self.length_input.read(cx).value().to_string();
        let known_defaults = ["64", "1", "10,2", "16"];
        let should_autofill = length_now.trim().is_empty()
            || known_defaults
                .iter()
                .any(|d| length_now.trim() == *d);
        if should_autofill {
            let next = if type_uses_length(kind, &data_type) {
                default_length_for_type(kind, &data_type).unwrap_or("")
            } else {
                ""
            };
            // set_value needs Window; defer via edit_request-like path is awkward.
            // Store desired length in a side channel by reading through spawn...
            // Instead: use cx to get window from listener — SelectEvent handler has no window.
            // Keep length sync in render via pending flag.
            self.pending_length = Some(next.to_string());
        }
        if !type_allows_auto_increment(kind, &data_type) {
            self.auto_increment = false;
        }
        if !type_uses_charset(kind, &data_type) {
            self.pending_clear_charset = true;
        }
        cx.notify();
    }

    fn on_charset_changed(&mut self, cx: &mut Context<Self>) {
        self.pending_refresh_collation = true;
        cx.notify();
    }

    /// 双击行/点新增后才有 window，可以在这里填输入框并聚焦。
    fn tick_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self.edit_request.take() else { return };
        let spec = request.and_then(|row| {
            self.state
                .read(cx)
                .tab(self.tab)
                .and_then(|tab| tab.structure_specs.get(row).cloned())
        });
        let kind = self.driver_kind(cx);
        let default_type = default_column_type(kind);
        let default_charset = if common_charsets(kind).contains(&"utf8mb4") {
            "utf8mb4"
        } else {
            ""
        };
        let (name, data_type, length, default, comment, charset, collation) = match &spec {
            Some(spec) => (
                spec.name.as_str(),
                spec.data_type.as_str(),
                spec.length.as_deref().unwrap_or(""),
                spec.default.as_deref().unwrap_or(""),
                spec.comment.as_deref().unwrap_or(""),
                spec.charset.as_deref().unwrap_or(""),
                spec.collation.as_deref().unwrap_or(""),
            ),
            None => (
                "",
                default_type,
                default_length_for_type(kind, default_type).unwrap_or(""),
                "",
                "",
                default_charset,
                "",
            ),
        };
        for (input, value) in [
            (&self.name_input, name),
            (&self.length_input, length),
            (&self.default_input, default),
            (&self.comment_input, comment),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        let mut options: Vec<String> = common_column_types(kind)
            .iter()
            .map(|ty| (*ty).to_string())
            .collect();
        if !options.iter().any(|ty| ty.eq_ignore_ascii_case(data_type)) {
            options.insert(0, data_type.to_string());
        }
        let selected = options
            .iter()
            .position(|ty| ty.eq_ignore_ascii_case(data_type))
            .unwrap_or(0);
        self.type_select.update(cx, |select, cx| {
            select.set_items(options, window, cx);
            select.set_selected_index(Some(IndexPath::new(selected)), window, cx);
        });
        self.refresh_charset_collation(charset, collation, window, cx);
        self.nullable = spec.as_ref().is_none_or(|s| s.nullable);
        self.primary_key = spec.as_ref().is_some_and(|s| s.primary_key);
        self.auto_increment = spec.as_ref().is_some_and(|s| s.auto_increment)
            && type_allows_auto_increment(kind, data_type);
        self.pending_length = None;
        self.pending_clear_charset = false;
        self.pending_refresh_collation = false;
        self.editing = true;
        self.edit_row = request;
        cx.notify();
    }

    fn apply_pending_select_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(length) = self.pending_length.take() {
            self.length_input
                .update(cx, |input, cx| input.set_value(length, window, cx));
        }
        if self.pending_clear_charset {
            self.pending_clear_charset = false;
            self.refresh_charset_collation("", "", window, cx);
        }
        if self.pending_refresh_collation {
            self.pending_refresh_collation = false;
            let kind = self.driver_kind(cx);
            let charset = Self::selected_optional(&self.charset_select, cx).unwrap_or_default();
            let current = Self::selected_optional(&self.collation_select, cx).unwrap_or_default();
            let options = Self::collation_options(kind, &charset);
            let keep = if options.iter().any(|o| o.eq_ignore_ascii_case(&current)) {
                current
            } else {
                String::new()
            };
            Self::fill_select(&self.collation_select, options, &keep, window, cx);
        }
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let read = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let optional = |text: String| (!text.trim().is_empty()).then_some(text.trim().to_string());
        let kind = self.driver_kind(cx);
        let data_type = self.selected_type(cx);
        let uses_charset = type_uses_charset(kind, &data_type);
        let allows_auto = type_allows_auto_increment(kind, &data_type);
        let uses_length = type_uses_length(kind, &data_type);
        let mut length = optional(read(&self.length_input));
        if !uses_length {
            length = None;
        }
        let (charset, collation) = if uses_charset {
            (
                Self::selected_optional(&self.charset_select, cx),
                Self::selected_optional(&self.collation_select, cx),
            )
        } else {
            (None, None)
        };
        let spec = ColumnSpec {
            name: read(&self.name_input).trim().to_string(),
            data_type,
            length,
            nullable: self.nullable,
            primary_key: self.primary_key,
            default: optional(read(&self.default_input)),
            comment: optional(read(&self.comment_input)),
            charset,
            collation,
            auto_increment: self.auto_increment && allows_auto,
        };
        if spec.name.is_empty() {
            self.state.update(cx, |state, cx| {
                state.set_tab_status(self.tab, t("列名不能为空").into());
                cx.notify();
            });
            return;
        }
        let row = self.edit_row;
        let tab = self.tab;
        self.state.update(cx, |state, cx| {
            match row {
                Some(row) => state.structure_edit_column(tab, row, spec),
                None => state.structure_add_column(tab, spec),
            }
            cx.notify();
        });
        self.editing = false;
        self.edit_row = None;
    }

    fn index_column_names(&self, cx: &App) -> Vec<String> {
        self.state
            .read(cx)
            .tab(self.tab)
            .map(|tab| {
                tab.structure_specs
                    .iter()
                    .map(|spec| spec.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn new_index_column_field(
        &mut self,
        options: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> IndexColumnField {
        let id = self.next_index_field_id;
        self.next_index_field_id += 1;
        let select = cx.new(|cx| SelectState::new(options, None, window, cx));
        IndexColumnField { id, select }
    }

    fn reset_index_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let options = self.index_column_names(cx);
        let field = self.new_index_column_field(options.clone(), window, cx);
        self.index_columns = vec![field];
        self.index_column_options = options;
    }

    fn refresh_index_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let options = self.index_column_names(cx);
        if options == self.index_column_options {
            return;
        }
        self.index_column_options = options.clone();
        for field in &self.index_columns {
            field
                .select
                .update(cx, |select, cx| select.set_items(options.clone(), window, cx));
        }
    }

    fn add_index_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let options = self.index_column_names(cx);
        let field = self.new_index_column_field(options, window, cx);
        self.index_columns.push(field);
        cx.notify();
    }

    fn remove_index_column(&mut self, id: u64) {
        self.index_columns.retain(|field| field.id != id);
    }

    fn open_index(
        &mut self,
        index: &IndexMeta,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let options = self.index_column_names(cx);
        self.index_column_options = options.clone();
        self.index_name
            .update(cx, |input, cx| input.set_value(index.name.as_str(), window, cx));
        self.index_unique = index.unique;
        let cols = if index.columns.is_empty() {
            vec![String::new()]
        } else {
            index.columns.clone()
        };
        self.index_columns.clear();
        for col in cols {
            let field = self.new_index_column_field(options.clone(), window, cx);
            let selected = options.iter().position(|item| item == &col);
            field.select.update(cx, |select, cx| {
                if let Some(ix) = selected {
                    select.set_selected_index(Some(IndexPath::new(ix)), window, cx);
                }
            });
            self.index_columns.push(field);
        }
        self.index_open_name = Some(index.name.clone());
        self.show_index_form = true;
        cx.notify();
    }

    fn save_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.index_name.read(cx).value().to_string();
        let columns: Vec<String> = self
            .index_columns
            .iter()
            .filter_map(|field| field.select.read(cx).selected_value().cloned())
            .collect();
        if columns.is_empty() {
            self.state.update(cx, |state, cx| {
                state.set_tab_status(self.tab, t("索引至少需要一个字段").into());
                cx.notify();
            });
            return;
        }
        if has_duplicate_column(&columns) {
            self.state.update(cx, |state, cx| {
                state.set_tab_status(self.tab, t("索引字段不能重复").into());
                cx.notify();
            });
            return;
        }
        let unique = self.index_unique;
        let tab = self.tab;
        let open = self.index_open_name.clone();
        self.state.update(cx, |state, cx| {
            match open.as_deref() {
                Some(old_name) => {
                    state.structure_update_index(tab, old_name, name, columns, unique);
                }
                None => {
                    state.structure_add_index(tab, name, columns, unique);
                }
            }
            cx.notify();
        });
        self.close_index_form(window, cx);
    }

    fn delete_opened_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.index_open_name.clone() else {
            return;
        };
        let tab = self.tab;
        self.state.update(cx, |state, cx| {
            state.structure_remove_index(tab, &name);
            cx.notify();
        });
        self.close_index_form(window, cx);
    }

    fn close_index_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.index_name
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.reset_index_fields(window, cx);
        self.index_unique = false;
        self.show_index_form = false;
        self.index_open_name = None;
        cx.notify();
    }

    fn cancel_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_index_form(window, cx);
    }

    fn section_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut bar = div().h_flex().items_center().gap_1().px_2().py_1();
        for section in [Section::Ddl, Section::Columns, Section::Indexes] {
            let id = ("structure-section", section as usize);
            let active = self.section == section;
            let mut button = Button::new(id).label(t(section.label())).ghost();
            if active {
                button = button.primary();
            }
            let target = section;
            bar = bar.child(button.on_click(cx.listener(move |this, _, _, cx| {
                this.section = target;
                cx.notify();
            })));
        }
        bar.child(
            div()
                .flex_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .truncate(),
        )
    }

    fn render_columns(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (read_only, loading, row_count) = {
            let state = self.state.read(cx);
            let tab = state.tab(self.tab);
            (
                state.is_structure_read_only(self.tab),
                tab.is_some_and(|tab| tab.structure_loading),
                tab.map(|tab| tab.structure_specs.len()).unwrap_or(0),
            )
        };
        let has_selection = self.selected.is_some();
        let mut bar = div().h_flex().items_center().gap_2().px_2().py_1();
        if !read_only {
            bar = bar
                .child(
                    Button::new("column-add")
                        .primary()
                        .label(t("添加列"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.edit_request = Some(None);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("column-edit")
                        .label(t("编辑选中列"))
                        .disabled(!has_selection)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let row = this.selected;
                            this.edit_request = row.map(Some);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("column-remove")
                        .label(t("删除选中列"))
                        .disabled(!has_selection)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(row) = this.selected {
                                let tab = this.tab;
                                this.state.update(cx, |state, cx| {
                                    state.structure_remove_column(tab, row);
                                    cx.notify();
                                });
                            }
                        })),
                );
        }

        let message = if loading {
            t("加载表结构…")
        } else if row_count == 0 {
            t("无字段")
        } else {
            ""
        };
        let mut grid = div()
            .flex_1()
            .min_h_0()
            .border_t_1()
            .border_color(cx.theme().border);
        if message.is_empty() {
            grid = grid.child(DataTable::new(&self.table).stripe(true));
        } else {
            grid = grid.child(
                div()
                    .p_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(message),
            );
        }
        let body = div().v_flex().flex_1().min_h_0().child(bar).child(grid);
        if self.editing {
            body.child(self.render_column_editor(cx))
        } else {
            body
        }
    }

    fn render_column_editor(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let kind = self.driver_kind(cx);
        let data_type = self.selected_type(cx);
        let uses_length = type_uses_length(kind, &data_type);
        let uses_charset = type_uses_charset(kind, &data_type) && !common_charsets(kind).is_empty();
        let allows_auto = type_allows_auto_increment(kind, &data_type);
        let title: String = match self.edit_row {
            Some(row) => tr("编辑第 {row} 列", &[("row", &(row + 1).to_string())]),
            None => t("新增列").into(),
        };
        let style_input = |input: &Entity<InputState>, label: &str, disabled: bool| {
            div()
                .v_flex()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label.to_string()),
                )
                .child(Input::new(input).flex_1().min_w_0().disabled(disabled))
        };
        let style_select = |select: &Entity<SelectState<Vec<String>>>, label: &str, placeholder: &str| {
            div()
                .v_flex()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label.to_string()),
                )
                .child(
                    Select::new(select)
                        .flex_1()
                        .min_w_0()
                        .placeholder(placeholder.to_string())
                        .accessibility_label(placeholder.to_string()),
                )
        };
        let name = self.name_input.clone();
        let type_select = self.type_select.clone();
        let length = self.length_input.clone();
        let default = self.default_input.clone();
        let comment = self.comment_input.clone();
        let charset = self.charset_select.clone();
        let collation = self.collation_select.clone();

        let mut form = div()
            .v_flex()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .px_2()
            .py_2()
            .child(div().text_sm().child(title))
            .child(
                div().h_flex().gap_2().items_start().children([
                    style_input(&name, t("Column"), false).into_any_element(),
                    style_select(&type_select, t("Type"), t("类型")).into_any_element(),
                    style_input(&length, t("Length"), !uses_length).into_any_element(),
                ]),
            )
            .child(
                div().h_flex().gap_2().items_start().children([
                    style_input(&default, t("Default"), false).into_any_element(),
                    style_input(&comment, t("Comment"), false).into_any_element(),
                ]),
            );
        if uses_charset {
            form = form.child(
                div().h_flex().gap_2().items_start().children([
                    style_select(&charset, t("Charset"), t("Charset")).into_any_element(),
                    style_select(&collation, t("Collation"), t("Collation")).into_any_element(),
                ]),
            );
        }
        form.child(
            div().h_flex().gap_3().items_center().children([
                Checkbox::new("column-nullable")
                    .label(t("可空"))
                    .checked(self.nullable)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.nullable = *checked;
                        cx.notify();
                    }))
                    .into_any_element(),
                Checkbox::new("column-primary")
                    .label(t("主键"))
                    .checked(self.primary_key)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.primary_key = *checked;
                        cx.notify();
                    }))
                    .into_any_element(),
                Checkbox::new("column-auto")
                    .label(t("自增"))
                    .checked(self.auto_increment && allows_auto)
                    .disabled(!allows_auto)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.auto_increment = *checked;
                        cx.notify();
                    }))
                    .into_any_element(),
            ]),
        )
        .child(
            div().h_flex().gap_2().child(
                Button::new("column-save")
                    .primary()
                    .label(t("保存"))
                    .on_click(cx.listener(|this, _, _, cx| this.save_edit(cx))),
            )
            .child(
                Button::new("column-cancel")
                    .label(t("取消"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.editing = false;
                        this.edit_row = None;
                        cx.notify();
                    })),
            ),
        )
    }

    fn render_indexes(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (muted_foreground, border, muted) = {
            let theme = cx.theme();
            (theme.muted_foreground, theme.border, theme.muted)
        };
        let read_only = self.state.read(cx).is_structure_read_only(self.tab);
        let indexes: Vec<IndexMeta> = {
            let state = self.state.read(cx);
            let Some(tab) = state.tab(self.tab) else { return div().into_any_element() };
            let mut list: Vec<IndexMeta> = tab
                .structure_indexes
                .iter()
                .filter(|idx| {
                    !tab.structure_changes.index_ops.iter().any(|op| {
                        matches!(op, thinsql_core::IndexOp::Drop { name } if name == &idx.name)
                    })
                })
                .cloned()
                .collect();
            for op in &tab.structure_changes.index_ops {
                if let thinsql_core::IndexOp::Create {
                    name,
                    columns,
                    unique,
                } = op
                {
                    // 保留真实 name（可为空），展示层再替换「（自动命名）」
                    list.push(IndexMeta {
                        name: name.clone(),
                        columns: columns.clone(),
                        unique: *unique,
                    });
                }
            }
            list
        };
        if self.selected_index.is_some_and(|ix| ix >= indexes.len()) {
            self.selected_index = None;
        }

        let mut body = div().v_flex().flex_1().min_h_0();
        if !read_only {
            body = body.child(
                div().h_flex().gap_2().px_2().py_1().child(
                    Button::new("index-add")
                        .primary()
                        .label(t("新建索引"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.show_index_form {
                                return;
                            }
                            this.index_open_name = None;
                            this.show_index_form = true;
                            this.index_name
                                .update(cx, |input, cx| input.set_value("", window, cx));
                            this.reset_index_fields(window, cx);
                            this.index_unique = false;
                            cx.notify();
                        })),
                ),
            );
        }

        let mut list = div().v_flex().flex_1().min_h_0();
        if indexes.is_empty() {
            list = list.child(
                div()
                    .p_3()
                    .text_xs()
                    .text_color(muted_foreground)
                    .child(t("无索引")),
            );
        }
        for (index_ix, index) in indexes.into_iter().enumerate() {
            let real_name = index.name.clone();
            let display_name = if real_name.is_empty() {
                t("（自动命名）").to_string()
            } else {
                real_name.clone()
            };
            let label = format!(
                "{} · {}",
                index.columns.join(", "),
                if index.unique { t("UNIQUE") } else { t("INDEX") },
            );
            let selected = self.selected_index == Some(index_ix);
            let index_for_open = index.clone();
            let mut row = div()
                .id(("index-row", index_ix))
                .h_flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(border)
                .child(div().w_48().text_xs().child(display_name))
                .child(div().flex_1().text_xs().text_color(muted_foreground).child(label));
            if selected {
                row = row.bg(muted);
            }
            if !read_only {
                row = row.on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.selected_index = Some(index_ix);
                    if event.click_count() >= 2 {
                        this.open_index(&index_for_open, window, cx);
                    } else {
                        cx.notify();
                    }
                }));
            } else {
                row = row.on_click(cx.listener(move |this, _, _, cx| {
                    this.selected_index = Some(index_ix);
                    cx.notify();
                }));
            }
            list = list.child(row);
        }
        body = body.child(list);

        if self.show_index_form && !read_only {
            self.refresh_index_options(window, cx);
            let options_empty = self.index_column_options.is_empty();
            let placeholder = if options_empty {
                t("无可用字段")
            } else {
                t("选择列")
            };
            let editing_existing = self.index_open_name.is_some();
            let title = if editing_existing {
                t("编辑索引")
            } else {
                t("新建索引")
            };
            let mut fields = div().v_flex().gap_1().min_w_0().flex_1();
            for field in &self.index_columns {
                let id = field.id;
                let removable = self.index_columns.len() > 1;
                fields = fields.child(
                    div()
                        .h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Select::new(&field.select)
                                .flex_1()
                                .min_w_0()
                                .placeholder(placeholder)
                                .disabled(options_empty)
                                .accessibility_label(t("索引字段")),
                        )
                        .child(
                            Button::new(("index-field-remove", id))
                                .ghost()
                                .label(t("删除字段"))
                                .disabled(!removable)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.remove_index_column(id);
                                    cx.notify();
                                })),
                        ),
                );
            }
            fields = fields.child(
                div().child(
                    Button::new("index-field-add")
                        .ghost()
                        .label(t("添加字段"))
                        .disabled(options_empty)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.add_index_column(window, cx);
                        })),
                ),
            );
            let mut actions = div().h_flex().gap_2().child(
                Button::new("index-save")
                    .primary()
                    .label(t("保存"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.save_index(window, cx);
                    })),
            );
            if editing_existing {
                actions = actions.child(
                    Button::new("index-delete")
                        .label(t("删除"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.delete_opened_index(window, cx);
                        })),
                );
            }
            actions = actions.child(
                Button::new("index-cancel")
                    .label(t("取消"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.cancel_index(window, cx);
                    })),
            );
            body = body.child(
                div()
                    .v_flex()
                    .gap_2()
                    .border_t_1()
                    .border_color(border)
                    .px_2()
                    .py_2()
                    .child(div().text_sm().child(title))
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(muted_foreground).child(t("Name")))
                            .child(Input::new(&self.index_name)),
                    )
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(muted_foreground).child(t("Columns")))
                            .child(fields),
                    )
                    .child(
                        Checkbox::new("index-unique")
                            .label(t("唯一"))
                            .checked(self.index_unique)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.index_unique = *checked;
                                cx.notify();
                            })),
                    )
                    .child(actions),
            );
        }

        body.into_any_element()
    }

    fn render_ddl(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let ddl = self
            .state
            .read(cx)
            .tab(self.tab)
            .map(|tab| tab.structure_ddl.clone())
            .unwrap_or_default();
        let mut body = div().v_flex().flex_1().min_h_0().overflow_hidden().p_3();
        if ddl.is_empty() {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t("暂无 DDL")),
            );
        }
        for line in ddl.lines() {
            body = body.child(
                div()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .child(line.to_string()),
            );
        }
        body
    }

    fn render_pending(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let Some(tab) = state.tab(self.tab) else { return div().into_any_element() };
        if tab.structure_changes.is_empty() {
            return div().into_any_element();
        }
        let count = tab.structure_changes.column_ops.len() + tab.structure_changes.index_ops.len();
        let sqls = state.structure_preview_sql(self.tab);
        let unsupported = sqls.is_empty();
        let theme = cx.theme();
        let read_only = state.is_structure_read_only(self.tab);
        let mut bar = div()
            .v_flex()
            .border_t_1()
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
                            .child(tr("待提交 {n} 项结构变更", &[("n", &count.to_string())])),
                    )
                    .child(
                        Button::new("structure-preview")
                            .label(t("SQL"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_preview = !this.show_preview;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("structure-apply")
                            .primary()
                            .label(t("应用"))
                            .disabled(read_only || unsupported)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let tab = this.tab;
                                this.state.update(cx, |state, cx| {
                                    state.apply_structure(tab);
                                    cx.notify();
                                });
                            })),
                    )
                    .child(
                        Button::new("structure-discard")
                            .label(t("放弃"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                let tab = this.tab;
                                this.state.update(cx, |state, cx| {
                                    state.structure_discard(tab);
                                    cx.notify();
                                });
                            })),
                    ),
            );
        if self.show_preview {
            if unsupported {
                bar = bar.child(
                    div()
                        .px_2()
                        .pb_1()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(t("当前驱动不支持修改表结构")),
                );
            }
            for sql in sqls {
                bar = bar.child(
                    div()
                        .px_2()
                        .pb_1()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(sql),
                );
            }
        }
        bar.into_any_element()
    }
}

impl Render for StructureView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick_edit(window, cx);
        self.apply_pending_select_sync(window, cx);
        self.sync(cx);
        // 先拷贝主题 token，后续渲染函数还要可变借用 cx
        let (background, muted_foreground) = {
            let theme = cx.theme();
            (theme.background, theme.muted_foreground)
        };
        let status = self
            .state
            .read(cx)
            .tab(self.tab)
            .map(|tab| tab.status.clone())
            .unwrap_or_default();
        let section = self.section_bar(cx).into_any_element();
        let content = match self.section {
            Section::Ddl => self.render_ddl(cx).into_any_element(),
            Section::Columns => self.render_columns(cx).into_any_element(),
            Section::Indexes => self.render_indexes(window, cx).into_any_element(),
        };
        div()
            .v_flex()
            .size_full()
            .bg(background)
            .child(section)
            .child(content)
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(muted_foreground)
                    .truncate()
                    .child(status),
            )
            .child(self.render_pending(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::has_duplicate_column;

    #[test]
    fn index_columns_allow_empty_and_distinct_selection() {
        assert!(!has_duplicate_column(&[]));
        assert!(!has_duplicate_column(&["id".into(), "status".into()]));
    }

    #[test]
    fn index_columns_reject_repeated_selection() {
        assert!(!has_duplicate_column(&["id".into()]));
        assert!(has_duplicate_column(&["id".into(), "id".into()]));
        assert!(has_duplicate_column(&[
            "id".into(),
            "status".into(),
            "id".into()
        ]));
    }
}
