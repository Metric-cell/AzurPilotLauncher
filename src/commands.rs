//! 只面向通过来源校验的主框架提供原生操作。
//!
//! 服务回调把后端重试、托盘策略与 splash 状态留给各自模块；命令不从
//! 前端接收端口、日志路径、目标窗口或保存目录。

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use base64::{prelude::BASE64_STANDARD, Engine};
use chrono::Local;
use tauri::{Manager, State, Url, WebviewWindow};
use tauri_plugin_dialog::{DialogExt, FilePath};
use tracing::{error, info};

use crate::webview_security::Caller;

/// 同时限制 IPC 解码产生的附加内存与 JS FileReader 输入。
pub const MAX_EXPORT_BYTES: usize = 32 * 1024 * 1024;
const MAX_EXPORT_BASE64_BYTES: usize = MAX_EXPORT_BYTES.div_ceil(3) * 4;

#[cfg(not(target_os = "linux"))]
type HideWindow = dyn Fn(&WebviewWindow) + Send + Sync;
type StillAuthorized<'a> = dyn Fn() -> Result<(), String> + 'a;
type RetryBackend = dyn Fn(&StillAuthorized<'_>) -> Result<Option<Url>, String> + Send + Sync;
type SplashReady = dyn Fn(&WebviewWindow) + Send + Sync;

/// 启动时建立一次；log_dir 必须是初始化日志使用的固定目录。
pub struct CommandServices {
    log_dir: PathBuf,
    pending_export: Arc<AtomicBool>,
    allow_exit: Arc<AtomicBool>,
    #[cfg(not(target_os = "linux"))]
    hide_window: Arc<HideWindow>,
    retry_backend: Arc<RetryBackend>,
    splash_ready: Arc<SplashReady>,
}

impl CommandServices {
    pub fn new(
        log_dir: impl AsRef<Path>,
        allow_exit: Arc<AtomicBool>,
        hide_window: impl Fn(&WebviewWindow) + Send + Sync + 'static,
        retry_backend: impl Fn(&StillAuthorized<'_>) -> Result<Option<Url>, String>
            + Send
            + Sync
            + 'static,
        splash_ready: impl Fn(&WebviewWindow) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        // 在启动时固定绝对路径，之后不再依赖可变工作目录或页面参数。
        let log_dir = fs::canonicalize(log_dir).map_err(|error| error.to_string())?;
        if !log_dir.is_dir() {
            return Err("Log export directory is not a directory".into());
        }
        #[cfg(target_os = "linux")]
        let _ = hide_window;
        Ok(Self {
            log_dir,
            pending_export: Arc::new(AtomicBool::new(false)),
            allow_exit,
            #[cfg(not(target_os = "linux"))]
            hide_window: Arc::new(hide_window),
            retry_backend: Arc::new(retry_backend),
            splash_ready: Arc::new(splash_ready),
        })
    }

    fn begin_export(&self) -> Result<ExportPermit, String> {
        self.pending_export
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| rust_i18n::t!("errors.export_busy").to_string())?;
        Ok(ExportPermit(self.pending_export.clone()))
    }
}

/// 取消对话框、验证失败、IO 失败或正常完成时都自动释放额度。
struct ExportPermit(Arc<AtomicBool>);

impl Drop for ExportPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 所有平台都只把单个可移植文件名传入系统对话框，不能建议任意路径。
fn validate_filename(filename: &str) -> Result<(), String> {
    let invalid = filename.is_empty()
        || filename.len() > 255
        || filename.trim() != filename
        || filename.ends_with('.')
        || matches!(filename, "." | "..")
        || filename.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'
                )
                || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        });
    let stem = filename
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if invalid || reserved {
        return Err(rust_i18n::t!("errors.export_invalid_filename").to_string());
    }
    Ok(())
}

