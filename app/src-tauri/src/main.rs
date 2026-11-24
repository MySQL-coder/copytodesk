#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine;
use ctd_core::client::{ConnectParams, ScreenshotResult};
use ctd_core::{HostConfig, HostHandle, HostStatus, RemoteClient};
use ctd_protocol::HelloAckPayload;
use std::sync::Arc;
use tauri::State;
use tokio::sync::Mutex;

struct AppState {
    host: Arc<Mutex<Option<HostHandle>>>,
    client: Arc<Mutex<Option<RemoteClient>>>,
    client_cancel: Arc<tokio::sync::Notify>,
}

#[tauri::command]
fn get_platform_info() -> serde_json::Value {
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "mode": "tauri-native",
        "version": env!("CARGO_PKG_VERSION")
    })
}

#[tauri::command]
async fn start_host(state: State<'_, AppState>, config: HostConfig) -> Result<HostStatus, String> {
    let mut guard = state.host.lock().await;
    if let Some(prev) = guard.take() {
        prev.stop();
    }
    let handle = HostHandle::start(config).await?;
    let status = handle.status();
    *guard = Some(handle);
    Ok(status)
}

#[tauri::command]
async fn stop_host(state: State<'_, AppState>) -> Result<bool, String> {
    let mut guard = state.host.lock().await;
    if let Some(prev) = guard.take() {
        prev.stop();
        return Ok(true);
    }
    Ok(false)
}

#[tauri::command]
async fn host_status(state: State<'_, AppState>) -> Result<Option<HostStatus>, String> {
    let guard = state.host.lock().await;
    Ok(guard.as_ref().map(|h| h.status()))
}

#[tauri::command]
async fn connect_remote(state: State<'_, AppState>, params: ConnectParams) -> Result<HelloAckPayload, String> {
    let client = RemoteClient::connect(&params).await?;
    let ack = client.ack.clone();
    *state.client.lock().await = Some(client);
    Ok(ack)
}

#[tauri::command]
async fn disconnect_remote(state: State<'_, AppState>) -> Result<bool, String> {
    state.client_cancel.notify_waiters();
    *state.client.lock().await = None;
    Ok(true)
}

#[tauri::command]
async fn remote_screenshot(state: State<'_, AppState>, max_width: u16, quality: u8) -> Result<ScreenshotResult, String> {
    let cancel = state.client_cancel.clone();
    let mut guard = state.client.lock().await;
    let c = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
    tokio::select! {
        res = c.screenshot(max_width, quality, false) => res,
        _ = cancel.notified() => Err("Disconnected".to_string()),
    }
}

#[tauri::command]
async fn remote_click(state: State<'_, AppState>, x: u32, y: u32, button: u8) -> Result<(), String> {
    let mut guard = state.client.lock().await;
    let c = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
    c.click(x, y, button).await
}

#[tauri::command]
async fn remote_mouse_event(
    state: State<'_, AppState>,
    action: u8,
    button: u8,
    x: u32,
    y: u32,
    wheel_delta: i16,
) -> Result<(), String> {
    let mut guard = state.client.lock().await;
    let c = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
    c.mouse_event(action, button, x, y, wheel_delta).await
}

#[tauri::command]
async fn remote_key_event(
    state: State<'_, AppState>,
    action: u8,
    vk: u16,
    unicode_char: u32,
) -> Result<(), String> {
    let mut guard = state.client.lock().await;
    let c = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
    c.key_event(action, vk, unicode_char).await
}

#[tauri::command]
async fn remote_type(state: State<'_, AppState>, text: String) -> Result<(), String> {
    let mut guard = state.client.lock().await;
    let c = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
    c.type_text(&text).await
}

#[tauri::command]
async fn remote_clipboard_get(state: State<'_, AppState>) -> Result<String, String> {
    let mut guard = state.client.lock().await;
    let c = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
    c.clipboard_get().await
}

#[tauri::command]
async fn remote_clipboard_set(state: State<'_, AppState>, text: String) -> Result<(), String> {
    let mut guard = state.client.lock().await;
    let c = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
    c.clipboard_set(&text).await
}

#[tauri::command]
async fn remote_list_dir(state: State<'_, AppState>, remote_path: String) -> Result<String, String> {
    let mut guard = state.client.lock().await;
    let c = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
    c.list_dir(&remote_path).await
}

#[tauri::command]
async fn remote_upload_base64(state: State<'_, AppState>, remote_path: String, content_base64: String) -> Result<String, String> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(content_base64.trim())
        .map_err(|e| e.to_string())?;
    let mut guard = state.client.lock().await;
    let c = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
    c.upload_bytes(&remote_path, &data).await
}

#[tauri::command]
async fn remote_exec_guarded(state: State<'_, AppState>, command: String) -> Result<String, String> {
    let mut guard = state.client.lock().await;
    let c = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
    c.exec_guarded(&command, 5000).await
}

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            host: Arc::new(Mutex::new(None)),
            client: Arc::new(Mutex::new(None)),
            client_cancel: Arc::new(tokio::sync::Notify::new()),
        })
        .invoke_handler(tauri::generate_handler![
            get_platform_info,
            start_host,
            stop_host,
            host_status,
            connect_remote,
            disconnect_remote,
            remote_screenshot,
            remote_click,
            remote_mouse_event,
            remote_key_event,
            remote_type,
            remote_clipboard_get,
            remote_clipboard_set,
            remote_list_dir,
            remote_upload_base64,
            remote_exec_guarded
        ])
        .run(tauri::generate_context!())
        .expect("error while running CopyToDesk desktop app");
}
