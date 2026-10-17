//! AzurPilot Launcher——AzurLaneAutoScript（碧蓝航线自动化脚本）的跨平台
//! （Windows/macOS/Linux）桌面启动器，Tauri 2 + Rust。本文件是应用入口，
//! 承担窗口管理（splash 启动画面 + 主窗口）、系统托盘、启动器自更新、
//! 时间炸弹与后端 ManagedBackend 的生命周期衔接。
//!
//! 启动流程概览：初始化日志（log/{日期}_launcher.txt）→ splash 窗口 →
//! 后台线程依次完成启动器自更新检查、setup_alas_repo 仓库环境准备、
//! ManagedBackend 启动 gui.py → SSE 通知流与反向控制流就绪后销毁 splash
//! 并把主窗口导航到 WebUI（默认端口 22267）。
//!
//! 关键机制：
//! - 自定义 URI 协议：alas-splash:// 提供启动画面页面，alas-error:// 提供
//!   后端连接失败的错误页面（Windows 上以 http://*.localhost 假域名实现）；
//! - 标题栏 JS 注入（page_load_injector）：注入红绿灯窗口按钮与拖拽区、
//!   覆盖 window.saveAs 改走 Tauri save_as 命令、阻止浏览器后退；
//! - 时间炸弹：运行时从内嵌 Cargo.toml 解析过期配置，经 HTTP Date 头获取
//!   网络时间比对，过期则弹窗并阻止启动；
//! - 三平台关闭行为：Windows/macOS 首次询问并保存退出或托盘偏好，
//!   托盘菜单可修改；Linux 关闭窗口退出。

// 在 Windows 上不创建默认控制台窗口
#![windows_subsystem = "windows"]

/// 开机自启动管理。
mod autostart;
/// gui.py 后端子进程的托管（启动、就绪探测、终止等）。
mod backend;
/// 共享进程边界的 Windows 模拟器脱离回归测试。
#[cfg(windows)]
mod backend_process;
mod commands;
/// 多语言（i18n）初始化与本地化支持。
mod i18n;
/// 启动器反向控制流：接收后端经 SSE 下发的退出等指令。
mod launcher_control;
/// Windows 运行时 Node.js 的检测与安装引导。
mod nodejs;
/// SSE 通知流：接收后端推送的系统通知与点击回调。
mod notify;
mod popup_guard;
mod preferences;
mod process_security;
/// 首次启动环境准备（deploy.yaml、仓库克隆/更新、依赖同步）。
mod setup;
mod splash_state;
/// 旧版 SSE 接口的能力检测。
mod sse;
mod startup_state;
mod ui;
mod update;
mod webview_security;
/// 窗口工具（控制台附着状态等平台辅助）。
mod window_util;

#[macro_use]
extern crate rust_i18n;
i18n!("locales", fallback = "en");

use std::{
    cell::Cell,
    fs,
    io::Read,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::{self},
    time::{Duration, Instant},
};

use crate::{
    backend::{is_backend_startup_timeout, ManagedBackend, WebuiLaunchConfig},
    launcher_control::start_launcher_control_stream,
    notify::{start_notify_stream, NotificationClickHandler},
    setup::{
        get_deploy_config, rebuild_venv_and_sync_dependencies, repository_is_development_workspace,
        setup_alas_repo, setup_environment, SplashUpdate,
    },
};
use anyhow::{anyhow, bail, Context, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use chrono::{DateTime, FixedOffset, Local, Utc};
use reqwest::header::DATE;
use rust_i18n::t;
use serde_json::to_string;
#[cfg(any(windows, target_os = "macos"))]
use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
};
use tauri::{
    webview::{PageLoadEvent, PageLoadPayload},
    Manager, Url, WebviewWindow,
};
use tauri_plugin_dialog::DialogExt;
#[cfg(windows)]
use tauri_plugin_dialog::{MessageDialogButtons, MessageDialogKind};
#[cfg(test)]
use tempfile::Builder as TempDirBuilder;
use tracing::{debug, error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};
use ui::{
    backend_error_html, html_response, main_window_titlebar_injection_script, splash_response,
};
use webview_security::{BACKEND_ERROR_URL_BASE, SPLASH_URL};

/// macOS 托盘图标（2x 分辨率 PNG）：Retina 屏幕菜单栏使用的高清版本。
#[cfg(target_os = "macos")]
const MENUBAR_ICON_2X: &[u8] = include_bytes!("../icons/menubar@2x.png");
/// macOS 托盘图标（1x 分辨率 PNG）：2x 图标解码失败时的降级备选。
#[cfg(target_os = "macos")]
const MENUBAR_ICON_1X: &[u8] = include_bytes!("../icons/menubar.png");
/// Windows 系统托盘图标（PNG）。使用内嵌字节而非文件路径，保证打包后
/// 即使资源缺失托盘图标也能正常加载。
#[cfg(windows)]
const WINDOWS_TRAY_ICON: &[u8] = include_bytes!("../icons/icon.png");
/// 后端端口探测的单次 TCP 连接超时：只做快速可达性判断，不宜过长。
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// 主窗口导航前等待后端就绪的总时限，超时后转入错误页面。
const BACKEND_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(10);
/// 时间炸弹配置源：编译期内嵌的 Cargo.toml 文本，运行时从中解析
/// package.metadata.alas-launcher.time-bomb 段，免于新增独立配置文件。
const TIME_BOMB_CONFIG_SOURCE: &str = include_str!("../Cargo.toml");
/// 预览"跳过更新检查"模式的命令行参数：横线与斜杠两种风格都接受，
/// 便于测试时模拟"无更新可用"的启动路径。
const PREVIEW_NO_UPDATE_ARGS: &[&str] = &[
    "--preview-no-update",
    "--skip-update",
    "--no-update",
    "--disable-update",
    "/preview-no-update",
    "/skip-update",
    "/no-update",
];
/// 预览"启动即报错"模式的命令行参数，用于测试 splash 错误态的展示效果。
const PREVIEW_CRASH_ARGS: &[&str] = &[
    "--preview-crash",
    "--preview-error",
    "--crash-preview",
    "--error-preview",
    "/preview-crash",
    "/preview-error",
];
/// 请求"启动后最小化到托盘"的命令行参数。
const START_MINIMIZED_ARGS: &[&str] = &["--start-minimized", "/start-minimized"];

// ---------------------------------------------------------------------------
// 启动器信任免密登录
//
// 启动器为每次会话生成一个随机信任密钥，经环境变量 TRUST_SECRET_ENV 注入到
// gui.py 子进程。WebUI 启动后会据当前 --key / deploy.yaml Password 登记该
// 密钥；启动器窗口导航前先向后端换发一次性令牌，再进入 /launcher-login 页面
// 预置登录态实现免密。信任密钥与会话密钥解耦，其它浏览器仍走原密码门禁。
// 手动 gui.py 启动时密钥未注入，WebUI 端整体关闭该通道。
// ---------------------------------------------------------------------------
/// 注入 gui.py 子进程的免密信任密钥所用的环境变量名。
pub(crate) const TRUST_SECRET_ENV: &str = "ALAS_WEBUI_TRUST_SECRET";
/// 信任密钥的原始随机字节数（base64 编码后约 32 字符）。
const TRUST_SECRET_LENGTH: usize = 24;
/// 换发免密令牌的请求超时：失败即回退普通登录页，不能拖慢主窗口展示。
const TRUST_LOGIN_TIMEOUT: Duration = Duration::from_secs(3);

/// 本次会话的信任密钥缓存：首次调用时生成并全程复用，进程结束即失效。
static LAUNCHER_TRUST_SECRET: OnceLock<String> = OnceLock::new();
/// 一次启动期间所有网络路径使用同一份配置，避免重试重新落回 HTTP。
static WEBUI_LAUNCH_CONFIG: OnceLock<WebuiLaunchConfig> = OnceLock::new();

fn backend_config(port: u16) -> Result<std::borrow::Cow<'static, WebuiLaunchConfig>> {
    if let Some(config) = WEBUI_LAUNCH_CONFIG.get() {
        if port != config.port {
            bail!("Backend port does not match the active launcher configuration");
        }
        return Ok(std::borrow::Cow::Borrowed(config));
    }
    // 纯函数测试和启动配置登记前的保守默认值。
    let mut config = WebuiLaunchConfig::from_deploy_config(None);
    config.port = port;
    Ok(std::borrow::Cow::Owned(config))
}

/// 生成（首次）并返回本次会话的启动器信任密钥。
pub(crate) fn launcher_trust_secret() -> &'static str {
    LAUNCHER_TRUST_SECRET.get_or_init(|| {
        use rand::RngCore;
        let mut bytes = [0u8; TRUST_SECRET_LENGTH];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        BASE64_STANDARD.encode(bytes)
    })
}

