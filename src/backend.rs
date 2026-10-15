//! 后端进程托管模块：负责 gui.py（AzurPilot WebUI）子进程的完整生命周期。
//! 启动前检测端口冲突，再以独立的进程边界拉起 Python 解释器；仅凭本次
//! spawn 保留的内核身份终止子进程，不使用端口或环境标记授予终止权限。
//! main.rs 在启动流程中调用 [`ManagedBackend::new`] 拉起后端，退出时依赖
//! [`ManagedBackend`] 的 [`Drop`] 实现杀掉遗留子进程，防止进程泄漏。

use std::{
    io::Read as _,
    net::{IpAddr, TcpListener, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    sync::atomic::{AtomicBool, Ordering},
    thread::sleep,
    time::Duration,
};

use anyhow::{anyhow, Result};
use chrono::Local;
use serde_json::Value as JsonValue;
use tracing::warn;

use crate::process_security::{ListenerState, OwnedProcessGroup};
use crate::setup::{alas_repo_dir, isolate_python_child_environment, venv_python};
use crate::window_util::CreateNoWindow as _;
use crate::{launcher_trust_secret, TRUST_SECRET_ENV};

/// 等待后端端口就绪的总超时时长，超时后判定启动失败并交由上层恢复。
///
/// gui.py 首次启动可能包含依赖同步、前端构建等耗时步骤，因此留足 5 分钟；
/// 等待期间每 100 毫秒探测一次端口，并同时检查子进程是否已提前退出。
const BACKEND_STARTUP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// gui.py 在启动失败时写下的日志标记。
const BACKEND_FAILURE_LOG_MARKER: &str = "[GUI] AzurPilot Web服务启动失败：";
/// 读取日志尾部的上限，失败原因位于文件末尾。
const BACKEND_FAILURE_LOG_TAIL_BYTES: u64 = 64 * 1024;

/// 等待后端端口就绪超时的专用错误类型。
///
/// 独立成类型而非普通字符串错误，便于上层用 [`is_backend_startup_timeout`]
/// 精确识别"启动超时"这一场景并执行针对性恢复，不会误伤其他启动错误。
#[derive(Debug)]
pub(crate) struct BackendStartupTimeout {
    /// 尚未就绪的目标端口，仅用于错误信息展示。
    port: u16,
    last_error: String,
}

/// 按约定实现 `Display`，让超时错误在上层日志与界面中可读。
impl std::fmt::Display for BackendStartupTimeout {
    /// 输出形如 "Timeout waiting for port 22267 to be ready" 的英文提示。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Timeout waiting for port {} to be ready: {}",
            self.port, self.last_error
        )
    }
}

/// 标记为标准错误类型，使其能被 `anyhow` 的 downcast 机制识别。
impl std::error::Error for BackendStartupTimeout {}

/// 判断任意 `anyhow` 错误是否为本模块产生的启动超时错误。
///
/// 上层捕获错误后据此决定是否走"超时恢复"分支，而非当作普通启动失败
/// 直接向用户报错。
pub(crate) fn is_backend_startup_timeout(error: &anyhow::Error) -> bool {
    error.downcast_ref::<BackendStartupTimeout>().is_some()
}

/// 一次 WebUI 启动的完整参数，来源为 ALAS 的 deploy 配置或保守默认值。
///
/// 各字段与 gui.py 的命令行开关一一对应，由 [`WebuiLaunchConfig::args`]
/// 负责翻译成实际的参数列表。
#[derive(Clone, Debug)]
pub struct WebuiLaunchConfig {
    /// WebUI 监听地址，默认 `127.0.0.1`，仅允许本机访问。
    pub host: String,
    /// WebUI 监听端口，默认 22267，与 ALAS 上游约定一致。
    pub port: u16,
    /// WebUI 访问密码；缺失时后端以无密码模式运行。
    pub password: Option<String>,
    /// 是否启用 CDN 加速静态资源加载。
    pub cdn: bool,
    /// HTTPS 私钥路径；与 `ssl_cert` 成对配置时以 HTTPS 启动。
    pub ssl_key: Option<String>,
    /// HTTPS 证书路径。
    pub ssl_cert: Option<String>,
    /// 启动后立即执行的 ALAS 任务名列表，经 `--run` 透传给 gui.py。
    pub run: Vec<String>,
}

