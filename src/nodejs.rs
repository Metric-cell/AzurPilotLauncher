//! Windows Node.js 探测与用户级便携安装。
//!
//! 缺失与过旧共用一个安装流程：只下载固定摘要的官方 HTTPS zip，先在
//! 安装根的 .launcher/nodejs 随机目录内解压并验证，再原子发布。不会运行 MSI、写
//! HKLM/系统 PATH、使用 ProgramData 或改变目录 ACL。

#[cfg(any(windows, test))]
mod source;
#[cfg(any(windows, test))]
mod storage;

#[cfg(windows)]
use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[cfg(windows)]
use anyhow::{bail, Context, Result};
#[cfg(windows)]
use rust_i18n::t;
#[cfg(windows)]
use sha2::{Digest, Sha256};
#[cfg(windows)]
use tracing::{info, warn};
#[cfg(windows)]
use winapi::um::sysinfoapi::GetSystemDirectoryW;

#[cfg(windows)]
use crate::{
    process_security::OwnedProcessGroup, setup::SplashUpdate, window_util::CreateNoWindow as _,
};
#[cfg(windows)]
use source::{
    archive_for_architecture, meets_minimum_version, parse_version, validate_source, NodeArchive,
};

#[cfg(windows)]
pub const NODEJS_MIN_FRONTEND_VERSION_TEXT: &str = source::MIN_FRONTEND_VERSION_TEXT;
#[cfg(windows)]
const NODEJS_DOWNLOAD_BUFFER_BYTES: usize = 64 * 1024;
#[cfg(windows)]
const NODEJS_REGISTRY_PATH: &str = r"SOFTWARE\Node.js";
#[cfg(windows)]
const WINDOWS_CURRENT_VERSION_REGISTRY_PATH: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion";

#[cfg(windows)]
#[derive(Debug)]
struct NodeJsInstallation {
    executable: PathBuf,
    version: String,
    parsed: (u32, u32, u32),
}

#[cfg(windows)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeJsAvailability {
    Missing,
    Outdated(String),
    Ready,
}

#[cfg(windows)]
fn private_nodejs_root() -> Result<PathBuf> {
    Ok(crate::runtime_paths::state_dir().join("nodejs"))
}

#[cfg(windows)]
fn private_nodejs_directory() -> Result<PathBuf> {
    Ok(private_nodejs_root()?.join(archive_for_architecture(env::consts::ARCH)?.directory_name()))
}

#[cfg(windows)]
fn nodejs_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(directory) = private_nodejs_directory() {
        candidates.push(directory.join("node.exe"));
    }
    candidates.extend(system_nodejs_candidates());
    candidates
}

#[cfg(windows)]
pub fn is_nodejs_available() -> NodeJsAvailability {
    let mut outdated = None;
    for executable in nodejs_candidates() {
        let Some(installation) = probe_node(&executable) else {
            continue;
        };
        if !meets_minimum_version(installation.parsed) {
            warn!(version = %installation.version, executable = %installation.executable.display(), "Node.js runtime is below the frontend requirement");
            outdated = Some(installation.version);
            continue;
        }
        if let Some(directory) = installation.executable.parent() {
            prepend_to_path(directory);
        }
        info!(version = %installation.version, executable = %installation.executable.display(), "Node.js runtime is available");
        return NodeJsAvailability::Ready;
    }
    outdated
        .map(NodeJsAvailability::Outdated)
        .unwrap_or(NodeJsAvailability::Missing)
}

