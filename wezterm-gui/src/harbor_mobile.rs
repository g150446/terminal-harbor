//! Local LAN bridge for Terminal Harbor Mobile (QR pairing + REST API).

use crate::harbor_workspace::{self, WorkspaceActivity};
use crate::termwindow::TermWindowNotif;
use anyhow::{anyhow, Context};
use window::WindowOps;
use mux::Mux;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const DEFAULT_PORT: u16 = 7780;
const PAIR_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
const API_VERSION: &str = "1.0.0";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceRecord {
    token: String,
    name: String,
    created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct PersistedBridgeState {
    devices: Vec<DeviceRecord>,
}

#[derive(Clone, Debug)]
struct PairOffer {
    token: String,
    expires_at: Instant,
}

#[derive(Clone, Debug)]
pub struct PairingView {
    pub uri: String,
    pub host: String,
    pub port: u16,
    pub expires_in_sec: u64,
    pub device_count: usize,
}

struct BridgeInner {
    port: u16,
    host: String,
    pair_offer: Option<PairOffer>,
    devices: HashMap<String, DeviceRecord>,
    pairing_ui_visible: bool,
    /// PNG rendered for the current offer: (token, path).
    qr_png: Option<(String, PathBuf)>,
}

lazy_static::lazy_static! {
    static ref INNER: Mutex<BridgeInner> = Mutex::new(BridgeInner {
        port: DEFAULT_PORT,
        host: String::new(),
        pair_offer: None,
        devices: HashMap::new(),
        pairing_ui_visible: false,
        qr_png: None,
    });
}

static SERVER_STARTED: AtomicBool = AtomicBool::new(false);

fn state_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("terminal-harbor")
}

fn devices_path() -> PathBuf {
    state_dir().join("mobile-devices.json")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_token() -> String {
    format!("{}", Uuid::new_v4().simple())
}

fn guess_lan_ip() -> String {
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                let ip = addr.ip();
                if !ip.is_loopback() {
                    return ip.to_string();
                }
            }
        }
    }
    "127.0.0.1".to_string()
}

fn load_devices(inner: &mut BridgeInner) {
    let path = devices_path();
    let Ok(data) = fs::read(&path) else {
        return;
    };
    if let Ok(state) = serde_json::from_slice::<PersistedBridgeState>(&data) {
        inner.devices = state
            .devices
            .into_iter()
            .map(|d| (d.token.clone(), d))
            .collect();
    }
}

fn save_devices(inner: &BridgeInner) -> anyhow::Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let state = PersistedBridgeState {
        devices: inner.devices.values().cloned().collect(),
    };
    let temp = dir.join("mobile-devices.json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(&state)?)?;
    fs::rename(temp, devices_path())?;
    Ok(())
}

fn activity_str(activity: WorkspaceActivity) -> &'static str {
    match activity {
        WorkspaceActivity::Error => "error",
        WorkspaceActivity::Waiting => "waiting",
        WorkspaceActivity::Running => "running",
        WorkspaceActivity::Unread => "unread",
        WorkspaceActivity::Done => "done",
        WorkspaceActivity::Idle => "idle",
    }
}

/// Render the pairing URI as a square PNG with a quiet zone.
/// Text-cell rendering distorts module aspect ratio, so phones cannot scan it;
/// a real image is required for reliable pairing.
fn write_pair_qr_png(uri: &str) -> anyhow::Result<PathBuf> {
    let code = qrcode::QrCode::new(uri.as_bytes()).context("QR encode")?;
    let img = code
        .render::<image023::Luma<u8>>()
        .quiet_zone(true)
        .min_dimensions(360, 360)
        .module_dimensions(8, 8)
        .build();
    let (width, height) = (img.width(), img.height());
    let raw = img.into_raw();
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join("pair-qr.png");
    image::save_buffer(&path, &raw, width, height, image::ColorType::L8)
        .map_err(|err| anyhow!("saving pair QR PNG: {err}"))?;
    Ok(path)
}

fn build_pair_uri(host: &str, port: u16, token: &str) -> String {
    format!("harbor://pair?v=1&host={host}&port={port}&tls=0&token={token}")
}

/// Start the LAN bridge once (idempotent). Safe to call from the GUI thread.
pub fn ensure_running() {
    if SERVER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    {
        let mut inner = INNER.lock();
        load_devices(&mut inner);
        if inner.host.is_empty() {
            inner.host = guess_lan_ip();
        }
    }

    thread::Builder::new()
        .name("harbor-mobile-bridge".into())
        .spawn(|| {
            if let Err(err) = run_server() {
                log::error!("harbor mobile bridge stopped: {err:#}");
                SERVER_STARTED.store(false, Ordering::SeqCst);
            }
        })
        .expect("spawn harbor mobile bridge");
}

