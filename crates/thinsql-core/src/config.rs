//! 配置持久化：连接信息进 JSON，密码进系统钥匙串。
//!
//! 明确不引入 SQLite：配置就是几十条连接，纯文本足够，还能让人直接看、直接改。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind};
use crate::model::{ConnectionConfig, secret_ref_for};

const CONFIG_VERSION: u32 = 1;
const KEYRING_SERVICE: &str = "thinsql";

/// 主题外观模式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeModePref {
    #[default]
    Dark,
    Light,
}

/// 日志级别：启动时生效，运行中不热切换。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevelPref {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevelPref {
    /// 转成 EnvFilter 里可用的级别字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// 界面语言偏好。`System` 表示跟随系统 locale，在启动时解析一次（重启生效）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LanguagePref {
    #[default]
    System,
    Zh,
    En,
}

/// 解析后的具体界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Zh,
    En,
}

impl LanguagePref {
    /// 按当前进程环境解析成具体语言。显式选择（Zh/En）直接生效。
    pub fn resolve(self) -> Language {
        match self {
            Self::Zh => Language::Zh,
            Self::En => Language::En,
            Self::System => Language::from_locale(&system_locale()),
        }
    }
}

impl Language {
    /// 从 locale 字符串推断语言：`zh` 开头（zh_CN、zh_TW…）视为中文，其余（含空串）视为英文。
    pub fn from_locale(locale: &str) -> Self {
        if locale.to_lowercase().starts_with("zh") {
            Self::Zh
        } else {
            Self::En
        }
    }
}

/// 系统语言探测顺序：LC_ALL > LC_MESSAGES > LANG（与 gettext 一致）；都没有返回空串。
fn system_locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|var| std::env::var(var).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_default()
}

/// 全局应用设置。旧 config.json 缺整个字段也能加载：`#[serde(default)]` 兜底。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub theme_mode: ThemeModePref,
    /// 界面字体家族；空串表示跟随系统默认。
    pub font_family: String,
    /// 等宽字体家族（SQL 编辑器/结果表格）；空串表示跟随系统默认。
    pub mono_font_family: String,
    pub font_size: f32,
    pub mono_font_size: f32,
    /// 点表预览时每页取多少行。
    pub default_page_limit: u32,
    /// 单个结果标签最多加载的行数（加载更多会停在它这里）。
    pub max_loaded_rows: u32,
    /// 查询历史最多保留多少条。
    pub history_limit: u32,
    /// 新建连接对话框中"只读"的初始勾选值。
    pub default_read_only: bool,
    pub log_level: LogLevelPref,
    /// 界面语言；System 表示跟随系统 locale，重启后生效。
    pub language: LanguagePref,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme_mode: ThemeModePref::Dark,
            font_family: String::new(),
            mono_font_family: String::new(),
            font_size: 14.0,
            mono_font_size: 13.0,
            default_page_limit: 500,
            max_loaded_rows: 50_000,
            history_limit: 500,
            default_read_only: true,
            log_level: LogLevelPref::Info,
            language: LanguagePref::System,
        }
    }
}

impl AppSettings {
    /// 把越界值收拢回合法区间。旧文件手改坏了、或对话框绕过校验时兜底。
    pub fn clamped(mut self) -> Self {
        self.font_family = self.font_family.trim().to_string();
        self.mono_font_family = self.mono_font_family.trim().to_string();
        self.font_size = self.font_size.clamp(9.0, 24.0);
        self.mono_font_size = self.mono_font_size.clamp(9.0, 24.0);
        self.default_page_limit = self.default_page_limit.clamp(10, 5_000);
        self.max_loaded_rows = self.max_loaded_rows.clamp(1_000, 1_000_000);
        self.history_limit = self.history_limit.clamp(10, 5_000);
        self
    }

    /// 界面字体家族；空串返回 None 表示交给主题默认（.SystemUIFont）。
    pub fn font_family_override(&self) -> Option<&str> {
        non_empty(&self.font_family)
    }