/// 计算主窗口应导航到的后端地址：若后端支持启动器免密（本机回环 + 密钥匹配），
/// 则返回带一次性令牌的 /launcher-login 页面；否则回退普通后端首页，维持原有
/// 登录行为。本函数不抛错，任何失败都静默回退。
fn webui_navigate_url(port: u16) -> Result<String> {
    let config = backend_config(port)?;
    let fallback = config.backend_url()?;
    if !webview_security::is_loopback_http_origin(&Url::parse(&fallback)?) {
        return Ok(fallback);
    }
    let client = match config.http_client(TRUST_LOGIN_TIMEOUT) {
        Ok(client) => client,
        Err(_) => return Ok(fallback),
    };
    let response = client
        .post(format!("{fallback}api/launcher/trusted-login"))
        .header("X-Webui-Launcher-Secret", launcher_trust_secret())
        .send();
    let response = match response {
        Ok(response) if response.status().is_success() => response,
        _ => return Ok(fallback),
    };
    let mut body_text = String::new();
    if response
        .take(16 * 1024 + 1)
        .read_to_string(&mut body_text)
        .is_err()
        || body_text.len() > 16 * 1024
    {
        return Ok(fallback);
    }
    let body: serde_json::Value = match serde_json::from_str(&body_text) {
        Ok(body) => body,
        Err(_) => return Ok(fallback),
    };
    let Some(token) = body
        .get("token")
        .and_then(|token| token.as_str())
        .filter(|token| !token.is_empty() && token.len() <= 4096)
    else {
        return Ok(fallback);
    };
    // query_pairs_mut 编码服务端令牌，避免把它当作 URL 语法拼接。
    let mut url = Url::parse(&fallback)?.join("launcher-login")?;
    url.query_pairs_mut().append_pair("token", token);
    Ok(url.to_string())
}

/// URL 的日志安全形式：只保留 scheme/host/port/path，剔除 query，避免
/// /launcher-login 的一次性令牌经日志落盘。
fn redacted_url_log(url: &Url) -> String {
    let Some(host) = url.host_str() else {
        return url.to_string();
    };
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let mut out = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out.push_str(url.path());
    out
}

/// 时间炸弹配置：到期时间、网络时间来源 URL 与过期提示文案。
///
/// 由 Cargo.toml 的 package.metadata.alas-launcher.time-bomb 段解析而来；
/// 未启用时间炸弹时整体缺省（None）。
#[derive(Clone, Debug)]
struct TimeBombConfig {
    /// 到期时间：RFC 3339 解析为带固定时区的时间，网络时间不早于该值即过期。
    expires_at: DateTime<FixedOffset>,
    /// 用于获取"可信当前时间"的 URL，取其 HTTP Date 响应头判断。
    network_time_url: String,
    /// 过期后弹窗展示的提示文案。
    message: String,
}

#[cfg(target_os = "macos")]
fn tray_icon_for_platform() -> Image<'static> {
    info!("Loading macOS tray icon from embedded bytes...");
    let result = Image::from_bytes(MENUBAR_ICON_2X)
        .or_else(|_| {
            info!("2x icon failed, trying 1x...");
            Image::from_bytes(MENUBAR_ICON_1X)
        })
        .unwrap_or_else(|err| {
            error!(
                ?err,
                "Failed to load tray icon from embedded menubar icon bytes (2x and 1x)."
            );
            panic!("Failed to load tray icon from embedded menubar icon bytes: {err}");
        });
    info!("Tray icon loaded successfully");
    result
}

/// 加载 Windows 托盘图标（从内嵌 PNG 字节解码）。
///
/// # Panics
/// 图标字节解码失败时 panic——没有图标时托盘无法创建。
#[cfg(windows)]
fn tray_icon_for_platform() -> Image<'static> {
    Image::from_bytes(WINDOWS_TRAY_ICON).unwrap_or_else(|err| {
        error!(?err, "Failed to load tray icon from embedded icon bytes.");
        panic!("Failed to load tray icon from embedded icon bytes: {err}");
    })
}

/// 启动取消入口：等待正在使用环境的工作结束，保留现有文件后退出。
/// 多个退出路径共用幂等保护，绝不在取消时删除运行环境。
fn begin_startup_cleanup(
    app_handle: tauri::AppHandle,
    allow_exit: Arc<AtomicBool>,
    setup_cancel_requested: Arc<AtomicBool>,
    setup_running: Arc<AtomicBool>,
    startup_cleanup_started: Arc<AtomicBool>,
) {
    if startup_cleanup_started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    setup_cancel_requested.store(true, Ordering::SeqCst);
    if let Some(splash) = app_handle.get_webview_window("splash") {
        update_splash(
            &splash,
            &SplashUpdate::loading(
                t!("dialog.stopping_startup"),
                t!("dialog.stopping_startup_detail"),
                99,
            ),
        );
    }

    thread::spawn(move || {
        let started_at = Instant::now();
        let mut warned = false;
        while setup_running.load(Ordering::SeqCst) {
            if !warned && started_at.elapsed() >= Duration::from_secs(30) {
                warn!("Startup cancellation is taking longer than expected; preserving environment and waiting for owned work to stop");
                warned = true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        info!("Startup cancelled; existing runtime environment preserved");
        allow_exit.store(true, Ordering::SeqCst);
        app_handle.exit(0);
    });
}

/// 从内嵌 Cargo.toml 解析时间炸弹配置。
///
/// # Errors
/// 配置段声明 enabled 但缺少 expires-at，或时间格式不是合法 RFC 3339 时
/// 返回 Err；未启用或没有配置段时返回 Ok(None)。
fn time_bomb_config() -> Result<Option<TimeBombConfig>> {
    let Some(section) = cargo_toml_section("package.metadata.alas-launcher.time-bomb") else {
        return Ok(None);
    };
    // enabled 字段缺省按 false 处理：解析不到就当作未启用时间炸弹。
    let enabled = cargo_toml_value(section, "enabled")
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(false);
    if !enabled {
        return Ok(None);
    }

    let expires_at = cargo_toml_value(section, "expires-at")
        .ok_or_else(|| anyhow!(t!("errors.time_bomb_not_configured")))?;
    let expires_at = DateTime::parse_from_rfc3339(&expires_at)
        .map_err(|err| anyhow!(t!("errors.time_bomb_format_error", error = err.to_string())))?;
    // network-time-url 与 message 均有内置默认值，缺省也能工作。
    let network_time_url = cargo_toml_value(section, "network-time-url")
        .unwrap_or_else(|| "http://www.gstatic.com/generate_204".to_owned());
    let message = cargo_toml_value(section, "message")
        .unwrap_or_else(|| t!("errors.time_bomb_expired").to_string());

    Ok(Some(TimeBombConfig {
        expires_at,
        network_time_url,
        message,
    }))
}

/// 在内嵌 Cargo.toml 文本中定位 `[section_name]` 段，返回段体文本
/// （到下一个段头或文件末尾为止）。找不到段时返回 None。
///
/// 采用手写查找而非完整 TOML 库：仅需读取一个已知段，避免额外依赖。
fn cargo_toml_section(section_name: &str) -> Option<&'static str> {
    let header = format!("[{section_name}]");
    let start = TIME_BOMB_CONFIG_SOURCE.find(&header)? + header.len();
    let rest = &TIME_BOMB_CONFIG_SOURCE[start..];
    let end = rest.find("\n[").unwrap_or(rest.len());
    Some(&rest[..end])
}

/// 在 TOML 段文本中按行查找 `key = value`，返回去掉两侧引号后的字符串值。
///
/// 只处理单行简单键值：`#` 之后视为注释；找不到 key 时返回 None。
fn cargo_toml_value(section: &str, key: &str) -> Option<String> {
    for line in section.lines() {
        let line = line
            .split_once('#')
            .map(|(left, _)| left)
            .unwrap_or(line)
            .trim();
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        if left.trim() != key {
            continue;
        }
        let value = right.trim();
        return Some(
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value)
                .to_owned(),
        );
    }
    None
}

/// 判断时间炸弹是否已过期。
///
/// # Returns
/// 已过期返回 Some(配置的提示文案)；未配置或未过期返回 Ok(None)。
///
/// # Errors
/// 配置解析或网络时间获取失败时返回 Err（调用方会告警并放行启动）。
fn time_bomb_expiration_message() -> Result<Option<String>> {
    let Some(config) = time_bomb_config()? else {
        return Ok(None);
    };
    let network_time = fetch_network_time(&config.network_time_url)?;
    if network_time >= config.expires_at.with_timezone(&Utc) {
        Ok(Some(config.message))
    } else {
        Ok(None)
    }
}