/// `WebuiLaunchConfig` 的构造与命令行翻译逻辑。
impl WebuiLaunchConfig {
    /// 从 ALAS 的 deploy 配置中提取 WebUI 参数。
    ///
    /// `config` 为 `None` 或缺少 `Deploy.Webui` 节点时，各字段独立回退到
    /// 默认值，因此即使配置部分损坏也能以保守默认值启动后端。
    pub fn from_deploy_config(config: Option<&JsonValue>) -> Self {
        // 配置路径固定为 config["Deploy"]["Webui"]，任一层缺失都会得到 None；
        // 之后每个字段独立判空并回退默认值，保证部分缺失的配置仍可启动。
        let webui = config
            .and_then(|config| config.get("Deploy"))
            .and_then(|deploy| deploy.get("Webui"));

        Self {
            host: webui
                .and_then(|webui| webui.get("WebuiHost"))
                .and_then(value_as_string)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "127.0.0.1".to_owned()),
            port: webui
                .and_then(|webui| webui.get("WebuiPort"))
                .and_then(value_as_u16)
                .unwrap_or(22267),
            password: webui
                .and_then(|webui| webui.get("Password"))
                .and_then(value_as_string)
                .filter(|value| !value.trim().is_empty()),
            cdn: webui
                .and_then(|webui| webui.get("CDN"))
                .and_then(value_as_bool)
                .unwrap_or(false),
            ssl_key: webui
                .and_then(|webui| webui.get("WebuiSSLKey"))
                .and_then(value_as_string)
                .filter(|value| !value.trim().is_empty()),
            ssl_cert: webui
                .and_then(|webui| webui.get("WebuiSSLCert"))
                .and_then(value_as_string)
                .filter(|value| !value.trim().is_empty()),
            run: webui
                .and_then(|webui| webui.get("Run"))
                .map(value_as_string_list)
                .unwrap_or_default(),
        }
    }

    fn listen_host(&self) -> &str {
        let host = self.host.trim();
        host.strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host)
    }

    fn validate(&self) -> Result<()> {
        let host = self.listen_host();
        let bracketed = self.host.trim().starts_with('[') || self.host.trim().ends_with(']');
        let valid_ip = host.parse::<IpAddr>().ok();
        let valid_dns = !bracketed
            && host.len() <= 253
            && host.trim_end_matches('.').split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    && !label.starts_with('-')
                    && !label.ends_with('-')
            });
        if valid_ip.is_none() && !valid_dns
            || bracketed && !valid_ip.is_some_and(|address| address.is_ipv6())
        {
            return Err(anyhow!(rust_i18n::t!("errors.backend_invalid_host")));
        }
        if self.ssl_key.is_some() != self.ssl_cert.is_some() {
            return Err(anyhow!(rust_i18n::t!("errors.backend_invalid_tls")));
        }
        if self.port == 0 {
            return Err(anyhow!(rust_i18n::t!("errors.backend_invalid_port")));
        }
        Ok(())
    }

    /// 浏览器、通知流和就绪检查共享的后端地址；通配监听使用对应回环地址。
    pub fn backend_url(&self) -> Result<String> {
        self.validate()?;
        let host = match self.listen_host() {
            "0.0.0.0" => "127.0.0.1",
            "::" => "::1",
            host => host,
        };
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let scheme = if self.ssl_key.is_some() && self.ssl_cert.is_some() {
            "https"
        } else {
            "http"
        };
        let url = format!("{scheme}://{host}:{}/", self.port);
        reqwest::Url::parse(&url)?;
        Ok(url)
    }

    /// 后端专用客户端。不使用系统代理，TLS 始终验证证书链和主机名。
    /// 配置中的 PEM 证书作为额外可信根，用于显式配置的本地自签名服务。
    pub fn http_client(&self, timeout: Duration) -> Result<reqwest::blocking::Client> {
        Ok(self.http_client_builder()?.timeout(timeout).build()?)
    }

    /// SSE 调用者可仅设置 connect_timeout，避免总时限截断长连接。
    pub fn http_client_builder(&self) -> Result<reqwest::blocking::ClientBuilder> {
        self.validate()?;
        let mut builder = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none());
        if let Some(path) = &self.ssl_cert {
            let path = Path::new(path);
            let path = if path.is_absolute() {
                path.to_owned()
            } else {
                alas_repo_dir().join(path)
            };
            let pem = std::fs::read(path)?;
            for certificate in reqwest::Certificate::from_pem_bundle(&pem)? {
                builder = builder.add_root_certificate(certificate);
            }
        }
        Ok(builder)
    }

    /// 把配置翻译为传给 Python 解释器的 gui.py 参数列表（不含解释器本身）。
    ///
    /// `--host`/`--port` 恒定存在；其余开关仅在对应字段有值时追加，
    /// 保证未配置的选项不会以空值形式传给后端。
    fn args(&self) -> Vec<String> {
        // 基础参数：gui.py 入口与监听地址、端口，决定 WebUI 的访问位置。
        let mut args = vec![
            "gui.py".to_owned(),
            "--host".to_owned(),
            self.listen_host().to_owned(),
            "--port".to_owned(),
            self.port.to_string(),
        ];

        if let Some(password) = &self.password {
            args.push("--key".to_owned());
            args.push(password.clone());
        }
        if self.cdn {
            args.push("--cdn".to_owned());
        }
        if let Some(ssl_key) = &self.ssl_key {
            args.push("--ssl-key".to_owned());
            args.push(ssl_key.clone());
        }
        if let Some(ssl_cert) = &self.ssl_cert {
            args.push("--ssl-cert".to_owned());
            args.push(ssl_cert.clone());
        }
        if !self.run.is_empty() {
            args.push("--run".to_owned());
            args.extend(self.run.iter().cloned());
        }

        args
    }
}

