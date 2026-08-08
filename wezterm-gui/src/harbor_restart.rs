use anyhow::{bail, Context};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use wezterm_gui_subcommands::harbor_control_socket_path;
use window::{Connection, ConnectionOps};

static CONTROL_SERVER_STARTED: AtomicBool = AtomicBool::new(false);
static RESTARTING: AtomicBool = AtomicBool::new(false);

#[derive(serde::Deserialize)]
struct ControlRequest {
    version: u8,
    command: String,
    #[serde(default)]
    full: bool,
}

#[derive(serde::Serialize)]
struct ControlResponse<'a> {
    ok: bool,
    message: &'a str,
}

fn sibling_binary(name: &str) -> anyhow::Result<std::path::PathBuf> {
    let current = std::env::current_exe().context("resolve Terminal Harbor executable")?;
    let dir = current
        .parent()
        .context("Terminal Harbor executable has no parent directory")?;
    Ok(dir.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }))
}

fn prepare_restart(full: bool) -> anyhow::Result<()> {
    if RESTARTING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        bail!("a Terminal Harbor restart is already in progress");
    }

    let result = (|| {
        if full {
            crate::harbor_workspace::snapshot_workspace_cwds()
                .context("save workspace directories before complete restart")?;
        }
        let wezterm = sibling_binary("wezterm")?;
        if !full {
            let status = Command::new(&wezterm)
                .arg("_check-harbor-mux")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("check persistent session host compatibility")?;
            if !status.success() {
                bail!(
                    "the new Terminal Harbor GUI is incompatible with the running session host; use a complete restart"
                );
            }
        }

        let mut helper = Command::new(wezterm);
        helper
            .arg("_restart-helper")
            .arg("--gui-pid")
            .arg(std::process::id().to_string())
            .arg("--class")
            .arg(crate::termwindow::get_window_class())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if full {
            helper.arg("--full");
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            helper.process_group(0);
        }
        helper
            .spawn()
            .context("spawn Terminal Harbor restart helper")?;
        Ok(())
    })();

    if result.is_err() {
        RESTARTING.store(false, Ordering::SeqCst);
    }
    result
}

fn terminate_gui() {
    if let Some(connection) = Connection::get() {
        connection.terminate_message_loop();
    }
}

pub fn restart_application(full: bool) -> anyhow::Result<()> {
    if let Err(err) = prepare_restart(full) {
        wezterm_toast_notification::persistent_toast_notification(
            "Terminal Harbor restart failed",
            &format!("{err:#}"),
        );
        return Err(err);
    }
    terminate_gui();
    Ok(())
}

pub fn shutdown_session_host() {
    if let Err(err) = crate::harbor_workspace::snapshot_workspace_cwds() {
        log::error!("saving workspace directories before shutdown: {err:#}");
    }
    let path = wezterm_gui_subcommands::harbor_mux_pid_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        log::error!(
            "invalid Terminal Harbor session host pid in {}",
            path.display()
        );
        return;
    };
    let Some(executable) = procinfo::LocalProcessInfo::executable_path(pid) else {
        return;
    };
    let is_mux_server = executable
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == "wezterm-mux-server" || name == "wezterm-mux-server.exe")
        .unwrap_or(false);
    if !is_mux_server {
        log::error!(
            "refusing to stop pid {pid}: {} is not wezterm-mux-server",
            executable.display()
        );
        return;
    }
    #[cfg(unix)]
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::NotFound {
            log::error!("stopping Terminal Harbor session host {pid}: {err}");
        }
    }
}

#[cfg(unix)]
fn handle_control_stream(mut stream: std::os::unix::net::UnixStream) -> anyhow::Result<()> {
    let mut request_line = String::new();
    BufReader::new(stream.try_clone()?)
        .read_line(&mut request_line)
        .context("read restart request")?;
    let request: ControlRequest =
        serde_json::from_str(&request_line).context("parse restart request")?;
    if request.version != 1 || request.command != "restart" {
        bail!("unsupported Terminal Harbor control request");
    }

    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    promise::spawn::spawn_into_main_thread(async move {
        tx.send(prepare_restart(request.full).map(|_| ())).ok();
    })
    .detach();

    let result = rx
        .recv_timeout(Duration::from_secs(15))
        .context("restart request timed out")?;
    match result {
        Ok(()) => {
            serde_json::to_writer(
                &mut stream,
                &ControlResponse {
                    ok: true,
                    message: "restart accepted",
                },
            )?;
            stream.write_all(b"\n")?;
            stream.flush()?;
            promise::spawn::spawn_into_main_thread(async move { terminate_gui() }).detach();
        }
        Err(err) => {
            let message = format!("{err:#}");
            serde_json::to_writer(
                &mut stream,
                &serde_json::json!({"ok": false, "message": message}),
            )?;
            stream.write_all(b"\n")?;
            stream.flush()?;
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn start_control_server(window_class: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    if CONTROL_SERVER_STARTED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let path = harbor_control_socket_path(window_class);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("remove stale Terminal Harbor control socket"),
    }
    let listener = UnixListener::bind(&path).context("bind Terminal Harbor control socket")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new()
        .name("harbor-restart-control".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Err(err) = handle_control_stream(stream) {
                            log::error!("Terminal Harbor restart control: {err:#}");
                        }
                    }
                    Err(err) => {
                        log::error!("Terminal Harbor restart control listener: {err:#}");
                        break;
                    }
                }
            }
        })?;
    Ok(())
}

#[cfg(not(unix))]
pub fn start_control_server(_window_class: &str) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_preserving_restart_request() {
        let request: ControlRequest =
            serde_json::from_str(r#"{"version":1,"command":"restart","full":false}"#).unwrap();
        assert_eq!(request.version, 1);
        assert_eq!(request.command, "restart");
        assert!(!request.full);
    }

    #[test]
    fn missing_full_field_defaults_to_preserving() {
        let request: ControlRequest =
            serde_json::from_str(r#"{"version":1,"command":"restart"}"#).unwrap();
        assert!(!request.full);
    }
}