/// 从指定 URL 的 HTTP Date 响应头获取"可信"网络时间。
///
/// 直接读取响应头而非解析响应体，可复用任意轻量端点（如 gstatic 的
/// generate_204），无需依赖专门的授时服务。
///
/// # Errors
/// 请求失败、响应缺少 Date 头或 Date 头不是合法 RFC 2822 时间时返回 Err。
fn fetch_network_time(url: &str) -> Result<DateTime<Utc>> {
    // 5 秒超时并禁用系统代理：取的是响应头时间，必须直连目标服务器，
    // 避免代理故障拖慢启动或干扰结果。
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()?;
    let response = client.get(url).send()?;
    let date_header = response
        .headers()
        .get(DATE)
        .ok_or_else(|| anyhow!(t!("errors.network_time_missing")))?
        .to_str()?;
    Ok(DateTime::parse_from_rfc2822(date_header)?.with_timezone(&Utc))
}

/// 构造更新请求的公共 HTTP 头：伪装成 Chrome 浏览器并附带语言偏好，
/// 检查命令行参数（跳过 argv[0]）中是否出现给定标志之一（忽略大小写）。
fn launcher_arg_present(flags: &[&str]) -> bool {
    std::env::args().skip(1).any(|arg| {
        let arg = arg.to_ascii_lowercase();
        flags.iter().any(|flag| arg == *flag)
    })
}

/// 是否以"跳过更新检查"预览模式启动（用于测试无更新时的启动路径）。
fn preview_no_update_arg_present() -> bool {
    launcher_arg_present(PREVIEW_NO_UPDATE_ARGS)
}

/// 是否以"启动即报错"预览模式启动（用于测试 splash 错误态展示）。
fn preview_crash_arg_present() -> bool {
    launcher_arg_present(PREVIEW_CRASH_ARGS)
}

/// 是否以"启动后最小化到托盘"模式启动。
fn start_minimized_arg_present() -> bool {
    launcher_arg_present(START_MINIMIZED_ARGS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_navigation_url_preserves_ipv6_and_removes_secrets() {
        let url = Url::parse("https://[::1]:22267/launcher-login?token=private#fragment").unwrap();
        assert_eq!(redacted_url_log(&url), "https://[::1]:22267/launcher-login");
        assert!(is_backend_url(
            &Url::parse("http://127.0.0.1:22267/tasks").unwrap(),
            22267
        ));
        assert!(!is_backend_url(
            &Url::parse("http://127.0.0.1:22268/tasks").unwrap(),
            22267
        ));
        assert!(!is_backend_url(
            &Url::parse("https://example.com:22267/tasks").unwrap(),
            22267
        ));
    }

    /// 时间炸弹段 enabled 时应能解析出配置（且与 enabled 值一致）。
    #[test]
    fn test_time_bomb_config_parses_when_enabled() {
        let section =
            cargo_toml_section("package.metadata.alas-launcher.time-bomb").expect("section exists");
        let enabled = cargo_toml_value(section, "enabled").as_deref() == Some("true");
        let config = time_bomb_config().expect("time bomb config parses");
        assert_eq!(config.is_some(), enabled);
    }

    /// cargo_toml_value 应能读到 expires-at 与中文 message 字段。
    #[test]
    fn test_cargo_toml_value_reads_time_bomb_fields() {
        let section =
            cargo_toml_section("package.metadata.alas-launcher.time-bomb").expect("section exists");
        assert!(cargo_toml_value(section, "expires-at").is_some());
        assert_eq!(
            Some("测试已结束，请安装正式版".to_owned()),
            cargo_toml_value(section, "message")
        );
    }

    /// backend_unready_error 携带日志失败原因时拼接展示，无原因时保持原样。
    #[test]
    fn backend_unready_error_carries_the_reason_from_the_log() {
        let with_reason = backend_unready_error(
            anyhow!("connect failed"),
            Some("React 前端构建失败".to_owned()),
        );
        assert!(format!("{with_reason:#}").contains("React 前端构建失败"));

        let without_reason = backend_unready_error(anyhow!("connect failed"), None);
        assert_eq!(format!("{without_reason:#}"), "connect failed");
    }

    /// 截断日志文件应把已存在的旧内容清空。
    #[test]
    fn test_truncate_log_file_replaces_existing_contents() {
        let temp_dir = TempDirBuilder::new()
            .prefix("launcher-log-truncate-test-")
            .tempdir()
            .expect("create temporary log directory");
        let filename = "launcher.txt";
        let path = temp_dir.path().join(filename);
        fs::write(&path, "old launcher log").expect("write old log");

        truncate_log_file(temp_dir.path(), filename).expect("truncate launcher log");

        assert_eq!(fs::read(&path).expect("read truncated log"), b"");
    }
}

#[cfg(target_os = "macos")]
fn set_macos_activation_policy(app: &tauri::AppHandle, regular: bool) {
    let policy = if regular {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    };
    if let Err(e) = app.set_activation_policy(policy) {
        error!("Failed to set activation policy: {}", e);
    }
}

/// Windows 专用：检查 Node.js 可用性，缺失或版本过低时弹窗引导安装。
///
/// 仅 Windows 需要：其发布包可能在运行时现场构建前端产物，依赖 Node.js；
/// macOS/Linux 发布包通常已自带构建产物。
///
/// # Returns
/// 是否可继续启动流程：Node 就绪、用户拒绝安装、安装失败（已提示）均返回
/// true；用户取消或安装被中断返回 false。
#[cfg(windows)]
fn prompt_for_missing_nodejs(
    app_handle: &tauri::AppHandle,
    splash: &WebviewWindow,
    mut status_updater: &mut impl FnMut(SplashUpdate),
    cancel_requested: &AtomicBool,
    start_minimized: bool,
) -> bool {
    let availability = crate::nodejs::is_nodejs_available();
    if matches!(availability, crate::nodejs::NodeJsAvailability::Ready) {
        return true;
    }
    if cancel_requested.load(Ordering::SeqCst) {
        return false;
    }

    // 两种情形文案不同：完全没装，与装了但版本低于构建要求。
    let (title, message, log) = match &availability {
        crate::nodejs::NodeJsAvailability::Outdated(found) => (
            t!("dialog.nodejs_outdated_title"),
            t!(
                "dialog.nodejs_outdated_message",
                found = found,
                minimum = crate::nodejs::NODEJS_MIN_FRONTEND_VERSION_TEXT
            ),
            "Node.js version is below the version required by the frontend build",
        ),
        _ => (
            t!("dialog.nodejs_missing_title"),
            t!("dialog.nodejs_missing_message"),
            "Node.js was not found on this Windows system",
        ),
    };
    warn!("{log}");
    // 最小化启动时先把 splash 唤起，保证弹窗有父窗口可依附。
    if start_minimized {
        let _ = reveal_window(splash);
    }
    status_updater(SplashUpdate::loading(
        t!("setup.checking_nodejs"),
        t!("setup.checking_nodejs"),
        5,
    ));

    // 阻塞式弹窗：用户选择"立即安装"才进入安装流程，"暂不"则继续启动。
    let install_requested = app_handle
        .dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            t!("dialog.nodejs_install").to_string(),
            t!("dialog.nodejs_not_now").to_string(),
        ))
        .parent(splash)
        .blocking_show();
    if !install_requested {
        info!("Node.js installation was declined by the user");
        return true;
    }

    match crate::nodejs::install_nodejs(&availability, cancel_requested, &mut status_updater) {
        Ok(()) => {
            info!("Node.js installation completed successfully");
            true
        }
        Err(_) if cancel_requested.load(Ordering::SeqCst) => false,
        Err(error) => {
            error!("Node.js installation failed: {error:#}");
            // 安装失败：splash 进入错误态并弹窗提示，但流程仍继续——
            // 后续步骤失败时会给出更具体的报错。
            status_updater(SplashUpdate::error(
                t!("dialog.nodejs_install_failed"),
                t!(
                    "dialog.nodejs_install_failed_detail",
                    error = format!("{error:#}")
                ),
                7,
            ));
            app_handle
                .dialog()
                .message(t!(
                    "dialog.nodejs_install_failed_detail",
                    error = format!("{error:#}")
                ))
                .title(t!("dialog.nodejs_install_failed"))
                .kind(MessageDialogKind::Error)
                .parent(splash)
                .blocking_show();
            true
        }
    }
}

