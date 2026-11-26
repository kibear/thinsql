//! SQL 编辑器辅助：创建编辑器、取选区或全文。
//!
//! 编辑器实体依赖 `Window`，只能由工作区面板持有；这里只放与窗口无关的
//! 构造与取文本逻辑，避免状态层碰到 UI 实体。
//!
//! 语法高亮依赖 `gpui-kit` feature `tree-sitter-sql`（已在 Cargo.toml 打开），
//! 并通过 [`EditorState::language`] 设为 `"sql"`。

use gpui_kit::component::input::EditorState;
use gpui_kit::*;

pub fn create_editor<T>(
    sql: &str,
    window: &mut Window,
    cx: &mut Context<T>,
) -> Entity<EditorState> {
    cx.new(|cx| {
        EditorState::new(window, cx)
            .language("sql")
            .default_value(sql)
    })
}

/// 执行用的 SQL：选区优先，没有选区就是全文。
pub fn sql_to_run(editor: &Entity<EditorState>, app: &App) -> String {
    let state = editor.read(app);
    let selected = state.selected_text().to_string();
    if selected.trim().is_empty() {
        state.value().to_string()
    } else {
        selected
    }
}

/// 「执行所有」始终针对整段文本，不看选区。
pub fn full_sql(editor: &Entity<EditorState>, app: &App) -> String {
    editor.read(app).value().to_string()
}
