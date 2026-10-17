//! 环境搭建模块：负责 AzurPilot 运行所需的完整环境生命周期。
//!
//! 主要职责（由 main.rs 在启动画面阶段的后台线程调用）：
//! - 定位 AzurLaneAutoScript 仓库目录并切换工作目录；
//! - 通过内嵌/系统 uv 安装 Python 3.14.6、创建可重定位 `.venv`，
//!   并将 uv / git / adb 一并部署进 `.venv`（`ensure_runtime_tools`）；
//! - 经 git 拉取最新仓库代码（`git_update`，最多重试 20 次）；
//! - 按 `uv.lock` 同步 Python 依赖（`uv_sync_project`）；
//! - 迁移与校验 `config/deploy.yaml` 部署配置（`migrate_dependency_config`）；
//! - 普通发行环境在后端超时后可重建 `.venv`；取消启动保留已有环境。
//!
//! 阶段进度通过 [`SplashUpdate`] 回调上报启动画面；所有长时间操作均支持
//! 经 `cancel_requested` 原子标志请求取消。

mod config;
mod progress;
mod runtime_cleanup;
mod sources;
mod workspace;
pub use config::get_deploy_config;
use config::{migrate_dependency_config, UvIndexOverride};
use progress::{
    dependency_start_update, dependency_wait_update, is_uv_progress_line, phase_display_name,
    pypi_index_fallback_update, runtime_tools_update, runtime_wait_update, splash_retry_update,
    splash_update_for_output, GitProgressState, ScriptPhase, UvProgressState,
};
#[allow(unused_imports)] // Preserve the setup module's public progress types.
pub use progress::{get_tip, SplashUpdate, UvProgress};
use runtime_cleanup::{remove_runtime_entry_with_retry, remove_venv_for_backend_recovery};
use sources::{
    ranked_pypi_indexes, uv_python_install_mirrors, DEFAULT_PYPI_INDEX,
    DEFAULT_UV_PYTHON_INSTALL_MIRRORS,
};
pub use workspace::repository_is_development_workspace;
use workspace::{LockfileRestore, WorkspaceProtected, GIT_UPDATE_SCRIPT};

use anyhow::{anyhow, bail, Context, Result};
use std::env::set_current_dir;
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, RecvTimeoutError, Sender},
    Arc,
};
use std::thread;
use std::time::Duration;
use tracing::{info, warn};

use crate::process_security::OwnedProcessGroup;
use crate::window_util::CreateNoWindow as _;
use rust_i18n::t;

/// git 更新的最大重试次数：网络抖动时自动重试，20 次（约 20 秒）已能覆盖常见短暂断网。
const MAX_UPDATE_RETRIES: usize = 20;
/// 每次重试之间的固定间隔。
const RETRY_DELAY: Duration = Duration::from_secs(1);
/// 内置 Python 解释器版本，需与 CI 打包及 uv 安装的版本保持一致。
const PYTHON_VERSION: &str = "3.14.6";
/// 可复用 `.venv` 的最低 Python 版本：低于此版本说明环境过旧，直接删除重建以保证一致性。
const MIN_REUSABLE_VENV_PYTHON_VERSION: (u16, u16, u16) = (3, 14, 5);
/// 编译期内嵌的 uv 引导二进制：由 build.rs 依据 `ALAS_BOOTSTRAP_UV` 写入；
/// 空文件表示本地开发构建未内嵌，运行时回退到 `UV` 环境变量或 PATH 中的 uv。
const BOOTSTRAP_UV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bootstrap_uv.bin"));

/// 定位 AzurLaneAutoScript 仓库根目录。
///
/// 优先按"便携式同目录发行"查找：启动器可执行文件所在目录若存在
/// `deploy/installer.py` 即认为是仓库根（Windows/Linux 的常规布局）。
/// macOS 额外支持 `.app` 包布局：从 `AzurPilot.app/Contents/MacOS`
/// 回退到 `AzurPilot.app/Contents/AzurLaneAutoScript`。
///
/// # Panics
/// 两种布局都未命中时 panic——启动器必须与仓库同发行，无法继续运行。
pub fn alas_repo_dir() -> PathBuf {
    // 优先检查常规的"同目录便携发行"布局
    let exe_folder = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut installer_py = exe_folder.clone();
    installer_py.extend(["deploy", "installer.py"]);
    if fs::exists(installer_py).unwrap() {
        return exe_folder;
    }
    // macOS 需额外考虑 .app 包布局：AzurPilot.app/Contents/AzurLaneAutoScript
    #[cfg(target_os = "macos")]
    {
        use std::ffi::OsStr;
        if exe_folder.file_name() == Some(OsStr::new("MacOS")) {
            let mut repo_folder = exe_folder;
            repo_folder.pop();
            repo_folder.push("AzurLaneAutoScript");
            if fs::exists(&repo_folder).unwrap() {
                return repo_folder;
            }
        }
    }
    panic!("Cannot find AzurPilot repo folder");
}

/// 把给定路径前插到指定环境变量（PATH 类）的首位，使其优先级最高。
///
/// 用于让 `.venv` 内的工具链（uv/git/adb）压过系统同名工具，
/// 保证启动流程使用的是受控版本。
fn prepend_path_to_env(key: &str, path: PathBuf) {
    let mut paths = Vec::new();
    paths.push(path);
    if let Some(ref old_path) = &std::env::var_os(key) {
        paths.extend(std::env::split_paths(old_path));
    }
    std::env::set_var(key, std::env::join_paths(paths).unwrap());
}

/// 仓库内虚拟环境目录：`.venv`（所有平台一致）。
fn venv_dir() -> PathBuf {
    alas_repo_dir().join(".venv")
}

/// `.venv` 的可执行文件目录：Windows 为 `Scripts`，Unix 为 `bin`。
fn venv_bin_dir() -> PathBuf {
    let venv = venv_dir();
    if cfg!(windows) {
        venv.join("Scripts")
    } else {
        venv.join("bin")
    }
}

/// `.venv` 内 Python 解释器的完整路径。
pub fn venv_python() -> PathBuf {
    venv_bin_dir().join(if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    })
}

/// uv managed Python 的实际安装目录（`.venv/python`），与标准 venv 布局并存。
fn venv_python_install_dir() -> PathBuf {
    venv_dir().join("python")
}

/// `.venv` 内 uv 的完整路径。
fn venv_uv() -> PathBuf {
    venv_bin_dir().join(if cfg!(windows) { "uv.exe" } else { "uv" })
}

/// `.venv` 内 adb 的完整路径。
fn venv_adb() -> PathBuf {
    venv_bin_dir().join(if cfg!(windows) { "adb.exe" } else { "adb" })
}

/// `.venv` 内 git 的完整路径；Windows 发行版为 MinGit 完整布局
/// （`Scripts/git/cmd/git.exe`），Unix 为单文件 `bin/git`。
fn venv_git() -> PathBuf {
    if cfg!(windows) {
        venv_bin_dir().join("git").join("cmd").join("git.exe")
    } else {
        venv_bin_dir().join("git")
    }
}

/// git 的 `GIT_EXEC_PATH` 目录（Windows/Unix 发行包布局均含 `libexec/git-core`）。
fn venv_git_exec_path() -> PathBuf {
    venv_dir().join("libexec").join("git-core")
}

/// git 模板目录（`share/git-core/templates`），供 `GIT_TEMPLATE_DIR` 使用。
fn venv_git_template_dir() -> PathBuf {
    venv_dir().join("share").join("git-core").join("templates")
}

