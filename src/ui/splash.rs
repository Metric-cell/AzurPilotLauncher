//! 启动画面渲染；启动状态通过主框架 bridge 握手后更新。
use super::assets::{font_base64, progress_head_base64, script_json, splash_video_base64};
use super::{escape_html, html_response};
use rust_i18n::t;

pub fn splash_response() -> tauri::http::Response<Vec<u8>> {
    html_response(splash_redesigned_shell_html(
        splash_video_base64(),
        font_base64(),
    ))
}

pub(super) fn splash_redesigned_shell_html(video_bg_b64: &str, mi_sans_font_b64: &str) -> String {
    // 页面文案聚合为 JSON 注入，前端按 key 取用，避免逐个拼接转义。
    let i18n = serde_json::json!({
        "defaultTip": t!("tips.17"),
        "loading": t!("splash.loading_badge"),
        "webuiInit": t!("splash.webui_init"),
        "starting": t!("splash.starting"),
        "errorBadge": t!("splash.error_badge"),
        "initStopped": t!("splash.init_stopped"),
        "progressMetaReady": t!("splash.progress_meta_ready"),
        "preparingLog": t!("splash.preparing_log"),
        "logSavedPrefix": t!("splash.log_saved_prefix"),
        "logFailed": t!("splash.log_failed"),
    });
    let i18n_json = script_json(&i18n);

    include_str!("splash.html")
        // 占位符替换：$PROGRESS_HEAD/$VIDEO_BG/$MI_SANS_FONT 为 base64 资源，
        // $LAUNCHER_VERSION 版本号，$I18N_* 为本地化文案（已 HTML 转义），
        // $NATIVE_TOUCH_DRAG 仅 Windows 为 true（WebView2 原生拖拽区域开关）。
        .replace("$PROGRESS_HEAD", progress_head_base64())
        .replace("$VIDEO_BG", video_bg_b64)
        .replace("$MI_SANS_FONT", mi_sans_font_b64)
        .replace("$LAUNCHER_VERSION", env!("CARGO_PKG_VERSION"))
        .replace("$I18N_JSON", &i18n_json)
        .replace(
            "$NATIVE_TOUCH_DRAG",
            if cfg!(windows) { "true" } else { "false" },
        )
        .replace(
            "$I18N_INITIALIZING",
            &escape_html(t!("splash.initializing")),
        )
        .replace("$I18N_MINIMIZE", &escape_html(t!("titlebar.minimize")))
        .replace("$I18N_CLOSE", &escape_html(t!("titlebar.close")))
        .replace("$I18N_STARTING", &escape_html(t!("splash.starting")))
        .replace("$I18N_WEBUI_INIT", &escape_html(t!("splash.webui_init")))
        .replace("$I18N_DEFAULT_TIP", &escape_html(t!("tips.17")))
        .replace(
            "$I18N_PROGRESS_META",
            &escape_html(t!("splash.progress_meta_ready")),
        )
        .replace(
            "$I18N_DOWNLOAD_LOG",
            &escape_html(t!("splash.download_log")),
        )
}
