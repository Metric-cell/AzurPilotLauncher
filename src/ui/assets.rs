//! 离线首屏的嵌入资源与 HTML script 上下文转义。

use std::sync::OnceLock;

use base64::{prelude::BASE64_STANDARD, Engine};

static VIDEO: OnceLock<String> = OnceLock::new();
static FONT: OnceLock<String> = OnceLock::new();
static PROGRESS_HEAD: OnceLock<String> = OnceLock::new();

pub(super) fn splash_video_base64() -> &'static str {
    VIDEO.get_or_init(|| BASE64_STANDARD.encode(include_bytes!("../../bg/bg.mp4")))
}

pub(super) fn font_base64() -> &'static str {
    FONT.get_or_init(|| BASE64_STANDARD.encode(include_bytes!("../../fonts/MiSansLauncher.ttf")))
}

pub(super) fn progress_head_base64() -> &'static str {
    PROGRESS_HEAD.get_or_init(|| BASE64_STANDARD.encode(include_bytes!("../../bg/loading.webp")))
}

/// JSON 字面量本身不会转义 HTML 的 </script>，必须在 script 上下文另行转义。
pub(super) fn script_json(value: &serde_json::Value) -> String {
    value
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_json_cannot_end_the_html_script_element() {
        let original =
            serde_json::json!("</script><script>alert('native')</script>&\u{2028}\u{2029}");
        let encoded = script_json(&original);
        assert!(!encoded.contains('<'));
        assert!(!encoded.contains('>'));
        assert!(!encoded.contains('&'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded).unwrap(),
            original
        );
    }
}
