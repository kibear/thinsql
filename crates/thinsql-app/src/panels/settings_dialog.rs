//! 设置弹窗：外观、字体、数据、历史、连接、日志。
//!
//! 所有编辑都发生在 `SettingsForm` 实体持有的草稿上，点「保存」才校验、
//! 应用主题并持久化到 config.json；「取消」直接丢弃。与连接弹窗一样，
//! 实体必须在 `open_dialog` 之前创建。

use gpui_kit::base::StyledExt;
use gpui_kit::component::Selectable;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::rc::Rc;
use thinsql_core::config::{AppSettings, LanguagePref, LogLevelPref, ThemeModePref};

use crate::i18n::{t, tr};
use crate::state::{AppState, StateEvent};
use crate::theme;

/// 弹窗保存动作：返回 false 表示校验失败，弹窗保持打开。
type DialogSaveAction = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

/// 与 `AppSettings::clamped` 的取值范围保持一致，非法输入在保存时明确拒绝。
const FONT_SIZE_MIN: f32 = 9.0;
const FONT_SIZE_MAX: f32 = 24.0;
const PAGE_LIMIT_MIN: u32 = 10;
const PAGE_LIMIT_MAX: u32 = 5_000;
const MAX_ROWS_MIN: u32 = 1_000;
const MAX_ROWS_MAX: u32 = 1_000_000;
const HISTORY_LIMIT_MIN: u32 = 10;
const HISTORY_LIMIT_MAX: u32 = 5_000;

fn parse_u32(value: &str, label: &str, min: u32, max: u32) -> Result<u32, String> {
    let parsed: u32 = value.trim().parse().map_err(|_| {
        tr(
            "{label}必须是 {min}-{max} 的整数，当前是 {value}",
            &[
                ("label", label),
                ("min", &min.to_string()),
                ("max", &max.to_string()),
                ("value", &format!("{value:?}")),
            ],
        )
    })?;
    if !(min..=max).contains(&parsed) {
        return Err(tr(
            "{label}必须在 {min}-{max} 之间",
            &[
                ("label", label),
                ("min", &min.to_string()),
                ("max", &max.to_string()),
            ],
        ));
    }
    Ok(parsed)
}

fn parse_font_size(value: &str, label: &str) -> Result<f32, String> {
    let parsed: f32 = value.trim().parse().map_err(|_| {
        tr(
            "{label}必须是 {min}-{max} 的数字，当前是 {value}",
            &[
                ("label", label),
                ("min", &FONT_SIZE_MIN.to_string()),
                ("max", &FONT_SIZE_MAX.to_string()),
                ("value", &format!("{value:?}")),
            ],
        )
    })?;
    if !(FONT_SIZE_MIN..=FONT_SIZE_MAX).contains(&parsed) {
        return Err(tr(
            "{label}必须在 {min}-{max} 之间",
            &[
                ("label", label),
                ("min", &FONT_SIZE_MIN.to_string()),
                ("max", &FONT_SIZE_MAX.to_string()),
            ],
        ));
    }
    Ok(parsed)
}

/// 文本输入框；`value` 为空串时不预填，交给 placeholder 提示。
fn value_input(
    value: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    let value = value.to_string();
    let placeholder = placeholder.to_string();
    cx.new(|cx| {
        let mut input = InputState::new(window, cx).placeholder(placeholder);
        if !value.is_empty() {
            input = input.default_value(value);
        }
        input
    })
}

/// 分段选项：key 用于元素 id 与草稿赋值，label 为界面文案。
type Segment = (&'static str, &'static str);

const THEME_SEGMENTS: [Segment; 2] = [("dark", "深色"), ("light", "浅色")];
/// 语言名按惯例显示本名（中文/English），"跟随系统" 走翻译。
const LANGUAGE_SEGMENTS: [Segment; 3] = [
    ("system", "跟随系统"),
    ("zh", "中文"),
    ("en", "English"),
];
const LOG_LEVEL_SEGMENTS: [Segment; 5] = [
    ("error", "错误"),
    ("warn", "警告"),
    ("info", "信息"),
    ("debug", "调试"),
    ("trace", "跟踪"),
];

