//! 自绘标题栏和关闭选择菜单；macOS 保留系统窗口按钮。
use rust_i18n::t;

pub fn main_window_titlebar_injection_script() -> String {
    // 标题栏文案以 JSON 注入，避免字符串拼接时的转义问题。
    let i18n = serde_json::json!({
        "hideLabel": t!("titlebar.minimize_to_tray"),
        "minimizeLabel": t!("titlebar.minimize_window"),
        "minimizeTitle": t!("titlebar.minimize"),
        "maximizeLabel": t!("titlebar.maximize_restore_window"),
        "maximizeTitle": t!("titlebar.maximize"),
        "closeLabel": t!("titlebar.close_window"),
        "closeTitle": t!("titlebar.close"),
        "restoreTitle": t!("titlebar.restore"),
        "maximizeActionTitle": t!("titlebar.maximize_action"),
        "restoreLabel": t!("titlebar.restore_window"),
        "maximizeLabelText": t!("titlebar.maximize_window"),
        "closePrompt": t!("dialog.confirm_exit"),
        "closeRememberHint": t!("dialog.close_remember_hint"),
        "closeSaveFailed": t!("dialog.close_save_failed"),
        "exitAction": t!("dialog.exit"),
        "minimizeToTrayAction": t!("dialog.minimize_to_tray"),
    });
    let i18n_json = super::assets::script_json(&i18n);
    let mut s = String::with_capacity(8192);
    s.push_str("const i18n = ");
    s.push_str(&i18n_json);
    // Windows/macOS 首次关闭时询问；Linux 直接退出，不提供托盘。
    s.push_str(if cfg!(any(windows, target_os = "macos")) {
        ";const closePromptEnabled = true;"
    } else {
        ";const closePromptEnabled = false;"
    });
    s.push_str(if cfg!(target_os = "macos") {
        ";const nativeTitlebar = true;"
    } else {
        ";const nativeTitlebar = false;"
    });
    // 以下为原样注入的 JS 主体：创建标题栏 DOM 与样式、绑定拖拽和按钮
    // 事件、同步页面主题与布局。
    s.push_str(include_str!("titlebar.js"));
    s
}
