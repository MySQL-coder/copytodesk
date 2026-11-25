#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine;
use ctd_core::client::{ConnectInfo, ConnectParams, ScreenshotResult};
use ctd_core::{HostConfig, HostHandle, HostStatus, RemoteClient};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{Mutex, RwLock};

struct AppState {
    host: Arc<Mutex<Option<HostHandle>>>,
    // RwLock allows concurrent non-blocking mouse/key writes with ZERO lock contention!
    client: Arc<RwLock<Option<Arc<RemoteClient>>>>,
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
async fn connect_remote(
    app: AppHandle,
    state: State<'_, AppState>,
    params: ConnectParams,
) -> Result<ConnectInfo, String> {
    if let Some(prev) = state.client.write().await.take() {
        prev.disconnect();
    }
    let app_frames = app.clone();
    let app_rtt = app.clone();
    let client = RemoteClient::connect_streaming(
        &params,
        move |tile_ev| {
            let _ = app_frames.emit("remote-tile-frame", &tile_ev);
        },
        move |rtt_ms| {
            let _ = app_rtt.emit("remote-rtt", rtt_ms);
        },
    )
    .await?;

    let info = client.info.clone();
    *state.client.write().await = Some(Arc::new(client));
    Ok(info)
}

#[tauri::command]
async fn disconnect_remote(state: State<'_, AppState>) -> Result<bool, String> {
    if let Some(prev) = state.client.write().await.take() {
        prev.disconnect();
    }
    Ok(true)
}

#[tauri::command]
async fn remote_set_stream_quality(
    state: State<'_, AppState>,
    max_width: u16,
    quality: u8,
) -> Result<(), String> {
    let guard = state.client.read().await;
    let c = guard.as_ref().ok_or_else(|| "Not connected".to_string())?;
    c.set_stream_quality(max_width, quality)
}

#[tauri::command]
async fn remote_screenshot(
    state: State<'_, AppState>,
    max_width: u16,
    quality: u8,
) -> Result<ScreenshotResult, String> {
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.screenshot(max_width, quality, false).await
}

#[tauri::command]
async fn remote_click(state: State<'_, AppState>, x: u32, y: u32, button: u8) -> Result<(), String> {
    let guard = state.client.read().await;
    let c = guard.as_ref().ok_or_else(|| "Not connected".to_string())?;
    c.click(x, y, button)
}

#[tauri::command]
async fn remote_mouse_norm(
    state: State<'_, AppState>,
    action: u8,
    button: u8,
    norm_x: u16,
    norm_y: u16,
    wheel_delta: i16,
) -> Result<(), String> {
    let guard = state.client.read().await;
    let c = guard.as_ref().ok_or_else(|| "Not connected".to_string())?;
    c.mouse_event_norm(action, button, norm_x, norm_y, wheel_delta)
}

#[tauri::command]
async fn remote_key_event(
    state: State<'_, AppState>,
    action: u8,
    vk: u16,
    unicode_char: u32,
) -> Result<(), String> {
    let guard = state.client.read().await;
    let c = guard.as_ref().ok_or_else(|| "Not connected".to_string())?;
    c.key_event(action, vk, unicode_char)
}

#[tauri::command]
async fn remote_type(state: State<'_, AppState>, text: String) -> Result<(), String> {
    let guard = state.client.read().await;
    let c = guard.as_ref().ok_or_else(|| "Not connected".to_string())?;
    c.type_text(&text)
}

#[tauri::command]
async fn remote_clipboard_get(state: State<'_, AppState>) -> Result<String, String> {
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.clipboard_get().await
}

#[tauri::command]
async fn remote_clipboard_set(state: State<'_, AppState>, text: String) -> Result<(), String> {
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.clipboard_set(&text).await
}

#[tauri::command]
async fn remote_list_dir(state: State<'_, AppState>, remote_path: String) -> Result<String, String> {
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.list_dir(&remote_path).await
}

#[tauri::command]
async fn remote_upload_base64(
    state: State<'_, AppState>,
    remote_path: String,
    content_base64: String,
) -> Result<String, String> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(content_base64.trim())
        .map_err(|e| e.to_string())?;
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.upload_bytes(&remote_path, &data).await
}

#[tauri::command]
async fn remote_exec_guarded(state: State<'_, AppState>, command: String) -> Result<String, String> {
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.exec_guarded(&command, 5000).await
}

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            host: Arc::new(Mutex::new(None)),
            client: Arc::new(RwLock::new(None)),
        })
        .invoke_handler(tauri::generate_handler![
            get_platform_info,
            start_host,
            stop_host,
            host_status,
            connect_remote,
            disconnect_remote,
            remote_set_stream_quality,
            remote_screenshot,
            remote_click,
            remote_mouse_norm,
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
