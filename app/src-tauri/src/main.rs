#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine;
use ctd_core::client::{ConnectInfo, ConnectParams, DownloadResult, ScreenshotResult};
use ctd_core::{clipboard, host, power, HostConfig, HostHandle, HostStatus, RemoteClient};
use ctd_protocol::{key_mod_flags, MonitorSelectAckPayload};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{
    ipc::{Channel, InvokeResponseBody},
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State, WindowEvent,
};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration};

struct AppState {
    host: Arc<Mutex<Option<HostHandle>>>,
    client: Arc<RwLock<Option<Arc<RemoteClient>>>>,
    close_to_tray: Arc<AtomicBool>,
    quitting: Arc<AtomicBool>,
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
    let launched_minimized = std::env::args().any(|a| a == "--minimized" || a == "--silent");
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "mode": "tauri-native",
        "version": env!("CARGO_PKG_VERSION"),
        "downloadsDir": default_downloads_dir().to_string_lossy(),
        "sharedDir": host::default_shared_files_dir().to_string_lossy(),
        "osAutoStart": power::is_os_autostart_enabled(),
        "launchedMinimized": launched_minimized
    })
}

#[tauri::command]
fn get_os_autostart() -> Result<bool, String> {
    Ok(power::is_os_autostart_enabled())
}

#[tauri::command]
fn set_os_autostart(enabled: bool) -> Result<bool, String> {
    power::set_os_autostart_enabled(enabled)
}

#[tauri::command]
fn hide_to_tray(app: AppHandle) -> Result<bool, String> {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
        return Ok(true);
    }
    Ok(false)
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
    frame_channel: Option<Channel<InvokeResponseBody>>,
) -> Result<ConnectInfo, String> {
    if let Some(prev) = state.client.write().await.take() {
        prev.disconnect();
    }
    let app_frames = app.clone();
    let app_cursor = app.clone();
    let app_rtt = app.clone();
    let app_clip = app.clone();
    let app_disc = app.clone();
    let client = RemoteClient::connect_streaming(
        &params,
        move |raw_frame| {
            if let Some(ref ch) = frame_channel {
                let _ = ch.send(InvokeResponseBody::Raw(raw_frame));
            } else if let Some(tile_ev) = RemoteClient::decode_raw_frame_to_event(&raw_frame) {
                let _ = app_frames.emit("remote-tile-frame", &tile_ev);
            }
        },
        move |cur_ev| {
            let _ = app_cursor.emit("remote-cursor", &cur_ev);
        },
        move |rtt_ms| {
            let _ = app_rtt.emit("remote-rtt", rtt_ms);
        },
        move |clip_txt| {
            let _ = app_clip.emit("remote-clipboard-push", clip_txt);
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
fn set_close_to_tray(state: State<'_, AppState>, enabled: bool) -> Result<bool, String> {
    state.close_to_tray.store(enabled, Ordering::Release);
    Ok(enabled)
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
async fn local_clipboard_get_image() -> Result<String, String> {
    let png = tokio::task::spawn_blocking(clipboard::get_image_png)
        .await
        .map_err(|e| e.to_string())?;
    if png.is_empty() {
        return Ok(String::new());
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(&png))
}

#[tauri::command]
async fn local_clipboard_set_image(png_base64: String) -> Result<bool, String> {
    let png = base64::engine::general_purpose::STANDARD
        .decode(png_base64.trim())
        .map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || clipboard::set_image_png(&png))
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
async fn remote_clipboard_get_image(state: State<'_, AppState>) -> Result<String, String> {
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.clipboard_get_image_base64().await
}

#[tauri::command]
async fn remote_clipboard_set_image(
    state: State<'_, AppState>,
    png_base64: String,
) -> Result<u32, String> {
    let png = base64::engine::general_purpose::STANDARD
        .decode(png_base64.trim())
        .map_err(|e| e.to_string())?;
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.clipboard_set_image_bytes(png).await
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
async fn remote_file_action(
    state: State<'_, AppState>,
    action: String,
    remote_path: String,
    new_name: Option<String>,
) -> Result<String, String> {
    let c = state
        .client
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not connected".to_string())?;
    c.file_action(&action, &remote_path, new_name).await
}

#[tauri::command]
async fn remote_cancel_transfer(state: State<'_, AppState>) -> Result<bool, String> {
    let guard = state.client.read().await;
    if let Some(c) = guard.as_ref() {
        c.cancel_transfer();
        return Ok(true);
    }
    Ok(false)
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
fn open_shared_dir(custom_path: String) -> Result<String, String> {
    let trimmed = custom_path.trim();
    let dir = if !trimmed.is_empty() && Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        host::default_shared_files_dir()
    };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
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

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            host: Arc::new(Mutex::new(None)),
            client: Arc::new(RwLock::new(None)),
            close_to_tray: Arc::new(AtomicBool::new(true)),
            quitting: Arc::new(AtomicBool::new(false)),
        })
        .setup(|app| {
            let show_item =
                MenuItem::with_id(app, "show", "显示 CopyToDesk 主界面", true, None::<&str>)?;
            let kick_item =
                MenuItem::with_id(app, "kick", "一键断开当前远控会话", true, None::<&str>)?;
            let quit_item =
                MenuItem::with_id(app, "quit", "彻底退出 CopyToDesk", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &kick_item, &quit_item])?;

            let mut builder = TrayIconBuilder::with_id("ctd-tray")
                .tooltip("CopyToDesk — 远程控制工作台")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .on_menu_event(|app_handle, event| match event.id.as_ref() {
                    "show" => show_main_window(app_handle),
                    "kick" => {
                        let st_host = app_handle.state::<AppState>().host.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Some(h) = st_host.lock().await.as_ref() {
                                h.kick_viewers();
                            }
                        });
                    }
                    "quit" => {
                        let st = app_handle.state::<AppState>();
                        st.quitting.store(true, Ordering::Release);
                        let host_arc = st.host.clone();
                        let client_arc = st.client.clone();
                        let ah = app_handle.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Some(h) = host_arc.lock().await.take() {
                                h.stop();
                            }
                            if let Some(c) = client_arc.write().await.take() {
                                c.disconnect();
                            }
                            ah.exit(0);
                        });
                    }
                    _ => {}
                });

            if let Some(icon) = app.default_window_icon().cloned() {
                builder = builder.icon(icon);
            }
            let _tray = builder.build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let st = window.state::<AppState>();
                if !st.quitting.load(Ordering::Acquire)
                    && st.close_to_tray.load(Ordering::Acquire)
                {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_platform_info,
            get_os_autostart,
            set_os_autostart,
            hide_to_tray,
            set_close_to_tray,
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
            local_clipboard_get_image,
            local_clipboard_set_image,
            remote_clipboard_get,
            remote_clipboard_set,
            remote_clipboard_get_image,
            remote_clipboard_set_image,
            remote_list_dir,
            remote_upload_base64,
            remote_upload_chunk,
            remote_download_file,
            remote_file_action,
            remote_cancel_transfer,
            open_downloads_dir,
            open_shared_dir,
            remote_exec_guarded
        ])
        .run(tauri::generate_context!())
        .expect("error while running CopyToDesk desktop app");
}
