//! 锁定 Tauri 2.5.1 / Wry 0.51.2 的原生新窗口防护。
//!
//! 该 Tauri 版本尚未公开 Wry 的 new_window_req_handler。平台钩子在
//! on_webview_ready 执行，早于把 about:blank 占位文档导航到内部页面。

use tauri::{Runtime, Webview};

pub fn install<R: Runtime>(webview: &Webview<R>) -> tauri::Result<()> {
    let window = webview.window();
    webview.with_webview(move |platform| {
        if let Err(error) = install_platform(platform) {
            tracing::error!("Native popup guard failed; closing WebView: {error}");
            let _ = window.close();
        }
    })
}

#[cfg(target_os = "linux")]
fn install_platform(platform: tauri::webview::PlatformWebview) -> Result<(), String> {
    use webkit2gtk::{PolicyDecisionExt, PolicyDecisionType, WebViewExt};
    let webview = platform.inner();
    webview.connect_decide_policy(|_, decision, decision_type| {
        if decision_type == PolicyDecisionType::NewWindowAction {
            decision.ignore();
            true
        } else {
            false
        }
    });
    webview.connect_create(|_, _| None);
    Ok(())
}

#[cfg(windows)]
fn install_platform(platform: tauri::webview::PlatformWebview) -> Result<(), String> {
    use webview2_com::NewWindowRequestedEventHandler;
    // WebView2 持有 handler 的 COM 引用，随 WebView 销毁释放。
    unsafe {
        let webview = platform
            .controller()
            .CoreWebView2()
            .map_err(|error| error.to_string())?;
        let handler = NewWindowRequestedEventHandler::create(Box::new(|_, args| {
            if let Some(args) = args {
                args.SetHandled(true)?;
            }
            Ok(())
        }));
        let mut registration = 0_i64;
        webview
            .add_NewWindowRequested(&handler, &mut registration)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_platform(platform: tauri::webview::PlatformWebview) -> Result<(), String> {
    use objc2::{
        msg_send,
        runtime::{AnyObject, Bool},
        sel,
    };
    // WebKit 原生约定：UIDelegate 未实现 createWebView... 时取消新窗口。
    // 验证锁定 Wry 的默认拒绝行为，保留现有 delegate 的上传面板等功能。
    unsafe {
        let view = platform
            .inner()
            .cast::<AnyObject>()
            .as_ref()
            .ok_or_else(|| "WKWebView pointer is unavailable".to_owned())?;
        let delegate: *mut AnyObject = msg_send![view, UIDelegate];
        if let Some(delegate) = delegate.as_ref() {
            let creates_windows: Bool = msg_send![delegate, respondsToSelector:
                sel!(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:)];
            if creates_windows.as_bool() {
                return Err("WKUIDelegate implements unreviewed popup creation".into());
            }
        }
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn install_platform(_platform: tauri::webview::PlatformWebview) -> Result<(), String> {
    Err("Native popup policy is not implemented for this platform".into())
}
