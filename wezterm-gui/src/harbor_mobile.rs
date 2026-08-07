//! Local LAN bridge for Terminal Harbor Mobile (QR pairing + REST API).

use crate::harbor_workspace::{self, WorkspaceActivity};
use crate::termwindow::TermWindowNotif;
use anyhow::{Context, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use config::keyassignment::SpawnTabDomain;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use mux::Mux;
use mux::pane::CachePolicy;
use parking_lot::Mutex;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, thread};
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};
use wezterm_term::{KeyCode, KeyModifiers};
use window::WindowOps;

pub const DEFAULT_PORT: u16 = 7780;
const PAIR_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
const API_VERSION: &str = "1.3.0";
const AUTH_VERSION: &str = "hmac-sha256-v1";
const AUTH_CLOCK_SKEW_SECS: u64 = 5 * 60;
const REPLAY_TTL_SECS: u64 = 10 * 60;

fn new_client_id() -> String {
    Uuid::new_v4().to_string()
}

fn default_auth_version() -> u8 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceRecord {
    #[serde(default)]
    client_id: String,
    token: String,
    name: String,
    created_at: u64,
    #[serde(default = "default_auth_version")]
    auth_version: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct PersistedBridgeState {
    #[serde(default)]
    server_id: String,
    devices: Vec<DeviceRecord>,
}

#[derive(Clone, Debug, Serialize)]
struct Endpoint {
    kind: &'static str,
    url: String,
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
    server_id: String,
    port: u16,
    host: String,
    pair_offer: Option<PairOffer>,
    devices: HashMap<String, DeviceRecord>,
    pairing_ui_visible: bool,
    /// PNG rendered for the current offer: (token, path).
    qr_png: Option<(String, PathBuf)>,
    replay_nonces: VecDeque<(String, String, u64)>,
}

lazy_static::lazy_static! {
    static ref SPEECH_TERM_RE: Regex = Regex::new(
        r"(?:[A-Za-z0-9_@.+-]+/)+[A-Za-z0-9_@.+-]+|[A-Za-z][A-Za-z0-9_@.+/-]{2,}"
    ).expect("valid speech term regex");
    static ref QUOTED_SPEECH_TERM_RE: Regex = Regex::new(
        r#"[`\"']([^`\"'\r\n]{2,64})[`\"']"#
    ).expect("valid quoted speech term regex");
    static ref INNER: Mutex<BridgeInner> = Mutex::new(BridgeInner {
        server_id: String::new(),
        port: DEFAULT_PORT,
        host: String::new(),
        pair_offer: None,
        devices: HashMap::new(),
        pairing_ui_visible: false,
        qr_png: None,
        replay_nonces: VecDeque::new(),
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

fn endpoint(kind: &'static str, url: String) -> Endpoint {
    Endpoint { kind, url }
}

fn tailscale_output(args: &[&str]) -> Option<Vec<u8>> {
    let candidates = [
        "/usr/local/bin/tailscale",
        "/opt/homebrew/bin/tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "tailscale",
    ];
    let mut child = candidates.iter().find_map(|executable| {
        std::process::Command::new(executable)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()
    })?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut output = Vec::new();
                child.stdout.take()?.read_to_end(&mut output).ok()?;
                return Some(output);
            }
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn connection_endpoints(host: &str, port: u16) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    if let Some(status_bytes) = tailscale_output(&["status", "--json"]) {
        if let Ok(status) = serde_json::from_slice::<serde_json::Value>(&status_bytes) {
            let running = status
                .get("BackendState")
                .and_then(|value| value.as_str())
                .map(|value| value.eq_ignore_ascii_case("running"))
                .unwrap_or(false);
            if running {
                let dns_name = status
                    .get("Self")
                    .and_then(|value| value.get("DNSName"))
                    .and_then(|value| value.as_str())
                    .map(|value| value.trim_end_matches('.'))
                    .filter(|value| !value.is_empty());
                let serve_enabled = tailscale_output(&["serve", "status", "--json"])
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .map(|text| {
                        text.contains("127.0.0.1:7780")
                            || text.contains("localhost:7780")
                            || text.contains("http://127.0.0.1:7780")
                    })
                    .unwrap_or(false);
                if serve_enabled {
                    if let Some(name) = dns_name {
                        endpoints.push(endpoint("tailscale_https", format!("https://{name}")));
                    }
                }
                if let Some(name) = dns_name {
                    endpoints.push(endpoint(
                        "tailscale_direct",
                        format!("http://{name}:{port}"),
                    ));
                }
                if let Some(ips) = status
                    .get("Self")
                    .and_then(|value| value.get("TailscaleIPs"))
                    .and_then(|value| value.as_array())
                {
                    for ip in ips.iter().filter_map(|value| value.as_str()) {
                        let authority = if ip.contains(':') {
                            format!("[{ip}]")
                        } else {
                            ip.to_string()
                        };
                        endpoints.push(endpoint(
                            "tailscale_direct",
                            format!("http://{authority}:{port}"),
                        ));
                    }
                }
            }
        }
    }
    endpoints.push(endpoint("lan", format!("http://{host}:{port}")));
    endpoints
}

fn start_dns_sd(server_id: &str, port: u16) -> anyhow::Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new().context("start DNS-SD daemon")?;
    daemon
        .set_service_name_len_max(20)
        .context("allow Terminal Harbor DNS-SD service name")?;
    let instance = device_name();
    let mut hostname = host_name().trim_end_matches('.').to_string();
    if !hostname.ends_with(".local") {
        hostname.push_str(".local");
    }
    hostname.push('.');
    let properties = [
        ("sid", server_id),
        ("api", API_VERSION),
        ("auth", AUTH_VERSION),
    ];
    let info = ServiceInfo::new(
        "_terminal-harbor._tcp.local.",
        &instance,
        &hostname,
        "",
        port,
        &properties[..],
    )?
    .enable_addr_auto();
    daemon.register(info).context("register DNS-SD service")?;
    Ok(daemon)
}

fn load_devices(inner: &mut BridgeInner) {
    let path = devices_path();
    let Ok(data) = fs::read(&path) else {
        inner.server_id = Uuid::new_v4().to_string();
        if let Err(err) = save_devices(inner) {
            log::error!("initializing mobile bridge identity: {err:#}");
        }
        return;
    };
    if let Ok(mut state) = serde_json::from_slice::<PersistedBridgeState>(&data) {
        let mut migrated = false;
        if state.server_id.is_empty() {
            state.server_id = Uuid::new_v4().to_string();
            migrated = true;
        }
        for device in &mut state.devices {
            if device.client_id.is_empty() {
                device.client_id = new_client_id();
                migrated = true;
            }
        }
        inner.server_id = state.server_id;
        inner.devices = state
            .devices
            .into_iter()
            .map(|d| (d.token.clone(), d))
            .collect();
        if migrated {
            if let Err(err) = save_devices(inner) {
                log::error!("migrating mobile bridge identity: {err:#}");
            }
        }
    } else {
        inner.server_id = Uuid::new_v4().to_string();
    }
}

fn save_devices(inner: &BridgeInner) -> anyhow::Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let state = PersistedBridgeState {
        server_id: inner.server_id.clone(),
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

fn build_pair_uri(host: &str, port: u16, token: &str, server_id: &str) -> String {
    let endpoints = connection_endpoints(host, port);
    let mut uri = format!(
        "harbor://pair?v=1&host={host}&port={port}&tls=0&token={token}&sid={server_id}&auth={AUTH_VERSION}"
    );
    for endpoint in endpoints {
        let value = format!("{},{}", endpoint.kind, endpoint.url);
        uri.push_str("&endpoint=");
        uri.push_str(&utf8_percent_encode(&value, NON_ALPHANUMERIC).to_string());
    }
    uri
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
    let (port, server_id) = {
        let inner = INNER.lock();
        (inner.port, inner.server_id.clone())
    };
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener =
        TcpListener::bind(addr).with_context(|| format!("bind mobile bridge on {addr}"))?;
    listener.set_nonblocking(false)?;
    log::info!("Terminal Harbor mobile bridge listening on http://0.0.0.0:{port}");
    let _dns_sd = match start_dns_sd(&server_id, port) {
        Ok(daemon) => Some(daemon),
        Err(err) => {
            log::warn!("Terminal Harbor DNS-SD unavailable: {err:#}");
            None
        }
    };

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

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

#[derive(Clone)]
struct HmacResponseAuth {
    key: Vec<u8>,
    request_nonce: String,
}

enum RequestAuth {
    Legacy {
        client_id: String,
    },
    Hmac {
        client_id: String,
        response: HmacResponseAuth,
    },
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn canonical_request(
    method: &str,
    path: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
) -> String {
    format!(
        "TH-HMAC-V1\n{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        path,
        timestamp,
        nonce,
        sha256_hex(body)
    )
}

fn hmac_value(key: &[u8], value: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(value);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn device_key(record: &DeviceRecord) -> Option<Vec<u8>> {
    if record.auth_version >= 2 {
        URL_SAFE_NO_PAD.decode(record.token.as_bytes()).ok()
    } else {
        Some(record.token.as_bytes().to_vec())
    }
}

fn signed_response(status: u16, body: String, auth: Option<HmacResponseAuth>) -> HttpResponse {
    let mut headers = Vec::new();
    if let Some(auth) = auth {
        let canonical = format!(
            "TH-HMAC-V1-RESPONSE\n{}\n{}\n{}",
            auth.request_nonce,
            status,
            sha256_hex(body.as_bytes())
        );
        headers.push((
            "X-Harbor-Response-Signature".to_string(),
            hmac_value(&auth.key, canonical.as_bytes()),
        ));
    }
    HttpResponse {
        status,
        body,
        headers,
    }
}

fn authenticate_hmac(
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> Option<RequestAuth> {
    let client_id = headers.get("x-harbor-client-id")?.to_string();
    let timestamp = headers.get("x-harbor-timestamp")?;
    let timestamp_value = timestamp.parse::<u64>().ok()?;
    let nonce = headers.get("x-harbor-nonce")?.to_string();
    let signature = URL_SAFE_NO_PAD
        .decode(headers.get("x-harbor-signature")?.as_bytes())
        .ok()?;
    let now = now_unix();
    if now.abs_diff(timestamp_value) > AUTH_CLOCK_SKEW_SECS || nonce.len() < 16 {
        return None;
    }

    let mut inner = INNER.lock();
    let record = inner
        .devices
        .values()
        .find(|record| record.client_id == client_id)?;
    let key = device_key(record)?;
    let canonical = canonical_request(method, path, timestamp, &nonce, body);
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).ok()?;
    mac.update(canonical.as_bytes());
    if mac.verify_slice(&signature).is_err() {
        return None;
    }
    while inner
        .replay_nonces
        .front()
        .map(|(_, _, seen)| now.saturating_sub(*seen) > REPLAY_TTL_SECS)
        .unwrap_or(false)
    {
        inner.replay_nonces.pop_front();
    }
    if inner
        .replay_nonces
        .iter()
        .any(|(seen_client, seen_nonce, _)| seen_client == &client_id && seen_nonce == &nonce)
    {
        return None;
    }
    inner
        .replay_nonces
        .push_back((client_id.clone(), nonce.clone(), now));
    while inner.replay_nonces.len() > 4096 {
        inner.replay_nonces.pop_front();
    }
    Some(RequestAuth::Hmac {
        client_id,
        response: HmacResponseAuth {
            key,
            request_nonce: nonce,
        },
    })
}

fn authenticate(
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> Option<RequestAuth> {
    if let Some(auth) = authenticate_hmac(method, path, headers, body) {
        return Some(auth);
    }
    let token = headers
        .get("authorization")
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })?
        .trim();
    let inner = INNER.lock();
    let record = inner.devices.get(token)?;
    Some(RequestAuth::Legacy {
        client_id: record.client_id.clone(),
    })
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
    let mut headers = HashMap::new();
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let header_end = req_text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(n);
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

    let response = dispatch(&method, &path, &headers, &body);
    write_response(&mut stream, response)?;
    Ok(())
}

fn write_response(stream: &mut TcpStream, response: HttpResponse) -> anyhow::Result<()> {
    let status = response.status;
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let extra_headers = response
        .headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Headers: Authorization, Content-Type, X-Harbor-Client-Id, X-Harbor-Timestamp, X-Harbor-Nonce, X-Harbor-Signature\r\n\
         Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\n\
         {extra_headers}Connection: close\r\n\r\n",
        response.body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(response.body.as_bytes())?;
    Ok(())
}

fn json_error(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

fn confirmed_destructive_request(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("confirm").and_then(|confirm| confirm.as_bool()))
        == Some(true)
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key { Some(v) } else { None }
    })
}

fn host_name() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "terminal-harbor".into())
}

#[cfg(target_os = "macos")]
fn device_name() -> String {
    std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(host_name)
}

#[cfg(not(target_os = "macos"))]
fn device_name() -> String {
    host_name()
}

fn dispatch(
    method: &str,
    raw_path: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> HttpResponse {
    if method == "OPTIONS" {
        return signed_response(200, String::new(), None);
    }
    let (path, query) = match raw_path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_path, ""),
    };

    if method == "POST" && path == "/v1/pair" {
        return pair(raw_path, headers, body);
    }

    let auth = authenticate(method, raw_path, headers, body);
    if method == "GET" && path == "/v1/identity" && auth.is_none() {
        let inner = INNER.lock();
        return signed_response(
            200,
            serde_json::json!({
                "server_id": inner.server_id,
                "version": API_VERSION,
                "auth_versions": [AUTH_VERSION],
            })
            .to_string(),
            None,
        );
    }
    let auth = match auth {
        Some(auth) => auth,
        None => return signed_response(401, json_error("unauthorized"), None),
    };
    let (client_id, response_auth) = match &auth {
        RequestAuth::Legacy { client_id } => (client_id.clone(), None),
        RequestAuth::Hmac {
            client_id,
            response,
        } => (client_id.clone(), Some(response.clone())),
    };

    let finish = |status, body| signed_response(status, body, response_auth.clone());
    let workspace_error = |err: anyhow::Error| {
        let message = format!("{err:#}");
        let status = if message.contains("last tab") {
            409
        } else if message.contains("not found")
            || message.contains("invalid workspace id")
            || message.contains("invalid tab id")
        {
            404
        } else {
            500
        };
        finish(status, json_error(&message))
    };

    if method == "GET" && path == "/v1/identity" {
        let inner = INNER.lock();
        return finish(
            200,
            serde_json::json!({
                "server_id": inner.server_id,
                "version": API_VERSION,
                "auth_versions": [AUTH_VERSION],
            })
            .to_string(),
        );
    }

    if method == "GET" && path == "/v1/session" {
        let (server_id, host, port) = {
            let inner = INNER.lock();
            (inner.server_id.clone(), inner.host.clone(), inner.port)
        };
        let endpoints = connection_endpoints(&host, port);
        return finish(
            200,
            serde_json::json!({
                "ok": true,
                "version": API_VERSION,
                "device_name": device_name(),
                "host_name": host_name(),
                "server_id": server_id,
                "client_id": client_id,
                "auth_versions": [AUTH_VERSION],
                "endpoints": endpoints,
            })
            .to_string(),
        );
    }

    if method == "GET" && path == "/v1/workspaces" {
        return match run_on_main(list_workspaces_json) {
            Ok(json) => finish(200, json),
            Err(err) => finish(500, json_error(&format!("{err:#}"))),
        };
    }

    if method == "POST" && path == "/v1/workspaces" {
        #[derive(Deserialize)]
        struct CreateWorkspaceBody {
            root: Option<String>,
        }
        let parsed: CreateWorkspaceBody = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return finish(400, json_error("invalid json body")),
        };
        return match run_on_main(move || create_workspace(parsed.root)) {
            Ok(json) => finish(201, json),
            Err(err) => finish(400, json_error(&format!("{err:#}"))),
        };
    }

    if let Some(id) = path
        .strip_prefix("/v1/workspaces/")
        .and_then(|rest| rest.strip_suffix("/tabs"))
    {
        let id = id.to_string();
        if method == "GET" {
            return match run_on_main(move || list_tabs_json(&id)) {
                Ok(json) => finish(200, json),
                Err(err) => workspace_error(err),
            };
        }
        if method == "POST" {
            return match run_on_main(move || create_tab(&id)) {
                Ok(()) => finish(202, serde_json::json!({"ok": true}).to_string()),
                Err(err) => workspace_error(err),
            };
        }
    }

    if let Some((workspace_id, tab_action)) = path
        .strip_prefix("/v1/workspaces/")
        .and_then(|rest| rest.split_once("/tabs/"))
    {
        let (tab_id, activate) = match tab_action.strip_suffix("/activate") {
            Some(tab_id) => (tab_id, true),
            None => (tab_action, false),
        };
        let workspace_id = workspace_id.to_string();
        let tab_id = tab_id.to_string();
        if method == "POST" && activate {
            return match run_on_main(move || activate_tab(&workspace_id, &tab_id)) {
                Ok(()) => finish(200, serde_json::json!({"ok": true}).to_string()),
                Err(err) => workspace_error(err),
            };
        }
        if method == "DELETE" && !activate {
            if !confirmed_destructive_request(body) {
                return finish(400, json_error("explicit confirmation is required"));
            }
            return match run_on_main(move || close_tab(&workspace_id, &tab_id)) {
                Ok(()) => finish(200, serde_json::json!({"ok": true}).to_string()),
                Err(err) => workspace_error(err),
            };
        }
    }

    if method == "DELETE" {
        if let Some(id) = path.strip_prefix("/v1/workspaces/") {
            if !id.contains('/') {
                if !confirmed_destructive_request(body) {
                    return finish(400, json_error("explicit confirmation is required"));
                }
                let id = id.to_string();
                return match run_on_main(move || close_workspace(&id)) {
                    Ok(()) => finish(200, serde_json::json!({"ok": true}).to_string()),
                    Err(err) => workspace_error(err),
                };
            }
        }
    }

    if let Some(id) = path
        .strip_prefix("/v1/workspaces/")
        .and_then(|rest| rest.strip_suffix("/activate"))
    {
        if method == "POST" {
            let id = id.to_string();
            return match run_on_main(move || activate_workspace(&id)) {
                Ok(()) => finish(200, serde_json::json!({"ok": true}).to_string()),
                Err(err) => {
                    let msg = format!("{err:#}");
                    if msg.contains("not found") {
                        finish(404, json_error(&msg))
                    } else {
                        finish(500, json_error(&msg))
                    }
                }
            };
        }
    }

    if let Some(id) = path
        .strip_prefix("/v1/workspaces/")
        .and_then(|rest| rest.strip_suffix("/key"))
    {
        if method == "POST" {
            #[derive(Deserialize)]
            struct KeyBody {
                key: String,
            }
            let parsed: KeyBody = match serde_json::from_slice(body) {
                Ok(value) => value,
                Err(_) => return finish(400, json_error("invalid json body")),
            };
            let (key, mods) = match terminal_key_code(&parsed.key) {
                Some(mapping) => mapping,
                None => return finish(400, json_error("unsupported terminal key")),
            };
            let id = id.to_string();
            return match run_on_main(move || send_terminal_key(&id, key, mods)) {
                Ok(()) => finish(200, serde_json::json!({"ok": true}).to_string()),
                Err(err) => workspace_error(err),
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
                Err(_) => return finish(400, json_error("invalid json body")),
            };
            let id = id.to_string();
            return match run_on_main(move || send_instruction(&id, &parsed.text, parsed.submit)) {
                Ok(()) => finish(200, serde_json::json!({"ok": true}).to_string()),
                Err(err) => {
                    let msg = format!("{err:#}");
                    if msg.contains("not found") {
                        finish(404, json_error(&msg))
                    } else {
                        finish(500, json_error(&msg))
                    }
                }
            };
        }
    }

    if let Some(id) = path
        .strip_prefix("/v1/workspaces/")
        .and_then(|rest| rest.strip_suffix("/speech/hints"))
    {
        if method == "GET" {
            let id = id.to_string();
            return match run_on_main(move || speech_hint_context(&id)) {
                Ok(context) => finish(200, speech_hints_json(context)),
                Err(err) => workspace_error(err),
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
                .clamp(1, 500);
            let id = id.to_string();
            return match run_on_main(move || screen_text(&id, lines)) {
                Ok(json) => finish(200, json),
                Err(err) => {
                    let msg = format!("{err:#}");
                    if msg.contains("not found") {
                        finish(404, json_error(&msg))
                    } else {
                        finish(500, json_error(&msg))
                    }
                }
            };
        }
    }

    if method != "GET" && method != "POST" && method != "DELETE" {
        return finish(405, json_error("method not allowed"));
    }
    finish(404, json_error("not found"))
}

