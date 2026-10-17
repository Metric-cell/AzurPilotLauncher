use super::splash::splash_redesigned_shell_html;
use super::*;
const TAURI_CONFIG_SOURCE: &str = include_str!("../../tauri.conf.json");

#[test]
fn error_details_cannot_inject_markup_or_select_the_retry_port() {
    let html = backend_error_html(
        "https://[::1]:22267/",
        "</script><script>window.__ALAS_NATIVE__.invoke('window_exit_application')</script>",
    );
    assert!(!html.contains("</script><script>"));
    assert!(html.contains("\\u003c/script\\u003e"));
    assert!(html.contains("https://[::1]:22267/"));
    assert!(html.contains("invoke('retry_backend_connection')"));
    assert!(!html.contains("const port ="));
}

#[test]
fn internal_responses_only_load_embedded_assets_and_ipc() {
    let response = splash_response();
    let headers = response.headers();
    let csp = headers[tauri::http::header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("frame-src 'none'"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(csp.contains("connect-src ipc: http://ipc.localhost https://ipc.localhost"));
    assert_eq!(headers[tauri::http::header::CACHE_CONTROL], "no-store");
    assert!(!response.body().is_empty());
}

/// 英文环境下 splash HTML 的文案应来自 JSON 字面量（而非 JS 单引号拼接）。
#[test]
fn test_english_splash_i18n_uses_json_literals() {
    rust_i18n::set_locale("en");

    let html = splash_redesigned_shell_html("video", "font");

    assert!(html.contains(r#""defaultTip":"Sakura Empire's cherry blossoms"#));
    assert!(!html.contains("const defaultTip = '"));
    assert!(html.contains("window.__ALAS_SPLASH_READY = true;"));
    assert!(html.contains("data:video/mp4;base64,video"));
    assert!(html.contains("font-family: \"MiSans\""));
    assert!(html.contains("data:font/ttf;base64,font"));
    assert!(!html.contains("text-transform: uppercase;"));
}

/// splash HTML 应包含可选的 uv 安装进度子条相关标记与样式。
#[test]
fn test_splash_includes_optional_uv_progress() {
    let html = splash_redesigned_shell_html("video", "font");

    assert!(html.contains("data:video/mp4;base64,video"));
    assert!(html.contains("id=\"uv-progress-container\""));
    assert!(html.contains("payload.uv_progress"));
    assert!(html.contains("id=\"uv-progress-detail\""));
    assert!(html.contains("grid-template-columns: minmax(0, 1fr) auto"));
    assert!(html.contains("background: rgba(250, 250, 247, 0.78)"));
    assert!(html.contains("content: \"✦\""));
    assert!(!html.contains("'Tips: ' + subtitle.tip"));
    assert!(!html.contains("animation: sweep"));
}

/// 标题栏应使用 WebView 可拖拽区域（pointerdown + app-region）支持触摸拖动。
#[test]
fn test_titlebars_use_webview_draggable_regions_for_touch_dragging() {
    let splash_html = splash_redesigned_shell_html("video", "font");

    assert!(splash_html.contains("touch-action: none;"));
    assert!(splash_html.contains("addEventListener('pointerdown'"));
    assert!(splash_html.contains("-webkit-app-region: drag;"));
    assert!(splash_html.contains("-webkit-app-region: no-drag;"));
    assert!(splash_html.contains("webviewDraggableRegionsEnabled"));
    assert!(splash_html.contains("if (webviewDraggableRegionsEnabled) {"));
    assert!(!splash_html.contains("$NATIVE_TOUCH_DRAG"));

    #[cfg(windows)]
    assert!(splash_html.contains("const webviewDraggableRegionsEnabled = true;"));

    #[cfg(not(target_os = "macos"))]
    let titlebar_script = main_window_titlebar_injection_script();

    #[cfg(not(target_os = "macos"))]
    {
        assert!(titlebar_script.contains("touch-action:none"));
        assert!(titlebar_script.contains("addEventListener('pointerdown'"));
        assert!(titlebar_script.contains("window_start_dragging"));
        assert!(titlebar_script.contains("-webkit-app-region:no-drag"));
        assert!(titlebar_script.contains(
                ".alas-titlebar-drag-zone{position:absolute;inset:0 148px 0 0;height:100%;pointer-events:none"
            ));
        assert!(titlebar_script.contains("getComputedStyle(element).cursor === 'pointer'"));
        assert!(titlebar_script.contains("min-height:28px"));
        assert!(titlebar_script.contains("background:rgba(250,250,247,.78)"));
        assert!(titlebar_script.contains(".icon-close{color:#e64f58}"));
        assert!(titlebar_script.contains("--alas-titlebar-height:56px"));
        assert!(titlebar_script.contains("transform:translateY(-6px) scale(.96)"));
        assert!(!titlebar_script.contains("scale(.72)"));
        assert!(titlebar_script.contains("alas-close-menu"));
        assert!(!titlebar_script.contains("alas-close-optics"));
        assert!(!titlebar_script.contains("alas-island-open"));
        assert!(titlebar_script.contains("__ALAS_OPEN_CLOSE_PROMPT"));
        assert!(titlebar_script.contains("window_choose_close_behavior"));
        assert!(!titlebar_script.contains("addEventListener('scroll'"));
        assert!(!titlebar_script.contains("MutationObserver"));
        assert!(titlebar_script.contains("data-theme*="));
        assert!(titlebar_script.contains("#alas-launcher-titlebar.is-dark"));
    }
}

/// tauri.conf.json 中 Windows 窗口应启用 WebView2 拖拽区域等浏览器参数。
#[test]
fn test_windows_enable_webview_draggable_regions() {
    let config: serde_json::Value =
        serde_json::from_str(TAURI_CONFIG_SOURCE).expect("valid config");
    let windows = config["app"]["windows"].as_array().expect("window configs");

    for window in windows {
        let args = window["additionalBrowserArgs"]
            .as_str()
            .expect("draggable regions arguments");
        assert!(args.contains("msWebView2EnableDraggableRegions"));
        assert!(args.contains("ElasticOverscroll"));
        assert!(args.contains("msWebOOUI,msPdfOOUI"));
        assert!(!args.contains("msSmartScreenProtection"));
        assert!(args.contains("--no-proxy-server"));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_keeps_native_window_controls_with_a_close_chooser() {
    let script = main_window_titlebar_injection_script();
    assert!(script.contains("const nativeTitlebar = true;"));
    assert!(script.contains("__ALAS_OPEN_CLOSE_PROMPT"));
    assert!(script.contains("window_choose_close_behavior"));
}
