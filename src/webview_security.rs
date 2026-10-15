//! WebView 的导航、主框架 bridge 和原生命令共用的来源策略。
//!
//! Tauri 2.5 的应用命令必须同时登记 AppManifest；capability 负责 IPC 请求
//! 的来源，`Caller` 再检查实际窗口、当前文档和主框架凭证。不要只检查
//! `WebviewWindow::url()`：它始终是顶层文档的 URL。

use rand::RngCore;
use serde_json::{json, Value};
use tauri::{
    ipc::{CommandArg, CommandItem, InvokeBody, InvokeError},
    Manager, Runtime, Url, WebviewWindow,
};

#[cfg(any(windows, target_os = "android"))]
pub const SPLASH_URL: &str = "http://alas-splash.localhost/";
#[cfg(not(any(windows, target_os = "android")))]
pub const SPLASH_URL: &str = "alas-splash://localhost/";

#[cfg(any(windows, target_os = "android"))]
pub const BACKEND_ERROR_URL_BASE: &str = "http://alas-error.localhost/backend";
#[cfg(not(any(windows, target_os = "android")))]
pub const BACKEND_ERROR_URL_BASE: &str = "alas-error://localhost/backend";

/// 也用于 build.rs 的 AppManifest；新增命令必须显式更新来源矩阵。
#[cfg(test)]
pub const APP_COMMANDS: &[&str] = &[
    "get_launcher_preferences",
    "set_launcher_close_behavior",
    "window_choose_close_behavior",
    "save_as",
    "splash_ready",
    "download_today_gui_log",
    "download_today_launcher_log",
    "retry_backend_connection",
    "window_hide",
    "window_minimize",
    "window_toggle_maximize",
    "window_close",
    "window_exit_application",
    "window_start_dragging",
    "window_is_maximized",
];

const SPLASH_COMMANDS: &[&str] = &[
    "splash_ready",
    "download_today_launcher_log",
    "window_minimize",
    "window_close",
    "window_start_dragging",
];
const ERROR_COMMANDS: &[&str] = &[
    "window_choose_close_behavior",
    "download_today_gui_log",
    "download_today_launcher_log",
    "retry_backend_connection",
    "window_hide",
    "window_minimize",
    "window_toggle_maximize",
    "window_close",
    "window_exit_application",
    "window_start_dragging",
    "window_is_maximized",
];
const BACKEND_COMMANDS: &[&str] = &[
    "get_launcher_preferences",
    "set_launcher_close_behavior",
    "window_choose_close_behavior",
    "save_as",
    "window_hide",
    "window_minimize",
    "window_toggle_maximize",
    "window_close",
    "window_exit_application",
    "window_start_dragging",
    "window_is_maximized",
];

const DENIED: &str = "Native command denied for this document";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Document {
    Splash,
    BackendError,
    Backend,
}

/// 本次启动固定的策略。私有凭证不实现 Debug，也不写入 HTML、URL 或日志。
#[derive(Clone)]
pub struct WebviewPolicy {
    backend: Url,
    backend_native: bool,
    splash: Url,
    error: Url,
    splash_token: String,
    error_token: String,
    backend_token: String,
}

/// 原生免密、反向控制流与 WebView IPC 共用的保守回环判断。
///
/// 仅接收 HTTP(S)、无用户信息、精确 localhost 或 IP 回环；不解析 DNS，
/// 不把 *.localhost、localhost.、IPv4-mapped IPv6 或私网地址当作回环。
pub fn is_loopback_http_origin(url: &Url) -> bool {
    if !is_http_document(url) {
        return false;
    }
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        None => false,
    }
}

fn is_http_document(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default().is_some_and(|port| port != 0)
}

fn internal_document(url: &Url, expected: &Url, allow_query: bool) -> bool {
    url.scheme() == expected.scheme()
        && url.host_str() == expected.host_str()
        && url.port_or_known_default() == expected.port_or_known_default()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == expected.path()
        && (allow_query || url.query().is_none())
        && url.fragment().is_none()
}

fn random_token() -> Result<String, String> {
    let mut token = [0_u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut token)
        .map_err(|_| "Unable to initialize WebView security".to_owned())?;
    use base64::Engine;
    Ok(base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(token))
}