fn derive_device_key(pair_token: &str, server_id: &str, client_id: &str, nonce: &[u8]) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(Some(server_id.as_bytes()), pair_token.as_bytes());
    let mut info = b"terminal-harbor/device/v2\0".to_vec();
    info.extend_from_slice(client_id.as_bytes());
    info.push(0);
    info.extend_from_slice(nonce);
    let mut key = vec![0u8; 32];
    hk.expand(&info, &mut key)
        .expect("valid HKDF output length");
    key
}

fn pair(raw_path: &str, headers: &HashMap<String, String>, body: &[u8]) -> HttpResponse {
    #[derive(Deserialize)]
    struct PairBody {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        device_name: Option<String>,
        #[serde(default)]
        auth_version: Option<String>,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        client_nonce: Option<String>,
    }
    let parsed: PairBody = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return signed_response(400, json_error("invalid json body"), None),
    };

    let mut inner = INNER.lock();
    let offer = match inner.pair_offer.clone() {
        Some(o) => o,
        None => return signed_response(401, json_error("no active pairing offer"), None),
    };
    if Instant::now() > offer.expires_at {
        inner.pair_offer = None;
        return signed_response(401, json_error("pairing token expired"), None);
    }

    let hmac_pairing = parsed.auth_version.as_deref() == Some(AUTH_VERSION);
    let (client_id, stored_secret, response_key, auth_version) = if hmac_pairing {
        let client_id = match parsed.client_id.filter(|id| Uuid::parse_str(id).is_ok()) {
            Some(id) => id,
            None => return signed_response(400, json_error("invalid client_id"), None),
        };
        let nonce = match parsed
            .client_nonce
            .as_deref()
            .and_then(|value| URL_SAFE_NO_PAD.decode(value.as_bytes()).ok())
            .filter(|value| value.len() == 32)
        {
            Some(value) => value,
            None => return signed_response(400, json_error("invalid client_nonce"), None),
        };
        let timestamp = match headers.get("x-harbor-timestamp") {
            Some(value) => value,
            None => return signed_response(401, json_error("missing pairing signature"), None),
        };
        let request_nonce = match headers.get("x-harbor-nonce") {
            Some(value) if value.len() >= 16 => value,
            _ => return signed_response(401, json_error("invalid pairing nonce"), None),
        };
        let timestamp_value = match timestamp.parse::<u64>() {
            Ok(value) if now_unix().abs_diff(value) <= AUTH_CLOCK_SKEW_SECS => value,
            _ => return signed_response(401, json_error("expired pairing signature"), None),
        };
        let _ = timestamp_value;
        let signature = match headers
            .get("x-harbor-signature")
            .and_then(|value| URL_SAFE_NO_PAD.decode(value.as_bytes()).ok())
        {
            Some(value) => value,
            None => return signed_response(401, json_error("invalid pairing signature"), None),
        };
        let canonical = canonical_request("POST", raw_path, timestamp, request_nonce, body);
        let mut mac = Hmac::<Sha256>::new_from_slice(offer.token.as_bytes()).unwrap();
        mac.update(canonical.as_bytes());
        if mac.verify_slice(&signature).is_err() {
            return signed_response(401, json_error("invalid pairing signature"), None);
        }
        let key = derive_device_key(&offer.token, &inner.server_id, &client_id, &nonce);
        (client_id, URL_SAFE_NO_PAD.encode(&key), Some(key), 2)
    } else {
        if parsed.token.as_deref() != Some(offer.token.as_str()) {
            return signed_response(401, json_error("invalid pairing token"), None);
        }
        (new_client_id(), random_token(), None, 1)
    };

    inner.pair_offer = None;
    let record = DeviceRecord {
        client_id: client_id.clone(),
        token: stored_secret.clone(),
        name: parsed.device_name.unwrap_or_else(|| "Mobile".to_string()),
        created_at: now_unix(),
        auth_version,
    };
    inner.devices.insert(stored_secret.clone(), record);
    if let Err(err) = save_devices(&inner) {
        log::error!("saving mobile devices: {err:#}");
    }

    let host = inner.host.clone();
    let port = inner.port;
    let server_id = inner.server_id.clone();
    let base_url = format!("http://{host}:{port}");
    let response_nonce = headers.get("x-harbor-nonce").cloned().unwrap_or_default();
    let response_auth = response_key.map(|key| HmacResponseAuth {
        key,
        request_nonce: response_nonce,
    });
    drop(inner);
    let endpoints = connection_endpoints(&host, port);
    signed_response(
        200,
        serde_json::json!({
            "device_token": if auth_version == 1 { Some(stored_secret) } else { None },
            "base_url": base_url,
            "server_id": server_id,
            "client_id": client_id,
            "auth_versions": [AUTH_VERSION],
            "endpoints": endpoints,
        })
        .to_string(),
        response_auth,
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

fn create_workspace(requested_root: Option<String>) -> anyhow::Result<String> {
    let root = match requested_root
        .as_deref()
        .map(str::trim)
        .filter(|root| !root.is_empty())
    {
        Some(root) => PathBuf::from(root),
        None => harbor_workspace::rows()
            .into_iter()
            .find(|row| row.selected)
            .and_then(|row| row.workspace.root)
            .or_else(dirs_next::home_dir)
            .ok_or_else(|| anyhow!("no default workspace directory is available"))?,
    };
    if !root.is_absolute() {
        anyhow::bail!("workspace directory must be an absolute path");
    }
    let root = fs::canonicalize(&root)
        .with_context(|| format!("workspace directory is unavailable: {}", root.display()))?;
    if !root.is_dir() {
        anyhow::bail!("workspace directory is not a directory: {}", root.display());
    }

    let workspace = harbor_workspace::create_from_path(root);
    activate_workspace(&workspace.id.to_string())?;
    Ok(serde_json::json!({
        "id": workspace.id,
        "name": workspace.name,
        "root": workspace.root.as_ref().map(|path| path.display().to_string()),
        "mux_workspace": workspace.mux_workspace,
        "activity": "idle",
        "process": null,
        "message": null,
        "selected": true,
    })
    .to_string())
}

fn list_tabs_json(workspace_id: &str) -> anyhow::Result<String> {
    let workspace = find_workspace(workspace_id)?;
    let mux = Mux::get();
    let mut tabs = Vec::new();
    let mut index = 0usize;
    for window_id in mux.iter_windows_in_workspace(&workspace.mux_workspace) {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        let active_id = window.get_active().map(|tab| tab.tab_id());
        for tab in window.iter() {
            index += 1;
            let title = match tab.get_title().trim() {
                "" => tab
                    .get_active_pane()
                    .map(|pane| pane.get_title())
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| format!("Tab {index}")),
                title => title.to_string(),
            };
            tabs.push(serde_json::json!({
                "id": tab.tab_id().to_string(),
                "title": title,
                "selected": active_id == Some(tab.tab_id()),
                "pane_count": tab.iter_panes().len(),
            }));
        }
    }
    Ok(serde_json::json!({"tabs": tabs}).to_string())
}