/// 应用入口：装配 Tauri Builder（自定义协议、命令、插件、托盘、单实例），
/// 注册 Ready/ExitRequested/WindowEvent 等运行事件处理，并在后台线程驱动
/// "自更新检查 → 仓库准备 → 后端启动 → 主窗口展示"的完整启动流程。
///
/// # Errors
/// 环境准备（setup_environment）或日志初始化失败时返回 Err，进程随即结束。
fn main() -> Result<()> {
    // Windows 更新助手模式：命中 --apply-launcher-update 参数时完成替换并
    // 重启目标，随后直接退出，不走正常启动流程。
    if update::try_apply_from_args()? {
        return Ok(());
    }

    // 更新助手拉起的进程不附着控制台；普通启动则尝试附着父进程控制台，
    // 使命令行运行时能看到 stderr 输出（结果记录进 HAS_CONSOLE）。
    #[cfg(windows)]
    unsafe {
        use crate::window_util::HAS_CONSOLE;
        use std::sync::atomic::Ordering;
        use winapi::um::wincon::{AttachConsole, ATTACH_PARENT_PROCESS};
        if std::env::var_os(update::NO_CONSOLE_ENV).is_some() {
            std::env::remove_var(update::NO_CONSOLE_ENV);
        } else {
            HAS_CONSOLE.store(AttachConsole(ATTACH_PARENT_PROCESS) != 0, Ordering::Relaxed);
        }
    }
    // 初始化运行环境（切换工作目录等，详见 setup 模块）。
    setup_environment()?;
    // 日志守卫必须存活到进程结束，退出时负责刷盘。
    let _log_guard = initialize_logging()?;
    crate::i18n::init();
    // 解析预览与最小化启动参数（测试/演示用）。
    let preview_crash = preview_crash_arg_present();
    let preview_no_update = preview_crash || preview_no_update_arg_present();
    let start_minimized_requested = start_minimized_arg_present();
    let start_minimized = start_minimized_requested && !cfg!(target_os = "linux");
    if start_minimized_requested && !start_minimized {
        info!("Ignoring --start-minimized on Linux because this launcher has no system tray");
    }
    let preserve_workspace =
        preview_no_update || repository_is_development_workspace(Path::new("."));
    if preserve_workspace && !preview_no_update {
        info!("Development checkout detected; skipping launcher and repository release updates");
    }

    info!("=== AzurPilot starting ===");
    info!(
        "Launcher log file: log/{}",
        format!("{}_launcher.txt", Local::now().format("%Y-%m-%d"))
    );
    if preview_no_update {
        info!("Preview no-update mode enabled; skipping launcher update check");
    }
    if preview_crash {
        info!("Preview crash mode enabled; splash will stop on an artificial error state");
    }
    if start_minimized {
        info!("Start minimized mode enabled; main window will stay in tray after backend is ready");
    }

    // 读取 deploy.yaml 得到 WebUI 启动配置；缺失时使用默认配置（端口 22267）。
    let deploy_config = get_deploy_config();
    let webui_config = WebuiLaunchConfig::from_deploy_config(deploy_config.as_ref());
    if deploy_config.is_none() {
        warn!("config/deploy.yaml not found or invalid, using default WebUI launch config");
    }
    // 固定 scheme/host/port；无效 TLS 配置在任何后端启动前返回明确错误。
    webui_config.backend_url()?;
    WEBUI_LAUNCH_CONFIG
        .set(webui_config.clone())
        .map_err(|_| anyhow!("WebUI configuration already initialized"))?;
    let port = webui_config.port;
    let webview_policy =
        webview_security::WebviewPolicy::new(&Url::parse(&webui_config.backend_url()?)?)
            .map_err(anyhow::Error::msg)?;
    let security_plugin = webview_policy.bridge_plugin().map_err(anyhow::Error::msg)?;

    // 各线程共享的运行状态：后端句柄与一组生命周期标志
    // （退出放行、启动阻塞、setup 取消/运行/完成、清理触发、主窗口重建中）。
    let backend = Arc::new(Mutex::new(None));
    let allow_exit = Arc::new(AtomicBool::new(false));
    let launch_blocked = Arc::new(AtomicBool::new(false));
    let setup_cancel_requested = Arc::new(AtomicBool::new(false));
    let startup_state = startup_state::StartupState::default();
    let setup_running = startup_state.running.clone();
    let setup_completed = startup_state.completed.clone();
    let startup_cleanup_started = Arc::new(AtomicBool::new(false));
    let recreating_main_window = Arc::new(AtomicBool::new(false));

    let allow_exit_for_setup = allow_exit.clone();
    let launch_blocked_for_setup = launch_blocked.clone();
    let recreating_main_window_for_single_instance = recreating_main_window.clone();
    #[cfg(any(windows, target_os = "macos"))]
    let recreating_main_window_for_setup = recreating_main_window.clone();
    let recreating_main_window_for_run = recreating_main_window.clone();
    let launch_blocked_for_run = launch_blocked.clone();
    let start_minimized_for_run = start_minimized;

    let command_services = commands::CommandServices::new(
        "log",
        allow_exit.clone(),
        |window| minimize_main_window_to_tray(window.app_handle()),
        move |still_authorized| {
            if wait_for_backend_connection(port, BACKEND_NAVIGATION_TIMEOUT).is_err() {
                return Ok(None);
            }
            still_authorized()?;
            let target = webui_navigate_url(port).map_err(|error| error.to_string())?;
            Ok(Some(
                Url::parse(&target).map_err(|error| error.to_string())?,
            ))
        },
        |window| {
            debug!("Splash main frame acknowledged readiness");
            window
                .state::<splash_state::SplashState>()
                .acknowledge(|payload| dispatch_splash_update(window, payload));
        },
    )
    .map_err(anyhow::Error::msg)?;

    info!("Starting Webview...");
    // 注册 alas-error:// 自定义协议：服务后端连接失败的错误页面。
    tauri::Builder::default()
        .register_uri_scheme_protocol("alas-error", |_ctx, request| {
            backend_error_response(request)
        })
        // 注册 alas-splash:// 自定义协议：服务 splash 启动画面页面。
        .register_uri_scheme_protocol("alas-splash", |_ctx, _request| splash_response())
        // 注册前端可调用的 Tauri 命令：文件保存、日志下载、窗口控制等。
        .manage(webview_policy)
        .manage(command_services)
        .plugin(security_plugin)
        .manage(splash_state::SplashState::default())
        .manage(startup_state)
        .invoke_handler(tauri::generate_handler![
            commands::save_as,
            commands::splash_ready,
            commands::download_today_gui_log,
            commands::download_today_launcher_log,
            commands::retry_backend_connection,
            commands::window_hide,
            commands::window_minimize,
            commands::window_toggle_maximize,
            commands::window_close,
            commands::get_launcher_preferences,
            commands::set_launcher_close_behavior,
            commands::window_choose_close_behavior,
            commands::window_exit_application,
            commands::window_start_dragging,
            commands::window_is_maximized
        ])
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        // 单实例插件：二次启动时唤起已有实例的主窗口，而不是再开一个进程。
        .plugin(tauri_plugin_single_instance::init(
            move |app, _argv, _cwd| {
                restore_main_window_from_tray(
                    app,
                    port,
                    recreating_main_window_for_single_instance.clone(),
                );
            },
        ))
        // setup 回调：时间炸弹校验、主窗口创建与系统托盘装配。
        .setup(move |app| {
            app.manage(preferences::PreferenceStore::load(
                app.path().app_config_dir()?.join("preferences.json"),
            ));
            app.state::<webview_security::WebviewPolicy>().install_capabilities(app.handle())?;
            // 时间炸弹校验：已过期则弹窗提示并以退出码 0 结束，阻止继续启动。
            match time_bomb_expiration_message() {
                Ok(Some(message)) => {
                    launch_blocked_for_setup.store(true, Ordering::SeqCst);
                    allow_exit_for_setup.store(true, Ordering::SeqCst);
                    let app_handle = app.handle().clone();
                    app.dialog()
                        .message(message)
                        .title(t!("dialog.test_ended"))
                        .show(move |_| {
                            app_handle.exit(0);
                        });
                    return Ok(());
                }
                Ok(None) => {}
                Err(err) => {
                    warn!("Unable to verify test expiration time: {:?}", err);
                }
            }

            // 提前创建主窗口（隐藏态），等后端就绪后再导航并显示。
            create_main_window(&app.handle(), port)?;

            // Windows 与 macOS：创建系统托盘（Linux 桌面托盘兼容性参差，暂不提供）。
            #[cfg(any(windows, target_os = "macos"))]
            {
                info!("Creating system tray...");
                let allow_exit = allow_exit_for_setup.clone();
                let recreating_main_window_for_menu = recreating_main_window_for_setup.clone();
                #[cfg(windows)]
                let recreating_main_window_for_tray = recreating_main_window_for_setup.clone();
                // 托盘菜单提供窗口操作及关闭偏好；偏好选项只保存，不立即关闭。
                let show_item = MenuItemBuilder::new(t!("tray.toggle_visibility"))
                    .id("toggle_visibility")
                    .build(app)?;
                let quit_item = MenuItemBuilder::new(t!("tray.quit"))
                    .id("quit")
                    .build(app)?;
                let close_behavior_menu = tauri::menu::SubmenuBuilder::new(app, t!("tray.close_behavior"))
                    .text("close_behavior_ask", t!("tray.close_ask"))
                    .text("close_behavior_tray", t!("tray.close_tray"))
                    .text("close_behavior_exit", t!("tray.close_exit"))
                    .build()?;
                let tray_menu = MenuBuilder::new(app)
                    .item(&show_item)
                    .item(&close_behavior_menu)
                    .separator()
                    .item(&quit_item)
                    .build()?;

                info!("Tray menu created successfully");

                // 使用内嵌图标字节，保证打包后的应用也能正确加载托盘图标。
                let icon = tray_icon_for_platform();

                info!("Building tray icon...");
                let mut tray_builder = TrayIconBuilder::with_id("main-tray")
                    .icon(icon)
                    .tooltip("AzurPilot")
                    .menu(&tray_menu);

                // Windows：右键点击托盘图标才弹出菜单。
                #[cfg(windows)]
                {
                    tray_builder = tray_builder.show_menu_on_left_click(false);
                }

                // macOS：左键点击即弹出菜单（菜单栏图标没有其他左键行为）。
                #[cfg(target_os = "macos")]
                {
                    info!("Setting macOS tray to show menu on left click");
                    tray_builder = tray_builder.show_menu_on_left_click(true);
                }

                match tray_builder
                    .on_menu_event(move |app, event| {
                        debug!("Tray menu event: {:?}", event.id());
                        match event.id().as_ref() {
                            "toggle_visibility" => {
                                toggle_main_window_visibility(
                                    app,
                                    port,
                                    recreating_main_window_for_menu.clone(),
                                );
                            }
                            "close_behavior_ask" | "close_behavior_tray" | "close_behavior_exit" => {
                                let behavior = match event.id().as_ref() {
                                    "close_behavior_tray" => preferences::CloseBehavior::Tray,
                                    "close_behavior_exit" => preferences::CloseBehavior::Exit,
                                    _ => preferences::CloseBehavior::Ask,
                                };
                                if let Err(message) = commands::save_close_behavior(
                                    &app.state::<preferences::PreferenceStore>(),
                                    behavior,
                                ) {
                                    warn!("Unable to save close behavior from tray: {message}");
                                    app.dialog()
                                        .message(message)
                                        .title(t!("dialog.close_save_failed"))
                                        .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                                        .show(|_| {});
                                }
                            }
                            "quit" => {
                                allow_exit.store(true, Ordering::SeqCst);
                                app.exit(0);
                            }
                            _ => {}
                        }
                    })
                    .on_tray_icon_event(move |tray, event| {
                        // Windows：左键单击切换主窗口可见性；macOS 左键已用于
                        // 弹出菜单，此处忽略。
                        #[cfg(windows)]
                        if let tauri::tray::TrayIconEvent::Click {
                            button: tauri::tray::MouseButton::Left,
                            button_state: tauri::tray::MouseButtonState::Up,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            toggle_main_window_visibility(
                                &app,
                                port,
                                recreating_main_window_for_tray.clone(),
                            );
                        }

                        #[cfg(target_os = "macos")]
                        {
                            let _ = tray;
                            let _ = event;
                        }
                    })
                    .build(app)
                {
                    Ok(_) => {
                        info!("System tray created successfully!");
                    }
                    Err(e) => {
                        error!("Failed to create system tray: {:?}", e);
                        return Err(Box::new(e));
                    }
                }
            }

            Ok(())
        })
        // 运行事件循环；Ready 事件触发后在后台线程执行完整启动流程。
        .build(tauri::generate_context!())?
        .run(move |app_handle, event| {
            match event {
                tauri::RunEvent::Ready => {
                    // 时间炸弹已过期：什么都不做，等待弹窗回调退出进程。
                    if launch_blocked_for_run.load(Ordering::SeqCst) {
                        debug!("Launch blocked by test expiration");
                        return;
                    }

                    debug!("RunEvent::Ready");
                    // Ctrl-C：置放行标志后直接退出，保证命令行下也能干净终止。
                    let allow_exit = allow_exit.clone();
                    let allow_exit_for_ctrlc = allow_exit.clone();
                    let handle1 = app_handle.clone();
                    ctrlc::set_handler(move || {
                        allow_exit_for_ctrlc.store(true, Ordering::SeqCst);
                        handle1.exit(0);
                    })
                    .expect("Error setting Ctrl-C handler");
                    let app_handle = app_handle.clone();
                    let backend = backend.clone();
                    let webui_config = webui_config.clone();
                    let setup_cancel_requested = setup_cancel_requested.clone();
                    let setup_completed = setup_completed.clone();
                    let recreating_main_window_for_notify = recreating_main_window_for_run.clone();
                    let start_minimized = start_minimized_for_run;
                    // 在调度线程前登记运行状态，避免关闭事件误认为没有待取消工作。
                    let startup_worker = app_handle.state::<startup_state::StartupState>().start_worker();
                    // 完整启动流程放在后台线程执行，避免阻塞 Tauri 事件循环。
                    thread::spawn(move || {
                        let _startup_worker = startup_worker;
                        let Some(splash) = app_handle.get_webview_window("splash") else {
                            warn!("Splash was closed before the startup worker began");
                            return;
                        };
                        initialize_splash(&splash, !start_minimized);
                        // 进度只升不降：记录历史最高值，防止不同阶段进度回跳。
                        let last_progress = Cell::new(0u8);
                        let mut status_updater = |mut update: SplashUpdate| {
                            update.progress = update.progress.max(last_progress.get());
                            last_progress.set(update.progress);
                            update_splash(&splash, &update);
                        };

                        status_updater(
                            SplashUpdate::loading(
                                t!("splash.starting"),
                                t!("splash.webui_init"),
                                4,
                            )
                            .with_subtitle(format!(
                                "{} | Tips:{}",
                                t!("splash.initializing"),
                                crate::setup::get_tip()
                            )),
                        );

                        // 启动器自更新检查：成功替换重启时直接退出本进程交棒新版本。
                        if !preserve_workspace {
                            // 自更新的进度单独记一条"历史最高"，与主流程互不回退。
                            let launcher_progress = Cell::new(0u8);
                            let launcher_status_updater = |mut update: SplashUpdate| {
                                update.progress = update.progress.max(launcher_progress.get());
                                launcher_progress.set(update.progress);
                                update_splash(&splash, &update);
                            };

                            match update::check_and_restart(&setup_cancel_requested, |progress| {
                                let (detail, percent) = match progress {
                                    update::Progress::Available { version } => (t!("launcher_update.available_detail", version = version).to_string(), 8),
                                    update::Progress::Downloading { downloaded, total } => (format!("{downloaded} / {total} bytes"), 8 + ((downloaded.saturating_mul(80) / total.max(1)).min(80) as u8)),
                                    update::Progress::Verifying => (t!("launcher_update.verifying_detail").to_string(), 90),
                                    update::Progress::Restarting => (t!("launcher_update.restarting_detail").to_string(), 95),
                                };
                                launcher_status_updater(SplashUpdate::loading(t!("launcher_update.updating"), detail, percent));
                            }) {
                                Ok(update::Outcome::Restarted) => {
                                    info!("Launcher update installed, restarting");
                                    setup_completed.store(true, Ordering::SeqCst);
                                    allow_exit.store(true, Ordering::SeqCst);
                                    app_handle.exit(0);
                                    return;
                                }
                                Ok(update::Outcome::Cancelled) => return,
                                Ok(_) => {}
                                Err(e) if update::can_continue_offline(&e, env!("CARGO_PKG_VERSION"), Path::new(".")) => {
                                    warn!("Launcher update service unavailable; continuing with installed launcher: {e:#}");
                                }
                                Err(e) => {
                                    warn!("Required launcher update failed: {e:#}");
                                    if start_minimized {
                                        let _ = reveal_window(&splash);
                                    }
                                    launcher_status_updater(SplashUpdate::error(
                                        t!("launcher_update.failed"),
                                        t!(
                                            "launcher_update.failed_detail",
                                            error = format!("{e:#}")
                                        ),
                                        launcher_progress.get().max(8),
                                    ));
                                    setup_completed.store(true, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }

                        // 预览崩溃模式：直接在 splash 呈现错误态并停止启动流程。
                        if preview_crash {
                            if start_minimized {
                                let _ = reveal_window(&splash);
                            }
                            status_updater(
                                SplashUpdate::error(
                                    t!("dialog.startup_failed"),
                                    t!("splash.preview_crash_detail"),
                                    42,
                                )
                                .with_subtitle(format!(
                                    "{} | Tips：{}",
                                    t!("splash.preview_crash_mode"),
                                    crate::setup::get_tip()
                                )),
                            );
                            setup_completed.store(true, Ordering::SeqCst);
                            return;
                        }

                        // Windows：检查 Node.js，缺失或版本过低时引导安装
                        // （前端可能需在运行时现场构建）。
                        #[cfg(windows)]
                        if !prompt_for_missing_nodejs(
                            &app_handle,
                            &splash,
                            &mut status_updater,
                            &setup_cancel_requested,
                            start_minimized,
                        ) {
                            setup_completed.store(true, Ordering::SeqCst);
                            return;
                        }
                        if setup_cancel_requested.load(Ordering::SeqCst) {
                            return;
                        }

                        // 准备 ALAS 仓库与 Python 运行环境（克隆/更新/依赖同步等耗时步骤）。
                        if let Err(e) = setup_alas_repo(
                            &mut status_updater,
                            setup_cancel_requested.clone(),
                            preserve_workspace,
                        ) {
                            error!("{e}");
                            setup_completed.store(true, Ordering::SeqCst);
                            if setup_cancel_requested.load(Ordering::SeqCst) {
                                return;
                            }
                            if start_minimized {
                                let _ = reveal_window(&splash);
                            }
                            status_updater(SplashUpdate::error(
                                t!("dialog.startup_failed"),
                                t!("dialog.repo_setup_failed", error = e.to_string()),
                                last_progress.get().max(8),
                            ));
                            return;
                        }
                        if setup_cancel_requested.load(Ordering::SeqCst) {
                            return;
                        }
                        info!("Starting gui.py on configured port {}", port);
                        status_updater(
                            SplashUpdate::loading(
                                t!("splash.starting"),
                                t!("splash.webui_init_slow"),
                                97,
                            )
                            .with_subtitle(format!(
                                "{} | Tips:{}",
                                t!("splash.starting_backend"),
                                crate::setup::get_tip()
                            )),
                        );
                        // 后端启动循环：启动超时且未恢复过时，重建 venv 并重试一次。
                        let mut backend_recovery_used = false;
                        let backend_result = loop {
                            match ManagedBackend::new(&webui_config, &setup_cancel_requested) {
                                Ok(backend) => break Ok(backend),
                                Err(error)
                                    if !backend_recovery_used
                                        && !preserve_workspace
                                        && webui_config.ssl_key.is_none()
                                        && is_backend_startup_timeout(&error) =>
                                {
                                    backend_recovery_used = true;
                                    if setup_cancel_requested.load(Ordering::SeqCst) {
                                        break Err(error);
                                    }

                                    warn!(
                                        "Backend startup timed out; rebuilding .venv and retrying once"
                                    );
                                    if let Err(recovery_error) = rebuild_venv_and_sync_dependencies(
                                        &mut status_updater,
                                        setup_cancel_requested.clone(),
                                        preserve_workspace,
                                    ) {
                                        break Err(recovery_error.context(
                                            "Failed to rebuild .venv after backend startup timeout",
                                        ));
                                    }

                                    info!(
                                        "Retrying gui.py after rebuilding dependencies on configured port {port}"
                                    );
                                    status_updater(
                                        SplashUpdate::loading(
                                            t!("splash.starting"),
                                            t!("splash.webui_init_slow"),
                                            97,
                                        )
                                        .with_subtitle(format!(
                                            "{} | Tips:{}",
                                            t!("splash.starting_backend"),
                                            crate::setup::get_tip()
                                        )),
                                    );
                                }
                                Err(error) => break Err(error),
                            }
                        };
                        let b = match backend_result {
                            Ok(backend) => backend,
                            Err(e) => {
                                error!("{e}");
                                setup_completed.store(true, Ordering::SeqCst);
                                if setup_cancel_requested.load(Ordering::SeqCst) {
                                    return;
                                }
                                if start_minimized {
                                    let _ = reveal_window(&splash);
                                }
                                status_updater(SplashUpdate::error(
                                    t!("dialog.startup_failed"),
                                    t!("dialog.backend_launch_failed", error = e.to_string()),
                                    last_progress.get().max(97),
                                ));
                                return;
                            }
                        };
                        // 就绪与取消可能同时发生；先回收局部句柄再放行取消线程。
                        if setup_cancel_requested.load(Ordering::SeqCst) {
                            drop(b);
                            return;
                        }
                        // 后端句柄存入共享状态，退出时统一 terminate。
                        *backend.lock().unwrap() = Some(b);
                        // 通知点击回调：唤起主窗口（可能在任意线程触发，需调度）。
                        let notification_click: NotificationClickHandler = {
                            let app_handle = app_handle.clone();
                            let recreating_main_window = recreating_main_window_for_notify.clone();
                            Arc::new(move || {
                                restore_main_window_from_any_thread(
                                    app_handle.clone(),
                                    port,
                                    recreating_main_window.clone(),
                                );
                            })
                        };
                        // 启动 SSE 通知流与反向控制流：接收后端推送的通知/弹窗
                        // 与退出指令。
                        start_notify_stream(
                            app_handle.clone(),
                            webui_config.clone(),
                            allow_exit.clone(),
                            notification_click,
                        );
                        start_launcher_control_stream(webui_config.clone(), allow_exit.clone());
                        status_updater(
                            SplashUpdate::loading(t!("splash.opening"), t!("splash.ready"), 100)
                                .with_subtitle(format!(
                                    "{} | Tips:{}",
                                    t!("splash.startup_complete"),
                                    crate::setup::get_tip()
                                )),
                        );
                        // 一切就绪：销毁 splash，主窗口导航到 WebUI 后按需显示。
                        let _ = splash.destroy();
                        debug!("Destroyed splash window after startup");

                        info!("Webview is ready");
                        let window = match app_handle.get_webview_window("main") {
                            Some(window) => window,
                            None => match create_main_window(&app_handle, port) {
                                Ok(window) => window,
                                Err(error) => {
                                    error!("Failed to create main window after startup: {error:#}");
                                    setup_completed.store(true, Ordering::SeqCst);
                                    allow_exit.store(true, Ordering::SeqCst);
                                    app_handle.exit(1);
                                    return;
                                }
                            },
                        };
                        if let Err(error) = window.set_resizable(true) {
                            warn!("Failed to enable main window resizing: {error}");
                        }
                        if let Err(e) = navigate_backend_or_error(&window, port) {
                            error!("Failed to navigate main window: {:?}", e);
                        }
                        // 最小化启动：后端就绪后仍保持隐藏，仅驻留托盘。
                        if start_minimized {
                            info!("Backend is ready; keeping main window hidden in tray");
                            let _ = window.hide();
                        } else {
                            if let Err(error) = reveal_window(&window) {
                                warn!("Failed to reveal main window: {error}");
                            }
                        }
                        setup_completed.store(true, Ordering::SeqCst);
                    });
                }
                tauri::RunEvent::ExitRequested { api, .. } => {
                    // 启动尚未完成时的退出请求（如 splash 阶段被关闭）：
                    // 先执行启动清理，清理完成后再自行退出。
                    if !setup_completed.load(Ordering::SeqCst)
                        && (!startup_cleanup_started.load(Ordering::SeqCst)
                            || setup_running.load(Ordering::SeqCst))
                    {
                        api.prevent_exit();
                        begin_startup_cleanup(
                            app_handle.clone(),
                            allow_exit.clone(),
                            setup_cancel_requested.clone(),
                            setup_running.clone(),
                            startup_cleanup_started.clone(),
                        );
                        return;
                    }

                    let should_allow = allow_exit.load(Ordering::SeqCst);
                    debug!("ExitRequested event: allow_exit={}", should_allow);

                    // 仅在显式放行（托盘退出、window_exit_application、Ctrl-C
                    // 等）时才允许退出；否则阻止退出并把主窗口最小化到托盘。
                    if !should_allow {
                        api.prevent_exit();
                        debug!("Minimizing main window to tray");
                        minimize_main_window_to_tray(&app_handle);
                        return;
                    }

                    debug!("allow_exit is TRUE, proceeding with app shutdown");
                    // 放行退出：先终止 gui.py 后端进程再结束应用。
                    info!("App exit allowed, shutting down backend...");
                    if let Some(ref mut b) = *backend.lock().unwrap() {
                        if let Err(e) = b.terminate() {
                            warn!("Failed to terminate backend process: {:?}", e);
                        }
                    }
                }
                // macOS Dock 图标点击重开（Reopen）：恢复主窗口并切回 Regular 策略。
                #[cfg(target_os = "macos")]
                tauri::RunEvent::Reopen { .. } => {
                    restore_main_window_from_any_thread(
                        app_handle.clone(),
                        port,
                        recreating_main_window_for_run.clone(),
                    );
                }
                tauri::RunEvent::WindowEvent {
                    label,
                    event: tauri::WindowEvent::CloseRequested { ref api, .. },
                    ..
                } => {
                    debug!("Window {} close requested", label);

                    // splash 阶段的关闭请求：启动未完成时转启动清理流程。
                    if label == "splash" && !setup_completed.load(Ordering::SeqCst) {
                        api.prevent_close();
                        begin_startup_cleanup(
                            app_handle.clone(),
                            allow_exit.clone(),
                            setup_cancel_requested.clone(),
                            setup_running.clone(),
                            startup_cleanup_started.clone(),
                        );
                        return;
                    }

                    // 其余情况下关闭 splash 视为放弃启动：置放行标志并退出。
                    if label == "splash" && !allow_exit.load(Ordering::SeqCst) {
                        api.prevent_close();
                        allow_exit.store(true, Ordering::SeqCst);
                        app_handle.exit(0);
                        return;
                    }

                    // 自绘关闭、系统关闭和 Alt+F4 都经过同一份原生偏好。
                    if label == "main" && !allow_exit.load(Ordering::SeqCst) {
                        api.prevent_close();
                        use preferences::CloseBehavior;
                        match app_handle.state::<preferences::PreferenceStore>()
                            .close_behavior(preferences::BACKGROUND_SUPPORTED)
                        {
                            CloseBehavior::Exit => {
                                allow_exit.store(true, Ordering::SeqCst);
                                app_handle.exit(0);
                            }
                            CloseBehavior::Tray => minimize_main_window_to_tray(&app_handle),
                            CloseBehavior::Ask => {
                                if let Some(main_window) = app_handle.get_webview_window("main") {
                                    if let Err(err) = main_window.eval(
                                        "if (typeof window.__ALAS_OPEN_CLOSE_PROMPT !== 'function') { throw new Error('close prompt is unavailable'); } window.__ALAS_OPEN_CLOSE_PROMPT();",
                                    ) {
                                        warn!("Unable to open close chooser: {err:?}");
                                        minimize_main_window_to_tray(&app_handle);
                                    }
                                } else {
                                    minimize_main_window_to_tray(&app_handle);
                                }
                            }
                        }
                    }
                }

                _ => {}
            };
        });
    Ok(())
}

/// 初始化 tracing 日志：当日文件（log/{日期}_launcher.txt）+ stderr 双输出。
///
/// 文件层不带 ANSI 颜色与 target 前缀，便于直接阅读；两层均为 DEBUG 级别。
/// 返回的 WorkerGuard 必须存活到进程结束，drop 时自动刷盘。
///
/// # Errors
/// 创建或截断日志文件失败时返回 Err。
fn initialize_logging() -> Result<WorkerGuard> {
    let log_dir = Path::new("log");
    let log_filename = format!("{}_launcher.txt", Local::now().format("%Y-%m-%d"));
    truncate_log_file(log_dir, &log_filename)?;
    // rolling::never 不做按时间/大小轮转，固定写当日文件（启动时已先截断）。
    let file_appender = tracing_appender::rolling::never(log_dir, log_filename);
    let (non_blocking_file, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking_file)
        .with_ansi(false)
        .with_target(false)
        .with_filter(tracing::level_filters::LevelFilter::DEBUG);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(tracing::level_filters::LevelFilter::DEBUG);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .init();

    Ok(guard)
}

/// 确保日志目录存在并把日志文件截断为空（File::create 语义）。
/// 每次启动重新记录，避免单文件无限增长。
///
/// # Errors
/// 目录创建或文件创建失败时返回 Err。
fn truncate_log_file(log_dir: &Path, filename: &str) -> Result<()> {
    fs::create_dir_all(log_dir)?;
    let path = log_dir.join(filename);
    fs::File::create(&path)
        .with_context(|| format!("truncate launcher log file {}", path.display()))?;
    Ok(())
}

/// 页面加载完成回调：向主窗口页面注入启动器辅助 JS。
///
/// 注入内容（一个幂等的 IIFE）：阻止浏览器历史后退；把 window.saveAs
/// 覆盖为"经 FileReader 转 base64 后调用 Tauri save_as 命令"的版本；并
/// 拼入标题栏辅助脚本；macOS 仅注入关闭菜单，保留系统窗口按钮。
fn page_load_injector(webview: WebviewWindow, payload: PageLoadPayload<'_>) {
    // 只在加载完成（Finished）时注入：开始加载阶段文档尚未就绪，注入无效。
    if payload.event() == PageLoadEvent::Finished {
        let Some(policy) = webview.try_state::<webview_security::WebviewPolicy>() else {
            return;
        };
        let Ok(current_url) = webview.url() else {
            return;
        };
        if !policy.can_inject(webview.label(), payload.url(), &current_url) {
            return;
        }
        info!(
            "Injecting saveFile function to loaded page: {}",
            redacted_url_log(payload.url())
        );
        let injected_js = r#"
if (window.top === window && window.__ALAS_NATIVE__ && !window.alas_launcher_injected) {
    window.alas_launcher_injected = true;
    (function () {
        // Prevent going back
        history.pushState(null, document.title, location.href);
        window.addEventListener('popstate', event => {
            history.pushState(null, document.title, location.href);
        });
        // Overwrite original saveAs function
        window.saveAs = function (blob, filename) {
            if (!(blob instanceof Blob) || blob.size > 32 * 1024 * 1024) {
                return Promise.reject(new Error('Export exceeds the size limit'));
            }
            const reader = new FileReader();
            reader.onload = async () => {
                const data = reader.result.split(',')[1];
                const tauriInvoke =
                    (window.__ALAS_NATIVE__ && window.__ALAS_NATIVE__.invoke);
                if (typeof tauriInvoke === 'function') {
                    tauriInvoke('save_as', { filename, data });
                }
            };
            reader.readAsDataURL(blob);
        };
__ALAS_TITLEBAR_SCRIPT__
    })();
}
"#
        .replace(
            "__ALAS_TITLEBAR_SCRIPT__",
            &main_window_titlebar_injection_script(),
        );
        // 注入失败不影响页面本身功能，仅记录日志。
        if let Err(e) = webview.eval(&injected_js) {
            error!("Failed to inject JS to webview: {:?}", e);
        }
    }
}

/// 导航前关闭推送；页面自身的 IPC 握手负责重放导航期间的最新状态。
fn initialize_splash(splash: &WebviewWindow, show_window: bool) {
    splash.state::<splash_state::SplashState>().reset();
    match Url::parse(SPLASH_URL) {
        Ok(url) => {
            if let Err(e) = splash.navigate(url) {
                error!("Failed to navigate splash page: {:?}", e);
            }
            if show_window {
                if let Err(e) = splash.show() {
                    error!("Failed to show splash window: {:?}", e);
                }
            }
        }
        Err(e) => error!("Failed to parse splash URL: {:?}", e),
    }
}

/// 每条更新先缓存；只有页面确认回调存在后才向 WebView 推送。
fn update_splash(splash: &WebviewWindow, update: &SplashUpdate) {
    let payload = to_string(update).unwrap();
    splash
        .state::<splash_state::SplashState>()
        .update(payload, |payload| {
            dispatch_splash_update(splash, payload);
        });
}

fn dispatch_splash_update(splash: &WebviewWindow, payload: &str) {
    let script = format!("window.__ALAS_SPLASH_UPDATE({payload});");
    if let Err(e) = splash.eval(&script) {
        error!("Failed to update splash page: {:?}", e);
    }
}

/// 获取本次启动配置中的后端地址，保留 HTTPS 和 IPv6。
fn backend_url(port: u16) -> Result<String> {
    backend_config(port)?.backend_url()
}

/// 用实际 HTTP(S) 请求探测后端是否可达，验证 TLS 证书。
///
/// # Errors
/// 地址无效、证书验证失败、HTTP 错误或连接超时时返回 Err。
fn check_backend_connection(port: u16) -> Result<()> {
    let config = backend_config(port)?;
    let url = config.backend_url()?;
    config
        .http_client(BACKEND_CONNECT_TIMEOUT)?
        .get(&url)
        .send()
        .and_then(|response| response.error_for_status())
        .map(|_| ())
        .map_err(|e| {
            anyhow!(t!(
                "errors.backend_unreachable",
                address = url,
                error = e.to_string()
            ))
        })
}

/// 未就绪提示与日志中的失败原因合成最终错误：日志里写了原因时一并带出。
fn backend_unready_error(timeout_error: anyhow::Error, reason: Option<String>) -> anyhow::Error {
    match reason {
        Some(reason) => anyhow!(t!(
            "errors.backend_unready_with_reason",
            error = format!("{timeout_error:#}"),
            reason = reason
        )),
        None => timeout_error,
    }
}

/// 在给定时限内轮询后端端口直到可达。
///
/// 每 200ms 探测一次；超时后把最后一次连接错误与后端日志中发现的失败
/// 原因合成最终错误（见 backend_unready_error）。
///
/// # Errors
/// 超时仍不可达时返回 Err（附带最后一次连接错误与可能的后端失败原因）。
fn wait_for_backend_connection(port: u16, timeout: Duration) -> Result<()> {
    let started_at = Instant::now();
    let mut last_error = None;
    while started_at.elapsed() < timeout {
        match check_backend_connection(port) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_error = Some(e);
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    let timeout_error = last_error.unwrap_or_else(|| anyhow!(t!("errors.backend_timeout")));
    Err(backend_unready_error(
        timeout_error,
        crate::backend::read_backend_failure_reason(),
    ))
}

/// 等待后端就绪后把窗口导航到 WebUI（必要时带免密令牌）；失败则导航到
/// alas-error:// 错误页面。
///
/// # Returns
/// Ok(true) 表示已导航到后端；Ok(false) 表示后端不可达、已转错误页面。
///
/// # Errors
/// 仅当错误页导航本身失败（URL 解析或 WebView 导航出错）时返回 Err。
fn navigate_backend_or_error(window: &WebviewWindow, port: u16) -> Result<bool> {
    match wait_for_backend_connection(port, BACKEND_NAVIGATION_TIMEOUT) {
        Ok(()) => {
            let url = webui_navigate_url(port)?;
            window.navigate(Url::parse(&url)?)?;
            Ok(true)
        }
        Err(e) => {
            warn!("Backend connection check failed before navigation: {:?}", e);
            navigate_to_backend_error(window, port, &e.to_string())?;
            Ok(false)
        }
    }
}

/// 把窗口导航到携带端口与错误详情的 alas-error:// 错误页面。
///
/// # Errors
/// 错误 URL 构造或导航失败时返回 Err。
fn navigate_to_backend_error(window: &WebviewWindow, port: u16, error_detail: &str) -> Result<()> {
    let url = backend_error_url(port, error_detail)?;
    window.navigate(url)?;
    Ok(())
}

/// 构造错误页面 URL：port 与错误详情作为 query 参数，经 URL 编码安全传递。
///
/// # Errors
/// URL 解析失败时返回 Err（基路径是编译期常量，正常不会失败）。
fn backend_error_url(port: u16, error_detail: &str) -> Result<Url> {
    let port = port.to_string();
    Ok(Url::parse_with_params(
        BACKEND_ERROR_URL_BASE,
        [("port", port.as_str()), ("detail", error_detail)],
    )?)
}

/// alas-error:// 协议处理器：从请求 URI 解析 port/detail，返回错误页 HTML。
fn backend_error_response(
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let (port, detail) = backend_error_request_params(request.uri().to_string().as_str());
    let address = backend_url(port).unwrap_or_else(|error| error.to_string());
    html_response(backend_error_html(&address, &detail))
}

/// 从错误页请求 URI 中提取 port 与 detail query 参数。
///
/// 缺省值为默认端口 22267 与通用"无法连接"文案；非法端口直接忽略。
fn backend_error_request_params(uri: &str) -> (u16, String) {
    let mut port = 22267;
    let mut detail = t!("error_page.unable_connect").to_string();

    if let Ok(url) = Url::parse(uri) {
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "port" => {
                    if let Ok(parsed_port) = value.parse::<u16>() {
                        port = parsed_port;
                    }
                }
                "detail" => detail = value.into_owned(),
                _ => {}
            }
        }
    }

    (port, detail)
}

/// 主窗口只允许固定后端来源与内部错误页；后端不可达时阻止导航并显示错误。
///
/// # Returns
/// true 表示放行导航，false 表示已拦截。
fn handle_backend_navigation(app: tauri::AppHandle, port: u16, url: &Url) -> bool {
    let Some(policy) = app.try_state::<webview_security::WebviewPolicy>() else {
        return false;
    };
    if !policy.allows_navigation("main", url) {
        warn!("Blocked navigation outside the configured backend origin");
        return false;
    }
    if policy.is_backend_error_url(url) {
        return true;
    }

    match check_backend_connection(port) {
        Ok(()) => true,
        Err(e) => {
            let blocked_url = redacted_url_log(url);
            warn!(
                "Blocked navigation to unreachable backend {}: {:?}",
                blocked_url, e
            );
            let error_detail = e.to_string();
            // 在独立线程里切换到错误页，避免在导航回调内同步改写导航目标。
            thread::spawn(move || {
                if let Some(window) = app.get_webview_window("main") {
                    if let Err(e) = navigate_to_backend_error(&window, port, &error_detail) {
                        error!("Failed to show backend error page: {:?}", e);
                    }
                }
            });
            false
        }
    }
}

/// 判断 URL 是否属于本次后端配置的同一来源（scheme、host、port）。
#[cfg(test)]
fn is_backend_url(url: &Url, port: u16) -> bool {
    backend_url(port)
        .ok()
        .and_then(|base| Url::parse(&base).ok())
        .is_some_and(|base| base.origin() == url.origin())
}

/// 按 tauri.conf.json 中 label 为 main 的配置创建主窗口。
///
/// 挂载导航拦截（handle_backend_navigation）与页面加载注入
/// （page_load_injector）。
///
/// # Errors
/// 配置缺失或窗口构建失败时返回 Err。
fn create_main_window(app: &tauri::AppHandle, port: u16) -> Result<WebviewWindow> {
    let main_config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .ok_or_else(|| anyhow!(t!("errors.main_window_missing")))?;

    let app_for_navigation = app.clone();
    let main_window = tauri::WebviewWindowBuilder::from_config(app, main_config)?
        .on_navigation(move |url| handle_backend_navigation(app_for_navigation.clone(), port, url))
        .on_page_load(page_load_injector)
        .build()?;
    main_window.set_resizable(true)?;

    // Windows/Linux：主窗口同样去掉原生装饰（无边框，配合注入的标题栏）；
    // splash 则已在 tauri.conf.json 中配置为无边框。
    #[cfg(not(target_os = "macos"))]
    {
        if app
            .state::<webview_security::WebviewPolicy>()
            .backend_has_native_access()
        {
            main_window.set_decorations(false)?;
        }
    }

    Ok(main_window)
}

/// 显示窗口并聚焦；若窗口处于最小化状态先还原。
///
/// # Errors
/// 任一窗口操作失败时返回 Err。
fn reveal_window(window: &WebviewWindow) -> tauri::Result<()> {
    if window.is_minimized()? {
        window.unminimize()?;
    }
    window.show()?;
    window.set_focus()?;
    Ok(())
}

/// 把主窗口最小化到托盘（按平台采取不同策略）。
///
/// 保留主窗口、WebView 与页面连接，仅隐藏窗口；macOS 额外切换到
/// Accessory 激活策略以隐藏 Dock 图标。
fn minimize_main_window_to_tray(app: &tauri::AppHandle) {
    if !app.state::<startup_state::StartupState>().can_show_main() {
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        if let Err(error) = window.hide() {
            warn!("Failed to hide main window for tray mode: {error:?}");
        }
    }

    #[cfg(target_os = "macos")]
    {
        set_macos_activation_policy(app, false);
    }
}

/// 任意线程安全地调度主窗口恢复：包装 run_on_main_thread，调度失败仅告警。
fn restore_main_window_from_any_thread(
    app: tauri::AppHandle,
    port: u16,
    recreating_main_window: Arc<AtomicBool>,
) {
    let app_for_restore = app.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        restore_main_window_from_tray(&app_for_restore, port, recreating_main_window);
    }) {
        warn!("Failed to schedule main window restore: {:?}", e);
    }
}