fn run_server() -> anyhow::Result<()> {
    let port = { INNER.lock().port };
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).with_context(|| format!("bind mobile bridge on {addr}"))?;
    listener.set_nonblocking(false)?;
    log::info!("Terminal Harbor mobile bridge listening on http://0.0.0.0:{port}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(err) = handle_client(stream) {
                        log::debug!("mobile bridge client error: {err:#}");
                    }
                });
            }
            Err(err) => {
                log::warn!("mobile bridge accept error: {err:#}");
            }
        }
    }
    Ok(())
}

fn handle_client(mut stream: TcpStream) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;

    let mut buf = vec![0u8; 64 * 1024];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let req_text = String::from_utf8_lossy(&buf[..n]);
    let mut lines = req_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    let mut authorization = String::new();
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.split_once(':') {
            if v.0.eq_ignore_ascii_case("authorization") {
                authorization = v.1.trim().to_string();
            }
        }
    }

    let header_end = req_text
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(n);
    let mut body = Vec::new();
    if header_end < n {
        body.extend_from_slice(&buf[header_end..n]);
    }
    while body.len() < content_length {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
        if body.len() >= content_length {
            body.truncate(content_length);
            break;
        }
    }

    let (status, payload) = dispatch(&method, &path, &authorization, &body);
    write_response(&mut stream, status, &payload)?;
    Ok(())
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Headers: Authorization, Content-Type\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    Ok(())
}

fn json_error(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            Some(v)
        } else {
            None
        }
    })
}

fn dispatch(method: &str, path: &str, authorization: &str, body: &[u8]) -> (u16, String) {
    if method == "OPTIONS" {
        return (200, String::new());
    }
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path, ""),
    };

    if method == "POST" && path == "/v1/pair" {
        return pair(body);
    }

    if !authorize(authorization) {
        return (401, json_error("unauthorized"));
    }

    if method == "GET" && path == "/v1/session" {
        return (
            200,
            serde_json::json!({
                "ok": true,
                "version": API_VERSION,
                "host_name": hostname::get()
                    .ok()
                    .and_then(|h| h.into_string().ok())
                    .unwrap_or_else(|| "terminal-harbor".into()),
            })
            .to_string(),
        );
    }

    if method == "GET" && path == "/v1/workspaces" {
        return match run_on_main(list_workspaces_json) {
            Ok(json) => (200, json),
            Err(err) => (500, json_error(&format!("{err:#}"))),
        };
    }

    if let Some(id) = path
        .strip_prefix("/v1/workspaces/")
        .and_then(|rest| rest.strip_suffix("/activate"))
    {
        if method == "POST" {
            let id = id.to_string();
            return match run_on_main(move || activate_workspace(&id)) {
                Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
                Err(err) => {
                    let msg = format!("{err:#}");
                    if msg.contains("not found") {
                        (404, json_error(&msg))
                    } else {
                        (500, json_error(&msg))
                    }
                }
            };
        }
    }

    if let Some(id) = path
        .strip_prefix("/v1/workspaces/")
        .and_then(|rest| rest.strip_suffix("/instruction"))
    {
        if method == "POST" {
            #[derive(Deserialize)]
            struct InstructionBody {
                text: String,
                #[serde(default = "default_true")]
                submit: bool,
            }
            fn default_true() -> bool {
                true
            }
            let parsed: InstructionBody = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return (400, json_error("invalid json body")),
            };
            let id = id.to_string();
            return match run_on_main(move || send_instruction(&id, &parsed.text, parsed.submit)) {
                Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
                Err(err) => {
                    let msg = format!("{err:#}");
                    if msg.contains("not found") {
                        (404, json_error(&msg))
                    } else {
                        (500, json_error(&msg))
                    }
                }
            };
        }
    }

    if let Some(id) = path
        .strip_prefix("/v1/workspaces/")
        .and_then(|rest| rest.strip_suffix("/screen"))
    {
        if method == "GET" {
            let lines = query_param(query, "lines")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(60)
                .clamp(1, 200);
            let id = id.to_string();
            return match run_on_main(move || screen_text(&id, lines)) {
                Ok(json) => (200, json),
                Err(err) => {
                    let msg = format!("{err:#}");
                    if msg.contains("not found") {
                        (404, json_error(&msg))
                    } else {
                        (500, json_error(&msg))
                    }
                }
            };
        }
    }

    if method != "GET" && method != "POST" {
        return (405, json_error("method not allowed"));
    }
    (404, json_error("not found"))
}