#[cfg(windows)]
pub fn install_nodejs(
    availability: &NodeJsAvailability,
    cancel_requested: &AtomicBool,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<()> {
    if matches!(availability, NodeJsAvailability::Ready) {
        return Ok(());
    }
    check_cancelled(cancel_requested)?;
    let archive = archive_for_architecture(env::consts::ARCH)?;
    validate_source(archive)?;
    let root = private_nodejs_root()?;
    fs::create_dir_all(&root)
        .with_context(|| format!("create user Node.js directory {}", root.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(&root)?;
    let archive_path = staging.path().join("nodejs.zip");
    status_updater(SplashUpdate::loading(
        t!("setup.installing_nodejs"),
        t!("setup.downloading_nodejs", version = archive.version),
        5,
    ));
    download_nodejs_archive(archive, &archive_path, cancel_requested)?;
    storage::verify_archive(&archive_path, archive.sha256, cancel_requested)?;
    check_cancelled(cancel_requested)?;
    status_updater(SplashUpdate::loading(
        t!("setup.installing_nodejs"),
        t!("setup.installing_nodejs"),
        7,
    ));
    let extracted = staging.path().join("runtime");
    extract_nodejs_zip(&archive_path, &extracted, cancel_requested)?;
    let installed = extracted.join("node.exe");
    if !probe_node(&installed)
        .is_some_and(|installation| meets_minimum_version(installation.parsed))
    {
        bail!(t!("errors.nodejs_exe_missing"));
    }
    // 验证完成前不触碰现有安装，取消只删除本次随机暂存目录。
    check_cancelled(cancel_requested)?;
    let target = root.join(archive.directory_name());
    storage::publish_runtime(&extracted, &target)?;
    // 立即设置子进程 PATH；不依赖调用方再做一次可用性探测。
    prepend_to_path(&target);
    Ok(())
}

#[cfg(windows)]
fn download_nodejs_archive(
    archive: NodeArchive,
    path: &Path,
    cancelled: &AtomicBool,
) -> Result<()> {
    // blocking reqwest 的 timeout 同样约束每次 Read；取消在最迟一次 20 秒
    // 网络等待后被观察，总下载另设 15 分钟上限。
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(20))
        .build()?;
    let started = Instant::now();
    check_cancelled(cancelled)?;
    let mut response = client.get(archive.url).send()?.error_for_status()?;
    if !response.status().is_success() {
        bail!("Node.js archive returned HTTP {}", response.status());
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; NODEJS_DOWNLOAD_BUFFER_BYTES];
    loop {
        check_cancelled(cancelled)?;
        if started.elapsed() > Duration::from_secs(15 * 60) {
            bail!("Node.js archive download timed out");
        }
        let count = response.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        file.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
    }
    file.flush()?;
    let actual = format!("{:x}", digest.finalize());
    if !actual.eq_ignore_ascii_case(archive.sha256) {
        bail!(
            "Node.js archive checksum mismatch: expected {}, got {actual}",
            archive.sha256
        );
    }
    Ok(())
}

#[cfg(windows)]
fn extract_nodejs_zip(archive: &Path, target: &Path, cancelled: &AtomicBool) -> Result<()> {
    check_cancelled(cancelled)?;
    fs::create_dir(target)?;
    let mut command = Command::new(system_powershell_path()?);
    command.args([
        "-NoProfile", "-NonInteractive", "-Command",
        "Expand-Archive -LiteralPath $env:AZURPILOT_NODE_ARCHIVE -DestinationPath $env:AZURPILOT_NODE_TARGET -ErrorAction Stop",
    ]);
    command.env("AZURPILOT_NODE_ARCHIVE", archive);
    command.env("AZURPILOT_NODE_TARGET", target);
    command.stdin(Stdio::null());
    let mut child = OwnedProcessGroup::spawn(command.create_no_window())?;
    let status = loop {
        if cancelled.load(Ordering::Acquire) {
            child.terminate()?;
            bail!(t!("setup.cancel_cleaning"));
        }
        if let Some(status) = child.try_wait()? {
            // 回收解压器及其可能派生的 worker 后再使用解压结果。
            child.terminate()?;
            break status;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        bail!(t!(
            "errors.nodejs_extract_exit",
            status = status.to_string()
        ));
    }
    storage::flatten_single_child_directory(target)?;
    if !target.join("node.exe").is_file() {
        bail!(t!(
            "errors.nodejs_archive_incomplete",
            path = target.join("node.exe").display().to_string()
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn system_powershell_path() -> Result<PathBuf> {
    let mut buffer = vec![0u16; 260];
    loop {
        let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 {
            bail!(t!("errors.windows_system_dir_not_found"));
        }
        let length = length as usize;
        if length >= buffer.len() {
            buffer.resize(length.saturating_add(1), 0);
            continue;
        }
        let powershell = PathBuf::from(OsString::from_wide(&buffer[..length]))
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if powershell.is_file() {
            return Ok(powershell);
        }
        bail!(t!(
            "errors.powershell_not_found",
            path = powershell.display().to_string()
        ));
    }
}

#[cfg(windows)]
fn probe_node(executable: &Path) -> Option<NodeJsInstallation> {
    if !executable.is_absolute() || !executable.is_file() {
        return None;
    }
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = OwnedProcessGroup::spawn(command.create_no_window()).ok()?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(3) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout_pipe = child.take_stdout()?;
    child.terminate().ok()?;
    if !status.success() {
        return None;
    }
    let mut stdout = Vec::new();
    stdout_pipe.take(4096).read_to_end(&mut stdout).ok()?;
    let parsed = parse_version(&stdout)?;
    Some(NodeJsInstallation {
        executable: executable.to_owned(),
        version: format!("{}.{}.{}", parsed.0, parsed.1, parsed.2),
        parsed,
    })
}

#[cfg(windows)]
fn system_nodejs_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(key) = windows_registry::LOCAL_MACHINE.open(NODEJS_REGISTRY_PATH) {
        if let Ok(directory) = key.get_string("InstallPath") {
            push_unique_absolute_path(&mut candidates, PathBuf::from(directory).join("node.exe"));
        }
    }
    if let Ok(key) = windows_registry::LOCAL_MACHINE.open(WINDOWS_CURRENT_VERSION_REGISTRY_PATH) {
        for name in [
            "ProgramFilesDir",
            "ProgramW6432Dir",
            "ProgramFilesDir (x86)",
        ] {
            if let Ok(directory) = key.get_string(name) {
                push_unique_absolute_path(
                    &mut candidates,
                    PathBuf::from(directory).join("nodejs").join("node.exe"),
                );
            }
        }
    }
    candidates
}

#[cfg(windows)]
fn push_unique_absolute_path(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if candidate.is_absolute()
        && !paths
            .iter()
            .any(|existing| same_windows_path(existing, &candidate))
    {
        paths.push(candidate);
    }
}

#[cfg(windows)]
fn same_windows_path(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

#[cfg(windows)]
fn prepend_to_path(directory: &Path) {
    let existing = env::var_os("PATH").unwrap_or_default();
    // 去重后始终放到最前，防止旧系统 Node 在 PATH 中仍然遮蔽新便携版本。
    let paths = std::iter::once(directory.to_owned())
        .chain(env::split_paths(&existing).filter(|path| !same_windows_path(path, directory)));
    if let Ok(path) = env::join_paths(paths) {
        env::set_var("PATH", path);
    }
}

#[cfg(windows)]
fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!(t!("setup.cancel_cleaning"));
    }
    Ok(())
}