fn backend_origin_pattern(url: &Url) -> String {
    // urlpattern 0.3 把 IPv6 冒号当作命名参数；只转义主机中的冒号，
    // 保留协议和端口分隔符，不能以通配主机代替精确 IPv6 来源。
    let hostname = url.host_str().unwrap_or_default().replace(':', "\\:");
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    format!("{}://{hostname}{port}/*", url.scheme())
}

impl WebviewPolicy {
    /// 必须传入同一份 WebuiLaunchConfig::backend_url()，不能从页面参数重建。
    pub fn new(backend: &Url) -> Result<Self, String> {
        Self::with_internal_urls(backend, SPLASH_URL, BACKEND_ERROR_URL_BASE)
    }

    fn with_internal_urls(backend: &Url, splash: &str, error: &str) -> Result<Self, String> {
        if !is_http_document(backend)
            || backend.path() != "/"
            || backend.query().is_some()
            || backend.fragment().is_some()
        {
            return Err("Backend URL must be a credential-free HTTP(S) origin".into());
        }
        // 这些名字由 WebView 自定义协议保留，不应被配置成网络后端。
        if backend.host_str().is_some_and(|host| {
            matches!(
                host,
                "alas-splash.localhost"
                    | "alas-error.localhost"
                    | "tauri.localhost"
                    | "ipc.localhost"
            ) || host
                .chars()
                .any(|c| matches!(c, '*' | '(' | ')' | '{' | '}' | '\\'))
        }) {
            return Err("Backend URL uses a reserved WebView hostname".into());
        }
        let backend_native = is_loopback_http_origin(backend);
        if backend_native {
            // Tauri 2.5 的 add_capability 内部会对无效 pattern panic；先以同一
            // 解析器验证受控 pattern，让失败通过启动 Result 正常传递。
            backend_origin_pattern(backend)
                .parse::<tauri::utils::acl::RemoteUrlPattern>()
                .map_err(|error| format!("Unsupported backend permission origin: {error}"))?;
        }
        Ok(Self {
            backend: backend.clone(),
            backend_native,
            splash: Url::parse(splash).map_err(|error| error.to_string())?,
            error: Url::parse(error).map_err(|error| error.to_string())?,
            splash_token: random_token()?,
            error_token: random_token()?,
            backend_token: random_token()?,
        })
    }

    #[cfg(test)]
    pub fn backend_url(&self) -> &Url {
        &self.backend
    }

    pub fn backend_has_native_access(&self) -> bool {
        self.backend_native
    }

    pub fn is_backend_url(&self, url: &Url) -> bool {
        is_http_document(url) && url.origin() == self.backend.origin()
    }

    pub fn is_splash_url(&self, url: &Url) -> bool {
        internal_document(url, &self.splash, false)
    }

    pub fn is_backend_error_url(&self, url: &Url) -> bool {
        internal_document(url, &self.error, true)
    }

    /// 用于两个窗口的 on_navigation；about:blank 仅可作为无权限初始占位。
    pub fn allows_navigation(&self, label: &str, url: &Url) -> bool {
        match label {
            "splash" => self.is_splash_url(url),
            "main" => self.is_backend_error_url(url) || self.is_backend_url(url),
            _ => false,
        }
    }

    fn native_document(&self, label: &str, url: &Url) -> Option<Document> {
        match label {
            "splash" if self.is_splash_url(url) => Some(Document::Splash),
            "main" if self.is_backend_error_url(url) => Some(Document::BackendError),
            "main" if self.backend_native && self.is_backend_url(url) => Some(Document::Backend),
            _ => None,
        }
    }

    /// 事件与当前窗口 URL 都必须可信；注入的 JS 仍需再次检查执行时来源。
    pub fn can_inject(&self, label: &str, event_url: &Url, current_url: &Url) -> bool {
        let Some(document) = self.native_document(label, event_url) else {
            return false;
        };
        self.native_document(label, current_url) == Some(document)
    }

