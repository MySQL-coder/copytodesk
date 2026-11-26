//! Cross-platform OS power management (prevent idle system sleep while Host runs)
//! and user-level OS boot auto-start registration (Windows Registry Run, macOS LaunchAgents, Linux XDG Autostart).

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Per-thread sleep prevention hook (used on Windows `SetThreadExecutionState` inside the long-lived Host thread).
pub fn set_thread_prevent_sleep(enable: bool) {
    #[cfg(target_os = "windows")]
    {
        const ES_CONTINUOUS: u32 = 0x8000_0000;
        const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
        extern "system" {
            fn SetThreadExecutionState(es_flags: u32) -> u32;
        }
        let flags = if enable {
            ES_CONTINUOUS | ES_SYSTEM_REQUIRED
        } else {
            ES_CONTINUOUS
        };
        unsafe {
            let _ = SetThreadExecutionState(flags);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = enable;
    }
}

/// Process-scoped idle sleep inhibitor for macOS (`caffeinate`) and Linux (`systemd-inhibit`).
pub struct SleepInhibitor {
    child: Option<Child>,
}

impl SleepInhibitor {
    pub fn acquire(enable: bool) -> Self {
        if !enable {
            return Self { child: None };
        }
        #[cfg(target_os = "macos")]
        {
            let pid = std::process::id().to_string();
            let child = Command::new("caffeinate")
                .args(["-i", "-w", &pid])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
            return Self { child };
        }
        #[cfg(target_os = "linux")]
        {
            let child = Command::new("systemd-inhibit")
                .args([
                    "--what=idle:sleep",
                    "--who=CopyToDesk",
                    "--why=Unattended Remote Desktop Host",
                    "sleep",
                    "infinity",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
            return Self { child };
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Self { child: None }
        }
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_plist_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join("io.copytodesk.cleanroom.plist"),
    )
}

#[cfg(target_os = "linux")]
fn linux_autostart_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            return Some(PathBuf::from(xdg).join("autostart").join("copytodesk.desktop"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("autostart")
            .join("copytodesk.desktop"),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Checks whether CopyToDesk is registered for user-login OS auto-start.
pub fn is_os_autostart_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("reg")
            .args([
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "CopyToDesk",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "macos")]
    {
        macos_plist_path().map(|p| p.exists()).unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        linux_autostart_path().map(|p| p.exists()).unwrap_or(false)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// Enables or disables user-login OS auto-start (`--minimized` silent tray daemon mode).
pub fn set_os_autostart_enabled(enable: bool) -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_str = exe.to_string_lossy().to_string();
    let _ = xml_escape(&exe_str);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if enable {
            let cmd_val = format!("\"{}\" --minimized", exe_str);
            let st = Command::new("reg")
                .args([
                    "add",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    "CopyToDesk",
                    "/t",
                    "REG_SZ",
                    "/d",
                    &cmd_val,
                    "/f",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|e| e.to_string())?;
            if !st.success() {
                return Err("写入 Windows 注册表开机启动项失败".into());
            }
        } else {
            let _ = Command::new("reg")
                .args([
                    "delete",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    "CopyToDesk",
                    "/f",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        return Ok(enable);
    }

    #[cfg(target_os = "macos")]
    {
        let plist = macos_plist_path().ok_or_else(|| "HOME not found".to_string())?;
        if enable {
            if let Some(parent) = plist.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let escaped = xml_escape(&exe_str);
            let content = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>io.copytodesk.cleanroom</string>
  <key>ProgramArguments</key>
  <array>
    <string>{escaped}</string>
    <string>--minimized</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
"#
            );
            std::fs::write(&plist, content).map_err(|e| e.to_string())?;
        } else if plist.exists() {
            let _ = std::fs::remove_file(&plist);
        }
        return Ok(enable);
    }

    #[cfg(target_os = "linux")]
    {
        let desktop = linux_autostart_path().ok_or_else(|| "HOME not found".to_string())?;
        if enable {
            if let Some(parent) = desktop.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let escaped_exe = exe_str.replace('"', "\\\"");
            let content = format!(
                "[Desktop Entry]\nType=Application\nName=CopyToDesk\nComment=CopyToDesk Remote Desktop Service\nExec=\"{escaped_exe}\" --minimized\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
            );
            std::fs::write(&desktop, content).map_err(|e| e.to_string())?;
        } else if desktop.exists() {
            let _ = std::fs::remove_file(&desktop);
        }
        return Ok(enable);
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_handles_special_chars() {
        assert_eq!(xml_escape("/Applications/A&B<1>.app"), "/Applications/A&amp;B&lt;1&gt;.app");
        let _guard = SleepInhibitor::acquire(false);
    }
}