/// 把 JSON 值宽松地转换为字符串：字符串原样返回，`null` 视为缺失，
/// 其余类型（数字、布尔等）经 `Display` 字符串化，兼容配置中的异构写法。
fn value_as_string(value: &JsonValue) -> Option<String> {
    if let Some(value) = value.as_str() {
        Some(value.to_owned())
    } else if value.is_null() {
        None
    } else {
        Some(value.to_string())
    }
}

/// 把 JSON 值转换为 `u16` 端口号：优先按数字解析，其次按字符串解析；
/// 超出 `u16` 范围或无法解析时返回 `None`，由调用方回退默认端口。
fn value_as_u16(value: &JsonValue) -> Option<u16> {
    if let Some(value) = value.as_u64() {
        u16::try_from(value).ok()
    } else {
        value.as_str()?.parse::<u16>().ok()
    }
}

/// 把 JSON 值宽松地转换为布尔值：除原生布尔外还接受常见的字符串写法
/// （"true"/"1"/"yes"/"on" 等），其余一律返回 `None` 以便回退默认值。
fn value_as_bool(value: &JsonValue) -> Option<bool> {
    if let Some(value) = value.as_bool() {
        Some(value)
    } else {
        match value.as_str()?.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        }
    }
}

/// 把 JSON 值转换为非空字符串列表：数组逐元素转换并丢弃空串；
/// 非数组值按单个字符串处理，兼容配置里把列表写成单值的情况。
fn value_as_string_list(value: &JsonValue) -> Vec<String> {
    match value {
        JsonValue::Array(values) => values
            .iter()
            .filter_map(value_as_string)
            .filter(|value| !value.trim().is_empty())
            .collect(),
        _ => value_as_string(value)
            .filter(|value| !value.trim().is_empty())
            .into_iter()
            .collect(),
    }
}

/// 只持有本次 spawn 创建的进程边界，所有退出路径都由 RAII 回收。
pub struct ManagedBackend {
    child: Option<OwnedProcessGroup>,
}

impl ManagedBackend {
    /// 按配置启动 gui.py；已有端口占用会报错，由调用方展示给用户。
    pub fn new(config: &WebuiLaunchConfig, cancel_requested: &AtomicBool) -> Result<Self> {
        check_startup_cancelled(cancel_requested)?;
        config.validate()?;
        let mut command = Command::new(venv_python());
        command.args(config.args());
        command.current_dir(alas_repo_dir());
        command.env(TRUST_SECRET_ENV, launcher_trust_secret());
        // 兼容后端的诊断用途；该标记从不用于发现或终止进程。
        command.env("ALAS_LAUNCHER_PID", std::process::id().to_string());
        isolate_python_child_environment(&mut command);
        Self::start_command(config, cancel_requested, &mut command)
    }

    fn start_command(
        config: &WebuiLaunchConfig,
        cancel_requested: &AtomicBool,
        command: &mut Command,
    ) -> Result<Self> {
        check_startup_cancelled(cancel_requested)?;
        config.validate()?;
        let addresses = ensure_port_available(config, cancel_requested)?;
        let client = config.http_client(Duration::from_millis(500))?;
        let url = config.backend_url()?;
        check_startup_cancelled(cancel_requested)?;
        let child = OwnedProcessGroup::spawn(command.create_no_window())?;
        let mut result = Self { child: Some(child) };
        let start_time = std::time::Instant::now();
        let mut last_error = "Backend has not responded".to_owned();
        while start_time.elapsed() < BACKEND_STARTUP_TIMEOUT {
            check_startup_cancelled(cancel_requested)?;
            let child = result.child.as_mut().unwrap();
            if let Some(status) = child.try_wait()? {
                return Err(backend_exit_error(config, &status));
            }
            // bind 预检到实际启动之间可能被其他程序抢占。只有本次进程边界内的
            // listener 能进入 HTTP 就绪检查；查询失败也不会误连到外部服务。
            match child.listener_state(config.port, &addresses)? {
                ListenerState::Foreign => return Err(port_in_use_error(config.port)),
                ListenerState::Pending => {
                    sleep(Duration::from_millis(100));
                    continue;
                }
                ListenerState::Owned => {}
            }
            match client.get(&url).send() {
                Ok(response) if !response.status().is_server_error() => {
                    check_startup_cancelled(cancel_requested)?;
                    if let Some(status) = child.try_wait()? {
                        return Err(backend_exit_error(config, &status));
                    }
                    // 响应期间退出或端口换主均不能将无关 HTTP 服务当成后端。
                    match child.listener_state(config.port, &addresses)? {
                        ListenerState::Owned => return Ok(result),
                        ListenerState::Foreign => return Err(port_in_use_error(config.port)),
                        ListenerState::Pending => {
                            last_error = "Backend listener disappeared".to_owned()
                        }
                    }
                }
                Ok(response) => last_error = format!("HTTP {}", response.status()),
                Err(error) => {
                    if is_tls_certificate_error(&error) {
                        let error = format!("{:#}", anyhow::Error::from(error));
                        return Err(anyhow!(rust_i18n::t!(
                            "errors.backend_tls_failed",
                            error = error
                        )));
                    }
                    last_error = format!("{:#}", anyhow::Error::from(error));
                }
            }
            sleep(Duration::from_millis(100));
        }
        check_startup_cancelled(cancel_requested)?;
        result.terminate().map_err(|error| {
            anyhow!(
                "Failed to stop timed out backend on port {} before recovery: {error:#}",
                config.port
            )
        })?;
        Err(BackendStartupTimeout {
            port: config.port,
            last_error,
        }
        .into())
    }

