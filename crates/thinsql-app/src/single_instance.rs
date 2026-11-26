//! 单实例保护。
//!
//! 两个实例同时跑会互相覆盖配置文件（各自持有旧的内存副本，保存时整份写回），
//! 这会真的丢连接。
//!
//! 互斥完全依赖 OS 文件锁（flock）：持锁即唯一实例，进程退出由内核自动释放，
//! 不存在需要回收的“陈旧锁”。锁文件永不删除——删掉再重建会让两个进程各自
//! 锁住不同 inode 却都以为自己是唯一实例（inode 竞态）。文件里的 PID 只是
//! 附加信息（advisory），解析失败不影响互斥。
//!
//! 锁文件不可用（父目录建不出来等）按 fail-closed 处理：拒绝启动，
//! 绝不静默跳过互斥——宁可让用户修一下目录权限，也不能冒丢配置的风险。

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::i18n::{t, tr};

#[derive(Debug)]
pub struct InstanceGuard {
    // 持有文件描述符即持有锁；Drop 关闭文件时内核自动释放 flock。
    _file: fs::File,
}

#[derive(Debug)]
pub struct AlreadyRunning {
    /// 锁文件中记录的 PID，仅用于提示；读取失败时为 None。
    pub pid: Option<u32>,
}

#[derive(Debug)]
pub enum AcquireError {
    /// 已有实例持锁运行。
    AlreadyRunning(AlreadyRunning),
    /// 锁文件无法创建或加锁失败（fail-closed，拒绝启动）。
    Io(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning(running) => match running.pid {
                Some(pid) => write!(
                    f,
                    "{}",
                    tr(
                        "ThinSQL 已在运行（进程 {pid}），请切换到已有窗口",
                        &[("pid", &pid.to_string())]
                    )
                ),
                None => write!(f, "{}", t("ThinSQL 已在运行，请切换到已有窗口")),
            },
            Self::Io(err) => write!(
                f,
                "{}",
                tr("无法获取单实例锁：{error}", &[("error", &err.to_string())])
            ),
        }
    }
}

impl std::error::Error for AcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AlreadyRunning(_) => None,
            Self::Io(err) => Some(err),
        }
    }
}

impl InstanceGuard {
    pub fn default_path() -> PathBuf {
        thinsql_core::config::AppConfig::dir().join("thinsql.lock")
    }

    pub fn acquire(path: PathBuf) -> Result<Self, AcquireError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(AcquireError::Io)?;
        }

        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(AcquireError::Io)?;

        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(AcquireError::AlreadyRunning(AlreadyRunning {
                    pid: read_pid(&path),
                }));
            }
            Err(std::fs::TryLockError::Error(err)) => return Err(AcquireError::Io(err)),
        }

        // 锁已到手，文件归本进程所有；截断写入自身 PID 供提示展示。
        // 写失败不影响互斥，只影响提示信息，记录日志即可。
        if let Err(err) = write_pid(&mut file, std::process::id()) {
            tracing::warn!(error = %err, path = %path.display(), "写入单实例锁 PID 失败");
        }

        Ok(Self { _file: file })
    }
}

fn write_pid(file: &mut fs::File, pid: u32) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{pid}")
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::{Mutex, MutexGuard};

    /// flock 的内核语义会被同进程内的 fork+exec 短暂干扰：spawn 出的子进程
    /// 在 exec 前持有父进程全部 fd 的副本，会使锁的释放/竞争结果变得不可靠
    /// （child 进程测试正是这样 spawn 的）。因此本模块所有测试串行执行。
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn contention_then_release_keeps_lock_file() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinsql.lock");
        let first = InstanceGuard::acquire(path.clone()).expect("首个实例应成功");
        assert!(
            InstanceGuard::acquire(path.clone()).is_err(),
            "第二个必须被拒"
        );
        assert_eq!(read_pid(&path), Some(std::process::id()));
        drop(first);
        assert!(path.exists(), "不能删除锁文件，否则会产生 inode 竞态");
        let _third = InstanceGuard::acquire(path).expect("释放后应可重新获取");
    }

    #[test]
    fn unlocked_pid_contents_do_not_prevent_acquisition() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinsql.lock");
        for contents in [
            "4294967295\n".to_string(),
            "invalid PID\n".into(),
            format!("{}\n", std::process::id()),
        ] {
            fs::write(&path, contents).unwrap();
            let guard =
                InstanceGuard::acquire(path.clone()).expect("过期、非法或复用 PID 不能阻止启动");
            assert_eq!(read_pid(&path), Some(std::process::id()));
            drop(guard);
        }
    }

    #[test]
    fn held_os_lock_rejects_missing_or_invalid_pid_without_modifying_file() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinsql.lock");
        for contents in ["", "invalid PID\n", "4294967295\n"] {
            fs::write(&path, contents).unwrap();
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            file.try_lock().unwrap();
            assert!(
                InstanceGuard::acquire(path.clone()).is_err(),
                "互斥只能依赖 OS 锁"
            );
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                contents,
                "竞争者不能截断或替换锁文件"
            );
        }
    }

    #[test]
    fn missing_parent_directories_are_created() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing/nested/thinsql.lock");
        let _guard = InstanceGuard::acquire(path.clone()).unwrap();
        assert!(path.is_file(), "首次启动必须真正创建并锁定文件");
        assert!(InstanceGuard::acquire(path).is_err());
    }

    #[test]
    fn io_failure_does_not_allow_startup() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("not-a-directory");
        fs::write(&parent, "file").unwrap();
        assert!(InstanceGuard::acquire(parent.join("thinsql.lock")).is_err());
        assert!(InstanceGuard::acquire(dir.path().to_path_buf()).is_err());
    }

    #[test]
    fn dropping_guard_keeps_same_inode() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinsql.lock");
        let guard = InstanceGuard::acquire(path.clone()).unwrap();
        let observer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        drop(guard);
        assert!(path.exists());
        observer.try_lock().unwrap();
        assert!(
            InstanceGuard::acquire(path).is_err(),
            "新实例必须锁定相同 inode"
        );
    }

    #[test]
    fn child_process_contention_and_exit_release() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinsql.lock");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "single_instance::tests::child_lock_holder",
                "--ignored",
                "--nocapture",
            ])
            .env("THINSQL_TEST_LOCK_PATH", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        let ready = loop {
            line.clear();
            match output.read_line(&mut line) {
                Ok(0) | Err(_) => break false,
                Ok(_) if line.trim() == "LOCK_READY" => break true,
                Ok(_) => {}
            }
        };
        if !ready {
            // 失败路径也要回收子进程，不能遗留一个等待 stdin 的进程
            let _ = child.kill();
            let _ = child.wait();
            panic!("子进程未成功加锁");
        }
        // 先让子进程退出再断言，确保失败时也不遗留等待 stdin 的子进程。
        let contention = InstanceGuard::acquire(path.clone());
        child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(contention.is_err(), "另一个进程持锁时必须拒绝启动");
        assert!(path.exists(), "进程退出不应删除锁文件");
        let _guard = InstanceGuard::acquire(path).expect("进程退出后 OS 必须释放锁");
    }

    #[test]
    #[ignore = "仅由 child_process_contention_and_exit_release 作为子进程执行"]
    fn child_lock_holder() {
        let _serial = serial();
        let path = std::env::var_os("THINSQL_TEST_LOCK_PATH").expect("缺少测试锁路径");
        let _guard = InstanceGuard::acquire(path.into()).unwrap();
        println!("LOCK_READY");
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        // 不运行 guard 的析构函数，验证 OS 在进程退出时释放文件锁。
        std::process::exit(0);
    }
}