/// 从托盘恢复主窗口：通常直接显示并聚焦；窗口意外丢失时以防重入标志
/// 保护，在新线程中重建并导航。
fn restore_main_window_from_tray(
    app: &tauri::AppHandle,
    port: u16,
    recreating_main_window: Arc<AtomicBool>,
) {
    if !app.state::<startup_state::StartupState>().can_show_main() {
        if let Some(splash) = app.get_webview_window("splash") {
            let _ = reveal_window(&splash);
        }
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(target_os = "macos")]
        set_macos_activation_policy(app, true);
        let _ = reveal_window(&window);
        return;
    }

    // 防重入：托盘、单实例、通知点击可能并发触发，只允许一次重建。
    if recreating_main_window
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        debug!("Main window recreation already in progress");
        return;
    }

    // 重建在新线程执行：窗口构建与导航可能耗时，不能阻塞调用线程。
    let app_handle = app.clone();
    thread::spawn(move || {
        // 重建期间同样先把 macOS 切回 Regular，保证 Dock 图标即时出现。
        #[cfg(target_os = "macos")]
        set_macos_activation_policy(&app_handle, true);

        let result = (|| -> Result<()> {
            let window = create_main_window(&app_handle, port)?;
            navigate_backend_or_error(&window, port)?;
            reveal_window(&window)?;
            Ok(())
        })();

        // 无论成败都复位重建标志，允许后续再次触发。
        recreating_main_window.store(false, Ordering::SeqCst);

        if let Err(e) = result {
            error!("Failed to recreate main window from tray: {:?}", e);
        }
    });
}

/// 托盘"显示/隐藏"菜单与左键点击的入口：按当前窗口状态在托盘化与恢复
/// 之间切换；窗口不存在时走恢复（重建）路径。
#[cfg(any(windows, target_os = "macos"))]
fn toggle_main_window_visibility(
    app: &tauri::AppHandle,
    port: u16,
    recreating_main_window: Arc<AtomicBool>,
) {
    if !app.state::<startup_state::StartupState>().can_show_main() {
        restore_main_window_from_tray(app, port, recreating_main_window);
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        let is_visible = window.is_visible().unwrap_or(false);
        let is_minimized = window.is_minimized().unwrap_or(false);
        if is_visible && !is_minimized {
            minimize_main_window_to_tray(app);
        } else {
            restore_main_window_from_tray(app, port, recreating_main_window);
        }
    } else {
        restore_main_window_from_tray(app, port, recreating_main_window);
    }
}