    /// 保留句柄直到清理成功；失败后 Drop 仍会重试拥有的进程边界。
    pub fn terminate(&mut self) -> Result<ExitStatus> {
        let Some(child) = self.child.as_mut() else {
            return Ok(ExitStatus::default());
        };
        let status = child.terminate()?;
        self.child.take();
        Ok(status)
    }
}

/// bind 只检测冲突，不扫描或终止端口占用者。返回的地址同时供 listener 归属过滤。
fn ensure_port_available(
    config: &WebuiLaunchConfig,
    cancelled: &AtomicBool,
) -> Result<Vec<IpAddr>> {
    let resolved: Vec<_> = (config.listen_host(), config.port)
        .to_socket_addrs()?
        .collect();
    if resolved.is_empty() {
        return Err(anyhow!(rust_i18n::t!("errors.backend_invalid_host")));
    }
    let mut addresses = Vec::new();
    let mut reservations = Vec::new();
    for address in resolved {
        if addresses.contains(&address.ip()) {
            continue;
        }
        let retry_until = std::time::Instant::now() + Duration::from_millis(300);
        loop {
            check_startup_cancelled(cancelled)?;
            match TcpListener::bind(address) {
                Ok(listener) => {
                    reservations.push(listener);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    // Unix 上并发 fork 暂时继承尚未 exec 关闭的 CLOEXEC socket。
                    // 只做短暂重试，不向占用者授予任何终止权限。
                    if std::time::Instant::now() >= retry_until {
                        warn!(port = config.port, %address, "Backend port bind preflight found an existing listener");
                        return Err(port_in_use_error(config.port));
                    }
                    sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        addresses.push(address.ip());
    }
    Ok(addresses)
}

fn port_in_use_error(port: u16) -> anyhow::Error {
    anyhow!(rust_i18n::t!("errors.backend_port_in_use", port = port))
}

impl Drop for ManagedBackend {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            warn!("Failed to terminate owned backend process group: {error:#}");
        }
    }
}

/// TLS 证书失败属于配置错误，继续等待不会修复；只做类型匹配，不猜错误文本。
fn is_tls_certificate_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if matches!(
        error.downcast_ref::<rustls::Error>(),
        Some(rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented)
    ) {
        return true;
    }
    // 某些网络层把 TLS 错误藏在 io::Error 的 payload，而 source() 跳过该层。
    if let Some(error) = error.downcast_ref::<std::io::Error>() {
        if error
            .get_ref()
            .is_some_and(|inner| is_tls_certificate_error(inner))
        {
            return true;
        }
    }
    error.source().is_some_and(is_tls_certificate_error)
}

fn check_startup_cancelled(cancel_requested: &AtomicBool) -> Result<()> {
    if cancel_requested.load(Ordering::Acquire) {
        Err(anyhow!(rust_i18n::t!("errors.backend_startup_cancelled")))
    } else {
        Ok(())
    }
}

/// 将子进程异常退出表述为可读原因：优先取 gui.py 写在日志里的失败原因，
/// 取不到时退回端口与退出码。
fn backend_exit_error(config: &WebuiLaunchConfig, status: &ExitStatus) -> anyhow::Error {
    match read_backend_failure_reason() {
        Some(reason) => anyhow!("{reason}（端口 {}，{status}）", config.port),
        None => anyhow!(
            "后端在端口 {} 就绪前退出：{status}（未能从日志读取失败原因）",
            config.port
        ),
    }
}