    /// 等宽字体家族；空串返回 None 表示交给主题默认。
    pub fn mono_font_family_override(&self) -> Option<&str> {
        non_empty(&self.mono_font_family)
    }
}

fn non_empty(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
    #[serde(default)]
    pub settings: AppSettings,
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            connections: Vec::new(),
            settings: AppSettings::default(),
        }
    }
}

/// 加载结果：值 + 可选警告。损坏文件被备份时，warning 会带上备份路径，供 UI 提示。
#[derive(Debug, Clone)]
pub struct LoadOutcome<T> {
    pub value: T,
    pub warning: Option<String>,
}

/// 把损坏的文件重命名成 `*.corrupt-<unix_nanos>`，避免后续保存覆盖原始内容。
pub(crate) fn backup_corrupted(path: &Path) -> std::io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let backup = path.with_file_name(format!("{file_name}.corrupt-{stamp}"));
    fs::rename(path, &backup)?;
    Ok(backup)
}

/// 生成"损坏并已备份"的 warning 文案；备份失败时也要如实告知。
pub(crate) fn corrupt_warning(path: &Path, reason: &str) -> String {
    match backup_corrupted(path) {
        Ok(backup) => format!("{reason}；已将损坏文件备份到 {}", backup.display()),
        Err(err) => format!(
            "{reason}；备份损坏文件失败（{err}），请手动检查 {}",
            path.display()
        ),
    }
}

impl AppConfig {
    /// 配置文件所在目录：遵循 XDG，Windows/macOS 后续用各自路径覆盖。
    pub fn dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("THINSQL_CONFIG_DIR") {
            return PathBuf::from(dir);
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg).join("thinsql");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config").join("thinsql");
        }
        PathBuf::from(".thinsql")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    /// 读取配置。文件不存在或损坏时返回默认值并记日志——
    /// 配置坏了不应该让应用起不来。
    pub fn load() -> Self {
        Self::load_with_warning().value
    }

    /// 带 warning 的启动加载：损坏文件会先备份，再回退默认值。
    pub fn load_with_warning() -> LoadOutcome<Self> {
        Self::load_from_with_warning(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        Self::load_from_with_warning(path).value
    }

    pub fn load_from_with_warning(path: &Path) -> LoadOutcome<Self> {
        match fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<AppConfig>(&text) {
                Ok(cfg) => LoadOutcome {
                    value: cfg,
                    warning: None,
                },
                Err(err) => {
                    tracing::error!(path = %path.display(), error = %err, "配置文件解析失败，使用默认配置");
                    LoadOutcome {
                        value: AppConfig::default(),
                        warning: Some(corrupt_warning(path, &format!("配置文件解析失败: {err}"))),
                    }
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => LoadOutcome {
                value: AppConfig::default(),
                warning: None,
            },
            Err(err) => {
                tracing::error!(path = %path.display(), error = %err, "配置文件读取失败，使用默认配置");
                LoadOutcome {
                    value: AppConfig::default(),
                    warning: Some(corrupt_warning(path, &format!("配置文件读取失败: {err}"))),
                }
            }
        }
    }

    /// 原子写入：先写临时文件再 rename，避免写一半断电留下半截 JSON。
    pub fn save(&self) -> Result<(), Error> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 后台保存：合并短时间内的多次写入，只落盘最新快照，避免 UI 线程等磁盘。
    pub fn save_async(&self) {
        if config_writer().send(self.clone()).is_err() {
            tracing::warn!("配置写入通道已关闭，跳过本次保存");
        }
    }

    pub fn find(&self, id: &str) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut ConnectionConfig> {
        self.connections.iter_mut().find(|c| c.id == id)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.connections.len();
        self.connections.retain(|c| c.id != id);
        before != self.connections.len()
    }

    pub fn upsert(&mut self, cfg: ConnectionConfig) {
        match self.find_mut(&cfg.id) {
            Some(existing) => *existing = cfg,
            None => self.connections.push(cfg),
        }
    }
}

/// 专用配置写入线程：排空队列后只写最新快照，避免旧快照覆盖新快照。
fn config_writer() -> &'static std::sync::mpsc::Sender<AppConfig> {
    use std::sync::{OnceLock, mpsc};
    static TX: OnceLock<mpsc::Sender<AppConfig>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<AppConfig>();
        std::thread::Builder::new()
            .name("config-writer".into())
            .spawn(move || {
                while let Ok(mut config) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        config = newer;
                    }
                    if let Err(err) = config.save() {
                        tracing::error!(error = %err.message, "后台保存配置失败");
                    }
                }
            })
            .ok();
        tx
    })
}

