//! 启动器自己的用户偏好；不依赖 WebView 存储或后端部署配置。

use std::{fs, io::Write, path::PathBuf, sync::Mutex};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub const BACKGROUND_SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloseBehavior {
    #[default]
    Ask,
    Tray,
    Exit,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Preferences {
    #[serde(default)]
    close_behavior: CloseBehavior,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreferencesSnapshot {
    pub close_behavior: CloseBehavior,
    pub background_supported: bool,
}

pub struct PreferenceStore {
    path: PathBuf,
    preferences: Mutex<Preferences>,
}

impl PreferenceStore {
    /// 缺失或损坏的偏好回退到首次询问；不会在读取时覆盖原文件。
    pub fn load(path: PathBuf) -> Self {
        let preferences = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                warn!("Unable to read launcher preferences: {error}");
                Preferences::default()
            }),
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    warn!("Unable to open launcher preferences: {error}");
                }
                Preferences::default()
            }
        };
        Self {
            path,
            preferences: Mutex::new(preferences),
        }
    }

    pub fn snapshot(&self) -> PreferencesSnapshot {
        PreferencesSnapshot {
            close_behavior: self.close_behavior(BACKGROUND_SUPPORTED),
            background_supported: BACKGROUND_SUPPORTED,
        }
    }

    /// 没有托盘的平台始终退出，避免保存的后台偏好使窗口无法恢复。
    pub fn close_behavior(&self, background_supported: bool) -> CloseBehavior {
        if background_supported {
            self.preferences.lock().unwrap().close_behavior
        } else {
            CloseBehavior::Exit
        }
    }

    pub fn set_close_behavior(&self, behavior: CloseBehavior) -> std::io::Result<()> {
        // 锁覆盖落盘与内存提交，避免设置页与关闭菜单并发写入后状态不一致。
        let mut current = self.preferences.lock().unwrap();
        let updated = Preferences {
            close_behavior: behavior,
        };
        let parent = self.path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Missing preferences directory",
            )
        })?;
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(temporary.as_file_mut(), &updated)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        *current = updated;
        info!(?behavior, "Saved launcher close behavior");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_choice_and_settings_changes_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/preferences.json");
        let store = PreferenceStore::load(path.clone());
        assert_eq!(store.close_behavior(true), CloseBehavior::Ask);
        assert!(!path.exists());
        for behavior in [CloseBehavior::Tray, CloseBehavior::Exit, CloseBehavior::Ask] {
            store.set_close_behavior(behavior).unwrap();
            assert_eq!(store.close_behavior(true), behavior);
            assert_eq!(
                PreferenceStore::load(path.clone()).close_behavior(true),
                behavior
            );
        }
    }

    #[test]
    fn failed_write_preserves_the_last_saved_choice() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preferences.json");
        let store = PreferenceStore::load(path.clone());
        store.set_close_behavior(CloseBehavior::Tray).unwrap();
        // 目标变成目录会使原子替换失败；不能把未保存的 Exit 提交到内存。
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(store.set_close_behavior(CloseBehavior::Exit).is_err());
        assert_eq!(store.close_behavior(true), CloseBehavior::Tray);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn malformed_preferences_ask_again_without_overwriting_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preferences.json");
        for bytes in [b"not json".as_slice(), br#"{"closeBehavior":"unknown"}"#] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(
                PreferenceStore::load(path.clone()).close_behavior(true),
                CloseBehavior::Ask
            );
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn platforms_without_a_tray_never_hide_the_only_window() {
        let directory = tempfile::tempdir().unwrap();
        let store = PreferenceStore::load(directory.path().join("preferences.json"));
        for behavior in [CloseBehavior::Ask, CloseBehavior::Tray, CloseBehavior::Exit] {
            store.set_close_behavior(behavior).unwrap();
            assert_eq!(store.close_behavior(false), CloseBehavior::Exit);
        }
    }
}