fn find_workspace_tab(
    workspace_id: &str,
    tab_id: &str,
) -> anyhow::Result<(harbor_workspace::HarborWorkspace, usize, usize)> {
    let workspace = find_workspace(workspace_id)?;
    let tab_id = tab_id
        .parse::<usize>()
        .map_err(|_| anyhow!("invalid tab id"))?;
    let mux = Mux::get();
    for window_id in mux.iter_windows_in_workspace(&workspace.mux_workspace) {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        if window.iter().any(|tab| tab.tab_id() == tab_id) {
            return Ok((workspace, window_id, tab_id));
        }
    }
    Err(anyhow!("tab not found"))
}

fn create_tab(workspace_id: &str) -> anyhow::Result<()> {
    let workspace = find_workspace(workspace_id)?;
    let mux = Mux::get();
    let window_id = mux
        .iter_windows_in_workspace(&workspace.mux_workspace)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("workspace has no active window"))?;
    let tab = mux
        .get_active_tab_for_window(window_id)
        .ok_or_else(|| anyhow!("workspace has no active tab"))?;
    let pane_id = tab.get_active_pane().map(|pane| pane.pane_id());
    let size = tab.get_size();
    let workspace_name = workspace.mux_workspace;
    promise::spawn::spawn(async move {
        if let Err(err) = mux
            .spawn_tab_or_window(
                Some(window_id),
                SpawnTabDomain::CurrentPaneDomain,
                None,
                None,
                size,
                pane_id,
                workspace_name,
                None,
            )
            .await
        {
            log::error!("mobile create tab: {err:#}");
        }
    })
    .detach();
    Ok(())
}

