//! Desktop HMAC client for another Terminal Harbor instance.
//!
//! Reuses the mobile bridge contract. Endpoint order matches the Flutter
//! companion: Tailscale HTTPS, Tailscale direct, then LAN after confirmation.

use crate::harbor_mobile::{
    self, canonical_request, derive_device_key, hmac_value, sha256_hex, TailscaleNode,
    TailscaleSnapshot, AUTH_VERSION, DEFAULT_PORT,
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
    /// Nonce this client generated for the request; the host binds its response
    /// signature to it.
    nonce: String,
    response_signature: Option<String>,
}

fn header_value(headers: &http_req::response::Headers, name: &str) -> Option<String> {
    headers.get(name).cloned()
}

fn method_name(method: Method) -> &'static str {
    match method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::DELETE => "DELETE",
        _ => "GET",
    }
}

/// Send one request to another Harbor. Without a `signing_key` the request is
/// sent unsigned, which only the pairing endpoints accept because they are the
/// endpoints that establish a secret in the first place.
fn send_request(
    method: Method,
    base_url: &str,
    path: &str,
    body: &[u8],
    signing_key: Option<&[u8]>,
    client_id: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<SignedResponse> {
    let timestamp = now_unix().to_string();
    let nonce = URL_SAFE_NO_PAD.encode(Uuid::new_v4().as_bytes());
    let url = format!("{}{path}", base_url.trim_end_matches('/'));
    let uri = Uri::try_from(url.as_str()).map_err(|err| anyhow!("invalid URL: {err}"))?;
    let mut request = Request::new(&uri);
    request
        .method(method)
        .timeout(timeout)
        .header("Accept", "application/json")
        .header("X-Harbor-Timestamp", &timestamp)
        .header("X-Harbor-Nonce", &nonce);
    if let Some(signing_key) = signing_key {
        let canonical = canonical_request(method_name(method), path, &timestamp, &nonce, body);
        request.header(
            "X-Harbor-Signature",
            &hmac_value(signing_key, canonical.as_bytes()),
        );
    }
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
    Ok(SignedResponse {
        status: u16::from(response.status_code()),
        body: writer,
        nonce,
        response_signature: header_value(response.headers(), "x-harbor-response-signature"),
    })
}

/// Check that the host knows the device secret, which is what proves the pair
/// response and later payloads came from the machine we paired with.
fn verify_response(response: &SignedResponse, key: &[u8]) -> anyhow::Result<()> {
    let signature = response
        .response_signature
        .as_deref()
        .ok_or_else(|| anyhow!("Harbor did not return an authenticated response"))?;
    let response_canonical = format!(
        "TH-HMAC-V1-RESPONSE\n{}\n{}\n{}",
        response.nonce,
        response.status,
        sha256_hex(&response.body)
    );
    let expected = hmac_value(key, response_canonical.as_bytes());
    if expected != signature {
        anyhow::bail!("Harbor response signature is invalid");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
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
    let response = send_request(
        method,
        base_url,
        path,
        body,
        Some(signing_key),
        client_id,
        timeout,
    )?;
    verify_response(&response, response_key.unwrap_or(signing_key))?;
    Ok(response)
}

fn parse_json_map(body: &[u8]) -> anyhow::Result<serde_json::Value> {
    serde_json::from_slice(body).context("invalid JSON from Harbor")
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
    "Paired Harbor".to_string()
}

pub fn peer_label(server_id: &str) -> String {
    INNER
        .lock()
        .peers
        .get(server_id)
        .map(|peer| peer_label_from(&peer.persisted))
        .unwrap_or_else(|| "Paired Harbor".to_string())
}

pub fn pair_from_uri(raw: &str) -> anyhow::Result<()> {
    INNER.lock().pair_status = Some("Pairing another Harbor…".to_string());
    notify_sidebar();
    let result = pair_from_uri_inner(raw);
    INNER.lock().pair_status = match &result {
        Ok(()) => None,
        Err(_) => Some("Could not pair with the other Harbor".to_string()),
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
            anyhow!("Could not reach the other Harbor through Tailscale or the local network")
        })
    })?;
    let json = parse_json_map(&response.body)?;
    let server_id = json
        .get("server_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("pair response came from a different Harbor"))?;
    if server_id != payload.server_id {
        anyhow::bail!("pair response came from a different Harbor");
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

/// Pair with another Harbor from clipboard or selection content: a
/// `harbor://pair` URI keeps the pairing-code path, anything else is read as a
/// Tailscale device id.
pub fn pair_from_text(raw: &str) -> anyhow::Result<()> {
    if is_pair_uri(raw) {
        pair_from_uri(raw)
    } else {
        pair_from_tailscale_id(raw)
    }
}

fn is_pair_uri(raw: &str) -> bool {
    raw.trim().starts_with("harbor://pair")
}

/// Normalize a clipboard Tailscale id: MagicDNS names arrive with a trailing
/// dot, hostnames may carry their search domain, and an id can be pasted with a
/// scheme or port still attached.
fn normalize_tailscale_id(raw: &str) -> String {
    let mut text = raw.trim().to_ascii_lowercase();
    for scheme in ["http://", "https://", "harbor://"] {
        if let Some(rest) = text.strip_prefix(scheme) {
            text = rest.to_string();
        }
    }
    text = text.trim_end_matches('/').trim_end_matches('.').to_string();
    if let Some(rest) = text.strip_prefix('[') {
        // A bracketed IPv6 literal may carry ":port" after the closing bracket.
        if let Some((inner, port)) = rest.split_once("]:") {
            if port.parse::<u16>().is_ok() {
                return inner.to_string();
            }
        }
        return rest.trim_end_matches(']').to_string();
    }
    // Drop a trailing ":port" from a host, but never the groups of an IPv6
    // address, which is only ever written with more than one colon.
    if let Some((host, port)) = text.rsplit_once(':') {
        if host.matches(':').count() <= 1 && port.parse::<u16>().is_ok() {
            text = host.to_string();
        }
    }
    text.trim().to_string()
}

fn direct_endpoint(authority: &str) -> PeerEndpoint {
    let authority = if authority.contains(':') {
        format!("[{authority}]")
    } else {
        authority.to_string()
    };
    PeerEndpoint {
        kind: "tailscale_direct".to_string(),
        url: format!("http://{authority}:{DEFAULT_PORT}"),
    }
}

fn first_label(value: &str) -> &str {
    value.split('.').next().unwrap_or(value)
}

/// A Tailscale node answers to its MagicDNS name, its machine name, the first
/// label of either, or any of its tailnet addresses.
fn node_matches_id(node: &TailscaleNode, needle: &str) -> bool {
    let dns = node.dns_name.to_ascii_lowercase();
    let host = node.host_name.to_ascii_lowercase();
    if needle == dns || needle == host {
        return true;
    }
    if needle == first_label(&dns) || needle == first_label(&host) {
        return true;
    }
    node.ips
        .iter()
        .any(|ip| ip.to_ascii_lowercase().trim_matches(['[', ']']) == needle)
}

fn endpoints_for_node(node: &TailscaleNode) -> Vec<PeerEndpoint> {
    let mut endpoints = Vec::new();
    if !node.dns_name.is_empty() {
        endpoints.push(direct_endpoint(&node.dns_name));
    }
    for ip in &node.ips {
        endpoints.push(direct_endpoint(ip));
    }
    ordered_endpoints(&endpoints, None)
}

/// Resolve a Tailscale device id into the addresses Harbor listens on.
fn tailnet_endpoints_for_id(
    needle: &str,
    snapshot: &TailscaleSnapshot,
) -> anyhow::Result<Vec<PeerEndpoint>> {
    if needle.is_empty() {
        anyhow::bail!("no Tailscale device id was given");
    }
    if let Some(self_node) = snapshot.self_node.as_ref() {
        if node_matches_id(self_node, needle) {
            anyhow::bail!("that is this Terminal Harbor instance, not another one");
        }
    }
    let matches: Vec<&TailscaleNode> = snapshot
        .peers
        .iter()
        .filter(|node| node_matches_id(node, needle))
        .collect();
    match matches.len() {
        1 => Ok(endpoints_for_node(matches[0])),
        0 => {
            // A peer that is not in our own status document can still be
            // reachable: MagicDNS resolves any node of the tailnet, and a bare
            // tailnet address needs no lookup at all.
            if needle.parse::<std::net::IpAddr>().is_ok() {
                return Ok(vec![direct_endpoint(needle)]);
            }
            if needle.contains('.') {
                return Ok(vec![direct_endpoint(needle)]);
            }
            if !snapshot.magic_dns_suffix.is_empty() {
                return Ok(vec![direct_endpoint(&format!(
                    "{needle}.{}",
                    snapshot.magic_dns_suffix
                ))]);
            }
            anyhow::bail!("no Tailscale device matched that id")
        }
        count => {
            anyhow::bail!("the id matches {count} Tailscale devices; use its MagicDNS name instead")
        }
    }
}

fn fetch_server_id(base_url: &str) -> anyhow::Result<String> {
    // `/v1/identity` is answered unsigned before a credential exists, so the
    // value is only a hint here. A wrong server id turns into a key mismatch in
    // `verify_response` below, and the signed identity check on the first poll
    // confirms it.
    let response = send_request(
        Method::GET,
        base_url,
        "/v1/identity",
        &[],
        None,
        None,
        PAIR_TIMEOUT,
    )?;
    require_ok(&response, "Identity")?;
    let json = parse_json_map(&response.body)?;
    json.get("server_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or_else(|| anyhow!("Harbor did not report a server id"))
}

/// Pair at one endpoint. The host has no secret to verify yet, so the request is
/// unsigned; the token it mints is what both sides derive the device key from.
fn pair_at_tailnet_endpoint(
    base_url: &str,
    client_id: &str,
    client_nonce: &[u8],
) -> anyhow::Result<(Vec<u8>, serde_json::Value)> {
    let server_id = fetch_server_id(base_url)?;
    let body = serde_json::to_vec(&serde_json::json!({
        "auth_version": AUTH_VERSION,
        "client_id": client_id,
        "client_nonce": URL_SAFE_NO_PAD.encode(client_nonce),
        "device_name": harbor_mobile::device_name(),
    }))?;
    let response = send_request(
        Method::POST,
        base_url,
        "/v1/pair/tailnet",
        &body,
        None,
        None,
        PAIR_TIMEOUT,
    )?;
    require_ok(&response, "Pairing")?;
    let json = parse_json_map(&response.body)?;
    if json
        .get("server_id")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        != server_id
    {
        anyhow::bail!("pair response came from a different Harbor");
    }
    let token = json
        .get("tailnet_pair_token")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("pair response did not carry a token"))?;
    let key = derive_device_key(token, &server_id, client_id, client_nonce);
    verify_response(&response, &key)?;
    Ok((key, json))
}

pub fn pair_from_tailscale_id(raw: &str) -> anyhow::Result<()> {
    INNER.lock().pair_status = Some("Pairing over Tailscale…".to_string());
    notify_sidebar();
    let result = pair_from_tailscale_id_inner(raw);
    INNER.lock().pair_status = match &result {
        Ok(()) => None,
        Err(_) => Some("Could not pair over Tailscale".to_string()),
    };
    notify_sidebar();
    result
}

fn pair_from_tailscale_id_inner(raw: &str) -> anyhow::Result<()> {
    let needle = normalize_tailscale_id(raw);
    let snapshot = harbor_mobile::tailscale_snapshot().ok_or_else(|| {
        anyhow!("Tailscale is not reachable from this machine, so no device id can resolve")
    })?;
    let endpoints = tailnet_endpoints_for_id(&needle, &snapshot)?;
    let client_id = Uuid::new_v4().to_string();
    let client_nonce = Uuid::new_v4().as_bytes().to_vec();

    let mut last_error = None;
    let mut connected = None;
    for endpoint in &endpoints {
        match pair_at_tailnet_endpoint(&endpoint.url, &client_id, &client_nonce) {
            Ok((key, json)) => {
                connected = Some((endpoint.clone(), key, json));
                break;
            }
            Err(err) => last_error = Some(err),
        }
    }
    let (connected_endpoint, key, json) = connected.ok_or_else(|| {
        last_error.unwrap_or_else(|| anyhow!("Could not reach the other Harbor through Tailscale"))
    })?;

    let server_id = json
        .get("server_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("pair response did not carry a server id"))?;
    let response_client_id = json
        .get("client_id")
        .and_then(|value| value.as_str())
        .unwrap_or(&client_id)
        .to_string();
    let mut persisted_endpoints = parse_endpoints(&json["endpoints"]);
    if persisted_endpoints.is_empty() {
        persisted_endpoints = endpoints.clone();
    }
    if !persisted_endpoints
        .iter()
        .any(|endpoint| endpoint.url == connected_endpoint.url)
    {
        persisted_endpoints.push(connected_endpoint.clone());
    }
    // A tailnet pairing never drops to the local network on its own; the
    // existing **Use local network** confirmation still governs LAN.
    let mut persisted = PersistedPeer {
        server_id: server_id.to_string(),
        client_id: response_client_id,
        secret: URL_SAFE_NO_PAD.encode(&key),
        device_name: String::new(),
        host_name: String::new(),
        endpoints: persisted_endpoints,
        last_successful_url: Some(connected_endpoint.url.clone()),
        allow_lan_fallback: false,
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
        anyhow::bail!("Harbor identity did not match");
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
    Err(last_error.unwrap_or_else(|| anyhow!("Host is unavailable")))
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
        "Host is unavailable".to_string()
    } else if message.contains("identity") {
        "Harbor identity did not match".to_string()
    } else if message.contains("unauthorized") || message.contains("401") {
        "Pairing is no longer valid".to_string()
    } else {
        "Could not reach the other Harbor".to_string()
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
        .ok_or_else(|| anyhow!("Host is unavailable"))
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
            .ok_or_else(|| anyhow!("Host is unavailable"))?
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
            .ok_or_else(|| anyhow!("Host is unavailable"))?
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

    fn fixture_node(id: &str, host: &str, dns: &str, user: &str, ips: &[&str]) -> TailscaleNode {
        TailscaleNode {
            id: id.to_string(),
            host_name: host.to_string(),
            dns_name: dns.to_string(),
            user_id: user.to_string(),
            ips: ips.iter().map(|ip| ip.to_string()).collect(),
        }
    }

    /// The tailnet of the working machine: one own laptop, one same-user PC, one
    /// foreign-user laptop.
    fn fixture_snapshot() -> TailscaleSnapshot {
        TailscaleSnapshot {
            self_user_id: "2116013432887087".to_string(),
            self_node: Some(fixture_node(
                "nse7E65fVN11CNTRL",
                "MacBookAir-g150446",
                "macbookm1.taile24f86.ts.net",
                "2116013432887087",
                &["100.100.155.9", "fd7a:115c:a1e0::7f3a:9b09"],
            )),
            peers: vec![
                fixture_node(
                    "n5BVcyFxxp11CNTRL",
                    "pc-office",
                    "pc-office.taile24f86.ts.net",
                    "2116013432887087",
                    &["100.108.3.23", "fd7a:115c:a1e0::3f32:318"],
                ),
                fixture_node(
                    "n99yGuestNode",
                    "guest-laptop",
                    "guest-laptop.taile24f86.ts.net",
                    "555",
                    &["100.77.77.77"],
                ),
            ],
            magic_dns_suffix: "taile24f86.ts.net".to_string(),
        }
    }

    fn endpoint_urls(endpoints: &[PeerEndpoint]) -> Vec<String> {
        endpoints
            .iter()
            .map(|endpoint| endpoint.url.clone())
            .collect()
    }

    #[test]
    fn tailscale_id_normalizes_the_pasted_forms() {
        assert_eq!(
            normalize_tailscale_id("  PC-Office.taile24f86.ts.net. "),
            "pc-office.taile24f86.ts.net"
        );
        assert_eq!(
            normalize_tailscale_id("http://pc-office.taile24f86.ts.net:7780/"),
            "pc-office.taile24f86.ts.net"
        );
        assert_eq!(normalize_tailscale_id("100.108.3.23:7780"), "100.108.3.23");
        assert_eq!(
            normalize_tailscale_id("[fd7a:115c:a1e0::3f32:318]:7780"),
            "fd7a:115c:a1e0::3f32:318"
        );
        assert_eq!(
            normalize_tailscale_id("[fd7a:115c:a1e0::3f32:318]"),
            "fd7a:115c:a1e0::3f32:318"
        );
        // An unbracketed IPv6 must keep its groups.
        assert_eq!(
            normalize_tailscale_id("fd7a:115c:a1e0::3f32:318"),
            "fd7a:115c:a1e0::3f32:318"
        );
    }

    #[test]
    fn tailscale_id_resolves_by_name_or_by_address() {
        let snapshot = fixture_snapshot();
        for pasted in [
            "pc-office.taile24f86.ts.net",
            "PC-Office.taile24f86.ts.net.",
            "pc-office",
            "100.108.3.23",
            "http://pc-office.taile24f86.ts.net:7780",
        ] {
            let needle = normalize_tailscale_id(pasted);
            let endpoints = tailnet_endpoints_for_id(&needle, &snapshot)
                .unwrap_or_else(|err| panic!("{} must resolve: {}", pasted, err));
            let urls = endpoint_urls(&endpoints);
            assert!(
                urls.contains(&"http://100.108.3.23:7780".to_string()),
                "{pasted} resolved to {urls:?}"
            );
            assert!(
                urls.contains(&"http://[fd7a:115c:a1e0::3f32:318]:7780".to_string()),
                "{pasted} resolved to {urls:?}"
            );
            assert!(
                urls.contains(&"http://pc-office.taile24f86.ts.net:7780".to_string()),
                "{pasted} resolved to {urls:?}"
            );
            assert!(
                endpoints
                    .iter()
                    .all(|endpoint| endpoint.kind == "tailscale_direct"),
                "a resolved peer is reached over the tailnet only: {endpoints:?}"
            );
        }
    }

    #[test]
    fn tailscale_id_refuses_this_machine_and_ambiguous_names() {
        let snapshot = fixture_snapshot();
        let error = tailnet_endpoints_for_id("macbookm1.taile24f86.ts.net", &snapshot)
            .expect_err("pairing with yourself is not a peer");
        assert!(error.to_string().contains("this Terminal Harbor instance"));

        let mut ambiguous = fixture_snapshot();
        ambiguous.peers.push(fixture_node(
            "nOtherOffice",
            "pc-office",
            "pc-office.other-tailnet.ts.net",
            "2116013432887087",
            &["100.9.9.9"],
        ));
        let error =
            tailnet_endpoints_for_id("pc-office", &ambiguous).expect_err("short name is ambiguous");
        assert!(error.to_string().contains("MagicDNS name"));
        // The fully qualified name still picks exactly one device.
        assert!(tailnet_endpoints_for_id("pc-office.other-tailnet.ts.net", &ambiguous).is_ok());
    }

    #[test]
    fn unknown_tailscale_id_falls_back_to_magicdns_or_a_bare_address() {
        let snapshot = fixture_snapshot();
        let endpoints = tailnet_endpoints_for_id("new-studio", &snapshot).expect("MagicDNS guess");
        assert_eq!(
            endpoint_urls(&endpoints),
            vec!["http://new-studio.taile24f86.ts.net:7780".to_string()]
        );

        let endpoints = tailnet_endpoints_for_id("100.108.99.9", &snapshot).expect("bare address");
        assert_eq!(
            endpoint_urls(&endpoints),
            vec!["http://100.108.99.9:7780".to_string()]
        );

        let mut no_magic_dns = fixture_snapshot();
        no_magic_dns.magic_dns_suffix.clear();
        assert!(tailnet_endpoints_for_id("new-studio", &no_magic_dns).is_err());
        assert!(tailnet_endpoints_for_id("", &no_magic_dns).is_err());
    }

    #[test]
    fn clipboard_text_splits_the_pair_uri_from_a_device_id() {
        assert!(is_pair_uri("harbor://pair?v=1&host=192.168.1.20"));
        assert!(is_pair_uri("  harbor://pair?v=1"));
        assert!(!is_pair_uri("pc-office.taile24f86.ts.net"));
        assert!(!is_pair_uri("https://example.com"));
    }
}
