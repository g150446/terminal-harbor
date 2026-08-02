use anyhow::{bail, Context};
use clap::Parser;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use wezterm_gui_subcommands::{
    harbor_control_socket_path, harbor_mux_pid_path, harbor_mux_socket_path, DEFAULT_WINDOW_CLASS,
};

#[derive(Debug, Parser, Clone)]
pub struct RestartCommand {
    /// Terminate terminal sessions and restart the persistent session host too.
    #[arg(long)]
    full: bool,

    /// Window class of the Terminal Harbor GUI to restart.
    #[arg(long, default_value = DEFAULT_WINDOW_CLASS)]
    class: String,
}

#[derive(Debug, Parser, Clone)]
pub struct RestartHelperCommand {
    #[arg(long)]
    gui_pid: u32,

    #[arg(long)]
    full: bool,

    #[arg(long, default_value = DEFAULT_WINDOW_CLASS)]
    class: String,
}

impl RestartCommand {
    #[cfg(unix)]
    pub fn run(&self) -> anyhow::Result<()> {
        use std::os::unix::net::UnixStream;

        if !self.full {
            check_mux_compatibility()?;
        }
        let socket = harbor_control_socket_path(&self.class);
        let mut stream = UnixStream::connect(&socket).with_context(|| {
            format!(
                "Terminal Harbor is not running or does not support restart control ({})",
                socket.display()
            )
        })?;
        serde_json::to_writer(
            &mut stream,
            &serde_json::json!({"version": 1, "command": "restart", "full": self.full}),
        )?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .context("read Terminal Harbor restart response")?;
        let response: serde_json::Value =
            serde_json::from_str(&response).context("parse Terminal Harbor restart response")?;
        if response.get("ok").and_then(|value| value.as_bool()) != Some(true) {
            bail!(
                "{}",
                response
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("Terminal Harbor rejected the restart")
            );
        }
        println!(
            "Terminal Harbor {} restart accepted",
            if self.full {
                "complete"
            } else {
                "session-preserving"
            }
        );
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn run(&self) -> anyhow::Result<()> {
        let _ = self;
        bail!("Terminal Harbor restart control is currently supported on Unix systems")
    }
}

fn check_mux_compatibility() -> anyhow::Result<()> {
    let unix_domain = config::UnixDomain {
        name: wezterm_gui_subcommands::HARBOR_PERSISTENT_DOMAIN.to_string(),
        socket_path: Some(harbor_mux_socket_path()),
        no_serve_automatically: true,
        ..Default::default()
    };
    let mut ui = mux::connui::ConnectionUI::new_headless();
    let client =
        wezterm_client::client::Client::new_unix_domain(None, &unix_domain, false, &mut ui, true)
            .context("connect to the Terminal Harbor persistent session host")?;
    let executor = promise::spawn::ScopedExecutor::new();
    promise::spawn::block_on(
        executor.run(async move { client.verify_version_compat(&mut ui).await.map(|_| ()) }),
    )
    .context("the installed GUI is incompatible with the persistent session host")
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> anyhow::Result<()> {
    let started = Instant::now();
    while process_is_running(pid) {
        if started.elapsed() >= timeout {
            bail!("timed out waiting for process {pid} to exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn stop_session_host() -> anyhow::Result<()> {
    let pid_path = harbor_mux_pid_path();
    let text = match std::fs::read_to_string(&pid_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context("read Terminal Harbor session host pid"),
    };
    let pid: u32 = text.trim().parse().context("parse session host pid")?;
    if !process_is_running(pid) {
        return Ok(());
    }
    let executable = procinfo::LocalProcessInfo::executable_path(pid)
        .context("resolve session host executable")?;
    let is_mux_server = executable
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == "wezterm-mux-server" || name == "wezterm-mux-server.exe")
        .unwrap_or(false);
    if !is_mux_server {
        bail!(
            "refusing to stop pid {pid}: {} is not wezterm-mux-server",
            executable.display()
        );
    }
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error()).context("stop session host");
        }
    }
    wait_for_process_exit(pid, Duration::from_secs(10))
        .context("wait for Terminal Harbor session host to stop")
}

impl RestartHelperCommand {
    pub fn run(&self) -> anyhow::Result<()> {
        wait_for_process_exit(self.gui_pid, Duration::from_secs(120))
            .context("wait for Terminal Harbor GUI to stop")?;
        if self.full {
            stop_session_host()?;
        }
        std::thread::sleep(Duration::from_millis(150));

        let current = std::env::current_exe()?;
        let gui = current
            .parent()
            .context("restart helper executable has no parent")?
            .join(if cfg!(windows) {
                "wezterm-gui.exe"
            } else {
                "wezterm-gui"
            });
        Command::new(gui)
            .arg("start")
            .arg("--class")
            .arg(&self.class)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("launch the restarted Terminal Harbor GUI")?;
        Ok(())
    }
}

pub fn run_compatibility_check() -> anyhow::Result<()> {
    check_mux_compatibility()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserving_restart_is_the_default() {
        let command = RestartCommand::try_parse_from(["restart"]).unwrap();
        assert!(!command.full);
    }

    #[test]
    fn full_restart_is_explicit() {
        let command = RestartCommand::try_parse_from(["restart", "--full"]).unwrap();
        assert!(command.full);
    }
}