fn activate_tab(workspace_id: &str, tab_id: &str) -> anyhow::Result<()> {
    let (workspace, window_id, tab_id) = find_workspace_tab(workspace_id, tab_id)?;
    let mux = Mux::get();
    let mut window = mux
        .get_window_mut(window_id)
        .ok_or_else(|| anyhow!("workspace window not found"))?;
    let tab_index = window
        .idx_by_id(tab_id)
        .ok_or_else(|| anyhow!("tab not found"))?;
    window.save_and_then_set_active(tab_index);
    drop(window);
    let frontend =
        crate::frontend::try_front_end().ok_or_else(|| anyhow!("frontend unavailable"))?;
    if frontend.active_workspace() != workspace.mux_workspace {
        frontend.switch_workspace(&workspace.mux_workspace);
    }
    Ok(())
}

fn close_tab(workspace_id: &str, tab_id: &str) -> anyhow::Result<()> {
    let (workspace, _, tab_id) = find_workspace_tab(workspace_id, tab_id)?;
    let mux = Mux::get();
    let tab_count = mux
        .iter_windows_in_workspace(&workspace.mux_workspace)
        .into_iter()
        .filter_map(|window_id| mux.get_window(window_id))
        .map(|window| window.len())
        .sum::<usize>();
    if tab_count <= 1 {
        anyhow::bail!("cannot close the last tab; close the workspace instead");
    }
    mux.remove_tab(tab_id);
    Ok(())
}

