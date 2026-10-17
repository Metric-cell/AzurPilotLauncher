//! Deployment configuration migration and temporary uv configuration storage.
//! Rewriting operates on strings; filesystem publication is kept at this boundary.

use anyhow::Result;
use serde_json::Value as JsonValue;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// 读取当前平台对应的内置 deploy.yaml 基线内容（编译期嵌入）。
pub(super) fn default_deploy_config() -> &'static str {
    #[cfg(windows)]
    {
        include_str!("../../deploy.windows.yaml")
    }
    #[cfg(target_os = "macos")]
    {
        include_str!("../../deploy.mac.yaml")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        include_str!("../../deploy.unix.yaml")
    }
}

/// 返回 deploy.yaml 中 Python 可执行文件的平台约定路径（.venv 内相对路径）。
fn platform_python_config_path() -> &'static str {
    if cfg!(windows) {
        "./.venv/Scripts/python.exe"
    } else {
        "./.venv/bin/python"
    }
}

/// 返回 deploy.yaml 中 adb 可执行文件的平台约定路径。
fn platform_adb_config_path() -> &'static str {
    if cfg!(windows) {
        "./.venv/Scripts/adb.exe"
    } else {
        "./.venv/bin/adb"
    }
}

/// 返回 deploy.yaml 中 git 可执行文件的平台约定路径。
pub(super) fn platform_git_config_path() -> &'static str {
    if cfg!(windows) {
        "./.venv/Scripts/git/cmd/git.exe"
    } else {
        "./.venv/bin/git"
    }
}

/// 读取解析后的 `config/deploy.yaml` 内容；文件缺失或格式错误时返回 `None`。
pub fn get_deploy_config() -> Option<JsonValue> {
    let config_content = fs::read_to_string("./config/deploy.yaml").ok()?;
    let config: JsonValue = serde_yaml::from_str(&config_content).ok()?;
    Some(config)
}

/// 迁移 `config/deploy.yaml` 到"自包含 .venv"的配置形态。
///
/// 逐行重写：可执行文件路径（Python/Adb/Git）强制指向 `.venv` 内的
/// 平台路径；移除已废弃的 `RequirementsFile`（依赖改由 uv.lock 管理）；
/// 强制 `InstallDependencies: true`；缺失的键按内置平台模板补齐。
/// 文件为空（首次启动）时直接写入内置 deploy.yaml 基线。
///
/// # Errors
/// 目录创建或文件写入失败时返回 Err。
pub(super) fn migrate_dependency_config() -> Result<()> {
    migrate_dependency_config_at(Path::new("./config/deploy.yaml"))
}

fn migrate_dependency_config_at(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let content = fs::read_to_string(path).unwrap_or_default();
    if let Some(output) = migrate_config_content(&content) {
        fs::write(path, output)?;
        info!(
            "Updated self-contained .venv settings in {}",
            path.display()
        );
    }
    Ok(())
}

