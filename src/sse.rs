//! 旧版后端的 SSE 能力检测；新版 SPA 的 HTML 回退不属于事件流。

use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use anyhow::{anyhow, Result};
use reqwest::{blocking::Response, header::CONTENT_TYPE, StatusCode};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StreamOutcome {
    Ended,
    Unsupported,
}

pub(crate) fn is_event_stream(response: &Response) -> Result<bool> {
    classify_response(
        response.status(),
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
    )
}

fn classify_response(status: StatusCode, content_type: Option<&str>) -> Result<bool> {
    if matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
    ) {
        return Ok(false);
    }
    if !status.is_success() {
        return Err(anyhow!("event stream server returned {status}"));
    }
    Ok(content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream")))
}

/// 重连退避可被退出标志中断，避免退出后继续发起请求。
pub(crate) fn wait_to_reconnect(allow_exit: &AtomicBool) {
    for _ in 0..30 {
        if allow_exit.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    #[test]
    fn accepts_only_sse_media_type() {
        assert!(
            classify_response(StatusCode::OK, Some("Text/Event-Stream; charset=utf-8")).unwrap()
        );
        for media_type in [
            None,
            Some("text/html"),
            Some("application/json"),
            Some("text/event-streaming"),
        ] {
            assert!(!classify_response(StatusCode::OK, media_type).unwrap());
        }
        assert!(!classify_response(StatusCode::NOT_FOUND, None).unwrap());
        assert!(!classify_response(StatusCode::METHOD_NOT_ALLOWED, None).unwrap());
        assert!(classify_response(StatusCode::UNAUTHORIZED, None).is_err());
        assert!(classify_response(StatusCode::SERVICE_UNAVAILABLE, None).is_err());
    }

    #[test]
    fn spa_fallback_is_not_a_notification_or_command_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: 13\r\nConnection: close\r\n\r\n<html></html>").unwrap();
        });
        let response = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}/api/notify_stream"))
            .send()
            .unwrap();
        assert!(!is_event_stream(&response).unwrap());
        server.join().unwrap();
    }
}