fn decoded_export_length(data: &str) -> Result<usize, String> {
    let invalid_data = || rust_i18n::t!("errors.export_invalid_data").to_string();
    let too_large = || rust_i18n::t!("errors.export_too_large").to_string();
    if data.len() > MAX_EXPORT_BASE64_BYTES {
        return Err(too_large());
    }
    if data.len() % 4 != 0 {
        return Err(invalid_data());
    }
    let padding = data
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    if padding > 2 {
        return Err(invalid_data());
    }
    let length = data
        .len()
        .checked_div(4)
        .and_then(|blocks| blocks.checked_mul(3))
        .and_then(|bytes| bytes.checked_sub(padding))
        .ok_or_else(invalid_data)?;
    if length > MAX_EXPORT_BYTES {
        return Err(too_large());
    }
    Ok(length)
}

fn decode_export(data: &str) -> Result<Vec<u8>, String> {
    // 先检查长度和 padding，再分配；decode_slice 不会按攻击者输入增长 Vec。
    let length = decoded_export_length(data)?;
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(length)
        .map_err(|_| rust_i18n::t!("errors.export_too_large").to_string())?;
    decoded.resize(length, 0);
    let written = BASE64_STANDARD
        .decode_slice(data, &mut decoded)
        .map_err(|_| rust_i18n::t!("errors.export_invalid_data").to_string())?;
    decoded.truncate(written);
    Ok(decoded)
}

fn dialog_path(path: Option<FilePath>) -> Option<PathBuf> {
    path.and_then(|path| path.as_path().map(Path::to_owned))
}

#[tauri::command]
pub fn save_as(
    caller: Caller,
    services: State<'_, CommandServices>,
    filename: &str,
    data: &str,
) -> Result<(), String> {
    caller.require_visible()?;
    validate_filename(filename)?;
    let permit = services.begin_export()?;
    let decoded = decode_export(data)?;
    let dialog = caller.window().dialog().file().set_file_name(filename);
    #[cfg(desktop)]
    let dialog = dialog.set_parent(caller.window());
    dialog.save_file(move |path| {
        let Some(path) = dialog_path(path) else {
            return;
        };
        // 对话框期间可能导航、隐藏或关闭；再次检查当前来源、角色和可见性。
        if caller.require_visible().is_err() {
            return;
        }
        std::thread::spawn(move || {
            let _permit = permit;
            match write_export_to(&path, &decoded) {
                Ok(()) => info!("Saved exported file to {}", path.display()),
                Err(error) => error!("Failed to save exported file: {error}"),
            }
        });
    });
    Ok(())
}

#[tauri::command]
pub fn download_today_gui_log(
    caller: Caller,
    services: State<'_, CommandServices>,
) -> Result<String, String> {
    download_log_file(caller, &services, "gui")
}

#[tauri::command]
pub fn download_today_launcher_log(
    caller: Caller,
    services: State<'_, CommandServices>,
) -> Result<String, String> {
    download_log_file(caller, &services, "launcher")
}

fn download_log_file(
    caller: Caller,
    services: &CommandServices,
    kind: &str,
) -> Result<String, String> {
    caller.require_visible()?;
    let permit = services.begin_export()?;
    let filename = format!("{}_{}.txt", Local::now().format("%Y-%m-%d"), kind);
    let source_path = services.log_dir.join(&filename);
    let source = open_log_source(&source_path).map_err(|error| {
        rust_i18n::t!(
            "errors.read_log_file",
            path = source_path.to_string_lossy(),
            error = error.to_string()
        )
        .to_string()
    })?;
    let dialog = caller.window().dialog().file().set_file_name(&filename);
    #[cfg(desktop)]
    let dialog = dialog.set_parent(caller.window());
    dialog.save_file(move |path| {
        let Some(path) = dialog_path(path) else {
            return;
        };
        if caller.require_visible().is_err() {
            return;
        }
        std::thread::spawn(move || {
            let _permit = permit;
            match copy_log_to(source, &source_path, &path) {
                Ok(()) => info!(
                    "Saved {} log to {}",
                    kind_for_log(&source_path),
                    path.display()
                ),
                Err(error) => error!("Failed to save local log: {error}"),
            }
        });
    });
    Ok(filename)
}

