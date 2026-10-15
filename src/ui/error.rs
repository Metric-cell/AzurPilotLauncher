//! 错误页渲染；显示地址由启动配置提供，页面不选择后端端口。
use super::assets::{font_base64, script_json, splash_video_base64};
use super::main_window_titlebar_injection_script;
use rust_i18n::t;

pub fn backend_error_html(backend_address: &str, error_detail: &str) -> String {
    let backend_url_json = script_json(&serde_json::json!(backend_address));
    let error_detail_json = script_json(&serde_json::json!(error_detail));
    let mi_sans_font_b64 = font_base64();
    let splash_video_b64 = splash_video_base64();
    let titlebar_script = main_window_titlebar_injection_script();
    let i18n = serde_json::json!({
        "title": t!("error_page.title"),
        "heading": t!("error_page.heading"),
        "description": t!("error_page.description"),
        "address": t!("error_page.address"),
        "errorLabel": t!("error_page.error_label"),
        "retry": t!("error_page.retry"),
        "downloadGuiLog": t!("error_page.download_gui_log"),
        "downloadLauncherLog": t!("error_page.download_launcher_log"),
        "reconnecting": t!("error_page.reconnecting"),
        "stillFailed": t!("error_page.still_failed"),
        "retryFailed": t!("error_page.retry_failed"),
        "preparing": t!("error_page.preparing"),
        "saved": t!("error_page.saved"),
        "downloadFailed": t!("error_page.download_failed"),
    });
    let i18n_json = script_json(&i18n);

    // HTML/CSS/JS 内的字面大括号在 format! 里需双写转义；文案经 JSON 注入
    // 避免引号/换行破坏 HTML 结构。
    format!(
        include_str!("error.html"),
        mi_sans_font_b64 = mi_sans_font_b64,
        splash_video_b64 = splash_video_b64,
        titlebar_script = titlebar_script,
        i18n_json = i18n_json,
        backend_url_json = backend_url_json,
        error_detail_json = error_detail_json,
        title = t!("error_page.title"),
        heading = t!("error_page.heading"),
        description = t!("error_page.description"),
        address = t!("error_page.address"),
        error_label = t!("error_page.error_label"),
        retry = t!("error_page.retry"),
        download_gui_log = t!("error_page.download_gui_log"),
        download_launcher_log = t!("error_page.download_launcher_log"),
        gui_log_label = t!("error_page.download_gui_log"),
        launcher_log_label = t!("error_page.download_launcher_log"),
        connection_info = t!("error_page.connection_info"),
    )
}
