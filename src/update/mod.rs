//! 启动器自更新：默认兼容原发行服务，可由维护者显式选择独立签名协议。
//!
//! 主进程只负责把 [`Progress`] 转成界面消息，并在 [`Outcome::Restarted`]
//! 后退出。网络策略、签名、回退保护、下载和替换均封装在此模块。
mod install;
mod legacy;
mod manifest;
mod transport;

use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};

use install::{Installation, StagingDir, UpdateVerifier};
use manifest::Config;
use transport::HttpTransport;

#[cfg(windows)]
pub(crate) const NO_CONSOLE_ENV: &str = "AZURPILOT_NO_ATTACH_CONSOLE";
const SKIP_ENV: &str = "AZURPILOT_SKIP_LAUNCHER_UPDATE";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Progress {
    Available { version: String },
    Downloading { downloaded: u64, total: u64 },
    Verifying,
    Restarting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Skipped,
    Current,
    Cancelled,
    Restarted,
}

enum UpdateConfig {
    Legacy(legacy::Config),
    Signed(Config),
}

impl UpdateConfig {
    fn from_values(
        url: Option<&str>,
        key: Option<&str>,
        payload_origins: Option<&str>,
        metadata_origins: Option<&str>,
        channel: Option<&str>,
        min_sequence: Option<&str>,
        legacy_url: &str,
    ) -> Result<Self> {
        let signed_requested = [url, key, payload_origins, metadata_origins]
            .into_iter()
            .flatten()
            .any(|value| !value.trim().is_empty());
        if signed_requested {
            let mut config = Config::from_values(url, key, payload_origins, channel, min_sequence)?
                .context("signed updates require both a compiled manifest URL and public key")?;
            config.allow_metadata_origins(metadata_origins)?;
            return Ok(Self::Signed(config));
        }
        Ok(Self::Legacy(legacy::Config::from_values(
            legacy_url,
            Some("https://ap.launcher-update.nanoda.work/updata/stable.json"),
        )?))
    }
}

impl UpdateVerifier for UpdateConfig {
    fn staged_artifact(
        &self,
        installation: &Installation,
        snapshot_path: &Path,
        cancelled: &AtomicBool,
    ) -> Result<manifest::Artifact> {
        match self {
            Self::Legacy(config) => config.staged_artifact(installation, snapshot_path, cancelled),
            Self::Signed(config) => config.staged_artifact(installation, snapshot_path, cancelled),
        }
    }
}

/// 协议仅由编译配置选择；签名失败不能触发退回旧协议。
pub(crate) fn check_and_restart(
    cancelled: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> Result<Outcome> {
    match check_and_restart_inner(cancelled, &mut progress) {
        Err(error) if error.downcast_ref::<Cancelled>().is_some() => Ok(Outcome::Cancelled),
        result => result,
    }
}

fn check_and_restart_inner(
    cancelled: &AtomicBool,
    progress: &mut impl FnMut(Progress),
) -> Result<Outcome> {
    check_cancelled(cancelled)?;
    if std::env::var_os(SKIP_ENV).is_some() {
        std::env::remove_var(SKIP_ENV);
        return Ok(Outcome::Skipped);
    }
    let config = match compiled_config()? {
        UpdateConfig::Legacy(config) => {
            return legacy::check_and_restart(&config, cancelled, progress);
        }
        UpdateConfig::Signed(config) => config,
    };
    let installation = Installation::current(cancelled)?;
    let _lock = installation.lock(cancelled)?;
    check_cancelled(cancelled)?;
    let client = HttpTransport::new()?;
    let signed = transport::fetch_manifest(&client, &config, cancelled)?;
    let verified = config.verify(&signed, unix_now()?, true)?;
    check_cancelled(cancelled)?;
    installation.accept_manifest(&config, &verified, cancelled)?;
    let current = manifest::parse_version(env!("CARGO_PKG_VERSION"))?;
    match verified.version.cmp_precedence(&current) {
        std::cmp::Ordering::Less => {
            bail!("signed update version is older than the running launcher")
        }
        std::cmp::Ordering::Equal => {
            if is_mini(env!("CARGO_PKG_VERSION")) {
                bail!("the mini launcher requires a newer signed full launcher");
            }
            return Ok(Outcome::Current);
        }
        std::cmp::Ordering::Greater => {}
    }
    let artifact = verified.artifact(manifest::platform_key())?;
    progress(Progress::Available {
        version: verified.data.version.clone(),
    });
    check_cancelled(cancelled)?;
    let mut staging = StagingDir::create(installation.parent())?;
    staging.write_snapshot(&signed)?;
    transport::download_payload(
        &client,
        &config,
        artifact,
        &staging.payload_path(),
        progress,
        cancelled,
    )?;
    progress(Progress::Verifying);
    check_cancelled(cancelled)?;
    // 替换入口再次验证元数据和文件，不能把此前成功下载当作执行授权。
    progress(Progress::Restarting);
    check_cancelled(cancelled)?;
    installation.replace_and_restart(&config, &mut staging, cancelled)?;
    Ok(Outcome::Restarted)
}

#[derive(Debug)]
struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("launcher update was cancelled")
    }
}
impl std::error::Error for Cancelled {}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::SeqCst) {
        return Err(Cancelled.into());
    }
    Ok(())
}

fn compiled_config() -> Result<UpdateConfig> {
    UpdateConfig::from_values(
        option_env!("AZURPILOT_UPDATE_MANIFEST_URL"),
        option_env!("AZURPILOT_UPDATE_PUBLIC_KEY"),
        option_env!("AZURPILOT_UPDATE_PAYLOAD_ORIGINS"),
        option_env!("AZURPILOT_UPDATE_METADATA_ORIGINS"),
        option_env!("AZURPILOT_UPDATE_CHANNEL"),
        option_env!("AZURPILOT_UPDATE_MIN_SEQUENCE"),
        env!("LAUNCHER_UPDATE_URL"),
    )
}

fn unix_now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .context("system clock predates the Unix epoch")
}

fn is_mini(version: &str) -> bool {
    version.strip_prefix('v').unwrap_or(version) == "0.0.1"
}

/// 正式版可延期已认证但过期的可选更新；服务不可用仅放行完整离线环境。
/// 无效签名、来源、格式和载荷校验失败不触发协议降级或离线放行。
pub(crate) fn can_continue_offline(error: &anyhow::Error, version: &str, root: &Path) -> bool {
    // 已认证但过期的可选签名元数据只阻止安装更新，不能把正式启动器变成
    // 七天后无法运行的程序；mini 仍必须获取完整启动器。
    if !is_mini(version) && error.downcast_ref::<manifest::ExpiredManifest>().is_some() {
        return true;
    }
    let python = if cfg!(windows) {
        ".venv/Scripts/python.exe"
    } else {
        ".venv/bin/python"
    };
    !is_mini(version)
        && error.downcast_ref::<transport::Unavailable>().is_some()
        && root.join("gui.py").is_file()
        && root.join(python).is_file()
}

/// Windows 助手入口；普通参数不携带签名授权，必须再次完成全部验证。
pub(crate) fn try_apply_from_args() -> Result<bool> {
    #[cfg(windows)]
    {
        install::try_apply_from_args(compiled_config)
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
