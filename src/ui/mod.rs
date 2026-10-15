//! 内部协议页面与窗口外观；渲染层不读取后端配置，也不执行原生命令。

mod assets;
mod error;
mod splash;
mod titlebar;

pub use error::backend_error_html;
pub use splash::splash_response;
pub use titlebar::main_window_titlebar_injection_script;

/// 自定义协议不会自动应用 tauri 资源协议的 CSP，必须在响应中显式设置。
/// 内部页只需内嵌资源和 IPC；不加载远端脚本、不允许框架或弹窗内容。
pub fn html_response(html: String) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::{header, HeaderValue, Response};
    let mut response = Response::new(html.into_bytes());
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
        "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:; media-src data:; connect-src ipc: http://ipc.localhost https://ipc.localhost; base-uri 'none'; form-action 'none'; frame-src 'none'; child-src 'none'; object-src 'none'; frame-ancestors 'none'",
    ));
    response
}

fn escape_html(input: impl AsRef<str>) -> String {
    input
        .as_ref()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests;
