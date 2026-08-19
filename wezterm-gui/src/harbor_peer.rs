//! Desktop HMAC client for another Terminal Harbor instance.
//!
//! Reuses the mobile bridge contract. Endpoint order matches the Flutter
//! companion: Tailscale HTTPS, Tailscale direct, then LAN after confirmation.

use crate::harbor_mobile::{
    self, canonical_request, derive_device_key, hmac_value, sha256_hex, AUTH_VERSION,
};
use crate::harbor_workspace::WorkspaceActivity;
use crate::termwindow::TermWindowNotif;
use anyhow::{anyhow, Context};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use http_req::request::{Method, Request};
use http_req::uri::Uri;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, thread};
use uuid::Uuid;
use window::WindowOps;

const PAIR_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_secs(3);

static POLLER_STARTED: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    static ref INNER: Mutex<Inner> = Mutex::new(Inner::load());
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerEndpoint {
    pub kind: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedPeer {
    server_id: String,
    client_id: String,
    secret: String,
    #[serde(default)]
    device_name: String,
    #[serde(default)]
    host_name: String,
    #[serde(default)]
    endpoints: Vec<PeerEndpoint>,
    #[serde(default)]
    last_successful_url: Option<String>,
    #[serde(default)]
    allow_lan_fallback: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct PersistedPeers {
    #[serde(default)]
    peers: Vec<PersistedPeer>,
}

#[derive(Clone, Debug)]
pub struct RemoteWorkspace {
    pub id: String,
    pub directory: String,
    pub activity: WorkspaceActivity,
    pub selected: bool,
}

#[derive(Clone, Debug)]
pub struct PeerView {
    pub server_id: String,
    pub label: String,
    pub error: Option<String>,
    pub needs_lan_confirm: bool,
    pub workspaces: Vec<RemoteWorkspace>,
}

#[derive(Clone, Debug)]
struct LivePeer {
    persisted: PersistedPeer,
    workspaces: Vec<RemoteWorkspace>,
    error: Option<String>,
    needs_lan_confirm: bool,
    last_refresh: Option<Instant>,
}

struct Inner {
    peers: BTreeMap<String, LivePeer>,
    pair_status: Option<String>,
}

impl Inner {
    fn load() -> Self {
        let mut peers = BTreeMap::new();
        if let Ok(bytes) = fs::read(peers_path()) {
            if let Ok(stored) = serde_json::from_slice::<PersistedPeers>(&bytes) {
                for peer in stored.peers {
                    peers.insert(
                        peer.server_id.clone(),
                        LivePeer {
                            persisted: peer,
                            workspaces: Vec::new(),
                            error: None,
                            needs_lan_confirm: false,
                            last_refresh: None,
                        },
                    );
                }
            }
        }
        Self {
            peers,
            pair_status: None,
        }
    }
}

fn peers_path() -> std::path::PathBuf {
    harbor_mobile::state_dir().join("paired-desktops.json")
}

fn save_peers(inner: &Inner) -> anyhow::Result<()> {
    let dir = harbor_mobile::state_dir();
    fs::create_dir_all(&dir)?;
    let stored = PersistedPeers {
        peers: inner
            .peers
            .values()
            .map(|peer| peer.persisted.clone())
            .collect(),
    };
    let temp = dir.join("paired-desktops.json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(&stored)?)?;
    fs::rename(temp, peers_path())?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn endpoint_rank(kind: &str) -> u8 {
    match kind {
        "tailscale_https" => 0,
        "tailscale_direct" => 1,
        "lan" => 2,
        _ => 3,
    }
}

fn is_lan_kind(kind: &str) -> bool {
    kind == "lan" || kind == "legacy"
}

fn sort_endpoints(endpoints: &mut [PeerEndpoint], last_successful: Option<&str>) {
    endpoints.sort_by(|a, b| {
        endpoint_rank(&a.kind)
            .cmp(&endpoint_rank(&b.kind))
            .then_with(|| match last_successful {
                Some(url) if a.url == url => std::cmp::Ordering::Less,
                Some(url) if b.url == url => std::cmp::Ordering::Greater,
                _ => a.url.cmp(&b.url),
            })
    });
}

fn ordered_endpoints(
    endpoints: &[PeerEndpoint],
    last_successful: Option<&str>,
) -> Vec<PeerEndpoint> {
    let mut unique = Vec::new();
    for endpoint in endpoints {
        if unique
            .iter()
            .any(|item: &PeerEndpoint| item.url == endpoint.url)
        {
            continue;
        }
        unique.push(endpoint.clone());
    }
    sort_endpoints(&mut unique, last_successful);
    unique
}

#[derive(Debug, Clone)]
struct PairPayload {
    host: String,
    port: u16,
    tls: bool,
    token: String,
    server_id: String,
    _auth: String,
    endpoints: Vec<PeerEndpoint>,
}

impl PairPayload {
    fn parse(raw: &str) -> anyhow::Result<Self> {
        let trimmed = raw.trim();
        let uri = url::Url::parse(trimmed).context("invalid pair URI")?;
        if uri.scheme() != "harbor" || uri.host_str() != Some("pair") {
            anyhow::bail!("not a Terminal Harbor pair URI");
        }
        let mut version = None;
        let mut host = None;
        let mut port = None;
        let mut tls = false;
        let mut token = None;
        let mut server_id = None;
        let mut auth = None;
        let mut endpoints = Vec::new();
        for (key, value) in uri.query_pairs() {
            match key.as_ref() {
                "v" => version = value.parse::<u32>().ok(),
                "host" => host = Some(value.into_owned()),
                "port" => port = value.parse::<u16>().ok(),
                "tls" => tls = value == "1" || value.eq_ignore_ascii_case("true"),
                "token" => token = Some(value.into_owned()),
                "sid" => server_id = Some(value.into_owned()),
                "auth" => auth = Some(value.into_owned()),
                "endpoint" => {
                    if let Some((kind, url)) = value.split_once(',') {
                        if looks_like_base_url(url) {
                            endpoints.push(PeerEndpoint {
                                kind: kind.to_string(),
                                url: url.trim_end_matches('/').to_string(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        if version != Some(1) {
            anyhow::bail!("unsupported pair URI version");
        }
        let host = host
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("pair URI is missing a host"))?;
        let port = port
            .filter(|value| *value > 0)
            .ok_or_else(|| anyhow!("pair URI is missing a port"))?;
        let token = token
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("pair URI is missing a token"))?;
        let server_id = server_id
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("pair URI is missing a server id"))?;
        let auth = auth.unwrap_or_default();
        if auth != AUTH_VERSION {
            anyhow::bail!("pair URI does not use HMAC authentication");
        }
        Ok(Self {
            host,
            port,
            tls,
            token,
            server_id,
            _auth: auth,
            endpoints,
        })
    }

    fn legacy_url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    fn pairing_endpoints(&self) -> Vec<PeerEndpoint> {
        let mut endpoints = self.endpoints.clone();
        let legacy = self.legacy_url();
        if !endpoints.iter().any(|endpoint| endpoint.url == legacy) {
            endpoints.push(PeerEndpoint {
                kind: "legacy".to_string(),
                url: legacy,
            });
        }
        ordered_endpoints(&endpoints, None)
    }
}

fn looks_like_base_url(raw: &str) -> bool {
    let Ok(uri) = url::Url::parse(raw) else {
        return false;
    };
    (uri.scheme() == "http" || uri.scheme() == "https")
        && uri.host_str().map(|host| !host.is_empty()).unwrap_or(false)
        && uri.username().is_empty()
        && uri.password().is_none()
        && (uri.path().is_empty() || uri.path() == "/")
        && uri.query().is_none()
        && uri.fragment().is_none()
}

struct SignedResponse {
    status: u16,
    body: Vec<u8>,
}

fn header_value(headers: &http_req::response::Headers, name: &str) -> Option<String> {
    headers.get(name).cloned()
}

fn signed_request(
    method: Method,
    base_url: &str,
    path: &str,
    body: &[u8],
    signing_key: &[u8],
    response_key: Option<&[u8]>,
    client_id: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<SignedResponse> {
    let timestamp = now_unix().to_string();
    let nonce = URL_SAFE_NO_PAD.encode(Uuid::new_v4().as_bytes());
    let method_name = match method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::DELETE => "DELETE",
        _ => "GET",
    };
    let canonical = canonical_request(method_name, path, &timestamp, &nonce, body);
    let signature = hmac_value(signing_key, canonical.as_bytes());
    let url = format!("{}{path}", base_url.trim_end_matches('/'));
    let uri = Uri::try_from(url.as_str()).map_err(|err| anyhow!("invalid URL: {err}"))?;
    let mut request = Request::new(&uri);
    request
        .method(method)
        .timeout(timeout)
        .header("Accept", "application/json")
        .header("X-Harbor-Timestamp", &timestamp)
        .header("X-Harbor-Nonce", &nonce)
        .header("X-Harbor-Signature", &signature);
    if let Some(client_id) = client_id {
        request.header("X-Harbor-Client-Id", client_id);
    }
    if !body.is_empty() {
        request
            .header("Content-Type", "application/json")
            .body(body);
    }
    let mut writer = Vec::new();
    let response = request
        .send(&mut writer)
        .map_err(|err| anyhow!("request failed: {err}"))?;
    let status = u16::from(response.status_code());
    let expected_key = response_key.unwrap_or(signing_key);
    let response_signature = header_value(response.headers(), "x-harbor-response-signature")
        .ok_or_else(|| anyhow!("Mac did not return an authenticated response"))?;
    let response_canonical = format!(
        "TH-HMAC-V1-RESPONSE\n{nonce}\n{status}\n{}",
        sha256_hex(&writer)
    );
    let expected = hmac_value(expected_key, response_canonical.as_bytes());
    if expected != response_signature {
        anyhow::bail!("Mac response signature is invalid");
    }
    Ok(SignedResponse {
        status,
        body: writer,
    })
}

fn parse_json_map(body: &[u8]) -> anyhow::Result<serde_json::Value> {
    serde_json::from_slice(body).context("invalid JSON from Mac")
}

fn require_ok(response: &SignedResponse, what: &str) -> anyhow::Result<()> {
    if (200..300).contains(&response.status) {
        Ok(())
    } else {
        anyhow::bail!("{what} failed")
    }
}

fn parse_endpoints(value: &serde_json::Value) -> Vec<PeerEndpoint> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let kind = item.get("kind")?.as_str()?;
            let url = item.get("url")?.as_str()?;
            if looks_like_base_url(url) {
                Some(PeerEndpoint {
                    kind: kind.to_string(),
                    url: url.trim_end_matches('/').to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn parse_workspaces(value: &serde_json::Value) -> Vec<RemoteWorkspace> {
    value
        .get("workspaces")
        .and_then(|item| item.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            let directory = item
                .get("directory")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .or_else(|| item.get("name").and_then(|value| value.as_str()))
                .unwrap_or("workspace")
                .to_string();
            Some(RemoteWorkspace {
                id,
                directory,
                activity: WorkspaceActivity::from_wire(
                    item.get("activity")
                        .and_then(|value| value.as_str())
                        .unwrap_or(""),
                ),
                selected: item
                    .get("selected")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect()
}

fn notify_sidebar() {
    promise::spawn::spawn_into_main_thread(async {
        if let Some(fe) = crate::frontend::try_front_end() {
            for gui in fe.gui_windows() {
                gui.window.notify(TermWindowNotif::Apply(Box::new(|tw| {
                    tw.invalidate_harbor_sidebar();
                    if let Some(window) = tw.window.as_ref() {
                        window.invalidate();
                    }
                })));
            }
        }
    })
    .detach();
}

fn with_peer_mut<T>(server_id: &str, func: impl FnOnce(&mut LivePeer) -> T) -> Option<T> {
    let mut inner = INNER.lock();
    inner.peers.get_mut(server_id).map(func)
}

fn persist() {
    let inner = INNER.lock();
    if let Err(err) = save_peers(&inner) {
        log::error!("saving paired desktops: {err:#}");
    }
}

pub fn ensure_running() {
    if POLLER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::Builder::new()
        .name("harbor-peer-poll".into())
        .spawn(|| loop {
            refresh_all();
            thread::sleep(POLL_INTERVAL);
        })
        .expect("spawn harbor peer poller");
}

pub fn snapshot() -> Vec<PeerView> {
    let inner = INNER.lock();
    inner
        .peers
        .values()
        .map(|peer| PeerView {
            server_id: peer.persisted.server_id.clone(),
            label: peer_label_from(&peer.persisted),
            error: peer.error.clone(),
            needs_lan_confirm: peer.needs_lan_confirm,
            workspaces: peer.workspaces.clone(),
        })
        .collect()
}

pub fn pair_status() -> Option<String> {
    INNER.lock().pair_status.clone()
}

fn peer_label_from(peer: &PersistedPeer) -> String {
    let name = peer.device_name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    let host = peer.host_name.trim();
    if !host.is_empty() {
        return host.to_string();
    }
    "Paired Mac".to_string()
}

pub fn peer_label(server_id: &str) -> String {
    INNER
        .lock()
        .peers
        .get(server_id)
        .map(|peer| peer_label_from(&peer.persisted))
        .unwrap_or_else(|| "Paired Mac".to_string())
}

pub fn pair_from_uri(raw: &str) -> anyhow::Result<()> {
    INNER.lock().pair_status = Some("Pairing another Harbor…".to_string());
    notify_sidebar();
    let result = pair_from_uri_inner(raw);
    INNER.lock().pair_status = match &result {
        Ok(()) => None,
        Err(_) => Some("Could not pair with the other Mac".to_string()),
    };
    notify_sidebar();
    result
}

fn pair_from_uri_inner(raw: &str) -> anyhow::Result<()> {
    let payload = PairPayload::parse(raw)?;
    let client_id = Uuid::new_v4().to_string();
    let client_nonce = Uuid::new_v4().as_bytes().to_vec();
    let body = serde_json::to_vec(&serde_json::json!({
        "auth_version": AUTH_VERSION,
        "client_id": client_id,
        "client_nonce": URL_SAFE_NO_PAD.encode(&client_nonce),
        "device_name": harbor_mobile::device_name(),
    }))?;
    let key = derive_device_key(
        &payload.token,
        &payload.server_id,
        &client_id,
        &client_nonce,
    );
    let mut last_error = None;
    let mut connected = None;
    for endpoint in payload.pairing_endpoints() {
        match signed_request(
            Method::POST,
            &endpoint.url,
            "/v1/pair",
            &body,
            payload.token.as_bytes(),
            Some(&key),
            None,
            PAIR_TIMEOUT,
        ) {
            Ok(response) => {
                if let Err(err) = require_ok(&response, "Pairing") {
                    last_error = Some(err);
                    continue;
                }
                connected = Some((endpoint, response));
                break;
            }
            Err(err) => last_error = Some(err),
        }
    }
    let (connected_endpoint, response) = connected.ok_or_else(|| {
        last_error.unwrap_or_else(|| {
            anyhow!("Could not reach the Mac through Tailscale or the local network")
        })
    })?;
    let json = parse_json_map(&response.body)?;
    let server_id = json
        .get("server_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("pair response came from a different Mac"))?;
    if server_id != payload.server_id {
        anyhow::bail!("pair response came from a different Mac");
    }
    let response_client_id = json
        .get("client_id")
        .and_then(|value| value.as_str())
        .unwrap_or(&client_id)
        .to_string();
    let mut endpoints = parse_endpoints(&json["endpoints"]);
    if endpoints.is_empty() {
        endpoints = payload.endpoints.clone();
    }
    if !endpoints
        .iter()
        .any(|endpoint| endpoint.url == connected_endpoint.url)
    {
        endpoints.push(connected_endpoint.clone());
    }
    let allow_lan_fallback = is_lan_kind(&connected_endpoint.kind);
    let mut persisted = PersistedPeer {
        server_id: server_id.to_string(),
        client_id: response_client_id,
        secret: URL_SAFE_NO_PAD.encode(&key),
        device_name: String::new(),
        host_name: String::new(),
        endpoints,
        last_successful_url: Some(connected_endpoint.url.clone()),
        allow_lan_fallback,
    };
    if let Ok(session) = fetch_session(&persisted, &connected_endpoint.url) {
        apply_session(&mut persisted, &session, &connected_endpoint.url);
    }
    {
        let mut inner = INNER.lock();
        inner.peers.insert(
            persisted.server_id.clone(),
            LivePeer {
                persisted: persisted.clone(),
                workspaces: Vec::new(),
                error: None,
                needs_lan_confirm: false,
                last_refresh: None,
            },
        );
        inner.pair_status = None;
        save_peers(&inner)?;
    }
    refresh_peer(&persisted.server_id);
    Ok(())
}

fn apply_session(peer: &mut PersistedPeer, session: &serde_json::Value, url: &str) {
    if let Some(name) = session.get("device_name").and_then(|value| value.as_str()) {
        if !name.trim().is_empty() {
            peer.device_name = name.to_string();
        }
    }
    if let Some(name) = session.get("host_name").and_then(|value| value.as_str()) {
        if !name.trim().is_empty() {
            peer.host_name = name.to_string();
        }
    }
    let endpoints = parse_endpoints(&session["endpoints"]);
    if !endpoints.is_empty() {
        peer.endpoints = endpoints;
    }
    peer.last_successful_url = Some(url.to_string());
}

fn decode_secret(peer: &PersistedPeer) -> anyhow::Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(peer.secret.as_bytes())
        .context("stored pairing secret is invalid")
}

fn fetch_session(peer: &PersistedPeer, base_url: &str) -> anyhow::Result<serde_json::Value> {
    let secret = decode_secret(peer)?;
    let response = signed_request(
        Method::GET,
        base_url,
        "/v1/session",
        &[],
        &secret,
        None,
        Some(&peer.client_id),
        REQUEST_TIMEOUT,
    )?;
    require_ok(&response, "Session")?;
    parse_json_map(&response.body)
}

fn check_identity(peer: &PersistedPeer, base_url: &str) -> anyhow::Result<()> {
    let secret = decode_secret(peer)?;
    let response = signed_request(
        Method::GET,
        base_url,
        "/v1/identity",
        &[],
        &secret,
        None,
        Some(&peer.client_id),
        REQUEST_TIMEOUT,
    )?;
    require_ok(&response, "Identity")?;
    let json = parse_json_map(&response.body)?;
    let server_id = json
        .get("server_id")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if server_id != peer.server_id {
        anyhow::bail!("Mac identity did not match");
    }
    Ok(())
}

fn connect_peer(peer: &mut LivePeer) -> anyhow::Result<String> {
    let endpoints = ordered_endpoints(
        &peer.persisted.endpoints,
        peer.persisted.last_successful_url.as_deref(),
    );
    let mut last_error = None;
    let mut try_group = |lan: bool| -> Option<String> {
        for endpoint in &endpoints {
            if is_lan_kind(&endpoint.kind) != lan {
                continue;
            }
            if lan && !peer.persisted.allow_lan_fallback {
                peer.needs_lan_confirm = true;
                continue;
            }
            match check_identity(&peer.persisted, &endpoint.url)
                .and_then(|_| fetch_session(&peer.persisted, &endpoint.url))
            {
                Ok(session) => {
                    apply_session(&mut peer.persisted, &session, &endpoint.url);
                    peer.needs_lan_confirm = false;
                    peer.error = None;
                    return Some(endpoint.url.clone());
                }
                Err(err) => last_error = Some(err),
            }
        }
        None
    };
    if let Some(url) = try_group(false) {
        return Ok(url);
    }
    if let Some(url) = try_group(true) {
        return Ok(url);
    }
    Err(last_error.unwrap_or_else(|| anyhow!("Mac is unavailable")))
}

fn refresh_peer(server_id: &str) {
    let persisted = {
        let inner = INNER.lock();
        match inner.peers.get(server_id) {
            Some(peer) => peer.persisted.clone(),
            None => return,
        }
    };
    let mut live = LivePeer {
        persisted,
        workspaces: Vec::new(),
        error: None,
        needs_lan_confirm: false,
        last_refresh: Some(Instant::now()),
    };
    match connect_peer(&mut live) {
        Ok(url) => match list_workspaces(&live.persisted, &url) {
            Ok(workspaces) => live.workspaces = workspaces,
            Err(err) => live.error = Some(user_error(err)),
        },
        Err(err) => live.error = Some(user_error(err)),
    }
    {
        let mut inner = INNER.lock();
        if let Some(existing) = inner.peers.get_mut(server_id) {
            existing.persisted = live.persisted;
            existing.workspaces = live.workspaces;
            existing.error = live.error;
            existing.needs_lan_confirm = live.needs_lan_confirm;
            existing.last_refresh = live.last_refresh;
        }
        let _ = save_peers(&inner);
    }
    notify_sidebar();
}

fn list_workspaces(peer: &PersistedPeer, base_url: &str) -> anyhow::Result<Vec<RemoteWorkspace>> {
    let secret = decode_secret(peer)?;
    let response = signed_request(
        Method::GET,
        base_url,
        "/v1/workspaces",
        &[],
        &secret,
        None,
        Some(&peer.client_id),
        REQUEST_TIMEOUT,
    )?;
    require_ok(&response, "Workspaces")?;
    Ok(parse_workspaces(&parse_json_map(&response.body)?))
}

fn user_error(err: anyhow::Error) -> String {
    let message = format!("{err:#}");
    if message.contains("unavailable") || message.contains("reach") {
        "Mac is unavailable".to_string()
    } else if message.contains("identity") {
        "Mac identity did not match".to_string()
    } else if message.contains("unauthorized") || message.contains("401") {
        "Pairing is no longer valid".to_string()
    } else {
        "Could not reach the other Mac".to_string()
    }
}

fn refresh_all() {
    let ids: Vec<String> = INNER.lock().peers.keys().cloned().collect();
    for id in ids {
        refresh_peer(&id);
    }
}

pub fn confirm_lan_fallback(server_id: &str) {
    if let Some(()) = with_peer_mut(server_id, |peer| {
        peer.persisted.allow_lan_fallback = true;
        peer.needs_lan_confirm = false;
    }) {
        persist();
        let server_id = server_id.to_string();
        thread::spawn(move || refresh_peer(&server_id));
    }
}

pub fn unpair(server_id: &str) {
    {
        let mut inner = INNER.lock();
        inner.peers.remove(server_id);
        let _ = save_peers(&inner);
    }
    notify_sidebar();
}

fn active_base_url(peer: &PersistedPeer) -> anyhow::Result<String> {
    peer.last_successful_url
        .clone()
        .ok_or_else(|| anyhow!("Mac is unavailable"))
}

pub fn activate_workspace(server_id: &str, workspace_id: &str) -> anyhow::Result<()> {
    call_workspace(
        server_id,
        &format!("/v1/workspaces/{workspace_id}/activate"),
        b"",
        Method::POST,
    )
}

pub fn send_instruction(
    server_id: &str,
    workspace_id: &str,
    text: &str,
    submit: bool,
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(&serde_json::json!({ "text": text, "submit": submit }))?;
    call_workspace(
        server_id,
        &format!("/v1/workspaces/{workspace_id}/instruction"),
        &body,
        Method::POST,
    )
}

pub fn send_key(server_id: &str, workspace_id: &str, key: &str) -> anyhow::Result<()> {
    let body = serde_json::to_vec(&serde_json::json!({ "key": key }))?;
    call_workspace(
        server_id,
        &format!("/v1/workspaces/{workspace_id}/key"),
        &body,
        Method::POST,
    )
}

pub fn fetch_screen(server_id: &str, workspace_id: &str, lines: u32) -> anyhow::Result<String> {
    let peer = {
        let inner = INNER.lock();
        inner
            .peers
            .get(server_id)
            .map(|peer| peer.persisted.clone())
            .ok_or_else(|| anyhow!("Mac is unavailable"))?
    };
    let base = active_base_url(&peer)?;
    let secret = decode_secret(&peer)?;
    let path = format!("/v1/workspaces/{workspace_id}/screen?lines={lines}");
    let response = signed_request(
        Method::GET,
        &base,
        &path,
        &[],
        &secret,
        None,
        Some(&peer.client_id),
        REQUEST_TIMEOUT,
    )?;
    require_ok(&response, "Screen")?;
    let json = parse_json_map(&response.body)?;
    Ok(json
        .get("text")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string())
}

fn call_workspace(server_id: &str, path: &str, body: &[u8], method: Method) -> anyhow::Result<()> {
    let peer = {
        let inner = INNER.lock();
        inner
            .peers
            .get(server_id)
            .map(|peer| peer.persisted.clone())
            .ok_or_else(|| anyhow!("Mac is unavailable"))?
    };
    let base = active_base_url(&peer)?;
    let secret = decode_secret(&peer)?;
    let response = signed_request(
        method,
        &base,
        path,
        body,
        &secret,
        None,
        Some(&peer.client_id),
        REQUEST_TIMEOUT,
    )?;
    require_ok(&response, "Request")
}

/// Shared with tests that should not log or persist secrets.
#[cfg(test)]
pub fn endpoint_order_for_tests(endpoints: &[PeerEndpoint], last: Option<&str>) -> Vec<String> {
    ordered_endpoints(endpoints, last)
        .into_iter()
        .map(|endpoint| format!("{}:{}", endpoint.kind, endpoint.url))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_uri_parses_typed_endpoints_and_rejects_non_pair_text() {
        let uri = "harbor://pair?v=1&host=192.168.1.20&port=7780&tls=0&token=secret&sid=11111111-1111-1111-1111-111111111111&auth=hmac-sha256-v1&endpoint=tailscale_https%2Chttps%3A%2F%2Fmac.tailnet.ts.net&endpoint=tailscale_direct%2Chttp%3A%2F%2F100.64.0.2%3A7780&endpoint=lan%2Chttp%3A%2F%2F192.168.1.20%3A7780";
        let parsed = PairPayload::parse(uri).expect("parse");
        assert_eq!(parsed.server_id, "11111111-1111-1111-1111-111111111111");
        let urls: Vec<_> = parsed
            .pairing_endpoints()
            .into_iter()
            .map(|endpoint| endpoint.kind)
            .collect();
        assert_eq!(urls, vec!["tailscale_https", "tailscale_direct", "lan"]);
        assert!(PairPayload::parse("https://example.com").is_err());
    }

    #[test]
    fn endpoints_prefer_tailscale_and_keep_last_success_inside_group() {
        let endpoints = vec![
            PeerEndpoint {
                kind: "lan".into(),
                url: "http://192.168.1.8:7780".into(),
            },
            PeerEndpoint {
                kind: "tailscale_direct".into(),
                url: "http://100.64.0.2:7780".into(),
            },
            PeerEndpoint {
                kind: "tailscale_direct".into(),
                url: "http://mac.tailnet.ts.net:7780".into(),
            },
            PeerEndpoint {
                kind: "tailscale_https".into(),
                url: "https://mac.tailnet.ts.net".into(),
            },
        ];
        let ordered = endpoint_order_for_tests(&endpoints, Some("http://mac.tailnet.ts.net:7780"));
        assert_eq!(
            ordered,
            vec![
                "tailscale_https:https://mac.tailnet.ts.net",
                "tailscale_direct:http://mac.tailnet.ts.net:7780",
                "tailscale_direct:http://100.64.0.2:7780",
                "lan:http://192.168.1.8:7780",
            ]
        );
    }

    #[test]
    fn lan_kind_is_confirmed_separately_from_tailscale() {
        assert!(is_lan_kind("lan"));
        assert!(is_lan_kind("legacy"));
        assert!(!is_lan_kind("tailscale_direct"));
        assert!(!is_lan_kind("tailscale_https"));
    }
}