/// 取得本次进程用于引导的 uv 可执行文件路径。
///
/// 优先级：内嵌二进制（释放到带进程 ID 的临时目录，避免多实例冲突）>
/// `UV` 环境变量 > PATH 搜索。本地开发构建未内嵌 uv 时走后两者。
///
/// # Errors
/// 未内嵌且系统上找不到 uv 时返回 Err（提示用户 uv 未找到）。
fn bootstrap_uv_path() -> Result<PathBuf> {
    // 临时目录带进程 ID：防止同一台机器并行运行多个启动器实例时互相覆盖
    let dir = std::env::temp_dir().join(format!("azurpilot-bootstrap-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let path = dir.join(if cfg!(windows) { "uv.exe" } else { "uv" });
    if !path.exists() {
        if BOOTSTRAP_UV.is_empty() {
            if let Some(path_uv) = std::env::var_os("UV").map(PathBuf::from) {
                return Ok(path_uv);
            }
            if let Some(path_uv) = find_on_path("uv") {
                return Ok(path_uv);
            }
            bail!(t!("errors.uv_not_found"));
        }
        fs::write(&path, BOOTSTRAP_UV)
            .with_context(|| t!("errors.write_uv_failed", path = path.display().to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions)?;
        }
    }
    Ok(path)
}

/// 在 PATH 各目录中查找可执行文件（Windows 自动尝试 `.exe` 后缀）。
fn find_on_path(executable: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(executable);
        if candidate.exists() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate = dir.join(format!("{executable}.exe"));
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 建立后续所有子进程的基础环境：切换工作目录到仓库根，
/// 并把 `.venv` 工具链目录提升到 PATH 最前。
///
/// # Errors
/// 仓库目录不存在或切换工作目录失败时返回 Err。
pub fn setup_environment() -> Result<()> {
    let dir = alas_repo_dir();
    info!("AzurPilot dir is {:?}", &dir);
    set_current_dir(&dir)?;
    prepend_path_to_env("PATH", venv_bin_dir());
    if cfg!(windows) {
        // Windows MinGit 布局的 git.exe 藏在 git/cmd 下，需单独加入 PATH
        prepend_path_to_env("PATH", venv_bin_dir().join("git").join("cmd"));
    } else {
        refresh_git_environment();
    }
    Ok(())
}

/// 设置 Unix 发行包 git 所需的环境变量（执行路径与模板目录）。
///
/// 仅在对应目录真实存在时设置，避免把不存在的路径传给 git。
fn refresh_git_environment() {
    let git_exec_path = venv_git_exec_path();
    if git_exec_path.exists() {
        std::env::set_var("GIT_EXEC_PATH", git_exec_path);
    }

    let git_template_dir = venv_git_template_dir();
    if git_template_dir.exists() {
        std::env::set_var("GIT_TEMPLATE_DIR", git_template_dir);
    }
}

/// Linux 专用：探测系统 CA 证书并配置给 git。
///
/// 内嵌 git 无法自动找到发行版的 CA 证书，git 拉取 HTTPS 仓库会失败；
/// 此处经 openssl-probe 定位证书文件，仅写进程环境，不修改开发仓库配置。
#[cfg(target_os = "linux")]
fn setup_git_ca_bundle() {
    let cert_file = openssl_probe::probe().cert_file;
    if let Some(file) = cert_file.as_ref().and_then(|f| f.to_str()) {
        std::env::set_var("GIT_SSL_CAINFO", file);
    }
}

/// 环境搭建总入口：按序完成工具安装 → 配置迁移 → 仓库更新 → 依赖同步。
///
/// 流程与 `deploy/installer.py` 保持一致，各阶段经 `status_updater`
/// 上报启动画面进度；`cancel_requested` 允许取消窗口随时中止全流程。
///
/// # Errors
/// 任一阶段失败（下载、更新、同步、配置迁移）或用户取消时返回 Err，
/// 由 main.rs 决定展示错误页面。
pub fn setup_alas_repo(
    mut status_updater: impl FnMut(SplashUpdate),
    cancel_requested: Arc<AtomicBool>,
    skip_repository_update: bool,
) -> Result<()> {
    info!("Starting setup for AzurPilot repository...");
    let mut protect_workspace =
        skip_repository_update || repository_is_development_workspace(Path::new("."));
    // Linux 的内嵌 git 需要显式指定 CA 证书，必须最先配置
    #[cfg(target_os = "linux")]
    setup_git_ca_bundle();
    // 流程与 deploy/installer.py 保持一致
    status_updater(
        SplashUpdate::loading(
            t!("setup.preparing_workspace"),
            t!("setup.cleaning_cache"),
            8,
        )
        .with_subtitle(t!("setup.checking_env", tip = get_tip())),
    );
    let bootstrap_uv = bootstrap_uv_path()?;
    ensure_runtime_tools(&bootstrap_uv, &cancel_requested, &mut status_updater)?;
    protect_workspace |= repository_is_development_workspace(Path::new("."));
    if !protect_workspace {
        atomic_failure_cleanup("./config", &cancel_requested)?;
        protect_workspace |= repository_is_development_workspace(Path::new("."));
    }
    if !protect_workspace {
        migrate_dependency_config()?;
    }
    protect_workspace |= repository_is_development_workspace(Path::new("."));
    if protect_workspace {
        info!("Skipping repository update and configuration writes to preserve local workspace");
        status_updater(
            SplashUpdate::loading(
                t!("setup.skipping_update"),
                t!("setup.skipping_update_detail"),
                18,
            )
            .with_subtitle(t!("setup.preview_mode", tip = get_tip())),
        );
    } else {
        status_updater(
            SplashUpdate::loading(t!("setup.updating"), t!("setup.fetching_patches"), 18)
                .with_subtitle(t!("setup.syncing", tip = get_tip())),
        );
        git_update(&mut status_updater, &bootstrap_uv, &cancel_requested)?;
    }
    protect_workspace |= repository_is_development_workspace(Path::new("."));
    status_updater(
        SplashUpdate::loading(t!("setup.installing_deps"), t!("setup.verifying_deps"), 64)
            .with_subtitle(t!("setup.syncing_deps", tip = get_tip())),
    );
    uv_sync_project_with_protection(
        &mut status_updater,
        &bootstrap_uv,
        &cancel_requested,
        protect_workspace,
    )?;
    status_updater(
        SplashUpdate::loading(t!("setup.finishing"), t!("setup.ready_to_launch"), 94)
            .with_subtitle(t!("setup.launching", tip = get_tip())),
    );
    Ok(())
}

/// 后端启动超时后的恢复流程：删除 `.venv` 并重建、重新同步依赖。
///
/// 与完整 setup 不同，此流程跳过 git 更新与配置迁移，只处理
/// Python 环境，用于"依赖损坏导致后端起不来"的场景兜底。
///
/// # Errors
/// 用户取消、`.venv` 删除失败或依赖同步失败时返回 Err。
pub fn rebuild_venv_and_sync_dependencies(
    mut status_updater: impl FnMut(SplashUpdate),
    cancel_requested: Arc<AtomicBool>,
    preserve_workspace: bool,
) -> Result<()> {
    // 调用者的超时分支之外也保留保护，避免未来重用恢复入口时删除开发环境。
    if preserve_workspace {
        bail!(t!("errors.workspace_protected"));
    }
    if cancel_requested.load(Ordering::SeqCst) {
        bail!(t!("setup.cancel_cleaning"));
    }

    status_updater(
        SplashUpdate::loading(
            t!("setup.preparing_env"),
            t!("setup.backend_timeout_recovery"),
            97,
        )
        .with_subtitle(t!("setup.rebuilding_env", tip = get_tip())),
    );
    remove_venv_for_backend_recovery(&alas_repo_dir(), &venv_dir(), &cancel_requested)?;

    let bootstrap_uv = bootstrap_uv_path()?;
    ensure_runtime_tools(&bootstrap_uv, &cancel_requested, &mut status_updater)?;
    if cancel_requested.load(Ordering::SeqCst) {
        bail!(t!("setup.cancel_cleaning"));
    }

    status_updater(
        SplashUpdate::loading(t!("setup.installing_deps"), t!("setup.verifying_deps"), 97)
            .with_subtitle(t!("setup.syncing_deps", tip = get_tip())),
    );
    uv_sync_project_with_protection(
        &mut status_updater,
        &bootstrap_uv,
        &cancel_requested,
        preserve_workspace || repository_is_development_workspace(Path::new(".")),
    )
}

/// 在独立线程中逐块读取子进程输出流，按自定义协议切分事件并转发。
///
/// git/uv 的进度输出使用"前缀:内容"格式（如 uv 的 `Resolved: ...`），
/// 本函数按 ASCII 范围识别可打印字符：遇到非可打印字符（进度条
/// 回车重绘、二进制块等）立即截断发送，避免进度条刷屏；遇到冒号
/// 时若发现重复前缀模式则切分出完整事件提前发送。最终每个事件以
/// `(is_err, 文本)` 形式送入通道，由主循环统一消费。
fn pipe_lines(read: impl Read + Send + 'static, tx: Sender<(bool, String)>, is_err: bool) {
    thread::spawn(move || {
        let mut reader = BufReader::new(read);
        let mut buffer = "".to_owned();
        loop {
            let mut line = [0u8; 64];
            match reader.read(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(size) => {
                    for c in &line[0..size] {
                        // 非 ASCII 可打印字符：视为进度重绘等噪声，截断当前缓冲
                        if *c < 32 || *c > 127 {
                            if !buffer.is_empty() {
                                let _ = tx.send((is_err, buffer));
                                buffer = "".to_owned();
                            }
                        } else if *c as char == ':' {
                            // "A:A..." 形式的重复前缀说明上一事件已完整，切分发送
                            let mut cut = 0usize;
                            if let Some((l, r)) = buffer.split_once(':') {
                                if r.ends_with(l) {
                                    cut = r.len() + 1;
                                }
                            }
                            if cut > 0 {
                                let (l, r) = buffer.split_at(cut);
                                let _ = tx.send((is_err, l.to_owned()));
                                buffer = r.to_owned();
                            }
                            buffer.push(*c as char);
                        } else {
                            buffer.push(*c as char);
                        }
                    }
                }
            }
        }
        if !buffer.is_empty() {
            let _ = tx.send((is_err, buffer));
        }
    });
}

/// 运行一个长时间子命令（git 更新或 uv 同步），实时解析输出并上报进度。
///
/// 子进程的 stdout/stderr 各由一个读取线程消费（见 [`pipe_lines`]），
/// 主循环经通道收取事件：可识别为进度的行驱动启动画面更新，普通
/// 输出写日志，stderr 行同时记为"最后错误"用于失败时展示。
/// 依赖阶段在 1 秒无输出时也会发出等待进度，避免画面长时间静止。
///
/// # Errors
/// 用户取消（会先杀掉子进程）、子进程启动失败或非零退出时返回 Err。
fn run_command(
    cmd: &mut Command,
    mut status_updater: impl FnMut(SplashUpdate),
    phase: ScriptPhase,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    let is_deps = matches!(phase, ScriptPhase::Dependencies);

    if cancel_requested.load(Ordering::SeqCst) {
        bail!(t!("setup.cancel_cleaning"));
    }
    let mut child = OwnedProcessGroup::spawn(
        cmd.create_no_window()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;

    // 接收读取线程输出的通道：(是否 stderr, 行文本)
    let (tx, rx) = mpsc::channel::<(bool, String)>();

    // 为 stdout 启动一个读取线程
    if let Some(stdout) = child.take_stdout() {
        pipe_lines(stdout, tx.clone(), false);
    }

    // 为 stderr 启动一个读取线程
    if let Some(stderr) = child.take_stderr() {
        pipe_lines(stderr, tx.clone(), true);
    }

    // 主动丢弃原始 sender：两个读取线程结束后通道即关闭，主循环可退出
    drop(tx);

    let mut last_err = "".to_owned();
    let mut git_progress = GitProgressState::default();
    let mut uv_progress = UvProgressState::new();
    // 依赖阶段起点 64%：前半段留给 git 更新，保证总进度单调
    let mut dependency_progress = 64u8;
    let mut dependency_elapsed_secs = 0u16;

    let mut exit_status = None;
    let mut output_closed = false;
    let mut cleanup_finished_at = None;
    // Output lifetime and process lifetime are independent: a worker can retain
    // a pipe after the root exits, and a running root can close both pipes.
    let status = loop {
        if cancel_requested.load(Ordering::SeqCst) {
            let _ = child.terminate();
            bail!(t!("setup.cancel_cleaning"));
        }
        if exit_status.is_none() {
            if let Some(status) = child.try_wait()? {
                child.terminate()?;
                exit_status = Some(status);
                cleanup_finished_at = Some(std::time::Instant::now());
            }
        }
        if output_closed {
            if let Some(status) = exit_status {
                break status;
            }
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        if cleanup_finished_at.is_some_and(|time| time.elapsed() >= Duration::from_secs(1)) {
            // Normally both readers report EOF immediately after group cleanup.
            // A process that deliberately escaped the Unix session cannot hold
            // this worker forever merely by retaining an inherited pipe.
            break exit_status.unwrap();
        }
        let poll_interval = if exit_status.is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        match rx.recv_timeout(poll_interval) {
            Ok((is_err, line)) => {
                if let Some(mut update) =
                    splash_update_for_output(&line, phase, &mut git_progress, &mut uv_progress)
                {
                    // 进度只前进不回退（max 保护），避免输出乱序导致画面跳动
                    if is_deps {
                        update.progress = update.progress.max(dependency_progress);
                        dependency_progress = update.progress;
                    }
                    status_updater(update);
                }

                if is_err {
                    // uv 的进度日志走 stderr，属正常输出降级为 info；
                    // 其余 stderr 视为错误信息并记住最后一行用于失败提示
                    if is_deps && is_uv_progress_line(&line) {
                        info!("{line}");
                    } else {
                        warn!("{line}");
                        last_err = line;
                    }
                } else {
                    info!("{line}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if is_deps && exit_status.is_none() {
                    dependency_elapsed_secs = dependency_elapsed_secs.saturating_add(1);
                    let update = dependency_wait_update(
                        dependency_elapsed_secs,
                        dependency_progress,
                        &mut uv_progress,
                    );
                    dependency_progress = update.progress;
                    status_updater(update);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                output_closed = true;
            }
        }
    };

    // Check the root status after reclaiming all remaining owned workers.
    if !status.success() {
        if matches!(phase, ScriptPhase::Git) && status.code() == Some(73) {
            return Err(WorkspaceProtected.into());
        }
        // stderr 为空时用阶段默认文案兜底，保证错误提示永远有内容
        if last_err.is_empty() {
            last_err = match phase {
                ScriptPhase::Git => t!("setup.update_failed").to_string(),
                ScriptPhase::Dependencies => t!("setup.deps_failed").to_string(),
            };
        }
        return Err(anyhow!(last_err));
    }
    Ok(())
}

/// 带重试的命令执行封装：失败后按固定间隔重试至 [`MAX_UPDATE_RETRIES`] 次。
///
/// 每次失败都会向启动画面上报"第 N/20 次重试"状态；取消请求在
/// 每轮循环开头检查。
///
/// # Errors
/// 用户取消或重试用尽仍失败时返回最后一次的错误。
fn run_command_with_retry(
    build_cmd: impl Fn() -> Command,
    mut status_updater: impl FnMut(SplashUpdate),
    phase: ScriptPhase,
    cancel_requested: &AtomicBool,
    protect_existing_checkout: bool,
) -> Result<()> {
    for retry in 0..=MAX_UPDATE_RETRIES {
        if cancel_requested.load(Ordering::SeqCst) {
            bail!(t!("setup.cancel_cleaning"));
        }
        if protect_existing_checkout
            && matches!(phase, ScriptPhase::Git)
            && repository_is_development_workspace(Path::new("."))
        {
            return Err(WorkspaceProtected.into());
        }

        match run_command(
            &mut build_cmd(),
            &mut status_updater,
            phase,
            cancel_requested,
        ) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if err.is::<WorkspaceProtected>() {
                    return Err(err);
                }
                if retry == MAX_UPDATE_RETRIES {
                    return Err(err);
                }

                let retry_count = retry + 1;
                let error_text = err.to_string();
                warn!(
                    "{} failed (retry {retry_count}/{MAX_UPDATE_RETRIES}): {error_text}",
                    phase_display_name(phase)
                );
                status_updater(splash_retry_update(
                    phase,
                    retry_count,
                    MAX_UPDATE_RETRIES,
                    &error_text,
                ));
                thread::sleep(RETRY_DELAY);
            }
        }
    }

    unreachable!()
}

/// 运行子命令并等待退出状态，支持取消，无进度回调。
///
/// 供环境搭建流程中的短命令使用（文件复制、目录创建等），
/// 不解析输出、不上报进度。
///
/// # Errors
/// 用户取消或子进程启动失败时返回 Err（退出码非零由调用方判断）。
pub fn run_status_command(
    cmd: &mut Command,
    cancel_requested: &AtomicBool,
) -> Result<std::process::ExitStatus> {
    run_status_command_with_tick(cmd, cancel_requested, || {})
}

/// 带周期回调的 [`run_status_command`] 内部实现。
///
/// `on_tick` 在每次轮询（约 100ms）时触发，供调用方刷新
/// "仍在等待"类的状态展示。
///
/// # Errors
/// 用户取消（先杀子进程）或启动失败时返回 Err。
fn run_status_command_with_tick(
    cmd: &mut Command,
    cancel_requested: &AtomicBool,
    mut on_tick: impl FnMut(),
) -> Result<std::process::ExitStatus> {
    if cancel_requested.load(Ordering::SeqCst) {
        bail!(t!("setup.cancel_cleaning"));
    }
    let mut child = OwnedProcessGroup::spawn(cmd.create_no_window())?;
    loop {
        if cancel_requested.load(Ordering::SeqCst) {
            let _ = child.terminate();
            bail!(t!("setup.cancel_cleaning"));
        }

        if let Some(status) = child.try_wait()? {
            child.terminate()?;
            return Ok(status);
        }
        on_tick();
        thread::sleep(Duration::from_millis(100));
    }
}

/// 经内嵌 Python 脚本调用 `deploy.git.GitManager` 拉取最新仓库代码。
///
/// 不直接调用 git，而是复用上游 ALAS 的 Python 更新逻辑，保证更新
/// 行为（远程配置、冲突处理）与官方安装器一致；脚本会给 `execute`
/// 打补丁，为 fetch 命令强制加上 `--progress` 以便解析进度条。
/// 失败时由 [`run_command_with_retry`] 自动重试（最多 20 次）。
///
/// # Errors
/// 用户取消或重试用尽仍失败时返回 Err。
fn git_update(
    status_updater: impl FnMut(SplashUpdate),
    bootstrap_uv: &Path,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    // Decorate execute() to get fetch progress
    let script = GIT_UPDATE_SCRIPT;
    let python = venv_python();
    let bootstrap_uv = bootstrap_uv.to_path_buf();
    let protect_existing_checkout = Path::new(".git").exists();
    run_command_with_retry(
        || {
            let mut cmd = Command::new(&python);
            cmd.args(["-c", script])
                .env("AZURPILOT_BOOTSTRAP_UV", &bootstrap_uv)
                .env(
                    "AZURPILOT_RELEASE_BOOTSTRAP",
                    if protect_existing_checkout { "0" } else { "1" },
                );
            isolate_python_child_environment(&mut cmd);
            bypass_proxy_for_child(&mut cmd);
            cmd
        },
        status_updater,
        ScriptPhase::Git,
        cancel_requested,
        protect_existing_checkout,
    )
}

/// 按 `uv.lock` 同步 Python 依赖，带 PyPI 镜像源的多级回退。
///
/// 依次尝试 [`ranked_pypi_indexes`] 排序后的镜像源：每个源先重锁
/// `uv.lock` 再同步；同步失败会重锁重试一次，仍失败则换下一个源。
/// 全部源用尽后返回最后一次错误。
///
/// # Errors
/// 用户取消或所有候选源均失败时返回 Err。
fn uv_sync_project_with_protection(
    mut status_updater: impl FnMut(SplashUpdate),
    bootstrap_uv: &Path,
    cancel_requested: &AtomicBool,
    protect_workspace: bool,
) -> Result<()> {
    if protect_workspace {
        info!("Preserving development configuration and uv.lock; syncing frozen dependencies");
        let mut cmd = uv_sync_command(bootstrap_uv, DEFAULT_PYPI_INDEX);
        cmd.arg("--frozen");
        return run_command(
            &mut cmd,
            &mut status_updater,
            ScriptPhase::Dependencies,
            cancel_requested,
        );
    }
    // Mirror resolution is a runtime concern. Restore the published lockfile
    // even on failure so the next launch is not mistaken for a dirty checkout.
    let _lock_restore = LockfileRestore::capture(Path::new("uv.lock"))?;
    let bootstrap_uv = bootstrap_uv.to_path_buf();
    let indexes = ranked_pypi_indexes(deploy_pypi_mirror().as_deref());
    let next_indexes = indexes.clone();
    let mut last_error = None;

    for (attempt, index) in indexes.iter().enumerate() {
        if cancel_requested.load(Ordering::SeqCst) {
            bail!(t!("setup.cancel_cleaning"));
        }

        info!("Syncing dependencies with PyPI index: {index}");
        status_updater(dependency_start_update());

        // uv.lock 锁定了下载 URL，且 uv 只有在锁定的 URL 失效时才会重写，
        // 所以必须先把锁解析到当前镜像源上——这决定了实际从哪个源下载；
        // 无法完成解析的源等于不可用，直接跳过。
        if !relock_onto_index(&bootstrap_uv, index, cancel_requested, &mut last_error) {
            if !disable_index(index, &next_indexes, attempt, &mut status_updater) {
                break;
            }
            continue;
        }

        if sync_succeeds(
            &bootstrap_uv,
            index,
            &mut status_updater,
            cancel_requested,
            &mut last_error,
        ) {
            return Ok(());
        }

        // 探测可达的镜像仍可能在下载环节失败（403、限流、CDN 损坏、
        // 包格式不被 uv 识别等），先重锁一次再试，然后才判定该源不可用。
        info!("Retrying dependency sync with relocked URLs for {index}");
        if relock_onto_index(&bootstrap_uv, index, cancel_requested, &mut last_error)
            && sync_succeeds(
                &bootstrap_uv,
                index,
                &mut status_updater,
                cancel_requested,
                &mut last_error,
            )
        {
            return Ok(());
        }

        if !disable_index(index, &next_indexes, attempt, &mut status_updater) {
            break;
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!(t!("setup.deps_failed").to_string())))
}

/// 重锁 `uv.lock`，使其下载 URL 全部指向指定镜像源；失败记录到 last_error。
///
/// 返回 true 表示重锁成功、该源可用；false 表示源无法解析依赖，
/// 应跳过换源。
fn relock_onto_index(
    bootstrap_uv: &Path,
    index: &str,
    cancel_requested: &AtomicBool,
    last_error: &mut Option<anyhow::Error>,
) -> bool {
    match uv_relock_project(bootstrap_uv, index, cancel_requested) {
        Ok(()) => {
            info!("Re-resolved uv.lock onto PyPI index: {index}");
            true
        }
        Err(err) => {
            warn!("Failed to relock dependencies against {index}: {err}");
            *last_error = Some(err);
            false
        }
    }
}

/// 执行一次依赖同步尝试；失败时记录错误并返回 false，由调用方决定回退。
fn sync_succeeds(
    bootstrap_uv: &Path,
    index: &str,
    status_updater: &mut impl FnMut(SplashUpdate),
    cancel_requested: &AtomicBool,
    last_error: &mut Option<anyhow::Error>,
) -> bool {
    let mut cmd = uv_sync_command(bootstrap_uv, index);
    match run_command(
        &mut cmd,
        status_updater,
        ScriptPhase::Dependencies,
        cancel_requested,
    ) {
        Ok(()) => true,
        Err(err) => {
            warn!("Dependency sync failed with PyPI index {index}: {err}");
            *last_error = Some(err);
            false
        }
    }
}

/// 切换到下一个候选镜像源；返回 false 表示已无源可试，应终止流程。
fn disable_index(
    index: &str,
    next_indexes: &[String],
    attempt: usize,
    status_updater: &mut impl FnMut(SplashUpdate),
) -> bool {
    warn!("Disabling PyPI index {index} for this run");
    let Some(next_index) = next_indexes.get(attempt + 1) else {
        return false;
    };
    status_updater(pypi_index_fallback_update(next_index));
    thread::sleep(RETRY_DELAY);
    true
}

/// 重写 `uv.lock` 中的下载 URL，使其单独解析到指定镜像源。
///
/// 镜像源经临时配置文件传入——只有这种方式能盖过项目自带的
/// `[tool.uv]` 索引列表，`--default-index` 做不到。
///
/// # Errors
/// 用户取消或 `uv lock` 执行失败时返回 Err。
fn uv_relock_project(
    bootstrap_uv: &Path,
    index: &str,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    let override_file = UvIndexOverride::create(index)?;
    let mut cmd = uv_lock_command(bootstrap_uv, override_file.path(), index);
    run_command(
        &mut cmd,
        &mut |_| {},
        ScriptPhase::Dependencies,
        cancel_requested,
    )
}

/// 构造 `uv sync` 命令：按当前平台的 `.venv` 路径同步依赖。
fn uv_sync_command(bootstrap_uv: &Path, index: &str) -> Command {
    uv_sync_command_with_paths(
        bootstrap_uv,
        &venv_python(),
        &venv_python_install_dir(),
        index,
    )
}

/// 组装 `uv sync --no-dev --no-install-project` 命令行与环境变量。
///
/// 显式指定 Python 与安装目录，保证不依赖系统 Python；
/// `--default-index` 指定当前镜像源，并屏蔽外部 UV_INDEX 环境变量
/// 的干扰（见 [`ignore_uv_index_env`]）。
fn uv_sync_command_with_paths(
    bootstrap_uv: &Path,
    python: &Path,
    python_install_dir: &Path,
    index: &str,
) -> Command {
    let mut cmd = Command::new(bootstrap_uv);
    cmd.args(["sync", "--no-dev", "--no-install-project", "--python"])
        .arg(python)
        .args(["--default-index", index])
        .env("UV_NO_PROGRESS", "1")
        .env("UV_PYTHON_INSTALL_DIR", python_install_dir);
    uv_python_env_with_install_dir(&mut cmd, python_install_dir);
    ignore_uv_index_env(&mut cmd);
    cmd
}

/// 构造 `uv lock` 命令，额外挂载临时配置文件以覆盖项目索引设置。
fn uv_lock_command(bootstrap_uv: &Path, config_file: &Path, index: &str) -> Command {
    let mut cmd = uv_lock_command_with_paths(
        bootstrap_uv,
        &venv_python(),
        &venv_python_install_dir(),
        index,
    );
    cmd.arg("--config-file").arg(config_file);
    cmd
}

/// 组装 `uv lock` 命令行与环境变量（指定 Python、安装目录与镜像源）。
fn uv_lock_command_with_paths(
    bootstrap_uv: &Path,
    python: &Path,
    python_install_dir: &Path,
    index: &str,
) -> Command {
    let mut cmd = Command::new(bootstrap_uv);
    cmd.args(["lock", "--python"])
        .arg(python)
        .args(["--default-index", index])
        .env("UV_NO_PROGRESS", "1")
        .env("UV_PYTHON_INSTALL_DIR", python_install_dir);
    uv_python_env_with_install_dir(&mut cmd, python_install_dir);
    ignore_uv_index_env(&mut cmd);
    cmd
}

/// 调用上游 `deploy.atomic.atomic_failure_cleanup` 清理上次失败的残留。
///
/// 与 git 更新同理复用上游 Python 逻辑，保证清理规则与官方安装器一致。
///
/// # Errors
/// 子命令执行失败时返回 Err（清理失败会阻断后续流程）。
fn atomic_failure_cleanup(path: &str, cancel_requested: &AtomicBool) -> Result<()> {
    let mut cmd = Command::new(venv_python());
    cmd.args([
        "-c",
        "import sys; from deploy.atomic import atomic_failure_cleanup; atomic_failure_cleanup(sys.argv[1])",
        path,
    ]);
    isolate_python_child_environment(&mut cmd);
    let _ = run_status_command(&mut cmd, cancel_requested)?;
    Ok(())
}

/// 确保运行时工具链齐备：Python 环境 → uv 引导依赖 → 复制 uv/adb/git 进 `.venv`。
///
/// 是环境搭建的第一阶段：先保证 `.venv` 内有可用的 Python 与 requests，
/// 再把 uv、adb、git 一并部署进 `.venv`，后续所有流程都只依赖 `.venv`
/// 内的工具（用户机器无需预装任何东西）。
///
/// # Errors
/// 用户取消、Python 安装失败或工具部署失败时返回 Err。
fn ensure_runtime_tools(
    bootstrap_uv: &Path,
    cancel_requested: &AtomicBool,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<()> {
    status_updater(runtime_tools_update(
        t!("setup.preparing_env"),
        t!("setup.checking_python"),
        9,
    ));
    ensure_self_contained_python(bootstrap_uv, cancel_requested, &mut status_updater)?;
    ensure_deploy_python_dependencies(bootstrap_uv, cancel_requested, &mut status_updater)?;

    status_updater(runtime_tools_update(
        t!("setup.preparing_env"),
        t!("setup.copying_tools"),
        16,
    ));
    copy_file_if_exists(bootstrap_uv, &venv_uv())?;
    ensure_adb_in_venv()?;
    ensure_git_in_venv()?;
    Ok(())
}

/// 读取 deploy.yaml 中用户配置的 PyPI 镜像（Deploy.Python.PypiMirror）。
///
/// 空值与字面量 "null" 均视为未配置，返回 `None`。
fn deploy_pypi_mirror() -> Option<String> {
    get_deploy_config()
        .as_ref()
        .and_then(|c| c.get("Deploy"))
        .and_then(|d| d.get("Python"))
        .and_then(|p| p.get("PypiMirror"))
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty() && *m != "null")
        .map(|m| m.to_owned())
}

/// 移除子进程环境中所有可能干扰镜像源选择的变量。
///
/// 用户机器上的 `UV_INDEX` / `PIP_INDEX_URL` 等会覆盖命令行
/// `--default-index`，导致镜像回退链失效，故显式移除。
fn ignore_uv_index_env(cmd: &mut Command) {
    for key in [
        "UV_INDEX",
        "UV_DEFAULT_INDEX",
        "UV_INDEX_URL",
        "UV_EXTRA_INDEX_URL",
        "PIP_INDEX_URL",
        "PIP_EXTRA_INDEX_URL",
    ] {
        cmd.env_remove(key);
    }
}

/// 让子进程绕过系统代理直连（下载/更新全部走镜像，避免代理故障干扰）。
///
/// 仅移除代理变量还不够——uv 在变量缺失时会回退到平台级代理设置，
/// 所以还要把 `NO_PROXY` 设为 `*` 明确声明"全部直连"。
fn bypass_proxy_for_child(cmd: &mut Command) {
    for key in [
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "PIP_PROXY",
        "pip_proxy",
    ] {
        cmd.env_remove(key);
    }
    // uv falls back to the platform proxy when these variables are absent.
    cmd.env("NO_PROXY", "*").env("no_proxy", "*");
}

/// 隔离子进程的 Python 相关环境变量。
///
/// 用户机器上残留的 `PYTHONHOME` / `PYTHONPATH` / `VIRTUAL_ENV`
/// 会让 `.venv` 内的解释器加载错误的库路径，是"本机能跑、
/// 启动器里跑不起来"的常见根因，因此所有 Python 子进程统一清洗。
pub(crate) fn isolate_python_child_environment(cmd: &mut Command) {
    for key in [
        "PYTHONHOME",
        "pythonhome",
        "PYTHONPATH",
        "pythonpath",
        "VIRTUAL_ENV",
        "virtual_env",
        "__PYVENV_LAUNCHER__",
    ] {
        cmd.env_remove(key);
    }
}

/// 确保 `.venv` 内已安装 `requests`（上游更新脚本的最小依赖）。
///
/// 先用 `import requests` 探测：已可用直接返回；缺失时按镜像源
/// 优先级逐源尝试 `uv pip install requests`，等待期间刷新进度。
///
/// # Errors
/// 用户取消或所有镜像源安装均失败时返回 Err。
fn ensure_deploy_python_dependencies(
    bootstrap_uv: &Path,
    cancel_requested: &AtomicBool,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<()> {
    status_updater(runtime_tools_update(
        t!("setup.preparing_env"),
        t!("setup.checking_requests"),
        14,
    ));
    let mut import_check = Command::new(venv_python());
    import_check.args(["-c", "import requests"]);
    isolate_python_child_environment(&mut import_check);
    let status = run_status_command(&mut import_check, cancel_requested)?;
    if status.success() {
        return Ok(());
    }

    let indexes = ranked_pypi_indexes(deploy_pypi_mirror().as_deref());
    for (attempt, index) in indexes.iter().enumerate() {
        status_updater(runtime_tools_update(
            t!("setup.preparing_env"),
            t!("setup.installing_requests"),
            15,
        ));
        info!("Installing requests with PyPI index: {index}");
        let mut cmd = Command::new(bootstrap_uv);
        cmd.args(["pip", "install", "--python"])
            .arg(venv_python())
            .arg("requests")
            .args(["--default-index", index])
            .arg("--no-config")
            .env("UV_NO_PROGRESS", "1")
            .env("UV_PYTHON_INSTALL_DIR", venv_python_install_dir());
        ignore_uv_index_env(&mut cmd);
        isolate_python_child_environment(&mut cmd);
        bypass_proxy_for_child(&mut cmd);

        let mut elapsed_ticks = 0u16;
        let status = run_status_command_with_tick(&mut cmd, cancel_requested, || {
            elapsed_ticks = elapsed_ticks.saturating_add(1);
            if elapsed_ticks == 1 || elapsed_ticks % 10 == 0 {
                status_updater(runtime_wait_update(
                    &t!("setup.preparing_env"),
                    &t!("setup.installing_requests"),
                    elapsed_ticks,
                    15,
                    16,
                ));
            }
        })?;
        if status.success() {
            return Ok(());
        }

        warn!("Failed to install requests with PyPI index: {index}");
        if attempt + 1 < indexes.len() {
            thread::sleep(RETRY_DELAY);
        }
    }

    bail!(t!("errors.requests_install_failed"));
}

/// 为 uv 子命令设置默认的 Python 安装目录环境（使用 `.venv` 内路径）。
fn uv_python_env(cmd: &mut Command) {
    uv_python_env_with_install_dir(cmd, &venv_python_install_dir());
}

/// 统一配置 uv 子命令的环境变量：关闭交互进度、清除 UV_PYTHON 干扰、
/// 指定 Python 安装目录与下载镜像、隔离 Python 环境、绕过代理。
///
/// 镜像仅在用户未通过 `UV_PYTHON_INSTALL_MIRROR` 显式指定时才注入
/// 默认值（第一个为国内 npmmirror 加速源）。
fn uv_python_env_with_install_dir(cmd: &mut Command, python_install_dir: &Path) {
    cmd.env("UV_NO_PROGRESS", "1")
        .env_remove("UV_PYTHON")
        .env("UV_PYTHON_INSTALL_DIR", python_install_dir);
    isolate_python_child_environment(cmd);
    bypass_proxy_for_child(cmd);
    if std::env::var_os("UV_PYTHON_INSTALL_MIRROR").is_none() {
        cmd.env(
            "UV_PYTHON_INSTALL_MIRROR",
            DEFAULT_UV_PYTHON_INSTALL_MIRRORS[0],
        );
    }
}

/// 确保 `.venv` 内有指定版本的自包含 Python（3.14.6）。
///
/// 版本检查逻辑：`.venv` 的 Python 低于最低可复用版本时删除整个
/// 环境重建；版本满足则直接复用；不存在则全新创建。创建过程经
/// uv 完成（managed python），并把 uv 镜像、隔离环境、代理直连
/// 等约束注入子进程。
///
/// # Errors
/// 用户取消、环境删除失败或 uv 安装 Python 失败时返回 Err。
fn ensure_self_contained_python(
    bootstrap_uv: &Path,
    cancel_requested: &AtomicBool,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<()> {
    status_updater(runtime_tools_update(
        t!("setup.preparing_env"),
        t!("setup.checking_python_version", version = PYTHON_VERSION),
        10,
    ));
    let existing_venv_python_version = venv_python_version();
    if let Some(version) = existing_venv_python_version
        .filter(|version| venv_python_version_requires_rebuild(*version))
    {
        let venv = venv_dir();
        info!(
            "Removing virtual environment with Python {}.{}.{}; minimum reusable version is {}.{}.{}",
            version.0,
            version.1,
            version.2,
            MIN_REUSABLE_VENV_PYTHON_VERSION.0,
            MIN_REUSABLE_VENV_PYTHON_VERSION.1,
            MIN_REUSABLE_VENV_PYTHON_VERSION.2,
        );
        remove_runtime_entry_with_retry(&venv).with_context(|| {
            t!(
                "errors.reset_venv_failed",
                error = venv.display().to_string()
            )
        })?;
    }

    if existing_venv_python_version.is_some_and(is_reusable_venv_python_version)
        && managed_python_executable().is_some()
    {
        return Ok(());
    }

    if managed_python_executable().is_none() {
        fs::create_dir_all(venv_python_install_dir()).with_context(|| {
            t!(
                "errors.python_dir_failed",
                error = venv_python_install_dir().display().to_string()
            )
        })?;
        let mirrors = uv_python_install_mirrors(std::env::var_os("UV_PYTHON_INSTALL_MIRROR"));
        let mut downloaded = false;
        for (index, mirror) in mirrors.iter().enumerate() {
            let mirror_label = if index == 0 {
                t!("setup.primary_mirror").to_string()
            } else {
                t!("setup.fallback_mirror").to_string()
            };
            status_updater(runtime_tools_update(
                t!("setup.download_python_title"),
                t!(
                    "setup.downloading_python",
                    version = PYTHON_VERSION,
                    mirror = mirror_label,
                    current = (index + 1).to_string(),
                    total = mirrors.len().to_string()
                ),
                11,
            ));
            let mut cmd = Command::new(bootstrap_uv);
            cmd.args(["python", "install", "--install-dir"])
                .arg(venv_python_install_dir())
                .args([
                    "--no-bin",
                    "--managed-python",
                    "--mirror",
                    mirror,
                    PYTHON_VERSION,
                ]);
            uv_python_env(&mut cmd);
            let mut elapsed_ticks = 0u16;
            let status = run_status_command_with_tick(&mut cmd, cancel_requested, || {
                elapsed_ticks = elapsed_ticks.saturating_add(1);
                if elapsed_ticks == 1 || elapsed_ticks % 10 == 0 {
                    status_updater(runtime_wait_update(
                        &t!("setup.download_python_title"),
                        &t!("setup.downloading_python_action", version = PYTHON_VERSION),
                        elapsed_ticks,
                        11,
                        13,
                    ));
                }
            })?;
            if status.success() {
                downloaded = true;
                break;
            }
            warn!(
                "{}",
                t!(
                    "errors.download_python_failed_mirror",
                    version = PYTHON_VERSION,
                    mirror = mirror
                )
            );
        }
        if !downloaded {
            bail!(t!(
                "errors.python_download_failed",
                version = PYTHON_VERSION
            ));
        }
    }

    let managed_python = managed_python_executable()
        .ok_or_else(|| anyhow!(t!("errors.python_not_found", version = PYTHON_VERSION)))?;
    status_updater(runtime_tools_update(
        t!("setup.creating_venv_title"),
        t!("setup.creating_venv"),
        13,
    ));
    reset_virtualenv_layout()?;
    let mut cmd = Command::new(bootstrap_uv);
    cmd.args(["venv", "--allow-existing", "--relocatable", "--python"])
        .arg(managed_python)
        .arg(venv_dir());
    uv_python_env(&mut cmd);
    let status = run_status_command(&mut cmd, cancel_requested)?;
    if !status.success() {
        bail!(t!("errors.venv_create_failed"));
    }
    Ok(())
}

/// 清空虚拟环境的标准布局目录（Scripts/Lib 等），为 `uv venv --allow-existing` 铺路。
///
/// uv 的 venv 命令不会覆盖已有布局文件，重建环境前必须先清掉旧布局；
/// 只删标准条目而非整个 `.venv`，尽量保留无关数据。
///
/// # Errors
/// 任一标准目录删除失败时返回 Err。
fn reset_virtualenv_layout() -> Result<()> {
    let venv = venv_dir();
    let entries = if cfg!(windows) {
        vec!["Scripts", "Lib", "Include", "pyvenv.cfg"]
    } else {
        vec!["bin", "lib", "include", "pyvenv.cfg"]
    };

    for entry in entries {
        let path = venv.join(entry);
        if path.exists() {
            remove_runtime_entry_with_retry(&path).with_context(|| {
                t!(
                    "errors.reset_venv_failed",
                    error = path.display().to_string()
                )
            })?;
        }
    }

    Ok(())
}

/// 在安装目录中查找 uv managed Python 的解释器路径。
///
/// uv 的安装目录名形如 `cpython-3.14.6-<平台标签>`，按前缀匹配
/// 后尝试各平台约定的解释器位置；未安装时返回 `None`。
fn managed_python_executable() -> Option<PathBuf> {
    let install_dir = venv_python_install_dir();
    let entries = fs::read_dir(install_dir).ok()?;
    let prefix = format!("cpython-{PYTHON_VERSION}-");
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) {
            continue;
        }
        let candidates = if cfg!(windows) {
            vec![path.join("python.exe")]
        } else {
            vec![
                path.join("bin").join("python3.14"),
                path.join("bin").join("python"),
            ]
        };
        for candidate in candidates {
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 读取 `.venv` 内 Python 的实际版本号（major.minor.patch）。
///
/// 解释器不存在、执行失败或输出无法解析时返回 `None`
/// （均视为"环境不可用"，由调用方走重建路径）。
fn venv_python_version() -> Option<(u16, u16, u16)> {
    let python = venv_python();
    if !python.exists() {
        return None;
    }
    let mut cmd = Command::new(python);
    cmd.args([
        "-c",
        "import sys; print('.'.join(map(str, sys.version_info[:3])))",
    ]);
    isolate_python_child_environment(&mut cmd);
    let output = cmd.create_no_window().output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_python_version(std::str::from_utf8(&output.stdout).ok()?)
}

/// 解析 `sys.version_info` 输出的 `major.minor.patch` 三段版本号；
/// 多于三段（含附加后缀）视为异常返回 `None`。
fn parse_python_version(version: &str) -> Option<(u16, u16, u16)> {
    let mut components = version.trim().split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    let patch = components.next()?.parse().ok()?;
    if components.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// 判断该 Python 版本是否低于最低可复用版本（需要删除重建整个 `.venv`）。
fn venv_python_version_requires_rebuild(version: (u16, u16, u16)) -> bool {
    version < MIN_REUSABLE_VENV_PYTHON_VERSION
}

/// 判断该版本是否可直接复用：必须是 Python 3.14 系且不低于最低可复用版本
/// （大版本不一致说明环境来自旧发行版，不能沿用）。
fn is_reusable_venv_python_version(version: (u16, u16, u16)) -> bool {
    version.0 == 3 && version.1 == 14 && !venv_python_version_requires_rebuild(version)
}

/// 复制单个文件（源不存在时静默成功；内容相同则跳过写入）。
///
/// 跳过相同文件的检查可避免重复启动时无谓的写入；Unix 上复制后
/// 补 0755 权限，保证工具可执行。
///
/// # Errors
/// 目录创建或复制失败时返回 Err（含 i18n 错误信息）。
fn copy_file_if_exists(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    if to.exists() && files_match(from, to).unwrap_or(false) {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).with_context(|| {
            t!(
                "errors.create_dir_failed",
                path = parent.display().to_string()
            )
        })?;
    }
    fs::copy(from, to).with_context(|| {
        t!(
            "errors.copy_file_failed",
            src = from.display().to_string(),
            dest = to.display().to_string()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(to)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(to, permissions)?;
    }
    Ok(())
}

/// 比较两个文件内容是否完全一致（先比大小再逐字节比较，尽早短路）。
///
/// # Errors
/// 任一文件元数据或内容读取失败时返回 Err。
fn files_match(left: &Path, right: &Path) -> Result<bool> {
    let left_metadata = fs::metadata(left).with_context(|| {
        t!(
            "errors.read_info_failed",
            error = left.display().to_string()
        )
    })?;
    let right_metadata = fs::metadata(right).with_context(|| {
        t!(
            "errors.read_info_failed",
            error = right.display().to_string()
        )
    })?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }

    let left_bytes = fs::read(left).with_context(|| {
        t!(
            "errors.read_file_failed",
            error = left.display().to_string()
        )
    })?;
    let right_bytes = fs::read(right).with_context(|| {
        t!(
            "errors.read_file_failed",
            error = right.display().to_string()
        )
    })?;
    Ok(left_bytes == right_bytes)
}

/// 确保 adb 已部署进 `.venv`（已存在则跳过）。
///
/// Windows 发行包中 adb 与其依赖 DLL 分开放置，需一并复制；
/// Unix 为单文件。
///
/// # Errors
/// 工具复制失败时返回 Err。
fn ensure_adb_in_venv() -> Result<()> {
    if venv_adb().exists() {
        return Ok(());
    }
    if cfg!(windows) {
        copy_first_packaged_tool(&["adb.exe"], &venv_bin_dir())?;
        copy_matching_packaged_tools("Adb", "dll", &venv_bin_dir())?;
    } else {
        copy_first_packaged_tool(&["adb"], &venv_bin_dir())?;
    }
    Ok(())
}

/// 确保 git 已部署进 `.venv`（已存在则跳过）。
///
/// Windows 为 MinGit 完整目录布局（`Scripts/git/`）；Unix 为单文件
/// `bin/git` 加 `libexec/git-core`（HTTPS 远程支持）与模板目录，
/// 复制完统一刷新 git 环境变量。
///
/// # Errors
/// 工具复制失败时返回 Err。
fn ensure_git_in_venv() -> Result<()> {
    if cfg!(windows) {
        if !venv_git().exists() {
            let src = PathBuf::from("bootstrap").join("git");
            let dst = venv_bin_dir().join("git");
            if src.exists() {
                copy_dir_all(&src, &dst)?;
            }
        }
    } else {
        if !venv_git().exists() {
            copy_first_packaged_tool(&["git"], &venv_bin_dir())?;
        }

        let git_core_src = PathBuf::from("bootstrap").join("git-core");
        let git_core_dst = venv_git_exec_path();
        let git_remote_https = git_core_dst.join("git-remote-https");
        if (!git_core_dst.exists() || !git_remote_https.exists()) && git_core_src.exists() {
            copy_dir_all(&git_core_src, &git_core_dst)?;
        }

        let templates_src = PathBuf::from("bootstrap").join("git-templates");
        let templates_dst = venv_git_template_dir();
        if !templates_dst.exists() && templates_src.exists() {
            copy_dir_all(&templates_src, &templates_dst)?;
        }

        refresh_git_environment();
    }
    Ok(())
}

/// 从发行包的 `bootstrap/` 目录复制第一个存在的同名工具到目标目录。
///
/// 同一工具可能以多个候选名出现（如 `adb` / `adb.exe`），
/// 取先命中者复制即止。
///
/// # Errors
/// 复制失败时返回 Err。
fn copy_first_packaged_tool(names: &[&str], target_dir: &Path) -> Result<()> {
    for name in names {
        let source = PathBuf::from("bootstrap").join(name);
        if source.exists() {
            copy_file_if_exists(&source, &target_dir.join(name))?;
            return Ok(());
        }
    }
    Ok(())
}

/// 复制 `bootstrap/` 目录下所有"前缀+扩展名"匹配的文件（如 adb 的 DLL 依赖）。
///
/// 目录不存在时静默成功——发行包可能不包含该平台的附加文件。
///
/// # Errors
/// 复制失败时返回 Err。
fn copy_matching_packaged_tools(prefix: &str, extension: &str, target_dir: &Path) -> Result<()> {
    let dir = PathBuf::from("bootstrap");
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(prefix) && name.ends_with(extension) {
            copy_file_if_exists(&entry.path(), &target_dir.join(name.as_ref()))?;
        }
    }
    Ok(())
}

/// 递归复制整个目录树（子目录递归，文件走 [`copy_file_if_exists`] 的去重逻辑）。
///
/// # Errors
/// 目录创建或任一文件复制失败时返回 Err。
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            copy_file_if_exists(&entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Verify runtime command construction and environment isolation without installs.
#[cfg(test)]
mod tests {
    use super::*;

    fn command_fixture(mode: &str, ready: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "setup::tests::command_cleanup_fixture",
                "--nocapture",
            ])
            .env("AZURPILOT_SETUP_FIXTURE_MODE", mode)
            .env("AZURPILOT_SETUP_FIXTURE_READY", ready)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn ready_port(path: &Path) -> Option<u16> {
        fs::read_to_string(path).ok()?.parse().ok()
    }

    fn wait_for_command_fixture(mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "command fixture timed out"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn command_cleanup_fixture() {
        let Ok(mode) = std::env::var("AZURPILOT_SETUP_FIXTURE_MODE") else {
            return;
        };
        let ready = PathBuf::from(std::env::var_os("AZURPILOT_SETUP_FIXTURE_READY").unwrap());
        #[cfg(unix)]
        if mode == "worker" {
            unsafe {
                nix::libc::signal(nix::libc::SIGTERM, nix::libc::SIG_IGN);
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        if mode.starts_with("tree") {
            let worker_ready = ready.with_extension("worker");
            // Inherit the root's pipes and process boundary, just as installer
            // workers do. A root exit alone must not leave this worker alive.
            command_fixture("worker", &worker_ready)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .create_no_window()
                .spawn()
                .unwrap();
            wait_for_command_fixture(|| ready_port(&worker_ready).is_some());
        }
        #[cfg(unix)]
        if mode == "closed-output" {
            unsafe {
                nix::libc::close(1);
                nix::libc::close(2);
            }
        }
        fs::write(&ready, listener.local_addr().unwrap().port().to_string()).unwrap();
        if mode == "tree-exit" {
            return;
        }
        loop {
            thread::park();
        }
    }

    fn assert_command_fixture_ports_released(ready: &Path, tree: bool) {
        let mut paths = vec![ready.to_owned()];
        if tree {
            paths.push(ready.with_extension("worker"));
        }
        for path in paths {
            let port = ready_port(&path).expect("fixture reported its listening port");
            wait_for_command_fixture(|| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
        }
    }

    fn assert_command_cancellation(mode: &str, progress_output: bool) {
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("command");
        let cancelled = AtomicBool::new(false);
        let finished = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                wait_for_command_fixture(|| {
                    ready_port(&ready).is_some() || finished.load(Ordering::Acquire)
                });
                if ready_port(&ready).is_some() {
                    cancelled.store(true, Ordering::Release);
                }
            });
            let mut command = command_fixture(mode, &ready);
            let result = if progress_output {
                run_command(&mut command, |_| {}, ScriptPhase::Dependencies, &cancelled)
            } else {
                run_status_command(&mut command, &cancelled).map(|_| ())
            };
            finished.store(true, Ordering::Release);
            assert!(
                ready_port(&ready).is_some(),
                "fixture must start: {result:?}"
            );
            assert!(result.is_err(), "cancelled command must return an error");
        });
        assert_command_fixture_ports_released(&ready, mode.starts_with("tree"));
    }

    #[test]
    fn status_command_cancellation_reclaims_owned_workers() {
        assert_command_cancellation("tree", false);
    }

    #[test]
    fn progress_command_cancellation_reclaims_owned_workers() {
        assert_command_cancellation("tree", true);
    }

    #[cfg(unix)]
    #[test]
    fn closed_output_does_not_prevent_command_cancellation() {
        assert_command_cancellation("closed-output", true);
    }

    #[test]
    fn exited_command_root_cannot_leave_a_worker_holding_its_output_pipe() {
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("command");
        let cancelled = AtomicBool::new(false);
        let finished = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !finished.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(20));
                }
                if !finished.load(Ordering::Acquire) {
                    cancelled.store(true, Ordering::Release);
                }
            });
            let result = run_command(
                &mut command_fixture("tree-exit", &ready),
                |_| {},
                ScriptPhase::Git,
                &cancelled,
            );
            finished.store(true, Ordering::Release);
            assert!(
                result.is_ok(),
                "root exit should clean workers and finish: {result:?}"
            );
            assert!(
                !cancelled.load(Ordering::Acquire),
                "command must finish before the watchdog"
            );
        });
        assert_command_fixture_ports_released(&ready, true);
    }

    #[test]
    fn test_uv_sync_uses_managed_python() {
        let python = Path::new(".venv/Scripts/python.exe");
        let command = uv_sync_command_with_paths(
            Path::new("uv"),
            python,
            Path::new(".venv/python"),
            "https://pypi.org/simple",
        );
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            args[0..4],
            ["sync", "--no-dev", "--no-install-project", "--python"]
        );
        assert_eq!(args[4], python.to_string_lossy());
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--default-index", "https://pypi.org/simple"]));
        assert_proxy_bypass_env(&command);
        assert_python_environment_isolated(&command);
    }

    #[test]
    fn test_uv_commands_ignore_global_python_selection() {
        let mut command = Command::new("uv");
        uv_python_env_with_install_dir(&mut command, Path::new(".venv/python"));

        assert!(command
            .get_envs()
            .any(|(key, value)| key == "UV_PYTHON" && value.is_none()));
        assert_proxy_bypass_env(&command);
        assert_python_environment_isolated(&command);
    }

    fn assert_proxy_bypass_env(command: &Command) {
        let no_proxy = command
            .get_envs()
            .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case("NO_PROXY"))
            .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()));
        assert_eq!(no_proxy.as_deref(), Some("*"));

        for key in ["ALL_PROXY", "HTTP_PROXY", "HTTPS_PROXY", "PIP_PROXY"] {
            let value = command
                .get_envs()
                .find(|(configured_key, _)| {
                    configured_key.to_string_lossy().eq_ignore_ascii_case(key)
                })
                .map(|(_, value)| value);
            assert!(matches!(value, Some(None)), "{key} must be removed");
        }
    }

    fn assert_python_environment_isolated(command: &Command) {
        for key in [
            "PYTHONHOME",
            "PYTHONPATH",
            "VIRTUAL_ENV",
            "__PYVENV_LAUNCHER__",
        ] {
            let value = command
                .get_envs()
                .find(|(configured_key, _)| {
                    configured_key.to_string_lossy().eq_ignore_ascii_case(key)
                })
                .map(|(_, value)| value);
            assert!(matches!(value, Some(None)), "{key} must be removed");
        }
    }

    #[test]
    fn test_isolate_python_child_environment() {
        let mut command = Command::new("python");
        isolate_python_child_environment(&mut command);

        assert_python_environment_isolated(&command);
    }

    #[test]
    fn test_parse_python_version() {
        assert_eq!(parse_python_version("3.14.5"), Some((3, 14, 5)));
        assert_eq!(parse_python_version(" 3.14.6\n"), Some((3, 14, 6)));
        assert_eq!(parse_python_version("3.14"), None);
        assert_eq!(parse_python_version("3.14.5.1"), None);
        assert_eq!(parse_python_version("not a version"), None);
    }

    #[test]
    fn test_venv_python_version_compatibility() {
        assert!(venv_python_version_requires_rebuild((3, 14, 4)));
        assert!(!venv_python_version_requires_rebuild((3, 14, 5)));
        assert!(!venv_python_version_requires_rebuild((3, 14, 6)));
        assert!(is_reusable_venv_python_version((3, 14, 5)));
        assert!(is_reusable_venv_python_version((3, 14, 6)));
        assert!(!is_reusable_venv_python_version((3, 15, 0)));
    }

    #[test]
    fn protected_workspace_recovery_stops_before_runtime_lookup() {
        // 测试可执行文件没有后端目录；若越过保护而查询运行路径，会直接 panic。
        let mut progress_reported = false;
        let result = rebuild_venv_and_sync_dependencies(
            |_| progress_reported = true,
            Arc::new(AtomicBool::new(false)),
            true,
        );
        assert!(result.is_err());
        assert!(!progress_reported);
    }
}