/// Return rewritten content only when the original migration would publish it.
fn migrate_config_content(content: &str) -> Option<String> {
    let mut changed = false;
    // 首次启动或配置为空时，以内置的平台模板作为迁移起点
    let content = if content.trim().is_empty() {
        changed = true;
        default_deploy_config().to_owned()
    } else {
        content.to_owned()
    };

    let mut found_python_executable = false;
    let mut found_adb_executable = false;
    let mut found_git_executable = false;
    let mut found_install_dependencies = false;
    let mut output = String::with_capacity(content.len());

    for line in content.lines() {
        let indent_len = line.len() - line.trim_start().len();
        let indent = &line[..indent_len];
        let trimmed = line.trim_start();
        if trimmed.starts_with("RequirementsFile:") {
            changed = true;
            continue;
        } else if trimmed.starts_with("PythonExecutable:") {
            found_python_executable = true;
            output.push_str(indent);
            output.push_str("PythonExecutable: ");
            output.push_str(platform_python_config_path());
            changed = true;
        } else if trimmed.starts_with("AdbExecutable:") {
            found_adb_executable = true;
            output.push_str(indent);
            output.push_str("AdbExecutable: ");
            output.push_str(platform_adb_config_path());
            changed = true;
        } else if trimmed.starts_with("GitExecutable:") {
            found_git_executable = true;
            output.push_str(indent);
            output.push_str("GitExecutable: ");
            output.push_str(platform_git_config_path());
            changed = true;
        } else if trimmed.starts_with("InstallDependencies:") {
            found_install_dependencies = true;
            if line.trim() != "InstallDependencies: true" {
                output.push_str(indent);
                output.push_str("InstallDependencies: true");
                changed = true;
            } else {
                output.push_str(line);
            }
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }

    if !found_git_executable {
        output.push_str("\nGitExecutable: ");
        output.push_str(platform_git_config_path());
        output.push('\n');
        changed = true;
    }
    if !found_python_executable {
        output.push_str("PythonExecutable: ");
        output.push_str(platform_python_config_path());
        output.push('\n');
        changed = true;
    }
    if !found_adb_executable {
        output.push_str("AdbExecutable: ");
        output.push_str(platform_adb_config_path());
        output.push('\n');
        changed = true;
    }
    if !found_install_dependencies {
        output.push_str("InstallDependencies: true\n");
        changed = true;
    }

    changed.then_some(output)
}

/// A private, per-attempt uv.toml; its directory lives for exactly the command.
pub(super) struct UvIndexOverride {
    directory: Option<tempfile::TempDir>,
    path: PathBuf,
}

impl UvIndexOverride {
    pub(super) fn create(index: &str) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("azurpilot-relock-")
            .tempdir()?;
        let path = directory.path().join("uv.toml");
        // JSON string quoting is compatible with TOML basic strings for URLs.
        // Treat the configured source as data even when it contains quotes.
        let quoted_index = serde_json::to_string(index)?;
        fs::write(&path, format!("index-url = {quoted_index}\n"))?;
        Ok(Self {
            directory: Some(directory),
            path,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UvIndexOverride {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let path = directory.path().to_owned();
            if let Err(error) = directory.close() {
                warn!(
                    "Unable to remove temporary uv index override {}: {error}",
                    path.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uv_index_override_declares_only_target_index() {
        let index = "https://mirrors.cloud.tencent.com/pypi/simple/";
        let override_file = UvIndexOverride::create(index).expect("create uv index override");
        let path = override_file.path().to_path_buf();

        assert!(path.exists(), "override file should exist while held");
        let content = fs::read_to_string(&path).expect("read override file");
        assert_eq!(content, format!("index-url = \"{index}\"\n"));
        assert!(
            !content.contains("index = ["),
            "override must not re-declare the project index list, which would outrank the flag"
        );

        drop(override_file);
        assert!(
            !path.exists(),
            "override file should be removed with its guard"
        );
    }

    #[test]
    fn migration_preserves_user_settings_and_uses_platform_runtime_paths() {
        let input = "# user configuration\nRepository: https://example.invalid/repo\nPythonExecutable: system-python\nAdbExecutable: old-adb\nGitExecutable: old-git\nRequirementsFile: obsolete\nInstallDependencies: false\n";
        let output = migrate_config_content(input).unwrap();
        assert!(
            output.starts_with("# user configuration\nRepository: https://example.invalid/repo\n")
        );
        assert!(output.contains(&format!(
            "PythonExecutable: {}\n",
            platform_python_config_path()
        )));
        assert!(output.contains(&format!("AdbExecutable: {}\n", platform_adb_config_path())));
        assert!(output.contains(&format!("GitExecutable: {}\n", platform_git_config_path())));
        assert!(output.contains("InstallDependencies: true\n"));
        assert!(!output.contains("RequirementsFile:"));
        assert_eq!(migrate_config_content(&output).unwrap(), output);
    }

    #[test]
    fn missing_configuration_is_created_in_the_requested_repository() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config/deploy.yaml");
        migrate_dependency_config_at(&path).unwrap();
        let actual = fs::read_to_string(path).unwrap();
        assert_eq!(actual, migrate_config_content("").unwrap());
    }

    #[test]
    fn simultaneous_index_overrides_have_distinct_directories_and_quote_values() {
        let first = UvIndexOverride::create("https://example.invalid/simple?x=\"quoted\"").unwrap();
        let second = UvIndexOverride::create("https://pypi.org/simple").unwrap();
        assert_ne!(first.path().parent(), second.path().parent());
        let first_content = fs::read_to_string(first.path()).unwrap();
        assert!(first_content.contains("\\\"quoted\\\""));
        drop(first);
        assert!(second.path().exists());
    }
}
