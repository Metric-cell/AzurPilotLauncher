//! 后端进程托管模块：负责 gui.py（AzurPilot WebUI）子进程的完整生命周期。
//! 启动前检查实际监听地址是否可用，绝不终止无法证明归属的占用进程。
//! 子进程使用独立进程组，仅凭持有的句柄回收本次后端及其派生进程。

use std::{
    io::Read as _,
    net::{TcpListener, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    sync::atomic::{AtomicBool, Ordering},
    thread::sleep,
    time::Duration,
};

use anyhow::{anyhow, Result};
use chrono::Local;
use command_group::{CommandGroup, GroupChild};
use serde_json::Value as JsonValue;
use tracing::warn;

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
        self.host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
    }

    fn validate(&self) -> Result<()> {
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

/// 托管中的 gui.py 子进程句柄。
///
/// 持有进程组的 [`GroupChild`]，使终止操作能连同 gui.py 派生的孙进程
/// （例如 uvicorn worker）一并结束，不依赖共享的启动器 PID 环境变量扫描。
pub struct ManagedBackend {
    /// 进程组子进程句柄；`None` 表示已被 [`ManagedBackend::terminate`]
    /// 取走或从未成功拉起，此时 `Drop` 不再重复强杀。
    child: Option<GroupChild>,
}

/// `ManagedBackend` 的启动与终止逻辑。
impl ManagedBackend {
    /// 按配置启动 gui.py 并阻塞等待其 WebUI 端口就绪。
    ///
    /// 检查监听地址、启动进程组，再等待 HTTP(S) 服务回应。
    /// 失败和超时路径均回收本次启动的进程组，不处理已有用户服务。
    ///
    /// # Errors
    ///
    /// 配置无效、监听地址占用、进程启动失败、提前退出或就绪超时均返回错误。
    pub fn new(config: &WebuiLaunchConfig, cancel_requested: &AtomicBool) -> Result<Self> {
        check_startup_cancelled(cancel_requested)?;
        config.validate()?;
        ensure_listener_available(config)?;
        let client = config.http_client(Duration::from_millis(500))?;
        let url = config.backend_url()?;

        // 使用仓库 venv 内的 Python 解释器，避免依赖系统全局 Python 环境。
        let mut command = Command::new(venv_python());
        command.args(config.args());
        command.current_dir(alas_repo_dir());
        command.env("ALAS_LAUNCHER_PID", std::process::id().to_string());
        // 注入启动器信任密钥，WebUI 据此为启动器窗口开启免密；子进程缺省继承，
        // isolate_python_child_environment 不清理该键。
        command.env(TRUST_SECRET_ENV, launcher_trust_secret());
        isolate_python_child_environment(&mut command);
        // 以进程组方式拉起：terminate/drop 时可整组回收，防止孙进程残留。
        let child = command.group().create_no_window().spawn()?;
        let mut res = Self { child: Some(child) };

        let start_time = std::time::Instant::now();
        let mut last_error = "Backend has not responded".to_owned();
        while start_time.elapsed() < BACKEND_STARTUP_TIMEOUT {
            check_startup_cancelled(cancel_requested)?;
            // 先检查退出状态，防止已退出子进程被其他监听者的响应掩盖。
            if let Some(child) = res.child.as_mut() {
                if let Some(status) = child.try_wait()? {
                    return Err(backend_exit_error(config, &status));
                }
            }
            // TCP 接通不能证明 HTTP 服务已就绪；TLS 请求保留证书和主机名验证。
            match client.get(&url).send() {
                Ok(response) if !response.status().is_server_error() => {
                    check_startup_cancelled(cancel_requested)?;
                    if let Some(child) = res.child.as_mut() {
                        if let Some(status) = child.try_wait()? {
                            return Err(backend_exit_error(config, &status));
                        }
                    }
                    return Ok(res);
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
        // 超时路径：先回收刚拉起的子进程，再向上报告超时，保证不留孤儿进程。
        res.terminate().map_err(|error| {
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

    /// 终止托管的 gui.py 进程组并等待其退出。
    ///
    /// Unix 上先向进程组发送 `SIGTERM` 并给予 500 毫秒优雅退出窗口，仍未
    /// 退出才强杀；Windows 没有 SIGTERM 语义，直接强杀。子进程句柄已被取走
    /// 或从未拉起时，返回默认的 `ExitStatus`（视同正常退出）。
    /// 子进程组长已退出时也会回收组内遗留进程；失败时保留句柄供 Drop 重试。
    ///
    /// # Errors
    ///
    /// 强杀或回收退出状态失败（例如句柄已失效、等待被中断）时返回错误。
    pub fn terminate(&mut self) -> Result<ExitStatus> {
        if let Some(child) = self.child.as_mut() {
            #[cfg(unix)]
            {
                use command_group::{Signal, UnixChildExt};
                let _ = child.signal(Signal::SIGTERM);
                let start_time = std::time::Instant::now();
                while start_time.elapsed() < Duration::from_millis(500) {
                    if let Some(status) = child.try_wait()? {
                        // 仍强杀进程组，以清理已退出组长留下的孙进程。
                        kill_owned_group(child)?;
                        self.child.take();
                        return Ok(status);
                    }
                    sleep(Duration::from_millis(50));
                }
            }
            // 失败时保留句柄，Drop 仍能重试回收。
            kill_owned_group(child)?;
            let status = child.wait()?;
            self.child.take();
            Ok(status)
        } else {
            Ok(ExitStatus::default())
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

/// 已不存在的 Unix 进程组视为已回收；其余错误保留句柄并传播。
fn kill_owned_group(child: &mut GroupChild) -> std::io::Result<()> {
    match child.kill() {
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(nix::libc::ESRCH) => Ok(()),
        result => result,
    }
}

/// 只回收当前句柄拥有的进程组；不扫描共享环境标记，避免旧实例误杀新实例。
impl Drop for ManagedBackend {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            warn!("Failed to stop gui.py process group: {error:#}");
        }
    }
}

/// 检查实际监听地址能否绑定。检查不会终止任何占用端口的进程。
fn ensure_listener_available(config: &WebuiLaunchConfig) -> Result<()> {
    let addresses: Vec<_> = (config.listen_host(), config.port)
        .to_socket_addrs()?
        .collect();
    if addresses.is_empty() {
        return Err(anyhow!("No listening address resolved for {}", config.host));
    }
    let mut listeners = Vec::new();
    for address in addresses {
        listeners.push(TcpListener::bind(address).map_err(|error| {
            anyhow!(rust_i18n::t!(
                "errors.backend_port_in_use",
                host = config.host.as_str(),
                port = config.port,
                error = error.to_string()
            ))
        })?);
    }
    Ok(())
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

    #[cfg(unix)]
    fn wait_for_fixture(condition: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(std::time::Instant::now() < deadline, "fixture timed out");
            sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    fn terminate_cleans_descendants_after_group_leader_exits() {
        let directory = tempfile::tempdir().unwrap();
        let heartbeat = directory.path().join("heartbeat");
        let pid_file = directory.path().join("pid");
        let child = Command::new("sh")
            .args(["-c", "sh -c 'trap \"\" TERM; while :; do echo tick >> \"$1\"; sleep 0.05; done' fixture \"$1\" & echo $! > \"$2\"", "fixture"])
            .arg(&heartbeat)
            .arg(&pid_file)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .group_spawn()
            .unwrap();
        let mut backend = ManagedBackend { child: Some(child) };
        wait_for_fixture(|| heartbeat.exists() && pid_file.exists());
        // 确保组长已被 try_wait 回收，孙进程仍持续运行。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while backend
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
        {
            assert!(std::time::Instant::now() < deadline);
            sleep(Duration::from_millis(20));
        }
        let pid: u32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        backend.terminate().unwrap();
        wait_for_fixture(|| {
            let system = sysinfo::System::new_all();
            system
                .process(sysinfo::Pid::from_u32(pid))
                .is_none_or(|process| {
                    process.status() == sysinfo::ProcessStatus::Zombie
                        || process.status() == sysinfo::ProcessStatus::Dead
                })
        });
        assert!(backend.child.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn dropping_backend_leaves_unrelated_same_launcher_pid_fixture_running() {
        fn fixture() -> ManagedBackend {
            let child = Command::new("sh")
                .args(["-c", "while :; do sleep 1; done"])
                .env("ALAS_LAUNCHER_PID", std::process::id().to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .group_spawn()
                .unwrap();
            ManagedBackend { child: Some(child) }
        }
        let mut unrelated = fixture();
        let owned = fixture();
        assert!(unrelated
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none());
        drop(owned);
        assert!(unrelated
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none());
        unrelated.terminate().unwrap();
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
    fn occupied_listener_is_reported_and_left_running() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = WebuiLaunchConfig::from_deploy_config(None);
        config.port = listener.local_addr().unwrap().port();
        assert!(ensure_listener_available(&config).is_err());
        assert!(std::net::TcpStream::connect(listener.local_addr().unwrap()).is_ok());
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