/// 读取当日 gui 日志尾部记录的失败原因。日志尚未写出时返回 None，由调用方退回通用提示。
pub(crate) fn read_backend_failure_reason() -> Option<String> {
    // 优先读当日日志；跨零点启动（如启动器挂到次日）当日文件尚未建立时，
    // 退回目录中修改时间最新的一份。
    let log_directory = alas_repo_dir().join("log");
    let today = log_directory.join(format!("{}_gui.txt", Local::now().format("%Y-%m-%d")));
    let log_path = if today.is_file() {
        today
    } else {
        newest_gui_log(&log_directory)?
    };
    read_log_tail(&log_path).and_then(|tail| extract_failure_reason(&tail))
}

/// 取目录中最新的一份 gui 日志；跨零点启动时当日文件尚未建立。
fn newest_gui_log(directory: &Path) -> Option<PathBuf> {
    std::fs::read_dir(directory)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_gui.txt"))
        })
        .max_by_key(|path| path.metadata().and_then(|meta| meta.modified()).ok())
}

/// 日志为 UTF-8，从尾部按字符边界回溯，避免截断多字节字符。
fn read_log_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(BACKEND_FAILURE_LOG_TAIL_BYTES);
    if start > 0 {
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start)).ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    if start > 0 {
        // 在 UTF-8 解码前丢弃被截断的首行，偏移可能落在多字节字符内部。
        let first_newline = bytes.iter().position(|byte| *byte == b'\n')?;
        bytes.drain(..=first_newline);
    }
    String::from_utf8(bytes).ok()
}