    fn commands_for(document: Document) -> &'static [&'static str] {
        match document {
            Document::Splash => SPLASH_COMMANDS,
            Document::BackendError => ERROR_COMMANDS,
            Document::Backend => BACKEND_COMMANDS,
        }
    }

    fn authorize(
        &self,
        command: &str,
        window_label: &str,
        webview_label: &str,
        current_url: &Url,
        token: &str,
    ) -> Result<(), String> {
        if window_label != webview_label {
            return Err(DENIED.into());
        }
        let document = self
            .native_document(window_label, current_url)
            .ok_or_else(|| DENIED.to_owned())?;
        let expected_token = match document {
            Document::Splash => &self.splash_token,
            Document::BackendError => &self.error_token,
            Document::Backend => &self.backend_token,
        };
        if token != expected_token || !Self::commands_for(document).contains(&command) {
            return Err(DENIED.into());
        }
        Ok(())
    }

    /// 不给任何 core/plugin 默认权限；Rust 内部 dialog 调用不需要 JS 权限。
    /// 内部协议在 Tauri 中归入 Local，故精确路由由 authorize 二次检查。
    pub fn capabilities(&self) -> Vec<Value> {
        let permissions = |commands: &[&str]| {
            commands
                .iter()
                .map(|command| format!("allow-{}", command.replace('_', "-")))
                .collect::<Vec<_>>()
        };
        let mut capabilities = vec![
            json!({
                "identifier": "launcher-splash",
                "local": true,
                "windows": ["splash"],
                "permissions": permissions(SPLASH_COMMANDS)
            }),
            json!({
                "identifier": "launcher-error",
                "local": true,
                "windows": ["main"],
                "permissions": permissions(ERROR_COMMANDS)
            }),
        ];
        if self.backend_native {
            capabilities.push(json!({
                "identifier": "launcher-backend",
                "local": false,
                "windows": ["main"],
                "remote": { "urls": [backend_origin_pattern(&self.backend)] },
                "permissions": permissions(BACKEND_COMMANDS)
            }));
        }
        capabilities
    }

    /// setup 的第一步调用，必须早于任何内部页面导航。
    pub fn install_capabilities<R: Runtime>(&self, app: &tauri::AppHandle<R>) -> tauri::Result<()> {
        let capabilities = serde_json::to_string(&self.capabilities())?;
        app.add_capability(capabilities)
    }

    /// 作为 plugin 的 js_init_script 运行，Tauri 会把它放在页面脚本之前。
    /// Windows 即使把初始化脚本送入子框架，JS 的 top !== self 检查也会拒绝。
    pub fn bridge_plugin<R: Runtime>(&self) -> Result<tauri::plugin::TauriPlugin<R>, String> {
        let policy = json!({
            "splashUrl": self.splash.as_str(),
            "errorUrl": self.error.as_str(),
            "backendOrigin": self.backend.origin().ascii_serialization(),
            "backendNative": self.backend_native,
            "commands": {
                "splash": SPLASH_COMMANDS,
                "error": ERROR_COMMANDS,
                "backend": BACKEND_COMMANDS
            }
        });
        let tokens = json!({
            "splash": self.splash_token,
            "error": self.error_token,
            "backend": self.backend_token
        });
        let script = include_str!("webview_bridge.js")
            .replace("__ALAS_SECURITY_POLICY__", &policy.to_string())
            .replace("__ALAS_SECURITY_TOKENS__", &tokens.to_string());
        let navigation_policy = self.clone();
        Ok(tauri::plugin::Builder::new("launcher-security")
            .js_init_script(script)
            .on_navigation(move |webview, url| {
                navigation_policy.allows_navigation(webview.label(), url)
            })
            .on_webview_ready(|webview| {
                if let Err(error) = crate::popup_guard::install(&webview) {
                    tracing::error!("Failed to install native popup guard: {error}");
                    let _ = webview.window().close();
                }
            })
            .build())
    }
}

/// 不从 JS 参数获取窗口；Tauri 的 CommandArg 提供实际接收请求的 WebView。
/// 这个参数必须是所有导出命令的第一个参数，早于大数据反序列化。
pub struct Caller<R: Runtime = tauri::Wry> {
    window: WebviewWindow<R>,
    command: &'static str,
    token: String,
}

