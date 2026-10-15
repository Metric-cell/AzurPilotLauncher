//! Windows 便携部署的状态、依赖与缓存均随 EXE 保存在安装目录中。

use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};

pub fn state_dir() -> PathBuf {
    env::current_exe()
        .expect("Cannot locate launcher executable")
        .parent()
        .expect("Launcher executable has no parent directory")
        .join(".launcher")
}

/// 在创建窗口或启动子进程之前设置，仅影响本次启动器及其子进程。
pub fn initialize() -> Result<()> {
    let state = state_dir();
    let directories = [
        ("TEMP", state.join("temp")),
        ("TMP", state.join("temp")),
        ("UV_CACHE_DIR", state.join("cache/uv")),
        ("npm_config_cache", state.join("cache/npm")),
        ("PIP_CACHE_DIR", state.join("cache/pip")),
    ];
    for (variable, directory) in directories {
        fs::create_dir_all(&directory)
            .with_context(|| format!("create portable directory {}", directory.display()))?;
        env::set_var(variable, directory);
    }
    // 旧版同目录发行仍使用系统密钥目录，避免把密钥写入后端项目或其备份。
    if !state.starts_with(crate::setup::alas_repo_dir()) {
        env::set_var("AZURPILOT_LOCAL_DATA_DIR", &state);
    }
    fs::create_dir_all(state.join("webview2"))?;
    Ok(())
}