/// 日志行形如「时间 | 级别 | [GUI] AzurPilot Web服务启动失败：原因」，取最后一次的原因。
fn extract_failure_reason(log_tail: &str) -> Option<String> {
    log_tail
        .lines()
        .rev()
        .find_map(|line| line.split_once(BACKEND_FAILURE_LOG_MARKER))
        .map(|(_, reason)| reason.trim().to_owned())
        .filter(|reason| !reason.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ephemeral ports cannot be reserved across bind/drop/spawn. Serialize these
    // fixtures while keeping their explicit child/fork/listener races concurrent.
    static PROCESS_FIXTURES: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn isolate_process_fixtures() -> std::sync::MutexGuard<'static, ()> {
        PROCESS_FIXTURES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    struct ProcessFixture(std::process::Child);

    impl Drop for ProcessFixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn wait_for_fixture(mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(std::time::Instant::now() < deadline, "fixture timed out");
            sleep(Duration::from_millis(20));
        }
    }

    fn fixture_command(mode: &str, ready: &Path, marked: bool) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "backend::tests::process_cleanup_fixture",
                "--nocapture",
            ])
            .env("AZURPILOT_PROCESS_FIXTURE_MODE", mode)
            .env("AZURPILOT_PROCESS_FIXTURE_READY", ready)
            .env_remove("ALAS_LAUNCHER_PID")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if marked {
            command.env("ALAS_LAUNCHER_PID", std::process::id().to_string());
        }
        command
    }

    fn spawn_process_fixture(mode: &str, ready: &Path, marked: bool) -> ProcessFixture {
        let mut command = fixture_command(mode, ready, marked);
        let fixture = ProcessFixture(command.create_no_window().spawn().unwrap());
        wait_for_fixture(|| fixture_ready(ready));
        fixture
    }

    fn fixture_ready(path: &Path) -> bool {
        std::fs::read_to_string(path).is_ok_and(|content| content.parse::<u16>().is_ok())
    }

    fn fixture_port(path: &Path) -> u16 {
        std::fs::read_to_string(path).unwrap().parse().unwrap()
    }

    // 子测试进程只创建临时监听或等待退出，不启动真实 Python 后端。
    #[test]
    fn process_cleanup_fixture() {
        let Ok(mode) = std::env::var("AZURPILOT_PROCESS_FIXTURE_MODE") else {
            return;
        };
        let ready = PathBuf::from(std::env::var_os("AZURPILOT_PROCESS_FIXTURE_READY").unwrap());
        if mode == "gated-http" {
            let gate = ready.with_extension("gate");
            wait_for_fixture(|| gate.exists());
        }
        #[cfg(unix)]
        if mode == "worker" {
            // 证明组长收到 TERM 先退出后，守卫仍会向 worker 发最后一次 KILL。
            unsafe {
                nix::libc::signal(nix::libc::SIGTERM, nix::libc::SIG_IGN);
            }
        }
        let listener = (mode != "idle").then(|| {
            let port = std::env::var("AZURPILOT_PROCESS_FIXTURE_PORT")
                .ok()
                .and_then(|port| port.parse::<u16>().ok())
                .unwrap_or(0);
            TcpListener::bind(("127.0.0.1", port)).unwrap()
        });
        if mode.starts_with("tree") {
            let worker_ready = ready.with_extension("worker");
            let mut worker = fixture_command("worker", &worker_ready, false);
            worker.create_no_window().spawn().unwrap();
            wait_for_fixture(|| fixture_ready(&worker_ready));
        }
        let port = listener
            .as_ref()
            .map(|listener| listener.local_addr().unwrap().port())
            .unwrap_or_default();
        std::fs::write(&ready, port.to_string()).unwrap();
        if mode == "tree-exit" {
            return;
        }
        if mode == "http" || mode == "gated-http" {
            for stream in listener.unwrap().incoming() {
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0u8; 4096];
                let _ = std::io::Read::read(&mut stream, &mut request);
                let _ = std::io::Write::write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                );
            }
            return;
        }
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn port_conflict_leaves_unrelated_fixture_listener_running() {
        let _fixtures = isolate_process_fixtures();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("listener-port");
        let mut fixture = spawn_process_fixture("listener", &ready, false);
        let port = fixture_port(&ready);
        let mut config = WebuiLaunchConfig::from_deploy_config(None);
        config.port = port;

        assert!(ensure_port_available(&config, &AtomicBool::new(false)).is_err());
        assert!(fixture.0.try_wait().unwrap().is_none());
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
    }

    #[test]
    fn dropping_backend_preserves_unrelated_launcher_pid_marked_fixture() {
        let _fixtures = isolate_process_fixtures();
        let directory = tempfile::tempdir().unwrap();
        let mut marked = spawn_process_fixture("idle", &directory.path().join("marked"), true);
        let mut unmarked = spawn_process_fixture("idle", &directory.path().join("unmarked"), false);

        drop(ManagedBackend { child: None });

        assert!(marked.0.try_wait().unwrap().is_none());
        assert!(unmarked.0.try_wait().unwrap().is_none());
    }

    #[test]
    fn owned_backend_drop_reclaims_its_worker_subtree() {
        let _fixtures = isolate_process_fixtures();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("tree");
        let child = OwnedProcessGroup::spawn(&mut fixture_command("tree", &ready, false)).unwrap();
        let mut backend = ManagedBackend { child: Some(child) };
        wait_for_fixture(|| fixture_ready(&ready));
        let parent_port = fixture_port(&ready);
        let worker_port = fixture_port(&ready.with_extension("worker"));
        assert_eq!(
            backend
                .child
                .as_mut()
                .unwrap()
                .listener_state(parent_port, &["127.0.0.1".parse().unwrap()])
                .unwrap(),
            ListenerState::Owned,
        );
        drop(backend);
        wait_for_fixture(|| TcpListener::bind(("127.0.0.1", parent_port)).is_ok());
        wait_for_fixture(|| TcpListener::bind(("127.0.0.1", worker_port)).is_ok());
    }

    #[test]
    fn exited_group_leader_keeps_identity_until_worker_cleanup() {
        let _fixtures = isolate_process_fixtures();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("tree-exit");
        let mut child =
            OwnedProcessGroup::spawn(&mut fixture_command("tree-exit", &ready, false)).unwrap();
        wait_for_fixture(|| fixture_ready(&ready));
        wait_for_fixture(|| child.try_wait().unwrap().is_some());
        // 重复观察退出不会消耗 Unix 组长身份，也不会丢失 Windows Job。
        assert!(child.try_wait().unwrap().is_some());
        let worker_port = fixture_port(&ready.with_extension("worker"));
        drop(child);
        wait_for_fixture(|| TcpListener::bind(("127.0.0.1", worker_port)).is_ok());
    }

    #[test]
    fn cancelling_started_backend_reclaims_its_worker_subtree() {
        let _fixtures = isolate_process_fixtures();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("cancel-tree");
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = WebuiLaunchConfig::from_deploy_config(None);
        config.port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let cancelled = AtomicBool::new(false);
        let finished = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                wait_for_fixture(|| fixture_ready(&ready) || finished.load(Ordering::Acquire));
                if fixture_ready(&ready) {
                    cancelled.store(true, Ordering::Release);
                }
            });
            let result = ManagedBackend::start_command(
                &config,
                &cancelled,
                &mut fixture_command("tree", &ready, false),
            );
            finished.store(true, Ordering::Release);
            assert!(
                fixture_ready(&ready),
                "backend did not start: {:?}",
                result.as_ref().err()
            );
            assert!(result.is_err());
        });
        for path in [&ready, &ready.with_extension("worker")] {
            let port = fixture_port(path);
            wait_for_fixture(|| TcpListener::bind(("127.0.0.1", port)).is_ok());
        }
    }

    #[test]
    fn a_foreign_listener_winning_the_spawn_race_is_preserved_and_rejected() {
        let _fixtures = isolate_process_fixtures();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("owned-tree");
        let foreign_ready = directory.path().join("foreign");
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = WebuiLaunchConfig::from_deploy_config(None);
        config.port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let mut foreign_command = fixture_command("gated-http", &foreign_ready, true);
        foreign_command.env("AZURPILOT_PROCESS_FIXTURE_PORT", config.port.to_string());
        let mut foreign = ProcessFixture(foreign_command.create_no_window().spawn().unwrap());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                wait_for_fixture(|| fixture_ready(&ready));
                std::fs::write(foreign_ready.with_extension("gate"), b"open").unwrap();
            });
            assert!(ManagedBackend::start_command(
                &config,
                &AtomicBool::new(false),
                &mut fixture_command("tree", &ready, false),
            )
            .is_err());
        });
        assert!(foreign.0.try_wait().unwrap().is_none());
        assert!(std::net::TcpStream::connect(("127.0.0.1", config.port)).is_ok());
        for path in [&ready, &ready.with_extension("worker")] {
            let port = fixture_port(path);
            wait_for_fixture(|| TcpListener::bind(("127.0.0.1", port)).is_ok());
        }
    }

    #[test]
    fn an_owned_http_fixture_can_become_ready_and_then_be_reclaimed() {
        let _fixtures = isolate_process_fixtures();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("owned-http");
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = WebuiLaunchConfig::from_deploy_config(None);
        config.port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let mut command = fixture_command("http", &ready, false);
        command.env("AZURPILOT_PROCESS_FIXTURE_PORT", config.port.to_string());
        let backend = ManagedBackend::start_command(&config, &AtomicBool::new(false), &mut command)
            .expect("owned fixture should become ready");
        drop(backend);
        wait_for_fixture(|| TcpListener::bind(("127.0.0.1", config.port)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn port_preflight_waits_for_concurrent_fork_to_close_inherited_socket() {
        let _fixtures = isolate_process_fixtures();
        use std::os::{
            fd::AsRawFd,
            unix::{net::UnixStream, process::CommandExt},
        };

        let directory = tempfile::tempdir().unwrap();
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = WebuiLaunchConfig::from_deploy_config(None);
        config.port = reservation.local_addr().unwrap().port();
        let (mut signal_read, signal_write) = UnixStream::pair().unwrap();
        signal_read
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let signal_fd = signal_write.as_raw_fd();
        let mut command = fixture_command("idle", &directory.path().join("forked"), false);
        unsafe {
            command.pre_exec(move || {
                // 只使用 async-signal-safe syscall，确定子进程正持有 fork 继承的 fd。
                let byte = 1u8;
                if nix::libc::write(signal_fd, (&byte as *const u8).cast(), 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                let pause = nix::libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 150_000_000,
                };
                nix::libc::nanosleep(&pause, std::ptr::null_mut());
                Ok(())
            });
        }
        std::thread::scope(|scope| {
            let spawning = scope.spawn(move || ProcessFixture(command.spawn().unwrap()));
            let mut byte = [0u8; 1];
            std::io::Read::read_exact(&mut signal_read, &mut byte).unwrap();
            drop(reservation);
            assert_eq!(
                TcpListener::bind(("127.0.0.1", config.port))
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::AddrInUse
            );
            ensure_port_available(&config, &AtomicBool::new(false)).unwrap();
            drop(spawning.join().unwrap());
        });
    }

    #[test]
    fn backend_host_rejects_url_component_injection() {
        let mut config = WebuiLaunchConfig::from_deploy_config(None);
        for host in [
            "127.0.0.1@evil.example",
            "127.0.0.1/path",
            "localhost?next=evil.example",
            "localhost#fragment",
            "user:password@localhost",
            "localhost:9999",
            "localhost\\evil.example",
            "[::1",
            "[[::1]]",
            "",
        ] {
            config.host = host.to_owned();
            assert!(config.backend_url().is_err(), "{host}");
        }
    }

    #[test]
    fn backend_url_respects_scheme_and_listener_address() {
        let mut config = WebuiLaunchConfig::from_deploy_config(None);
        config.host = "::".into();
        assert_eq!(config.backend_url().unwrap(), "http://[::1]:22267/");
        config.host = "192.0.2.5".into();
        config.ssl_key = Some("key.pem".into());
        assert!(config.backend_url().is_err());
        config.ssl_cert = Some("cert.pem".into());
        assert_eq!(config.backend_url().unwrap(), "https://192.0.2.5:22267/");
        config.host = "[::1]".into();
        assert_eq!(config.args()[2], "::1");
        assert_eq!(config.backend_url().unwrap(), "https://[::1]:22267/");
    }

    #[test]
    fn log_tail_handles_offset_inside_a_utf8_character() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("utf8_gui.txt");
        let reason = format!("\n{BACKEND_FAILURE_LOG_MARKER}正确原因\n");
        let padding = BACKEND_FAILURE_LOG_TAIL_BYTES as usize - reason.len() - 2;
        // 总长度比上限多一字节，使 seek 停在首个汉字的第二字节。
        std::fs::write(&path, format!("中{}{}", "x".repeat(padding), reason)).unwrap();
        let tail = read_log_tail(&path).unwrap();
        assert_eq!(extract_failure_reason(&tail).as_deref(), Some("正确原因"));
    }

    #[test]
    fn tls_certificate_errors_are_recognized_through_io_wrapping() {
        let certificate_error =
            rustls::Error::InvalidCertificate(rustls::CertificateError::Expired);
        let wrapped = std::io::Error::new(std::io::ErrorKind::InvalidData, certificate_error);
        assert!(is_tls_certificate_error(&wrapped));
        assert!(is_tls_certificate_error(
            &rustls::Error::NoCertificatesPresented
        ));
        assert!(!is_tls_certificate_error(&std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "fixture refused connection"
        )));
        assert!(!is_tls_certificate_error(&std::io::Error::new(
            std::io::ErrorKind::Other,
            "invalid peer certificate: misleading plain text"
        )));
    }

    #[test]
    fn cancelled_start_does_not_spawn_a_backend() {
        let config = WebuiLaunchConfig::from_deploy_config(None);
        let cancelled = AtomicBool::new(true);
        assert!(ManagedBackend::new(&config, &cancelled).is_err());
    }

    #[test]
    fn startup_timeout_preserves_last_http_failure() {
        let timeout = BackendStartupTimeout {
            port: 22267,
            last_error: "TLS certificate validation failed: invalid peer certificate".into(),
        };
        let message = timeout.to_string();
        assert!(message.contains("22267"));
        assert!(message.contains("invalid peer certificate"));
    }

    /// 启动超时阈值应为 5 分钟，覆盖依赖同步等慢启动场景。
    #[test]
    fn backend_startup_timeout_is_five_minutes() {
        assert_eq!(BACKEND_STARTUP_TIMEOUT, Duration::from_secs(5 * 60));
    }

    /// 超时错误可被精确识别，且不会误判其他 `anyhow` 错误。
    #[test]
    fn backend_startup_timeout_is_identified_without_matching_other_errors() {
        let timeout: anyhow::Error = BackendStartupTimeout {
            port: 22267,
            last_error: "connection refused".into(),
        }
        .into();

        assert!(is_backend_startup_timeout(&timeout));
        assert!(!is_backend_startup_timeout(&anyhow!("other startup error")));
    }
    /// 能从真实格式的 gui 日志行中提取失败原因。
    #[test]
    fn failure_reason_is_extracted_from_a_real_gui_log_line() {
        // 与 rich 日志实际落盘格式一致：时间 | 级别 | 消息
        let line = format!(
            "2026-09-21 20:14:40.595 | ERROR | {BACKEND_FAILURE_LOG_MARKER}React 前端构建失败\r"
        );

        assert_eq!(
            extract_failure_reason(&line).as_deref(),
            Some("React 前端构建失败")
        );
    }

    /// 多条失败记录并存时取最后一次（最新）的原因。
    #[test]
    fn failure_reason_uses_the_last_recorded_failure() {
        let tail =
            format!("{BACKEND_FAILURE_LOG_MARKER}旧原因\n{BACKEND_FAILURE_LOG_MARKER}新原因\n");

        assert_eq!(extract_failure_reason(&tail).as_deref(), Some("新原因"));
    }

    /// 无标记行、或标记后原因为空时，均不产生失败原因。
    #[test]
    fn failure_reason_is_absent_without_the_marker() {
        assert!(extract_failure_reason("2026-09-21 20:14:40 | INFO | 普通日志\n").is_none());
        // 标记出现但原因为空时同样不作为原因
        assert!(extract_failure_reason(&format!("{BACKEND_FAILURE_LOG_MARKER}\n")).is_none());
    }

    /// 从文件中部回溯读取后，落在多字节字符中间的不完整首行应被丢弃。
    #[test]
    fn log_tail_drops_the_incomplete_first_line_after_seeking() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("2026-09-21_gui.txt");
        // 超过读取上限，迫使实现从文件中部定位
        let mut content = "x".repeat(BACKEND_FAILURE_LOG_TAIL_BYTES as usize + 64);
        content.push_str("\n第一行完整\n");
        content.push_str(&format!("{BACKEND_FAILURE_LOG_MARKER}依赖同步失败\n"));
        std::fs::write(&path, content).expect("write log");

        let tail = read_log_tail(&path).expect("tail");

        assert!(!tail.starts_with("xxxx"), "首行应从字符边界之后开始");
        assert_eq!(
            extract_failure_reason(&tail).as_deref(),
            Some("依赖同步失败")
        );
    }

    /// 日志文件不存在时返回 `None` 而非报错。
    #[test]
    fn log_tail_is_absent_for_a_missing_file() {
        assert!(read_log_tail(Path::new("does-not-exist_gui.txt")).is_none());
    }
}