fn close_workspace(workspace_id: &str) -> anyhow::Result<()> {
    let workspace = find_workspace(workspace_id)?;
    let mux = Mux::get();
    let tab_ids = mux
        .iter_windows_in_workspace(&workspace.mux_workspace)
        .into_iter()
        .filter_map(|window_id| mux.get_window(window_id))
        .flat_map(|window| window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let was_active = crate::frontend::try_front_end()
        .map(|frontend| frontend.active_workspace() == workspace.mux_workspace)
        .unwrap_or(false);
    let next = harbor_workspace::remove_workspace(&workspace.mux_workspace);
    if was_active {
        if let Some(next) = next {
            activate_workspace(&next.id.to_string())?;
        }
    }
    for tab_id in tab_ids {
        mux.remove_tab(tab_id);
    }
    Ok(())
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
        // Let the terminal encode Enter for its current keyboard mode. Writing
        // a fixed CR bypasses CSI-u/Kitty keyboard encoding and can therefore
        // be ignored by applications that enabled an extended key protocol.
        pane.key_down(KeyCode::Enter, KeyModifiers::NONE)?;
    }
    Ok(())
}

fn terminal_key_code(key: &str) -> Option<(KeyCode, KeyModifiers)> {
    match key {
        "up" => Some((KeyCode::UpArrow, KeyModifiers::NONE)),
        "down" => Some((KeyCode::DownArrow, KeyModifiers::NONE)),
        // KeyCode has no Escape variant; it is spelled as the raw control byte
        // so the terminal can encode it for its active keyboard protocol.
        "escape" => Some((KeyCode::Char('\u{1b}'), KeyModifiers::NONE)),
        // Send the letter with CTRL held rather than a pre-encoded 0x03 so the
        // terminal derives the right sequence under CSI-u/Kitty keyboards too.
        "ctrl-c" => Some((KeyCode::Char('c'), KeyModifiers::CTRL)),
        _ => None,
    }
}