/// 密码存取。所有失败都降级为"没有密码"，而不是让调用方崩溃：
/// 无桌面会话（无 Secret Service）时应用仍要能用。
pub mod secret {
    use super::*;

    fn entry_for(cfg: &ConnectionConfig) -> Result<keyring::Entry, Error> {
        let user = cfg
            .secret_ref
            .clone()
            .unwrap_or_else(|| secret_ref_for(&cfg.id));
        keyring::Entry::new(KEYRING_SERVICE, &user)
            .map_err(|err| Error::with_source(ErrorKind::Config, "打开系统钥匙串失败", err))
    }

    pub fn store(cfg: &ConnectionConfig, password: &str) -> Result<(), Error> {
        let entry = entry_for(cfg)?;
        entry
            .set_password(password)
            .map_err(|err| Error::with_source(ErrorKind::Config, "写入系统钥匙串失败", err))
    }

    /// 读不到（未保存/钥匙串不可用）时返回 None，不报错。
    pub fn load(cfg: &ConnectionConfig) -> Option<String> {
        let entry = entry_for(cfg).ok()?;
        match entry.get_password() {
            Ok(pw) => Some(pw),
            Err(err) => {
                if !matches!(err, keyring::Error::NoEntry) {
                    tracing::warn!(error = %err, "读取系统钥匙串失败，按无密码处理");
                }
                None
            }
        }
    }

