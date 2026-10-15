//! 旧版后端的 SSE 能力检测；新版 SPA 的 HTML 回退不属于事件流。

use std::{
    io::{BufRead, Read},
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

const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_EVENT_BYTES: usize = 64 * 1024;

/// 共享的有界 SSE 解码器。无换行和无限 data 行不能持续占用内存。
pub(crate) struct EventReader<R> {
    reader: R,
    data: String,
}

impl<R: BufRead> EventReader<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            data: String::new(),
        }
    }

    pub(crate) fn next(&mut self, allow_exit: &AtomicBool) -> Result<Option<String>> {
        while !allow_exit.load(Ordering::SeqCst) {
            let mut line = Vec::new();
            let bytes = (&mut self.reader)
                .take((MAX_LINE_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)?;
            if allow_exit.load(Ordering::SeqCst) {
                self.data.clear();
                return Ok(None);
            }
            if bytes > MAX_LINE_BYTES {
                return Err(anyhow!("event stream line exceeds size limit"));
            }
            if bytes == 0 {
                return Ok((!self.data.is_empty()).then(|| std::mem::take(&mut self.data)));
            }
            let line = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
            if line.is_empty() && !self.data.is_empty() {
                return Ok(Some(std::mem::take(&mut self.data)));
            }
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.strip_prefix(' ').unwrap_or(data);
                if self.data.len() + data.len() + 1 > MAX_EVENT_BYTES {
                    return Err(anyhow!("event stream payload exceeds size limit"));
                }
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(data);
            }
        }
        Ok(None)
    }
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
    fn exit_during_blocked_read_discards_pending_event() {
        struct ExitOnDelimiter<'a> {
            inner: std::io::Cursor<Vec<u8>>,
            exiting: &'a AtomicBool,
        }
        impl Read for ExitOnDelimiter<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.inner.read(buffer)
            }
        }
        impl BufRead for ExitOnDelimiter<'_> {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                let buffer = self.inner.fill_buf()?;
                if buffer.starts_with(b"\n") {
                    self.exiting.store(true, Ordering::SeqCst);
                }
                Ok(buffer)
            }
            fn consume(&mut self, amount: usize) {
                self.inner.consume(amount);
            }
        }
        let exiting = AtomicBool::new(false);
        let mut reader = EventReader::new(ExitOnDelimiter {
            inner: std::io::Cursor::new(b"data: startup.set\n\n".to_vec()),
            exiting: &exiting,
        });
        assert!(reader.next(&exiting).unwrap().is_none());
        assert!(exiting.load(Ordering::SeqCst));
    }

    #[test]
    fn rejects_unbounded_lines_and_multiline_events() {
        let exiting = AtomicBool::new(false);
        let oversized = vec![b'x'; MAX_LINE_BYTES + 1];
        assert!(EventReader::new(oversized.as_slice())
            .next(&exiting)
            .is_err());
        let repeated = "data: ".to_owned() + &"x".repeat(MAX_LINE_BYTES / 2) + "\n";
        let oversized = repeated.repeat(9);
        assert!(EventReader::new(oversized.as_bytes())
            .next(&exiting)
            .is_err());
        let mut reader = EventReader::new(
            b": heartbeat\r\ndata: first\r\ndata: second\r\n\r\ndata: last".as_slice(),
        );
        assert_eq!(
            reader.next(&exiting).unwrap().as_deref(),
            Some("first\nsecond")
        );
        assert_eq!(reader.next(&exiting).unwrap().as_deref(), Some("last"));
        assert!(reader.next(&exiting).unwrap().is_none());
    }

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