fn send_terminal_key(id: &str, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
    let workspace = find_workspace(id)?;
    workspace_active_pane(&workspace)?.key_down(key, mods)?;
    Ok(())
}

#[derive(Debug)]
struct SpeechHintContext {
    workspace_name: String,
    root: Option<PathBuf>,
    cwd: Option<PathBuf>,
    agent_name: Option<String>,
    conversation: String,
}

fn speech_hint_context(id: &str) -> anyhow::Result<SpeechHintContext> {
    let workspace = find_workspace(id)?;
    let pane = workspace_active_pane(&workspace)?;
    let vars = pane.copy_user_vars();
    let cwd = pane
        .get_current_working_dir(CachePolicy::AllowStale)
        .and_then(|url| url.to_file_path().ok());
    let agent_name = vars.get("TH_AGENT_NAME").cloned().or_else(|| {
        pane.get_foreground_process_name(CachePolicy::AllowStale)
            .and_then(|name| {
                Path::new(&name)
                    .file_name()
                    .map(|part| part.to_string_lossy().into_owned())
            })
    });
    Ok(SpeechHintContext {
        workspace_name: workspace.name,
        root: workspace.root,
        cwd,
        agent_name,
        conversation: pane_text(&pane, 160),
    })
}

fn speech_hints_json(context: SpeechHintContext) -> String {
    let mut scores = HashMap::<String, i32>::new();
    add_speech_candidate(&mut scores, &context.workspace_name, 100, false);
    if let Some(agent) = &context.agent_name {
        add_speech_candidate(&mut scores, agent, 110, false);
    }
    if let Some(root) = &context.root {
        add_path_components(&mut scores, root, 85);
        collect_directory_candidates(&mut scores, root);
    }
    if let Some(cwd) = &context.cwd {
        add_path_components(&mut scores, cwd, 95);
    }
    collect_conversation_candidates(&mut scores, &context.conversation);

    let mut ranked: Vec<_> = scores.into_iter().collect();
    ranked.sort_by(|(left_term, left_score), (right_term, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_term.to_lowercase().cmp(&right_term.to_lowercase()))
    });

    let mut hints = Vec::new();
    let mut seen = HashSet::new();
    for (term, _) in ranked {
        push_unique_hint(&mut hints, &mut seen, term.clone());
        if let Some(spoken) = spoken_form(&term) {
            push_unique_hint(&mut hints, &mut seen, spoken);
        }
        if let Some(alias) = fixed_spoken_alias(&term) {
            push_unique_hint(&mut hints, &mut seen, alias.to_string());
        }
        if hints.len() >= 96 {
            hints.truncate(96);
            break;
        }
    }

    serde_json::json!({
        "hints": hints,
        "source": "workspace_context_v1",
    })
    .to_string()
}

fn push_unique_hint(hints: &mut Vec<String>, seen: &mut HashSet<String>, hint: String) {
    let normalized = hint.trim().to_string();
    if normalized.len() < 2 || normalized.len() > 64 {
        return;
    }
    if seen.insert(normalized.to_lowercase()) {
        hints.push(normalized);
    }
}