    pub fn delete(cfg: &ConnectionConfig) -> Result<(), Error> {
        let entry = entry_for(cfg)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(Error::with_source(
                ErrorKind::Config,
                "删除系统钥匙串条目失败",
                err,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DriverKind;

    fn sample() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.connections
            .push(ConnectionConfig::new("c1", "本地 MySQL", DriverKind::MySql));
        cfg
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let cfg = sample();
        cfg.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path);

        assert_eq!(loaded.connections.len(), 1);
        assert_eq!(loaded.connections[0].name, "本地 MySQL");
        assert_eq!(loaded.connections[0].kind, DriverKind::MySql);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = AppConfig::load_from(&dir.path().join("nope.json"));
        assert!(loaded.connections.is_empty());
    }

    #[test]
    fn corrupted_file_yields_defaults_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "{ this is not json").unwrap();
        assert!(AppConfig::load_from(&path).connections.is_empty());
    }

    #[test]
    fn corrupted_file_is_backed_up_before_falling_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "{ this is not json").unwrap();

        let outcome = AppConfig::load_from_with_warning(&path);

        assert!(outcome.value.connections.is_empty());
        assert!(outcome.warning.is_some(), "损坏配置必须透传 warning");

        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.json.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "损坏文件必须被备份，避免下次保存覆盖");
        assert_eq!(
            fs::read_to_string(backups[0].path()).unwrap(),
            "{ this is not json"
        );
        assert!(!path.exists(), "损坏文件应被移走，而不是留在原处");
    }

    #[test]
    fn password_is_never_serialized_in_plain_text() {
        let cfg = sample();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("password"), "配置里不允许出现任何密码字段");
        assert!(json.contains("thinsql/c1"), "只允许出现钥匙串引用键");
    }

    #[test]
    fn upsert_replaces_existing_by_id() {
        let mut cfg = sample();
        let mut updated = ConnectionConfig::new("c1", "改名了", DriverKind::MySql);
        updated.host = "10.0.0.2".into();
        cfg.upsert(updated);
        assert_eq!(cfg.connections.len(), 1);
        assert_eq!(cfg.connections[0].name, "改名了");
        assert!(cfg.remove("c1"));
        assert!(!cfg.remove("c1"));
    }

    #[test]
    fn settings_have_sane_defaults() {
        let settings = AppSettings::default();
        assert_eq!(settings.theme_mode, ThemeModePref::Dark);
        assert_eq!(settings.default_page_limit, 500);
        assert_eq!(settings.max_loaded_rows, 50_000);
        assert_eq!(settings.history_limit, 500);
        assert!(settings.default_read_only, "新连接默认只读");
        assert_eq!(settings.log_level, LogLevelPref::Info);
        assert!(
            settings.font_family_override().is_none(),
            "默认字体为空串，表示跟随系统"
        );
    }

    #[test]
    fn legacy_config_without_settings_loads_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"version":1,"connections":[]}"#).unwrap();

        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.settings, AppSettings::default());
    }

    #[test]
    fn settings_round_trip_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let mut cfg = AppConfig::default();
        cfg.settings.theme_mode = ThemeModePref::Light;
        cfg.settings.default_page_limit = 200;
        cfg.settings.default_read_only = false;
        cfg.settings.log_level = LogLevelPref::Debug;
        cfg.save_to(&path).unwrap();

        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.settings, cfg.settings);
    }

    #[test]
    fn settings_partially_missing_fields_fall_back_individually() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{"version":1,"connections":[],"settings":{"default_page_limit":100}}"#,
        )
        .unwrap();

        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.settings.default_page_limit, 100);
        assert_eq!(loaded.settings.history_limit, 500, "缺省字段逐个回退默认值");
    }

    #[test]
    fn clamped_pulls_out_of_range_values_back() {
        let mut settings = AppSettings::default();
        settings.default_page_limit = 0;
        settings.max_loaded_rows = 999_999_999;
        settings.history_limit = 7;
        settings.font_size = 100.0;
        settings.mono_font_size = 0.5;
        settings.font_family = "  Noto Sans CJK SC  ".into();

        let clamped = settings.clamped();
        assert_eq!(clamped.default_page_limit, 10);
        assert_eq!(clamped.max_loaded_rows, 1_000_000);
        assert_eq!(clamped.history_limit, 10);
        assert_eq!(clamped.font_size, 24.0);
        assert_eq!(clamped.mono_font_size, 9.0);
        assert_eq!(clamped.font_family_override(), Some("Noto Sans CJK SC"));
    }

    #[test]
    fn corrupted_settings_json_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "{ this is not json").unwrap();
        assert_eq!(AppConfig::load_from(&path).settings, AppSettings::default());
    }

    #[test]
    fn language_defaults_to_system() {
        assert_eq!(LanguagePref::default(), LanguagePref::System);
        assert_eq!(AppSettings::default().language, LanguagePref::System);
    }

    #[test]
    fn from_locale_detects_zh_prefix() {
        assert_eq!(Language::from_locale("zh_CN.UTF-8"), Language::Zh);
        assert_eq!(Language::from_locale("zh_TW"), Language::Zh);
        assert_eq!(Language::from_locale("ZH_cn"), Language::Zh, "大小写不敏感");
    }

    #[test]
    fn from_locale_falls_back_to_en() {
        assert_eq!(Language::from_locale("en_US.UTF-8"), Language::En);
        assert_eq!(Language::from_locale("C"), Language::En);
        assert_eq!(Language::from_locale(""), Language::En, "空 locale 视为英文");
    }

    #[test]
    fn explicit_language_overrides_locale() {
        // 显式选择不依赖环境变量，避免并行测试里改进程 env 互相污染
        assert_eq!(LanguagePref::Zh.resolve(), Language::Zh);
        assert_eq!(LanguagePref::En.resolve(), Language::En);
    }

    #[test]
    fn language_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let mut cfg = AppConfig::default();
        cfg.settings.language = LanguagePref::En;
        cfg.save_to(&path).unwrap();

        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.settings.language, LanguagePref::En);
    }

    #[test]
    fn missing_language_field_falls_back_to_system() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{"version":1,"connections":[],"settings":{"default_page_limit":100}}"#,
        )
        .unwrap();

        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.settings.language, LanguagePref::System);
    }
}