/// 分段按钮点击后的草稿赋值。key 与选项一一对应。
fn apply_segment(draft: &mut AppSettings, key: &str) {
    match key {
        "dark" => draft.theme_mode = ThemeModePref::Dark,
        "light" => draft.theme_mode = ThemeModePref::Light,
        "system" => draft.language = LanguagePref::System,
        "zh" => draft.language = LanguagePref::Zh,
        "en" => draft.language = LanguagePref::En,
        "error" => draft.log_level = LogLevelPref::Error,
        "warn" => draft.log_level = LogLevelPref::Warn,
        "info" => draft.log_level = LogLevelPref::Info,
        "debug" => draft.log_level = LogLevelPref::Debug,
        "trace" => draft.log_level = LogLevelPref::Trace,
        _ => {}
    }
}

fn selected_theme_key(draft: &AppSettings) -> &'static str {
    match draft.theme_mode {
        ThemeModePref::Dark => "dark",
        ThemeModePref::Light => "light",
    }
}

fn selected_language_key(draft: &AppSettings) -> &'static str {
    match draft.language {
        LanguagePref::System => "system",
        LanguagePref::Zh => "zh",
        LanguagePref::En => "en",
    }
}

fn selected_log_level_key(draft: &AppSettings) -> &'static str {
    match draft.log_level {
        LogLevelPref::Error => "error",
        LogLevelPref::Warn => "warn",
        LogLevelPref::Info => "info",
        LogLevelPref::Debug => "debug",
        LogLevelPref::Trace => "trace",
    }
}

/// 设置表单：草稿 + 文本输入实体。开关与分段按钮直接改草稿。
struct SettingsForm {
    draft: AppSettings,
    font_family: Entity<InputState>,
    mono_font_family: Entity<InputState>,
    font_size: Entity<InputState>,
    mono_font_size: Entity<InputState>,
    page_limit: Entity<InputState>,
    max_loaded_rows: Entity<InputState>,
    history_limit: Entity<InputState>,
}

impl SettingsForm {
    fn new(settings: AppSettings, window: &mut Window, cx: &mut App) -> Self {
        Self {
            font_family: family_or_empty_input(
                &settings.font_family,
                t("留空跟随系统默认"),
                window,
                cx,
            ),
            mono_font_family: family_or_empty_input(
                &settings.mono_font_family,
                t("留空跟随系统默认"),
                window,
                cx,
            ),
            font_size: value_input(&format_font_size(settings.font_size), "", window, cx),
            mono_font_size: value_input(&format_font_size(settings.mono_font_size), "", window, cx),
            page_limit: value_input(&settings.default_page_limit.to_string(), "", window, cx),
            max_loaded_rows: value_input(&settings.max_loaded_rows.to_string(), "", window, cx),
            history_limit: value_input(&settings.history_limit.to_string(), "", window, cx),
            draft: settings,
        }
    }

    /// 汇总草稿与输入框为一份新设置；数字字段非法时返回错误文案。
    fn collect(&self, app: &App) -> Result<AppSettings, String> {
        let font_size = parse_font_size(
            &self.font_size.read(app).value(),
            t("界面字号"),
        )?;
        let mono_font_size = parse_font_size(
            &self.mono_font_size.read(app).value(),
            t("等宽字号"),
        )?;
        let default_page_limit = parse_u32(
            &self.page_limit.read(app).value(),
            t("每页条数"),
            PAGE_LIMIT_MIN,
            PAGE_LIMIT_MAX,
        )?;
        let max_loaded_rows = parse_u32(
            &self.max_loaded_rows.read(app).value(),
            t("结果最大加载行数"),
            MAX_ROWS_MIN,
            MAX_ROWS_MAX,
        )?;
        let history_limit = parse_u32(
            &self.history_limit.read(app).value(),
            t("历史记录条数"),
            HISTORY_LIMIT_MIN,
            HISTORY_LIMIT_MAX,
        )?;
        let settings = AppSettings {
            font_family: self.font_family.read(app).value().to_string(),
            mono_font_family: self.mono_font_family.read(app).value().to_string(),
            font_size,
            mono_font_size,
            default_page_limit,
            max_loaded_rows,
            history_limit,
            ..self.draft.clone()
        };
        Ok(AppSettings::clamped(settings))
    }

