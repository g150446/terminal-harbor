# Mobile Bridge (terminal-harbor-mobile)

Developer handoff document for the Android/iOS remote control feature.
The companion app lives in the separate repository
[terminal-harbor-mobile](https://github.com/g150446/terminal-harbor-mobile)
(single Flutter codebase; Android today, iOS later).

## Architecture

```
┌────────────────────────────┐
│ terminal-harbor-mobile     │  Flutter (Android / iOS)
│  - QR scan / manual URI    │
│  - multiple secure pairings│
│  - screen mirror (1s poll) │
│  - instruction input       │
└─────────────┬──────────────┘
              │ HTTP JSON over LAN (no TLS yet)
              │ Authorization: Bearer <device_token>
              ▼
┌────────────────────────────┐
│ wezterm-gui                │
│  harbor_mobile bridge      │  0.0.0.0:7780, background thread
│  sidebar Pair mobile + QR  │  PNG opened in image viewer
└─────────────┬──────────────┘
              │ mux access marshalled onto the GUI thread (run_on_main)
              ▼
     Harbor workspaces / mux panes
```

The app speaks JSON HTTP to the bridge. New clients authenticate every request
and response with HMAC-SHA256; Bearer remains available for old clients. It never uses the WezTerm
binary mux protocol — keep it that way so the app stays simple and portable.

## Pairing flow

1. Sidebar **Pair mobile** toggles the pairing panel and mints a one-time
   token (5 minute TTL).
2. The pair URI keeps `v=1`, `host`, `port`, `tls`, and `token`, and adds the
   stable server ID, `auth=hmac-sha256-v1`, and typed Tailscale/LAN endpoints.
   It is rendered as a **square PNG with a quiet zone** (see "Why PNG" below),
   written to `state_dir()/pair-qr.png` and opened with the OS image viewer
   (`open` / `xdg-open` / `cmd /C start`). The URI is also copied to the
   clipboard for the app's Manual entry fallback.
3. New apps HMAC-sign `POST /v1/pair` without transmitting the one-time token.
   Both peers derive the long-lived secret using HKDF-SHA256. Legacy apps can
   still exchange the raw token for a bearer token.
4. New apps sign method, request target, timestamp, nonce, and body hash. The
   bridge rejects stale/replayed nonces and signs every response.

The bridge persists a `server_id` and per-device `client_id` in
`mobile-devices.json`. Existing records receive IDs on first load. It advertises
`_terminal-harbor._tcp.local.` with public `sid`, API, and auth TXT fields. The
mobile app accepts a discovered address only after its signed `/v1/identity`
response verifies for the saved server ID.

On macOS the state file is normally at
`~/Library/Application Support/terminal-harbor/mobile-devices.json`. It contains
long-lived secrets. Do not log, commit, or attach it to an issue. Deleting it
regenerates the server ID and invalidates existing pairings; back it up and
restore it only as protected operational data and only while Terminal Harbor is
fully stopped.

The mobile app stores multiple bridge URLs and tokens in its platform secure
storage. It lists those local records without calling the bridge at startup;
`/v1/session` is called only after the user selects a desktop.

Both the mobile secure-storage value and desktop `mobile-devices.json` contain
bearer tokens. Do not print either file in logs or attach it to bug reports.

`state_dir()` = `dirs_next::data_dir()/terminal-harbor`
(macOS: `~/Library/Application Support/terminal-harbor`).

### Why PNG (not text-cell QR)

A QR rendered into terminal cells is distorted by non-square cells and any
row/column decimation, so phone cameras fail to scan it. Always render with
`qrcode` → `image` as a real bitmap (`quiet_zone(true)`,
`min_dimensions(360, 360)`, `module_dimensions(8, 8)`).

## Endpoints

Full contract: `openapi/harbor-mobile.yaml` in the mobile repo.

| Method | Path | Notes |
|---|---|---|
| POST | `/v1/pair` | No auth. One-time token → device token |
| GET | `/v1/identity` | Stable server identity; signed for authenticated clients |
| GET | `/v1/session` | Connection check plus desktop identity metadata |
| GET | `/v1/workspaces` | Workspace list with activity state |
| POST | `/v1/workspaces/{id}/activate` | Switch active workspace |
| POST | `/v1/workspaces/{id}/instruction` | Body `{text, submit=true}` |
| GET | `/v1/workspaces/{id}/screen?lines=N` | Plain-text screen mirror, N=1..200, default 60 |

### Session identity and compatibility

`GET /v1/session` returns:

```json
{
  "ok": true,
  "version": "1.0.0",
  "device_name": "Kazami’s MacBook Air",
  "host_name": "kazami-macbook-air.local"
}
```

On macOS, `device_name` is read from
`/usr/sbin/scutil --get ComputerName`; an empty value or command failure falls
back to `host_name`. Other platforms also use `host_name` as the device name.
Keep `host_name` stable and treat new metadata fields as additive: mobile
clients intentionally accept an older response with no `device_name`.

The mobile list uses the computer name as its primary label and host name as
its secondary label. Existing pairings acquire these values the next time the
user selects them; no background discovery is performed.

### Instruction + Enter semantics (important)

`submit: true` sends the text via `pane.send_paste()` and then **Enter as a
real key event** via `pane.key_down(KeyCode::Enter, KeyModifiers::NONE)`.
This lets the terminal encode Enter for the active keyboard protocol; a fixed
CR can be ignored when an application has enabled CSI-u/Kitty key encoding.

Do **not** append `\r` to the pasted payload: shells and AI agents with
bracketed-paste mode treat a CR inside a paste as a literal insertion and
never execute the line. This was a real bug; keep the two steps separate.

### Screen endpoint implementation

Workspace id → mux workspace → window → active tab → active pane, then the
canonical "last N lines" pattern:

```rust
let dims = pane.get_dimensions();
let bottom_row = dims.physical_top + dims.viewport_rows as isize;
let top_row = bottom_row.saturating_sub(nlines as isize);
let (_first_row, lines) = pane.get_lines(top_row..bottom_row);
// per line: line.as_str().trim_end() + '\n', then trim_end() the whole text
```

Response: `{"text": ..., "lines": N, "alt_screen": bool}`.
All mux access goes through `run_on_main` (see below).

## Threading rules

- The HTTP server runs on the `harbor-mobile-bridge` thread plus one thread
  per client.
- `Pane: Send + Sync` and reads are internally locked, so it is *memory-safe*
  to touch mux from those threads, but by convention **all mux work is
  marshalled onto the GUI thread** with `run_on_main()` (10s timeout).
- Never call `run_on_main` from the GUI thread itself — it deadlocks. Code
  already on the GUI thread (e.g. `activate_workspace`) must use
  `TermWindowNotif::Apply` fire-and-forget instead.

## Deployment and health checks

The bridge is owned by `wezterm-gui`, not `wezterm-mux-server`. Build and deploy
the desktop before the mobile app when changing the shared contract. Replacing
`/Applications/Terminal Harbor.app` does not update an already-running GUI;
quit/relaunch it or use the supported session-preserving GUI restart. macOS does
not need to reboot for a bridge-only update.

After restart, verify the listener and the unauthenticated identity metadata:

```sh
/usr/sbin/lsof -nP -iTCP:7780 -sTCP:LISTEN
curl --fail http://127.0.0.1:7780/v1/identity
```

Use `dns-sd -B _terminal-harbor._tcp local` for a bounded manual Bonjour check
(stop it with Ctrl-C). Then verify one existing HMAC pairing and one new QR.
Authentication permits at most five minutes of clock skew, so clock sync is the
first check when every otherwise reachable endpoint fails authentication.

Build, bundle, signing, rollback, and process restart procedures are maintained
in [`maintenance.md`](maintenance.md).

## Window centering

`TermWindow::new_window` centers new windows when no explicit position was
requested (CLI `--position` / mux initial position wins). The size is assumed
to be the geometry's 75% of the same bounds `resolve_geometry` uses
(`GeometryOrigin` → `screens.{virtual_rect,main.rect,active.rect,by_name}`),
then `window.set_window_position(ScreenPoint)` is applied post-creation.

## File map

| File | Role |
|---|---|
| `wezterm-gui/src/harbor_mobile.rs` | Bridge server, token persistence, identity metadata, endpoints, and QR PNG |
| `wezterm-gui/src/termwindow/harbor_sidebar.rs` | Pairing panel UI (buttons: Open QR / Copy URI / New QR) |
| `wezterm-gui/src/termwindow/mouseevent.rs` | `UIItemType` handlers; toggle-on also opens QR + copies URI |
| `wezterm-gui/src/termwindow/mod.rs` | `UIItemType` variants; window centering in `new_window` |
| `wezterm-gui/src/frontend.rs` | `harbor_mobile::ensure_running()` at startup; `first_window()` helper |
| `wezterm-gui/src/harbor_workspace.rs` | Sidebar widths (default 480 / min 360, legacy 240 migration) |
| `wezterm-gui/src/termwindow/resize.rs` | `reset_font_and_window_size` = 75% of active screen |
| `config/src/config.rs` | `adjust_window_size_when_changing_font_size` default `false` |
| `wezterm-gui/Cargo.toml` | `qrcode = "0.12"`, `image023 = { package = "image", version = "0.23", default-features = false }` |

## Build constraints (this machine)

- **rustc 1.88.0** is pinned. Workspace `Cargo.toml` therefore pins
  `fixed = "=1.28.0"` (newer `fixed` needs a newer rustc) and uses
  `qrcode 0.12` (0.14 needs rustc 1.93).
- `qrcode 0.12`'s default features pull `image 0.23.14`, which coexists with
  the workspace's `image 0.25`; the 0.23 copy is aliased as `image023` for the
  QR `render::<Luma<u8>>()` step, while saving uses `image::save_buffer`
  (0.25 API: `impl Into<ExtendedColorType>`).
- Verify with: `cargo check -p wezterm-gui && cargo test -p wezterm-gui harbor`
  (includes tests for non-empty session names and a decodable QR PNG).

## Mobile app essentials (see mobile repo `docs/development.md`)

- Package id: `ai.terminalharbor.terminal_harbor_mobile`
- Screen mirror: 1s `Timer.periodic` polling of `/screen` (last 60 lines,
  plain text, monospace, auto-scroll only when near the bottom).
- Instruction input: `Focus(onKeyEvent:)` intercepts Enter with an
  IME-composition guard (Japanese input: 1st Enter confirms conversion,
  2nd Enter sends), plus `TextInputAction.done` for the software keyboard.

## Limitations / TODO

- LAN HTTP does not conceal request/response contents, but HMAC prevents secret
  disclosure, request forgery, replay, and response modification. Prefer
  Tailscale Serve HTTPS when confidentiality is required. The legacy `fp`
  field remains compatibility-only.
- Screen mirror is plain text (no colors/styles) and polling-based, not
  streamed; it targets the **active pane** of the workspace only.
- The mobile app can remove an individual local pairing, but this does not
  revoke or delete the corresponding record in desktop
  `state_dir()/mobile-devices.json`. Tokens do not expire and there is no
  server-side revoke UI/API yet. Treat clearing desktop state as an explicit
  administrative operation because it invalidates paired clients.
- iOS: same codebase, not yet tested on device/simulator.