fn authorize(authorization: &str) -> bool {
    let token = authorization
        .strip_prefix("Bearer ")
        .or_else(|| authorization.strip_prefix("bearer "))
        .unwrap_or("")
        .trim();
    if token.is_empty() {
        return false;
    }
    INNER.lock().devices.contains_key(token)
}

fn pair(body: &[u8]) -> (u16, String) {
    #[derive(Deserialize)]
    struct PairBody {
        token: String,
        #[serde(default)]
        device_name: Option<String>,
    }
    let parsed: PairBody = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json_error("invalid json body")),
    };

    let mut inner = INNER.lock();
    let offer = match inner.pair_offer.clone() {
        Some(o) => o,
        None => return (401, json_error("no active pairing offer")),
    };
    if Instant::now() > offer.expires_at {
        inner.pair_offer = None;
        return (401, json_error("pairing token expired"));
    }
    if parsed.token != offer.token {
        return (401, json_error("invalid pairing token"));
    }

    // one-time
    inner.pair_offer = None;
    let device_token = random_token();
    let record = DeviceRecord {
        token: device_token.clone(),
        name: parsed
            .device_name
            .unwrap_or_else(|| "Mobile".to_string()),
        created_at: now_unix(),
    };
    inner.devices.insert(device_token.clone(), record);
    if let Err(err) = save_devices(&inner) {
        log::error!("saving mobile devices: {err:#}");
    }

    let base_url = format!("http://{}:{}", inner.host, inner.port);
    (
        200,
        serde_json::json!({
            "device_token": device_token,
            "base_url": base_url,
        })
        .to_string(),
    )
}

fn run_on_main<T, F>(f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    promise::spawn::spawn_into_main_thread(async move {
        let result = f();
        let _ = tx.send(result);
    })
    .detach();
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|_| anyhow!("timed out waiting for GUI thread"))?
}

fn list_workspaces_json() -> anyhow::Result<String> {
    let rows = harbor_workspace::rows();
    let workspaces: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "id": row.workspace.id,
                "name": row.workspace.name,
                "root": row.workspace.root.as_ref().map(|p| p.display().to_string()),
                "mux_workspace": row.workspace.mux_workspace,
                "activity": activity_str(row.activity),
                "process": row.process,
                "message": row.message,
                "selected": row.selected,
            })
        })
        .collect();
    Ok(serde_json::json!({ "workspaces": workspaces }).to_string())
}

fn find_workspace(id: &str) -> anyhow::Result<harbor_workspace::HarborWorkspace> {
    let uuid = Uuid::parse_str(id).context("invalid workspace id")?;
    harbor_workspace::workspaces()
        .into_iter()
        .find(|w| w.id == uuid)
        .ok_or_else(|| anyhow!("workspace not found"))
}

fn activate_workspace(id: &str) -> anyhow::Result<()> {
    let workspace = find_workspace(id)?;
    let mux = Mux::get();
    if !mux
        .iter_windows_in_workspace(&workspace.mux_workspace)
        .is_empty()
    {
        if let Some(fe) = crate::frontend::try_front_end() {
            fe.switch_workspace(&workspace.mux_workspace);
            return Ok(());
        }
        return Err(anyhow!("frontend unavailable"));
    }

    // Need a TermWindow to spawn into an empty workspace.
    // Fire-and-forget: we are already on the GUI thread via run_on_main, so
    // blocking on Apply would deadlock the event loop.
    let fe = crate::frontend::try_front_end().ok_or_else(|| anyhow!("frontend unavailable"))?;
    let window = fe
        .first_window()
        .ok_or_else(|| anyhow!("no desktop window available"))?;
    let ws = workspace.clone();
    window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
        if let Err(err) = term_window.harbor_activate_workspace(ws) {
            log::error!("mobile activate workspace: {err:#}");
        }
    })));
    Ok(())
}

fn workspace_active_pane(
    workspace: &harbor_workspace::HarborWorkspace,
) -> anyhow::Result<std::sync::Arc<dyn mux::pane::Pane>> {
    let mux = Mux::get();
    let mut target = None;
    for window_id in mux.iter_windows_in_workspace(&workspace.mux_workspace) {
        if let Some(tab) = mux.get_active_tab_for_window(window_id) {
            if let Some(pane) = tab.get_active_pane() {
                target = Some(pane);
                break;
            }
        }
    }
    target.ok_or_else(|| anyhow!("workspace not found or has no panes"))
}

fn send_instruction(id: &str, text: &str, submit: bool) -> anyhow::Result<()> {
    let workspace = find_workspace(id)?;
    let pane = workspace_active_pane(&workspace)?;
    if !text.is_empty() {
        pane.send_paste(text)?;
    }
    if submit {
        // Enter must be a real key event: a CR embedded in a bracketed paste
        // is inserted literally and never executes the line.
        let mut writer = pane.writer();
        writer.write_all(b"\r")?;
        writer.flush()?;
    }
    Ok(())
}