    fn render_group(
        &self,
        title: &str,
        rows: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(title.to_string()),
            )
            .children(rows)
            .into_any_element()
    }

    /// 一行设置：左侧标签与说明，右侧控件。
    fn render_row(
        label: &str,
        hint: &str,
        control: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .v_flex()
                    .child(div().text_sm().child(label.to_string()))
                    .when(!hint.is_empty(), |cell| {
                        cell.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(hint.to_string()),
                        )
                    }),
            )
            .child(control)
            .into_any_element()
    }

    /// 分段选择：小型互斥选项组（深/浅、日志级别）。
    fn render_segmented(
        &self,
        id_prefix: &'static str,
        segments: &[Segment],
        selected_key: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .h_flex()
            .gap_1()
            .children(segments.iter().map(|(key, label)| {
                // key 是 &'static str，拷贝出来供 'static 的 on_click 捕获
                let key: &'static str = key;
                let label: &'static str = *label;
                Button::new(format!("{id_prefix}-{key}"))
                    .ghost()
                    .selected(key == selected_key)
                    .label(t(label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        apply_segment(&mut this.draft, key);
                        cx.notify();
                    }))
            }))
            .into_any_element()
    }
}

/// 字体家族输入：值为空串时不预填，让 placeholder 说明"留空跟随系统默认"。
fn family_or_empty_input(
    value: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    value_input(value.trim(), placeholder, window, cx)
}

/// 字号显示：整数不带小数点，非整数字段保留原值（13.5 → "13.5"）。
fn format_font_size(size: f32) -> String {
    let rounded = size.round();
    if (size - rounded).abs() < f32::EPSILON {
        format!("{}", rounded as u32)
    } else {
        format!("{size}")
    }
}

impl Render for SettingsForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let readonly_switch = Switch::new("settings-default-readonly")
            .checked(self.draft.default_read_only)
            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                this.draft.default_read_only = *checked;
                cx.notify();
            }));

        div()
            .v_flex()
            .gap_4()
            .child(self.render_group(
                t("外观"),
                vec![
                    Self::render_row(
                        t("主题模式"),
                        t("品牌色保持不变"),
                        self.render_segmented(
                            "settings-theme-mode",
                            &THEME_SEGMENTS,
                            selected_theme_key(&self.draft),
                            cx,
                        ),
                        cx,
                    ),
                    Self::render_row(
                        t("语言"),
                        t("重启后生效"),
                        self.render_segmented(
                            "settings-language",
                            &LANGUAGE_SEGMENTS,
                            selected_language_key(&self.draft),
                            cx,
                        ),
                        cx,
                    ),
                    Self::render_row(
                        t("界面字体"),
                        t("留空跟随系统默认"),
                        Input::new(&self.font_family),
                        cx,
                    ),
                    Self::render_row(t("界面字号"), "", Input::new(&self.font_size), cx),
                    Self::render_row(
                        t("等宽字体"),
                        t("SQL 编辑器与结果表格"),
                        Input::new(&self.mono_font_family),
                        cx,
                    ),
                    Self::render_row(t("等宽字号"), "", Input::new(&self.mono_font_size), cx),
                ],
                cx,
            ))
            .child(self.render_group(
                t("数据"),
                vec![
                    Self::render_row(
                        t("每页条数"),
                        t("点表预览时每页取多少行"),
                        Input::new(&self.page_limit),
                        cx,
                    ),
                    Self::render_row(
                        t("结果最大加载行数"),
                        t("「加载更多」累计到该行数后停止"),
                        Input::new(&self.max_loaded_rows),
                        cx,
                    ),
                ],
                cx,
            ))
            .child(self.render_group(
                t("历史"),
                vec![Self::render_row(
                    t("历史记录条数"),
                    t("超出后丢弃最旧的记录"),
                    Input::new(&self.history_limit),
                    cx,
                )],
                cx,
            ))
            .child(self.render_group(
                t("连接"),
                vec![Self::render_row(
                    t("新建连接默认只读"),
                    t("只影响新建连接的初始勾选"),
                    readonly_switch,
                    cx,
                )],
                cx,
            ))
            .child(self.render_group(
                t("日志"),
                vec![Self::render_row(
                    t("日志级别"),
                    t("下次启动生效"),
                    self.render_segmented(
                        "settings-log-level",
                        &LOG_LEVEL_SEGMENTS,
                        selected_log_level_key(&self.draft),
                        cx,
                    ),
                    cx,
                )],
                cx,
            ))
    }
}