fn kind_for_log(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("launcher")
}

fn open_log_source(path: &Path) -> io::Result<File> {
    // 由打开动作自身拒绝最终路径上的 symlink/reparse point，避免先检查再打开。
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(winapi::um::winbase::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if file.metadata()?.file_attributes() & winapi::um::winnt::FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Log source is a reparse point",
            ));
        }
    }
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Log source is not a regular file",
        ));
    }
    Ok(file)
}

fn write_export_to(destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut output = temporary_export(destination)?;
    output.write_all(bytes)?;
    finish_export(output, destination)
}

fn temporary_export(destination: &Path) -> io::Result<tempfile::NamedTempFile> {
    let directory = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Export destination has no parent directory",
        )
    })?;
    tempfile::NamedTempFile::new_in(directory)
}

fn finish_export(mut output: tempfile::NamedTempFile, destination: &Path) -> io::Result<()> {
    output.flush()?;
    output.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

fn copy_log_to(mut source: File, source_path: &Path, destination: &Path) -> io::Result<()> {
    // 不把日志读进内存，也不把内容返回 WebView。拒绝选择源文件自身，避免截断。
    if destination.exists() {
        let destination_file = File::open(destination)?;
        if fs::canonicalize(source_path)? == fs::canonicalize(destination)?
            || same_file(&source, &destination_file)?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot overwrite the log being exported",
            ));
        }
    }
    let mut output = temporary_export(destination)?;
    io::copy(&mut source, &mut output)?;
    finish_export(output, destination)
}

#[cfg(unix)]
fn same_file(left: &File, right: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(not(unix))]
fn same_file(_left: &File, _right: &File) -> io::Result<bool> {
    // 临时文件替换保证 Windows 上选择硬链接也不会截断原日志文件句柄。
    Ok(false)
}

#[tauri::command]
pub fn window_hide(caller: Caller, services: State<'_, CommandServices>) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let _ = services;
        caller
            .window()
            .minimize()
            .map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        (services.hide_window)(caller.window());
        Ok(())
    }
}

