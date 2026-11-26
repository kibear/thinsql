//! 查询历史：本地 JSON 文件，限制条数，连续重复不重复记。
//!
//! 只存 SQL 原文与结果摘要，绝不存结果集本身——历史文件必须保持小而快。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;

const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// Unix 毫秒时间戳
    pub executed_at_ms: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub row_count: usize,
    #[serde(default)]
    pub failed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    pub entries: Vec<HistoryEntry>,
}

impl History {
    pub fn path() -> PathBuf {
        crate::config::AppConfig::dir().join("history.json")
    }

    pub fn load() -> Self {
        Self::load_with_warning().value
    }

    /// 带 warning 的启动加载：损坏文件会先备份，再回退空历史。
    pub fn load_with_warning() -> crate::config::LoadOutcome<Self> {
        Self::load_from_with_warning(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        Self::load_from_with_warning(path).value
    }

    pub fn load_from_with_warning(path: &Path) -> crate::config::LoadOutcome<Self> {
        match fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<History>(&text) {
                Ok(history) => crate::config::LoadOutcome {
                    value: history,
                    warning: None,
                },
                Err(err) => {
                    tracing::warn!(error = %err, "历史文件解析失败，按空历史处理");
                    crate::config::LoadOutcome {
                        value: Self::default(),
                        warning: Some(crate::config::corrupt_warning(path, "历史文件解析失败")),
                    }
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => crate::config::LoadOutcome {
                value: Self::default(),
                warning: None,
            },
            Err(err) => {
                tracing::warn!(error = %err, "历史文件读取失败，按空历史处理");
                crate::config::LoadOutcome {
                    value: Self::default(),
                    warning: Some(crate::config::corrupt_warning(
                        path,
                        &format!("历史文件读取失败: {err}"),
                    )),
                }
            }
        }
    }

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

    /// 后台保存：合并短时间内的多次写入，只落盘最新快照。
    pub fn save_async(&self) {
        if history_writer().send(self.clone()).is_err() {
            tracing::warn!("历史写入通道已关闭，跳过本次保存");
        }
    }

    /// 记一条。与上一条完全相同的 SQL 只更新时间与摘要，不产生新行——
    /// 否则反复点"运行"会把历史刷满同一个语句。
    pub fn push(&mut self, entry: HistoryEntry) {
        self.push_with_limit(entry, MAX_ENTRIES);
    }

    /// 带上限的记录：上限来自应用设置（history_limit）。
    pub fn push_with_limit(&mut self, entry: HistoryEntry, limit: usize) {
        if let Some(last) = self.entries.last_mut()
            && last.sql == entry.sql
            && last.connection_id == entry.connection_id
        {
            last.executed_at_ms = entry.executed_at_ms;
            last.elapsed_ms = entry.elapsed_ms;
            last.row_count = entry.row_count;
            last.failed = entry.failed;
            return;
        }
        self.entries.push(entry);
        if self.entries.len() > limit {
            let overflow = self.entries.len() - limit;
            self.entries.drain(0..overflow);
        }
    }

    /// 最近的在前，供 UI 列表直接使用。
    pub fn recent(&self, limit: usize) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter().rev().take(limit)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 专用历史写入线程：排空队列后只写最新快照，避免旧快照覆盖新快照。
fn history_writer() -> &'static std::sync::mpsc::Sender<History> {
    use std::sync::{OnceLock, mpsc};
    static TX: OnceLock<mpsc::Sender<History>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<History>();
        std::thread::Builder::new()
            .name("history-writer".into())
            .spawn(move || {
                while let Ok(mut history) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        history = newer;
                    }
                    if let Err(err) = history.save() {
                        tracing::warn!(error = %err.message, "后台保存查询历史失败");
                    }
                }
            })
            .ok();
        tx
    })
}

/// 当前时间（Unix 毫秒）。测试里可以直接构造时间戳。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sql: &str, at: u64) -> HistoryEntry {
        HistoryEntry {
            sql: sql.to_string(),
            connection_id: Some("c1".into()),
            executed_at_ms: at,
            elapsed_ms: 5,
            row_count: 1,
            failed: false,
        }
    }

    #[test]
    fn consecutive_duplicates_are_merged() {
        let mut history = History::default();
        history.push(entry("select 1", 100));
        history.push(entry("select 1", 200));
        assert_eq!(history.entries.len(), 1, "连续重复不应新增条目");
        assert_eq!(history.entries[0].executed_at_ms, 200, "应更新为最新时间");
    }

    #[test]
    fn non_adjacent_duplicates_are_kept() {
        let mut history = History::default();
        history.push(entry("select 1", 100));
        history.push(entry("select 2", 150));
        history.push(entry("select 1", 200));
        assert_eq!(history.entries.len(), 3);
    }

    #[test]
    fn recents_are_newest_first() {
        let mut history = History::default();
        history.push(entry("a", 1));
        history.push(entry("b", 2));
        let recent: Vec<&str> = history.recent(10).map(|e| e.sql.as_str()).collect();
        assert_eq!(recent, vec!["b", "a"]);
    }

    #[test]
    fn history_is_capped_by_dropping_oldest() {
        let mut history = History::default();
        for i in 0..(MAX_ENTRIES + 10) {
            history.push(entry(&format!("select {i}"), i as u64));
        }
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        assert_eq!(history.entries[0].sql, "select 10", "应丢弃最旧的条目");
    }

    #[test]
    fn push_with_limit_caps_entries_from_settings() {
        let mut history = History::default();
        for i in 0..20 {
            history.push_with_limit(entry(&format!("select {i}"), i as u64), 10);
        }
        assert_eq!(history.entries.len(), 10);
        assert_eq!(
            history.entries[0].sql, "select 10",
            "自定义上限同样丢弃最旧条目"
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");

        let mut history = History::default();
        history.push(entry("select 1", 42));
        history.save_to(&path).unwrap();

        let loaded = History::load_from(&path);
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].sql, "select 1");
        assert_eq!(loaded.entries[0].executed_at_ms, 42);
    }

    #[test]
    fn corrupted_history_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        fs::write(&path, "not json at all").unwrap();
        assert!(History::load_from(&path).is_empty());
    }

    #[test]
    fn corrupted_history_is_backed_up_before_resetting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        fs::write(&path, "not json at all").unwrap();

        let outcome = History::load_from_with_warning(&path);

        assert!(outcome.value.is_empty());
        assert!(outcome.warning.is_some(), "损坏历史必须透传 warning");

        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("history.json.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "损坏历史必须被备份");
        assert!(!path.exists(), "损坏文件应被移走，而不是留在原处");
    }
}