/// 打开设置弹窗。保存时统一生效：主题/字体立即切换，配置异步落盘。
pub fn open_settings_dialog(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| SettingsForm::new(state.read(cx).config.settings.clone(), window, cx));

    window.open_dialog(cx, move |dialog, _, _app| {
        let form = form.clone();
        let state = state.clone();

        let save: DialogSaveAction = {
            let form = form.clone();
            let state = state.clone();
            Rc::new(move |window, app| {
                let settings = match form.read(app).collect(app) {
                    Ok(settings) => settings,
                    Err(message) => {
                        window.push_notification(Notification::error(message), app);
                        return false;
                    }
                };
                // 主题/字体立即生效；面板通过 Changed 事件刷新分页等展示
                theme::apply(&settings, app);
                state.update(app, |state, cx| {
                    state.config.settings = settings;
                    state.config.save_async();
                    cx.emit(StateEvent::Changed);
                    cx.notify();
                });
                true
            })
        };

        dialog
            .title(t("设置"))
            .w(px(560.))
            .footer(
                DialogFooter::new()
                    .child(div().flex_1())
                    .child(
                        Button::new("cancel-settings")
                            .label(t("取消"))
                            .on_click(|_, window, app| window.close_dialog(app)),
                    )
                    .child(
                        Button::new("save-settings")
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
            .child(form)
    });
}

#[cfg(test)]
mod tests {
    use super::{
        LANGUAGE_SEGMENTS, LOG_LEVEL_SEGMENTS, THEME_SEGMENTS, apply_segment, format_font_size,
        parse_font_size, parse_u32, selected_language_key, selected_log_level_key,
        selected_theme_key,
    };
    use thinsql_core::config::{AppSettings, LanguagePref, LogLevelPref, ThemeModePref};

    #[test]
    fn parse_u32_rejects_out_of_range_and_non_numeric() {
        assert_eq!(parse_u32("500", "每页条数", 10, 5_000), Ok(500));
        assert_eq!(parse_u32(" 20 ", "每页条数", 10, 5_000), Ok(20));
        assert!(parse_u32("abc", "每页条数", 10, 5_000).is_err());
        assert!(parse_u32("0", "每页条数", 10, 5_000).is_err());
        assert!(parse_u32("99999", "每页条数", 10, 5_000).is_err());
    }

    #[test]
    fn parse_font_size_rejects_out_of_range_and_non_numeric() {
        assert_eq!(parse_font_size("13", "界面字号"), Ok(13.0));
        assert_eq!(parse_font_size(" 13.5 ", "界面字号"), Ok(13.5));
        assert!(parse_font_size("big", "界面字号").is_err());
        assert!(parse_font_size("8", "界面字号").is_err());
        assert!(parse_font_size("30", "界面字号").is_err());
    }

    #[test]
    fn segmented_keys_round_trip_through_draft() {
        let mut draft = AppSettings::default();
        assert_eq!(selected_theme_key(&draft), "dark");

        for (key, _) in THEME_SEGMENTS {
            apply_segment(&mut draft, key);
            assert_eq!(selected_theme_key(&draft), key);
        }
        assert_eq!(draft.theme_mode, ThemeModePref::Light, "最后一个选项生效");

        for (key, _) in LOG_LEVEL_SEGMENTS {
            apply_segment(&mut draft, key);
            assert_eq!(selected_log_level_key(&draft), key);
        }
        assert_eq!(draft.log_level, LogLevelPref::Trace, "最后一个选项生效");
    }

    #[test]
    fn language_keys_round_trip_through_draft() {
        let mut draft = AppSettings::default();
        assert_eq!(selected_language_key(&draft), "system");

        for (key, _) in LANGUAGE_SEGMENTS {
            apply_segment(&mut draft, key);
            assert_eq!(selected_language_key(&draft), key);
        }
        assert_eq!(draft.language, LanguagePref::En, "最后一个选项生效");
    }

    #[test]
    fn font_size_display_stays_readable() {
        assert_eq!(format_font_size(13.0), "13");
        assert_eq!(format_font_size(13.5), "13.5");
    }
}
