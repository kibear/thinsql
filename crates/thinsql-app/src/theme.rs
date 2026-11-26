//! 深色为主的克制配色：高信息密度、低视觉噪音。
//!
//! 主题模式/字体/字号来自应用设置，品牌色不随用户设置变化。

use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::*;

use thinsql_core::config::{AppSettings, ThemeModePref};

/// 品牌色：偏冷的蓝，避免默认主题那种饱和度过高的"网页蓝"。
const PRIMARY: u32 = 0x4c8dff;
const ACCENT: u32 = 0x3b7bea;

/// 界面圆角半径。
const RADIUS: f32 = 6.;

pub fn init(cx: &mut App) {
    apply(&AppSettings::default(), cx);
}

/// 按设置应用主题：Dark/Light 切换、字体家族与字号。
/// 品牌色与圆角保持应用自己的定义，不开放给用户配置。
pub fn apply(settings: &AppSettings, cx: &mut App) {
    let mode = match settings.theme_mode {
        ThemeModePref::Dark => ThemeMode::Dark,
        ThemeModePref::Light => ThemeMode::Light,
    };
    // Theme::change 会按模式重置整套配色，之后必须重新叠加品牌色
    Theme::change(mode, None, cx);
    Theme::update(cx, |theme| {
        theme.colors.primary = rgb(PRIMARY).into();
        theme.colors.accent = rgb(ACCENT).into();
        theme.radius = px(RADIUS);
        theme.font_size = px(settings.font_size);
        theme.mono_font_size = px(settings.mono_font_size);
        if let Some(family) = settings.font_family_override() {
            theme.font_family = family.into();
        }
        if let Some(family) = settings.mono_font_family_override() {
            theme.mono_font_family = family.into();
        }
    });
}