#[tauri::command]
pub fn window_minimize(caller: Caller) -> Result<(), String> {
    caller
        .window()
        .minimize()
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn window_toggle_maximize(caller: Caller) -> Result<bool, String> {
    let window = caller.window();
    let maximized = window.is_maximized().map_err(|error| error.to_string())?;
    if maximized {
        window.unmaximize()
    } else {
        window.maximize()
    }
    .map_err(|error| error.to_string())?;
    Ok(!maximized)
}

#[tauri::command]
pub fn window_close(caller: Caller) -> Result<(), String> {
    caller.window().close().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn window_exit_application(
    caller: Caller,
    services: State<'_, CommandServices>,
) -> Result<(), String> {
    caller.require_visible()?;
    services.allow_exit.store(true, Ordering::SeqCst);
    caller.window().app_handle().exit(0);
    Ok(())
}

#[tauri::command]
pub fn window_start_dragging(caller: Caller) -> Result<(), String> {
    caller
        .window()
        .start_dragging()
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn window_is_maximized(caller: Caller) -> Result<bool, String> {
    caller
        .window()
        .is_maximized()
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn retry_backend_connection(
    caller: Caller,
    services: State<'_, CommandServices>,
) -> Result<bool, String> {
    caller.revalidate()?;
    let retry = services.retry_backend.clone();
    let (caller, target) = tauri::async_runtime::spawn_blocking(move || {
        caller.revalidate()?;
        let target = retry(&|| caller.revalidate())?;
        caller.revalidate()?;
        Ok::<_, String>((caller, target))
    })
    .await
    .map_err(|error| error.to_string())??;
    let Some(target) = target else {
        return Ok(false);
    };
    caller.revalidate()?;
    let policy = caller
        .window()
        .try_state::<crate::webview_security::WebviewPolicy>()
        .ok_or_else(|| "WebView security policy is unavailable".to_owned())?;
    if !policy.is_backend_url(&target) {
        return Err("Backend retry produced an unexpected origin".into());
    }
    caller
        .window()
        .navigate(target)
        .map_err(|error| error.to_string())?;
    Ok(true)
}

#[tauri::command]
pub fn splash_ready(caller: Caller, services: State<'_, CommandServices>) -> Result<(), String> {
    (services.splash_ready)(caller.window());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_cannot_suggest_paths_or_windows_devices() {
        for filename in ["任务.json", "report-2026.txt", ".settings.json", "a..b.txt"] {
            assert!(validate_filename(filename).is_ok());
        }
        for filename in [
            "",
            ".",
            "..",
            "../config.yaml",
            "a/b.txt",
            "C:\\a.txt",
            "\\\\host\\a",
            "a:stream",
            "/tmp/a",
            "CON",
            "con.txt",
            "CON .txt",
            "LPT9.log",
            "COM¹.txt",
            "NUL",
            " x.txt",
            "x. ",
            "x.",
            "a\n.txt",
            "a\u{202e}txt.exe",
        ] {
            assert!(validate_filename(filename).is_err(), "{filename:?}");
        }
        assert!(validate_filename(&"x".repeat(256)).is_err());
    }

    #[test]
    fn base64_is_bounded_before_decode_and_rejects_invalid_padding() {
        assert_eq!(decode_export("SGVsbG8=").unwrap(), b"Hello");
        assert!(decode_export("").unwrap().is_empty());
        for invalid in ["a", "====", "!!!!", "SGVsbG8", "SGVsbG8===", "AAAA===="] {
            assert!(decode_export(invalid).is_err(), "{invalid}");
        }
        // Exactly one byte beyond the decoded limit still fits the rounded encoded
        // length budget; padding-aware preflight must reject it without allocating.
        let mut boundary = "A".repeat(MAX_EXPORT_BASE64_BYTES);
        boundary.pop();
        boundary.push('=');
        assert_eq!(decoded_export_length(&boundary).unwrap(), MAX_EXPORT_BYTES);
        boundary.pop();
        boundary.push('A');
        assert!(decoded_export_length(&boundary).is_err());
        boundary.push('A');
        assert!(decoded_export_length(&boundary).is_err());
    }

    #[test]
    fn only_one_export_may_own_memory_and_a_dialog_until_completion_or_cancel() {
        let directory = tempfile::tempdir().unwrap();
        let services = CommandServices::new(
            directory.path(),
            Arc::new(AtomicBool::new(false)),
            |_| {},
            |_| Ok(None),
            |_| {},
        )
        .unwrap();
        let permit = services.begin_export().unwrap();
        assert!(services.begin_export().is_err());
        drop(permit);
        assert!(services.begin_export().is_ok());
    }

    #[test]
    fn log_export_streams_fixed_open_source_and_rejects_symlinks_and_self_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("2026-10-15_launcher.txt");
        let destination = directory.path().join("export.txt");
        fs::write(&source, b"launcher diagnostic\n").unwrap();
        copy_log_to(open_log_source(&source).unwrap(), &source, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"launcher diagnostic\n");
        #[cfg(unix)]
        {
            assert!(copy_log_to(open_log_source(&source).unwrap(), &source, &source).is_err());
            assert_eq!(fs::read(&source).unwrap(), b"launcher diagnostic\n");
            let link = directory.path().join("linked.txt");
            std::os::unix::fs::symlink(&source, &link).unwrap();
            assert!(open_log_source(&link).is_err());
        }
        assert!(open_log_source(directory.path()).is_err());
    }
}