fn add_path_components(scores: &mut HashMap<String, i32>, path: &Path, score: i32) {
    for component in path.components().rev().take(3) {
        add_speech_candidate(
            scores,
            &component.as_os_str().to_string_lossy(),
            score,
            false,
        );
    }
}

fn include_directory_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !name.starts_with('.')
        && !matches!(
            name.as_ref(),
            "build" | "node_modules" | "target" | "vendor" | "Pods" | "ephemeral" | "DerivedData"
        )
}

fn collect_directory_candidates(scores: &mut HashMap<String, i32>, root: &Path) {
    for entry in WalkDir::new(root)
        .max_depth(3)
        .follow_links(false)
        .into_iter()
        .filter_entry(include_directory_entry)
        .filter_map(Result::ok)
        .skip(1)
        .take(400)
    {
        let score = 55 - (entry.depth() as i32 * 6);
        add_speech_candidate(scores, &entry.file_name().to_string_lossy(), score, false);
    }
}

fn collect_conversation_candidates(scores: &mut HashMap<String, i32>, conversation: &str) {
    for (line_index, line) in conversation.lines().rev().take(160).enumerate() {
        let recency = 60 - (line_index as i32 / 8).min(20);
        for captures in QUOTED_SPEECH_TERM_RE.captures_iter(line) {
            if let Some(term) = captures.get(1) {
                add_speech_candidate(scores, term.as_str(), recency + 15, true);
            }
        }
        for term in SPEECH_TERM_RE.find_iter(line) {
            add_speech_candidate(scores, term.as_str(), recency, true);
        }
    }
}

fn add_speech_candidate(
    scores: &mut HashMap<String, i32>,
    raw: &str,
    score: i32,
    require_technical_shape: bool,
) {
    let raw = raw
        .trim_matches(|ch: char| ch.is_whitespace() || ",:;()[]{}<>!?".contains(ch))
        .trim();
    // Paths can reveal private topology and are poor spoken phrases. Keep only
    // the final component; the directory walk adds nearby names independently.
    let term = raw
        .rsplit('/')
        .find(|component| !component.is_empty())
        .unwrap_or(raw);
    if term.len() < 2
        || term.len() > 64
        || is_sensitive_speech_candidate(term)
        || (require_technical_shape && !has_technical_shape(term))
    {
        return;
    }
    *scores.entry(term.to_string()).or_default() += score;
}

fn has_technical_shape(term: &str) -> bool {
    let mut saw_lower = false;
    let mut saw_upper_after_lower = false;
    for ch in term.chars() {
        if ch.is_ascii_lowercase() {
            saw_lower = true;
        } else if saw_lower && ch.is_ascii_uppercase() {
            saw_upper_after_lower = true;
        }
    }
    saw_upper_after_lower
        || term.chars().any(|ch| ch.is_ascii_digit())
        || term.contains(['_', '-', '.', '/', '+', '@'])
        || matches!(
            term.to_ascii_lowercase().as_str(),
            "codex"
                | "claude"
                | "flutter"
                | "dart"
                | "rust"
                | "cargo"
                | "git"
                | "adb"
                | "tailscale"
                | "openrouter"
        )
}

fn is_sensitive_speech_candidate(term: &str) -> bool {
    let lower = term.to_ascii_lowercase();
    if lower.contains("sk-")
        || lower.starts_with("ghp_")
        || lower.contains("bearer")
        || lower.contains("harbor://pair")
        || term.parse::<std::net::IpAddr>().is_ok()
    {
        return true;
    }
    let compact: String = term
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect();
    if compact.len() >= 24 && compact.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return true;
    }
    (compact.len() >= 32
        && compact.chars().any(|ch| ch.is_ascii_digit())
        && compact.chars().any(|ch| ch.is_ascii_alphabetic()))
        || compact.len() >= 40
}

fn spoken_form(term: &str) -> Option<String> {
    let mut result = String::new();
    let chars: Vec<_> = term.chars().collect();
    for (index, ch) in chars.iter().copied().enumerate() {
        let separator = matches!(ch, '_' | '-' | '.' | '/' | '+');
        let camel_boundary =
            index > 0 && ch.is_ascii_uppercase() && chars[index - 1].is_ascii_lowercase();
        if separator || camel_boundary {
            if !result.ends_with(' ') && !result.is_empty() {
                result.push(' ');
            }
            if separator {
                continue;
            }
        }
        result.push(ch);
    }
    let spoken = result.split_whitespace().collect::<Vec<_>>().join(" ");
    (spoken != term && spoken.len() >= 2).then_some(spoken)
}

fn fixed_spoken_alias(term: &str) -> Option<&'static str> {
    match term.to_ascii_lowercase().as_str() {
        "codex" => Some("コーデックス"),
        "flutter" => Some("フラッター"),
        "tailscale" => Some("テールスケール"),
        "openrouter" => Some("オープンルーター"),
        "terminal-harbor" | "terminal harbor" => Some("ターミナルハーバー"),
        _ => None,
    }
}

/// Join already right-trimmed terminal rows, dropping the blank rows at both
/// ends. Full-screen programs repaint by clearing rows, so a requested window
/// can begin with a long run of empty rows; mirroring those verbatim makes the
/// mobile pane look blank until the reader scrolls past them.
fn join_pane_rows(rows: &[String]) -> String {
    let start = rows.iter().position(|row| !row.is_empty());
    let Some(start) = start else {
        return String::new();
    };
    let end = rows
        .iter()
        .rposition(|row| !row.is_empty())
        .map_or(start, |last| last + 1);
    rows[start..end].join("\n")
}

fn pane_text(pane: &std::sync::Arc<dyn mux::pane::Pane>, nlines: usize) -> String {
    let dims = pane.get_dimensions();
    let bottom_row = dims.physical_top + dims.viewport_rows as isize;
    let top_row = bottom_row.saturating_sub(nlines as isize);
    let (_first_row, lines) = pane.get_lines(top_row..bottom_row);

    let rows: Vec<String> = lines
        .iter()
        .map(|line| line.as_str().trim_end().to_string())
        .collect();
    join_pane_rows(&rows)
}

