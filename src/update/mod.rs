//! 已签名的启动器自更新。信任根仅来自编译期 URL、公钥和来源白名单。
//!
//! 主进程只负责把 [`Progress`] 转成界面消息，并在 [`Outcome::Restarted`]
//! 后退出。网络策略、签名、回退保护、下载和替换均封装在此模块。
mod install;
mod manifest;
mod transport;

use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use tracing::info;

use install::{Installation, StagingDir};
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
    Disabled,
    Skipped,
    Current,
    Cancelled,
    Restarted,
}

/// 检查更新，验证后替换本程序并重启。缺少 URL 或公钥时不进行任何网络请求。
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
    let Some(config) = compiled_config()? else {
        info!("Launcher self-update is disabled: no complete compiled trust configuration");
        return Ok(Outcome::Disabled);
    };
    if std::env::var_os(SKIP_ENV).is_some() {
        std::env::remove_var(SKIP_ENV);
        return Ok(Outcome::Skipped);
    }
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

fn compiled_config() -> Result<Option<Config>> {
    let mut config = Config::from_values(
        option_env!("AZURPILOT_UPDATE_MANIFEST_URL"),
        option_env!("AZURPILOT_UPDATE_PUBLIC_KEY"),
        option_env!("AZURPILOT_UPDATE_PAYLOAD_ORIGINS"),
        option_env!("AZURPILOT_UPDATE_CHANNEL"),
        option_env!("AZURPILOT_UPDATE_MIN_SEQUENCE"),
    )?;
    if let Some(config) = &mut config {
        config.allow_metadata_origins(option_env!("AZURPILOT_UPDATE_METADATA_ORIGINS"))?;
    }
    Ok(config)
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

/// 仅元数据服务的连接失败、超时或 5xx 允许完整离线安装继续启动。
/// 签名、来源、版本、格式和载荷校验失败不能转成离线放行。
pub(crate) fn can_continue_offline(error: &anyhow::Error, version: &str, root: &Path) -> bool {
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
        install::try_apply_from_args(compiled_config, unix_now)
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
