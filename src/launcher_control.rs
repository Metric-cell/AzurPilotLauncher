//! 反向控制流模块：接收 WebUI 后端经 SSE 下发的启动器控制指令并回报结果。
//!
//! 旧版后端通过配置的 HTTP/HTTPS 地址下 `/api/launcher/stream` 推送指令
//! （查询/设置开机自启等），本模块在独立线程中维持长连接、解析 SSE 事件、
//! 执行指令并把执行结果 POST 回 `/api/launcher/report`，形成命令-应答闭环。
//! `allow_exit` 标志用于在启动器关闭流程中优雅终止此线程。

use std::{
    io::BufReader,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::Result;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::{
    autostart,
    backend::WebuiLaunchConfig,
    sse::{self, StreamOutcome},
};

/// 后端下发的单条启动器控制指令。
///
/// `type` 为指令类型（如 `startup.query` / `startup.set`），
/// `payload` 为可选的指令参数；`id` 用于应答时与请求配对。
#[derive(Debug, Deserialize)]
struct LauncherCommand {
    id: String,
    #[serde(rename = "type")]
    command_type: String,
    payload: Option<Value>,
}

/// 启动反向控制流的后台线程，持续接收并处理启动器控制指令。
///
/// 线程内部带 3 秒间隔的自动重连：连接异常断开时只要启动器尚未进入
/// 退出流程（`allow_exit` 为 false）就会重新连接，保证指令通道可用。
/// `allow_exit` 由启动器的退出流程置位，用于让此线程尽快结束。
pub fn start_launcher_control_stream(config: WebuiLaunchConfig, allow_exit: Arc<AtomicBool>) {
    thread::spawn(move || {
        let base = match config.backend_url() {
            Ok(base) => base,
            Err(e) => {
                warn!("Invalid launcher control configuration: {e}");
                return;
            }
        };
        if !crate::webview_security::is_loopback_http_origin(&match tauri::Url::parse(&base) {
            Ok(url) => url,
            Err(_) => return,
        }) {
            info!("Native launcher control is disabled for non-loopback backends");
            return;
        }
        let stream_url = format!("{base}api/launcher/stream");
        let report_url = format!("{base}api/launcher/report");
        // 显式禁用系统代理并限制连接超时：控制流是本机回环通信，
        // 走代理既无意义，还可能因系统代理故障而中断
        let client = match config.http_client_builder().and_then(|builder| {
            Ok(builder
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()?)
        }) {
            Ok(client) => client,
            Err(e) => {
                warn!("Unable to create launcher control client: {e}");
                return;
            }
        };

        while !allow_exit.load(Ordering::SeqCst) {
            info!("Connecting to launcher control stream: {stream_url}");
            match read_launcher_control_stream(&client, &stream_url, &report_url, &allow_exit) {
                Ok(StreamOutcome::Ended) => debug!("Launcher control stream ended"),
                Ok(StreamOutcome::Unsupported) => {
                    info!("Backend does not provide the legacy launcher control stream; subscription stopped");
                    return;
                }
                Err(e) => warn!("Launcher control stream disconnected: {e}"),
            }

            // 断开后稍作退避再重连；启动器已进入退出流程时立即结束循环
            if !allow_exit.load(Ordering::SeqCst) {
                sse::wait_to_reconnect(&allow_exit);
            }
        }
    });
}

/// 建立一次 SSE 长连接并循环读取事件，直到流结束或启动器开始退出。
///
/// 按 SSE 规范解析：`data:` 前缀行累积为事件数据，空行表示一个事件
/// 结束并触发分发。函数在流正常结束（对端关闭、读到 0 字节）或
/// `allow_exit` 置位时返回。
///
/// # Errors
/// 连接失败、非 2xx 响应或读取流时发生 IO 错误时返回 Err，
/// 由调用方记录日志后重连。
fn read_launcher_control_stream(
    client: &Client,
    stream_url: &str,
    report_url: &str,
    allow_exit: &AtomicBool,
) -> Result<StreamOutcome> {
    let response = client
        .get(stream_url)
        .header(ACCEPT, "text/event-stream")
        .send()?;

    if !sse::is_event_stream(&response)? {
        return Ok(StreamOutcome::Unsupported);
    }

    let mut reader = sse::EventReader::new(BufReader::new(response));
    while let Some(data) = reader.next(allow_exit)? {
        match serde_json::from_str::<LauncherCommand>(&data) {
            Ok(command) if command.id.len() <= 256 && command.command_type.len() <= 64 => {
                if allow_exit.load(Ordering::SeqCst) {
                    break;
                }
                handle_command(client, report_url, command);
            }
            Ok(_) => warn!("Ignoring launcher command with oversized fields"),
            Err(e) => warn!("Ignoring invalid launcher command: {e}"),
        }
    }
    Ok(StreamOutcome::Ended)
}

/// 执行单条控制指令，并把执行结果回报给后端。
///
/// 目前支持 `startup.query`（查询开机自启状态）与 `startup.set`
/// （设置开机自启，payload 需含 `enabled` 布尔字段）；未知指令
/// 返回错误应答而不是 panic，保证流通道不受个别坏指令影响。
/// 回报失败只记警告——指令本身已执行，后端有超时兜底。
fn handle_command(client: &Client, report_url: &str, command: LauncherCommand) {
    let report = match command.command_type.as_str() {
        "startup.query" => match autostart::query() {
            Ok(status) => success_report(&command.id, status),
            Err(e) => error_report(&command.id, e.to_string()),
        },
        "startup.set" => {
            let enabled = command
                .payload
                .as_ref()
                .and_then(|payload| payload.get("enabled"))
                .and_then(Value::as_bool);
            match enabled {
                Some(enabled) => match autostart::set_enabled(enabled) {
                    Ok(status) => success_report(&command.id, status),
                    Err(e) => error_report(&command.id, e.to_string()),
                },
                None => error_report(&command.id, "missing enabled".to_owned()),
            }
        }
        other => error_report(&command.id, format!("unknown command: {other}")),
    };

    if let Err(e) = client
        .post(report_url)
        .timeout(Duration::from_secs(5))
        .header(CONTENT_TYPE, "application/json")
        .body(report.to_string())
        .send()
        .and_then(|response| response.error_for_status())
    {
        warn!("Failed to report launcher command result: {e}");
    }
}

/// 构造执行成功的应答 JSON，`data` 为指令的业务结果。
fn success_report<T: serde::Serialize>(id: &str, data: T) -> Value {
    json!({
        "id": id,
        "success": true,
        "data": data,
    })
}

/// 构造执行失败的应答 JSON，`error` 为可读的错误描述。
fn error_report(id: &str, error: String) -> Value {
    json!({
        "id": id,
        "success": false,
        "error": error,
    })
}
