//! 自绘标题栏；macOS 使用系统窗口按钮。
#[cfg(not(target_os = "macos"))]
use rust_i18n::t;

pub fn main_window_titlebar_injection_script() -> String {
    // macOS：系统自带红绿灯按钮，无需注入自绘标题栏。
    #[cfg(target_os = "macos")]
    {
        String::new()
    }
    #[cfg(not(target_os = "macos"))]
    {
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
            "exitAction": t!("dialog.exit"),
            "minimizeToTrayAction": t!("dialog.minimize_to_tray"),
        });
        let i18n_json = super::assets::script_json(&i18n);
        let mut s = String::with_capacity(8192);
        s.push_str("const i18n = ");
        s.push_str(&i18n_json);
        // 关闭确认菜单仅 Windows 启用；其余平台保持单击直接关闭的行为。
        s.push_str(if cfg!(windows) {
            ";const closePromptEnabled = true;"
        } else {
            ";const closePromptEnabled = false;"
        });
        // 以下为原样注入的 JS 主体：创建标题栏 DOM 与样式、绑定拖拽和按钮
        // 事件、同步页面主题与布局。
        s.push_str(include_str!("titlebar.js"));
        s
    }
}