/// Render the last `nlines` lines of the workspace's active pane as plain
/// text so the mobile app can mirror the terminal screen.
fn screen_text(id: &str, nlines: usize) -> anyhow::Result<String> {
    let workspace = find_workspace(id)?;
    let pane = workspace_active_pane(&workspace)?;

    let text = pane_text(&pane, nlines);

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
    let uri = build_pair_uri(&inner.host, inner.port, &offer.token, &inner.server_id);

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
    fn hmac_canonical_request_binds_method_target_and_body() {
        let canonical = canonical_request(
            "post",
            "/v1/workspaces/one/instruction",
            "1700000000",
            "abcdefghijklmnop",
            br#"{"text":"continue"}"#,
        );
        assert_eq!(
            canonical,
            format!(
                "TH-HMAC-V1\nPOST\n/v1/workspaces/one/instruction\n1700000000\nabcdefghijklmnop\n{}",
                sha256_hex(br#"{"text":"continue"}"#)
            )
        );
        assert_ne!(
            hmac_value(b"secret", canonical.as_bytes()),
            hmac_value(b"secret", canonical.replace("POST", "GET").as_bytes())
        );
    }

    #[test]
    fn device_key_derivation_is_stable_and_context_bound() {
        let nonce = [7u8; 32];
        let first = derive_device_key("pair-token", "server-one", "client-one", &nonce);
        let again = derive_device_key("pair-token", "server-one", "client-one", &nonce);
        let other_server = derive_device_key("pair-token", "server-two", "client-one", &nonce);
        assert_eq!(first.len(), 32);
        assert_eq!(first, again);
        assert_ne!(first, other_server);
        let hex = first
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            hex,
            "313c4fbd5eb8f46a4a6a25427b18281b3ad8b351aaad1e650f561b37aaa6fdff"
        );
    }

    #[test]
    fn session_names_are_non_empty() {
        assert!(!device_name().is_empty());
        assert!(!host_name().is_empty());
    }

    #[test]
    fn workspace_creation_rejects_relative_directories() {
        let error = create_workspace(Some("relative/project".to_string())).unwrap_err();
        assert!(error.to_string().contains("absolute path"));
    }

    #[test]
    fn destructive_requests_require_literal_confirmation() {
        assert!(confirmed_destructive_request(br#"{"confirm":true}"#));
        assert!(!confirmed_destructive_request(br#"{"confirm":false}"#));
        assert!(!confirmed_destructive_request(br#"{}"#));
        assert!(!confirmed_destructive_request(b"not json"));
    }

    #[test]
    fn pair_qr_png_is_written_and_decodable_shape() {
        let uri = build_pair_uri(
            "192.168.1.20",
            DEFAULT_PORT,
            "deadbeefcafe",
            "11111111-1111-1111-1111-111111111111",
        );
        let path = write_pair_qr_png(&uri).expect("write qr png");
        assert!(uri.contains("sid=11111111-1111-1111-1111-111111111111"));
        assert!(uri.contains("auth=hmac-sha256-v1"));
        assert!(uri.contains("endpoint="));
        let bytes = std::fs::read(&path).expect("read qr png");
        // PNG magic
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let img = image::load_from_memory(&bytes).expect("decode qr png");
        assert!(img.width() >= 360);
        assert_eq!(img.width(), img.height());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mirrored_rows_drop_blank_padding_at_both_ends() {
        let rows: Vec<String> = ["", "", "prompt", "", "output", "", ""]
            .iter()
            .map(|row| row.to_string())
            .collect();
        // Interior blank rows are real terminal content and must survive.
        assert_eq!(join_pane_rows(&rows), "prompt\n\noutput");

        let blank: Vec<String> = vec![String::new(); 4];
        assert_eq!(join_pane_rows(&blank), "");
        assert_eq!(join_pane_rows(&[]), "");
    }

    #[test]
    fn terminal_navigation_keys_are_explicitly_limited() {
        assert!(matches!(
            terminal_key_code("up"),
            Some((KeyCode::UpArrow, KeyModifiers::NONE))
        ));
        assert!(matches!(
            terminal_key_code("down"),
            Some((KeyCode::DownArrow, KeyModifiers::NONE))
        ));
        assert!(matches!(
            terminal_key_code("escape"),
            Some((KeyCode::Char('\u{1b}'), KeyModifiers::NONE))
        ));
        assert!(matches!(
            terminal_key_code("ctrl-c"),
            Some((KeyCode::Char('c'), KeyModifiers::CTRL))
        ));
        assert!(terminal_key_code("enter").is_none());
        assert!(terminal_key_code("ctrl-d").is_none());
    }

    #[test]
    fn speech_candidates_extract_recent_technical_terms_and_drop_secrets() {
        let mut scores = HashMap::new();
        collect_conversation_candidates(
            &mut scores,
            "Use HarborApiClient and client_test.dart\n\
             Update /Users/private/project/lib/client.dart\n\
             Run flutter analyze after sk-secret1234567890",
        );
        assert!(scores.contains_key("HarborApiClient"));
        assert!(scores.contains_key("client_test.dart"));
        assert!(scores.contains_key("flutter"));
        assert!(scores.contains_key("client.dart"));
        assert!(!scores.keys().any(|term| term.contains("/Users/")));
        assert!(!scores.keys().any(|term| term.starts_with("sk-")));
        assert!(!scores.contains_key("after"));
        assert!(is_sensitive_speech_candidate(
            "AbCdEfGhIjKlMnOpQrStUvWxYz_1234567890"
        ));
    }

    #[test]
    fn speech_candidates_create_spoken_identifier_forms() {
        assert_eq!(
            spoken_form("HarborApiClient").as_deref(),
            Some("Harbor Api Client")
        );
        assert_eq!(
            spoken_form("client_test.dart").as_deref(),
            Some("client test dart")
        );
        assert_eq!(fixed_spoken_alias("Codex"), Some("コーデックス"));
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
