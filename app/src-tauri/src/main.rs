#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine;
use ctd_core::client::{ConnectInfo, ConnectParams, DownloadResult, ScreenshotResult};
use ctd_core::{clipboard, HostConfig, HostHandle, HostStatus, RemoteClient};
use ctd_protocol::{key_mod_flags, MonitorSelectAckPayload};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration};

struct AppState {
    host: Arc<Mutex<Option<HostHandle>>>,
    client: Arc<RwLock<Option<Arc<RemoteClient>>>>,
}

fn default_downloads_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            let d = PathBuf::from(profile).join("Downloads").join("CopyToDesk");
            if std::fs::create_dir_all(&d).is_ok() {
                return d;
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(home) = std::env::var("HOME") {
            let d = PathBuf::from(home).join("Downloads").join("CopyToDesk");
            if std::fs::create_dir_all(&d).is_ok() {
                return d;
            }
        }
    }
    let fallback = std::env::temp_dir().join("CopyToDesk").join("Downloads");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

#[tauri::command]
fn get_platform_info() -> serde_json::Value {
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "mode": "tauri-native",
        "version": env!("CARGO_PKG_VERSION"),
        "downloadsDir": default_downloads_dir().to_string_lossy()
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
async fn host_kick_viewers(state: State<'_, AppState>) -> Result<bool, String> {
    let guard = state.host.lock().await;
    if let Some(h) = guard.as_ref() {
        h.kick_viewers();
        return Ok(true);
    }
    Ok(false)
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
    let app_cursor = app.clone();
    let app_rtt = app.clone();
    let app_disc = app.clone();
    let client = RemoteClient::connect_streaming(
        &params,
        move |tile_ev| {
            let _ = app_frames.emit("remote-tile-frame", &tile_ev);
        },
        move |cur_ev| {
            let _ = app_cursor.emit("remote-cursor", &cur_ev);
        },
        move |rtt_ms| {
            let _ = app_rtt.emit("remote-rtt", rtt_ms);
        },
        move |reason| {
            let _ = app_disc.emit("remote-disconnected", reason);
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
async fn remote_select_monitor(
    state: State<'_, AppState>,
    index: u16,
) -> Result<MonitorSelectAckPayload, String> {
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.select_monitor(index).await
}

#[tauri::command]
async fn remote_send_combo(state: State<'_, AppState>, combo: String) -> Result<(), String> {
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;

    // Each combo is `(modifiers_vks, mod_flags, main_vk)`
    let (mods, flags, main_vk): (&[u16], u16, u16) = match combo.as_str() {
        "task_mgr" => (&[0x11, 0x10], key_mod_flags::CTRL | key_mod_flags::SHIFT, 0x1B), // Ctrl+Shift+Esc
        "ctrl_alt_del" => (&[0x11, 0x12], key_mod_flags::CTRL | key_mod_flags::ALT, 0x2E), // Ctrl+Alt+Del
        "show_desktop_win" => (&[0x5B], key_mod_flags::META, 0x44), // Win+D
        "show_desktop_mac" => (&[], 0, 0x7A), // F11 (macOS Show Desktop)
        "mission_control" => (&[], 0, 0x72), // F3 (macOS Mission Control)
        "app_switch_win" => (&[0x12], key_mod_flags::ALT, 0x09), // Alt+Tab
        "app_switch_mac" => (&[0x5B], key_mod_flags::META, 0x09), // Cmd+Tab
        "lock_win" => (&[0x5B], key_mod_flags::META, 0x4C), // Win+L
        "lock_mac" => (
            &[0x11, 0x5B],
            key_mod_flags::CTRL | key_mod_flags::META,
            0x51,
        ), // Ctrl+Cmd+Q
        _ => return Err(format!("Unknown combo: {combo}")),
    };

    for &m in mods {
        c.key_event_ext(0, m, flags, 0)?;
    }
    c.key_event_ext(0, main_vk, flags, 0)?;
    sleep(Duration::from_millis(25)).await;
    c.key_event_ext(1, main_vk, flags, 0)?;
    for &m in mods.iter().rev() {
        c.key_event_ext(1, m, 0, 0)?;
    }
    Ok(())
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
async fn remote_click(
    state: State<'_, AppState>,
    x: u32,
    y: u32,
    button: u8,
) -> Result<(), String> {
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
    flags: Option<u16>,
    unicode_char: u32,
) -> Result<(), String> {
    let guard = state.client.read().await;
    let c = guard.as_ref().ok_or_else(|| "Not connected".to_string())?;
    c.key_event_ext(action, vk, flags.unwrap_or(0), unicode_char)
}

#[tauri::command]
async fn remote_type(state: State<'_, AppState>, text: String) -> Result<(), String> {
    let guard = state.client.read().await;
    let c = guard.as_ref().ok_or_else(|| "Not connected".to_string())?;
    c.type_text(&text)
}

#[tauri::command]
async fn local_clipboard_get() -> Result<String, String> {
    tokio::task::spawn_blocking(clipboard::get_text)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn local_clipboard_set(text: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let _ = clipboard::set_text(&text);
    })
    .await
    .map_err(|e| e.to_string())
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
async fn remote_list_dir(
    state: State<'_, AppState>,
    remote_path: String,
) -> Result<String, String> {
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
async fn remote_upload_chunk(
    state: State<'_, AppState>,
    upload_id: String,
    remote_path: String,
    offset: u64,
    total_size: u64,
    eof: bool,
    chunk_base64: String,
) -> Result<String, String> {
    let chunk = if chunk_base64.is_empty() {
        Vec::new()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(chunk_base64.trim())
            .map_err(|e| e.to_string())?
    };
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.upload_chunk(&upload_id, &remote_path, offset, total_size, eof, &chunk)
        .await
}

#[tauri::command]
async fn remote_download_file(
    app: AppHandle,
    state: State<'_, AppState>,
    remote_path: String,
) -> Result<DownloadResult, String> {
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;

    let safe_name = Path::new(&remote_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty() && s != "." && s != "..")
        .ok_or_else(|| "Invalid remote file name".to_string())?;

    let dest = default_downloads_dir().join(&safe_name);
    let name_for_ev = safe_name.clone();
    c.download_file_to_path(&remote_path, &dest, move |transferred, total| {
        let _ = app.emit(
            "file-transfer-progress",
            serde_json::json!({
                "direction": "download",
                "name": name_for_ev,
                "transferred": transferred,
                "total": total,
            }),
        );
    })
    .await
}

#[tauri::command]
fn open_downloads_dir() -> Result<String, String> {
    let dir = default_downloads_dir();
    #[cfg(target_os = "windows")]
    let res = Command::new("explorer").arg(&dir).spawn();
    #[cfg(target_os = "macos")]
    let res = Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "linux")]
    let res = Command::new("xdg-open").arg(&dir).spawn();
    res.map_err(|e| e.to_string())?;
    Ok(dir.to_string_lossy().to_string())
}

#[tauri::command]
async fn remote_exec_guarded(
    state: State<'_, AppState>,
    command: String,
) -> Result<String, String> {
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
            host_kick_viewers,
            connect_remote,
            disconnect_remote,
            remote_set_stream_quality,
            remote_select_monitor,
            remote_send_combo,
            remote_screenshot,
            remote_click,
            remote_mouse_norm,
            remote_key_event,
            remote_type,
            local_clipboard_get,
            local_clipboard_set,
            remote_clipboard_get,
            remote_clipboard_set,
            remote_list_dir,
            remote_upload_base64,
            remote_upload_chunk,
            remote_download_file,
            open_downloads_dir,
            remote_exec_guarded
        ])
        .run(tauri::generate_context!())
        .expect("error while running CopyToDesk desktop app");
}