impl<'de, R: Runtime> CommandArg<'de, R> for Caller<R> {
    fn from_command(command: CommandItem<'de, R>) -> Result<Self, InvokeError> {
        let name = command.name;
        let token = match command.message.payload() {
            InvokeBody::Json(body) => body.get("launcherToken").and_then(Value::as_str),
            InvokeBody::Raw(_) => None,
        }
        .filter(|token| token.len() == 43)
        .ok_or_else(|| InvokeError::from(DENIED))?
        .to_owned();
        let window = WebviewWindow::from_command(command)?;
        let caller = Self {
            window,
            command: name,
            token,
        };
        caller.revalidate().map_err(InvokeError::from)?;
        Ok(caller)
    }
}

impl<R: Runtime> Caller<R> {
    pub fn window(&self) -> &WebviewWindow<R> {
        &self.window
    }

    /// await 或系统对话框后再次检查当前来源和角色。
    /// 凭证按会话和角色生成，同角色重载不提供逐文档隔离。
    pub fn revalidate(&self) -> Result<(), String> {
        let policy = self
            .window
            .try_state::<WebviewPolicy>()
            .ok_or_else(|| DENIED.to_owned())?;
        let current_url = self.window.url().map_err(|_| DENIED.to_owned())?;
        let webview: &tauri::Webview<R> = self.window.as_ref();
        policy.authorize(
            self.command,
            webview.window().label(),
            webview.label(),
            &current_url,
            &self.token,
        )
    }