/// Render the last `nlines` lines of the workspace's active pane as plain
/// text so the mobile app can mirror the terminal screen.
fn screen_text(id: &str, nlines: usize) -> anyhow::Result<String> {
    let workspace = find_workspace(id)?;
    let pane = workspace_active_pane(&workspace)?;

    let dims = pane.get_dimensions();
    let bottom_row = dims.physical_top + dims.viewport_rows as isize;
    let top_row = bottom_row.saturating_sub(nlines as isize);
    let (_first_row, lines) = pane.get_lines(top_row..bottom_row);

    let mut text = String::new();
    for line in lines {
        text.push_str(line.as_str().trim_end());
        text.push('\n');
    }
    let trimmed = text.trim_end().len();
    text.truncate(trimmed);

    Ok(serde_json::json!({
        "text": text,
        "lines": nlines,
        "alt_screen": pane.is_alt_screen_active(),
    })
    .to_string())
}

/// Toggle the pairing QR panel in the sidebar and mint a fresh one-time token.
pub fn toggle_pairing_ui() -> bool {
    ensure_running();
    let mut inner = INNER.lock();
    inner.pairing_ui_visible = !inner.pairing_ui_visible;
    if inner.pairing_ui_visible {
        refresh_pair_offer_locked(&mut inner);
    }
    inner.pairing_ui_visible
}

pub fn pairing_ui_visible() -> bool {
    INNER.lock().pairing_ui_visible
}

pub fn refresh_pair_offer() {
    ensure_running();
    let mut inner = INNER.lock();
    refresh_pair_offer_locked(&mut inner);
}

fn refresh_pair_offer_locked(inner: &mut BridgeInner) {
    if inner.host.is_empty() {
        inner.host = guess_lan_ip();
    }
    inner.pair_offer = Some(PairOffer {
        token: random_token(),
        expires_at: Instant::now() + PAIR_TOKEN_TTL,
    });
}

pub fn pairing_view() -> Option<PairingView> {
    let mut inner = INNER.lock();
    if !inner.pairing_ui_visible {
        return None;
    }
    if inner.host.is_empty() {
        inner.host = guess_lan_ip();
    }
    let offer = match &inner.pair_offer {
        Some(o) if Instant::now() <= o.expires_at => o.clone(),
        _ => {
            refresh_pair_offer_locked(&mut inner);
            inner.pair_offer.clone().unwrap()
        }
    };
    let remaining = offer
        .expires_at
        .saturating_duration_since(Instant::now())
        .as_secs();
    let uri = build_pair_uri(&inner.host, inner.port, &offer.token);

    // (Re)render the scannable PNG when the offer changes.
    let needs_png = match &inner.qr_png {
        Some((token, _)) => token != &offer.token,
        None => true,
    };
    if needs_png {
        match write_pair_qr_png(&uri) {
            Ok(path) => inner.qr_png = Some((offer.token.clone(), path)),
            Err(err) => log::error!("rendering pair QR: {err:#}"),
        }
    }

    Some(PairingView {
        uri,
        host: inner.host.clone(),
        port: inner.port,
        expires_in_sec: remaining,
        device_count: inner.devices.len(),
    })
}

/// The current pair URI, if a pairing panel is active. Suitable for clipboard.
pub fn current_pair_uri() -> Option<String> {
    pairing_view().map(|view| view.uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_qr_png_is_written_and_decodable_shape() {
        let uri = build_pair_uri("192.168.1.20", DEFAULT_PORT, "deadbeefcafe");
        let path = write_pair_qr_png(&uri).expect("write qr png");
        let bytes = std::fs::read(&path).expect("read qr png");
        // PNG magic
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let img = image::load_from_memory(&bytes).expect("decode qr png");
        assert!(img.width() >= 360);
        assert_eq!(img.width(), img.height());
        let _ = std::fs::remove_file(&path);
    }
}

/// Open the pairing QR image with the OS default image viewer.
pub fn open_pair_qr() {
    // Ensure the PNG exists for the current offer.
    let _ = pairing_view();
    let path = { INNER.lock().qr_png.as_ref().map(|(_, p)| p.clone()) };
    let Some(path) = path else {
        return;
    };
    let spawned = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(&path).spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(&path)
            .spawn()
    } else {
        std::process::Command::new("xdg-open").arg(&path).spawn()
    };
    if let Err(err) = spawned {
        log::error!("opening pair QR image: {err:#}");
    }
}