    pub fn require_visible(&self) -> Result<(), String> {
        self.revalidate()?;
        if !self.window.is_visible().map_err(|_| DENIED.to_owned())?
            || self.window.is_minimized().map_err(|_| DENIED.to_owned())?
        {
            return Err(DENIED.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> WebviewPolicy {
        WebviewPolicy::new(&Url::parse("http://127.0.0.1:22267/").unwrap()).unwrap()
    }

    #[test]
    fn backend_origin_is_exact_and_credential_free() {
        let policy = policy();
        assert!(policy.is_backend_url(&Url::parse("http://127.0.0.1:22267/a?q=1#part").unwrap()));
        for untrusted in [
            "http://localhost:22267/",
            "http://127.0.0.1:22268/",
            "https://127.0.0.1:22267/",
            "http://user@127.0.0.1:22267/",
            "http://127.0.0.1:22267@evil.example/",
            "http://localhost.evil.example:22267/",
            "http://127.0.0.1:22267.evil.example/",
            "file:///tmp/page.html",
            "about:blank",
            "data:text/html,test",
            "blob:http://127.0.0.1:22267/id",
            "javascript:alert(1)",
        ] {
            if let Ok(url) = Url::parse(untrusted) {
                assert!(!policy.allows_navigation("main", &url), "{untrusted}");
            }
        }
    }

    #[test]
    fn both_platform_protocol_forms_only_accept_their_exact_authority_and_route() {
        for (splash, error) in [
            ("alas-splash://localhost/", "alas-error://localhost/backend"),
            (
                "http://alas-splash.localhost/",
                "http://alas-error.localhost/backend",
            ),
        ] {
            let policy = WebviewPolicy::with_internal_urls(
                &Url::parse("http://127.0.0.1:22267/").unwrap(),
                splash,
                error,
            )
            .unwrap();
            assert!(policy.allows_navigation("splash", &Url::parse(splash).unwrap()));
            assert!(!policy.allows_navigation("main", &Url::parse(splash).unwrap()));
            assert!(policy.allows_navigation(
                "main",
                &Url::parse(&format!("{error}?port=1&detail=text")).unwrap()
            ));
            assert!(!policy.allows_navigation("splash", &Url::parse(error).unwrap()));
            for bad in [
                splash.replace("localhost", "localhost.evil.example"),
                splash.replace("localhost", "localhost:22267"),
                splash.replace("localhost", "user@localhost"),
                format!("{splash}extra"),
                format!("{splash}?x=1"),
                format!("{splash}#x"),
                error.replace("/backend", "/untrusted"),
                error.replace("localhost", "localhost.evil.example"),
                error.replace("localhost", "localhost:22267"),
                error.replace("localhost", "user@localhost"),
            ] {
                let url = Url::parse(&bad).unwrap();
                assert!(!policy.allows_navigation("main", &url));
                assert!(!policy.allows_navigation("splash", &url));
            }
        }
    }

    #[test]
    fn external_backend_can_render_without_native_or_auth_privileges() {
        for origin in ["https://webui.example:8443/", "http://192.168.1.10:22267/"] {
            let policy = WebviewPolicy::new(&Url::parse(origin).unwrap()).unwrap();
            assert!(policy.allows_navigation("main", policy.backend_url()));
            assert!(!policy.backend_has_native_access());
            assert!(!policy.can_inject("main", policy.backend_url(), policy.backend_url()));
            assert_eq!(policy.capabilities().len(), 2);
            assert!(policy
                .authorize(
                    "save_as",
                    "main",
                    "main",
                    policy.backend_url(),
                    &policy.backend_token
                )
                .is_err());
        }
    }

    #[test]
    fn loopback_supports_https_and_ipv6_without_hostname_suffixes_or_mapped_addresses() {
        for origin in [
            "https://127.0.0.1:22267/",
            "https://[::1]:22267/",
            "http://localhost:22267/",
        ] {
            let policy = WebviewPolicy::new(&Url::parse(origin).unwrap()).unwrap();
            assert!(policy.backend_has_native_access());
            let urls = policy.capabilities()[2]["remote"]["urls"].clone();
            assert_eq!(urls[0], backend_origin_pattern(policy.backend_url()));
        }
        for origin in [
            "http://localhost.evil/",
            "http://localhost./",
            "http://evil.localhost/",
            "http://[::ffff:127.0.0.1]/",
            "http://192.168.1.1/",
            "https://user@localhost/",
            "file://localhost/a",
        ] {
            assert!(
                !is_loopback_http_origin(&Url::parse(origin).unwrap()),
                "{origin}"
            );
        }
    }

    #[test]
    fn commands_require_their_window_document_and_main_frame_token() {
        let policy = policy();
        let splash = &policy.splash;
        let error = &policy.error;
        let backend = &policy.backend;
        assert!(policy
            .authorize(
                "splash_ready",
                "splash",
                "splash",
                splash,
                &policy.splash_token
            )
            .is_ok());
        assert!(policy
            .authorize(
                "retry_backend_connection",
                "main",
                "main",
                error,
                &policy.error_token
            )
            .is_ok());
        assert!(policy
            .authorize("save_as", "main", "main", backend, &policy.backend_token)
            .is_ok());
        for (command, window, webview, url, token) in [
            (
                "window_exit_application",
                "splash",
                "splash",
                splash,
                policy.splash_token.as_str(),
            ),
            (
                "save_as",
                "main",
                "main",
                error,
                policy.error_token.as_str(),
            ),
            (
                "download_today_gui_log",
                "main",
                "main",
                backend,
                policy.backend_token.as_str(),
            ),
            (
                "retry_backend_connection",
                "main",
                "main",
                backend,
                policy.backend_token.as_str(),
            ),
            (
                "splash_ready",
                "main",
                "main",
                error,
                policy.error_token.as_str(),
            ),
            (
                "window_close",
                "main",
                "child",
                backend,
                policy.backend_token.as_str(),
            ),
            (
                "save_as",
                "main",
                "main",
                backend,
                policy.splash_token.as_str(),
            ),
            ("save_as", "main", "main", backend, ""),
            (
                "unknown_future_command",
                "main",
                "main",
                backend,
                policy.backend_token.as_str(),
            ),
            (
                "window_close",
                "main",
                "main",
                error,
                policy.backend_token.as_str(),
            ),
            (
                "window_close",
                "main",
                "main",
                backend,
                policy.error_token.as_str(),
            ),
        ] {
            assert!(
                policy
                    .authorize(command, window, webview, url, token)
                    .is_err(),
                "{command}"
            );
        }
        assert!(policy
            .authorize(
                "window_exit_application",
                "main",
                "main",
                &Url::parse("about:blank").unwrap(),
                &policy.backend_token
            )
            .is_err());
    }

    #[test]
    fn close_preferences_are_only_available_to_the_trusted_main_frame() {
        let policy = policy();
        for command in [
            "get_launcher_preferences",
            "set_launcher_close_behavior",
            "window_choose_close_behavior",
        ] {
            assert!(policy
                .authorize(
                    command,
                    "main",
                    "main",
                    &policy.backend,
                    &policy.backend_token
                )
                .is_ok());
            assert!(policy
                .authorize(
                    command,
                    "splash",
                    "splash",
                    &policy.splash,
                    &policy.splash_token
                )
                .is_err());
            assert!(policy
                .authorize(
                    command,
                    "main",
                    "child",
                    &policy.backend,
                    &policy.backend_token
                )
                .is_err());
            assert!(policy
                .authorize(
                    command,
                    "main",
                    "main",
                    &policy.backend,
                    &policy.error_token
                )
                .is_err());
            assert_eq!(
                policy
                    .authorize(command, "main", "main", &policy.error, &policy.error_token)
                    .is_ok(),
                command == "window_choose_close_behavior",
            );
        }
    }

    #[test]
    fn page_load_race_and_capability_scope_do_not_grant_additional_commands() {
        let policy = policy();
        assert!(!policy.can_inject(
            "main",
            &policy.backend,
            &Url::parse("https://evil.example/").unwrap()
        ));
        assert!(!policy.can_inject("main", &policy.backend, &policy.error));
        assert!(!policy.can_inject("splash", &policy.splash, &policy.backend));
        let all_commands = [SPLASH_COMMANDS, ERROR_COMMANDS, BACKEND_COMMANDS].concat();
        for command in APP_COMMANDS {
            assert!(
                all_commands.contains(command),
                "missing origin policy for {command}"
            );
        }
        for capability in policy.capabilities() {
            assert_eq!(capability["windows"].as_array().unwrap().len(), 1);
            for permission in capability["permissions"].as_array().unwrap() {
                assert!(!permission.as_str().unwrap().contains(':'));
            }
        }
    }

    #[test]
    fn locked_tauri_authority_enforces_exact_backend_origin_and_window_capabilities() {
        use std::collections::BTreeMap;
        use tauri::{
            ipc::{Origin, RuntimeAuthority},
            utils::acl::{manifest::Manifest, resolved::Resolved},
        };
        for backend in ["http://127.0.0.1:22267/", "https://[::1]:22267/"] {
            let policy = WebviewPolicy::new(&Url::parse(backend).unwrap()).unwrap();
            let permissions = APP_COMMANDS
                .iter()
                .map(|command| {
                    json!({
                        "identifier": format!("allow-{}", command.replace('_', "-")),
                        "commands": { "allow": [command] }
                    })
                })
                .collect::<Vec<_>>();
            let manifest = Manifest::new(
                vec![serde_json::from_value(json!({"permission": permissions})).unwrap()],
                None,
            );
            let mut authority = RuntimeAuthority::new(
                BTreeMap::from([(tauri::utils::acl::APP_ACL_KEY.to_owned(), manifest)]),
                Resolved::default(),
            );
            authority
                .add_capability(serde_json::to_string(&policy.capabilities()).unwrap())
                .unwrap();
            let backend_origin = Origin::Remote {
                url: policy.backend.clone(),
            };
            assert!(authority
                .resolve_access("save_as", "main", "main", &backend_origin)
                .is_some());
            assert!(authority
                .resolve_access("save_as", "splash", "splash", &backend_origin)
                .is_none());
            assert!(authority
                .resolve_access("retry_backend_connection", "main", "main", &backend_origin)
                .is_none());
            assert!(authority
                .resolve_access(
                    "download_today_launcher_log",
                    "splash",
                    "splash",
                    &Origin::Local
                )
                .is_some());
            assert!(authority
                .resolve_access(
                    "window_exit_application",
                    "splash",
                    "splash",
                    &Origin::Local
                )
                .is_none());
            for untrusted in [
                "http://127.0.0.1:22268/",
                "http://localhost:22267/",
                "https://evil.example/",
                "http://alas-error.evil.example/backend",
                "about:blank",
                "file:///tmp/a.html",
            ] {
                let origin = Origin::Remote {
                    url: Url::parse(untrusted).unwrap(),
                };
                assert!(
                    authority
                        .resolve_access("save_as", "main", "main", &origin)
                        .is_none(),
                    "{untrusted}"
                );
            }
        }
    }
}
