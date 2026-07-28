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
│  - secure device token     │
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

The app speaks **plain JSON HTTP** to the bridge. It never uses the WezTerm
binary mux protocol — keep it that way so the app stays simple and portable.

## Pairing flow

1. Sidebar **Pair mobile** toggles the pairing panel and mints a one-time
   token (5 minute TTL).
2. The pair URI `harbor://pair?v=1&host=<lan-ip>&port=7780&tls=0&token=...`
   is rendered as a **square PNG with a quiet zone** (see "Why PNG" below),
   written to `state_dir()/pair-qr.png` and opened with the OS image viewer
   (`open` / `xdg-open` / `cmd /C start`). The URI is also copied to the
   clipboard for the app's Manual entry fallback.
3. App `POST /v1/pair` with the token → receives a long-lived `device_token`
   (persisted in `state_dir()/mobile-devices.json`).
4. All other endpoints require `Authorization: Bearer <device_token>`.

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
| GET | `/v1/session` | Connection check |
| GET | `/v1/workspaces` | Workspace list with activity state |
| POST | `/v1/workspaces/{id}/activate` | Switch active workspace |
| POST | `/v1/workspaces/{id}/instruction` | Body `{text, submit=true}` |
| GET | `/v1/workspaces/{id}/screen?lines=N` | Plain-text screen mirror, N=1..200, default 60 |

### Instruction + Enter semantics (important)

`submit: true` sends the text via `pane.send_paste()` and then **Enter as a
real key event** via `pane.writer().write_all(b"\r")`.

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

## Window centering

`TermWindow::new_window` centers new windows when no explicit position was
requested (CLI `--position` / mux initial position wins). The size is assumed
to be the geometry's 75% of the same bounds `resolve_geometry` uses
(`GeometryOrigin` → `screens.{virtual_rect,main.rect,active.rect,by_name}`),
then `window.set_window_position(ScreenPoint)` is applied post-creation.

## File map

| File | Role |
|---|---|
| `wezterm-gui/src/harbor_mobile.rs` | Bridge server, pairing, endpoints, QR PNG |
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
  (includes a unit test that the QR PNG is written and decodable).

## Mobile app essentials (see mobile repo `docs/development.md`)

- Package id: `ai.terminalharbor.terminal_harbor_mobile`
- Screen mirror: 1s `Timer.periodic` polling of `/screen` (last 60 lines,
  plain text, monospace, auto-scroll only when near the bottom).
- Instruction input: `Focus(onKeyEvent:)` intercepts Enter with an
  IME-composition guard (Japanese input: 1st Enter confirms conversion,
  2nd Enter sends), plus `TextInputAction.done` for the software keyboard.

## Limitations / TODO

- No TLS; pair URI has `tls=0` and the `fp` fingerprint field is parsed by
  the app but not pinned/verified. LAN-only trust model for now.
- Screen mirror is plain text (no colors/styles) and polling-based, not
  streamed; it targets the **active pane** of the workspace only.
- Device management: tokens never expire and there is no UI to revoke them.
- iOS: same codebase, not yet tested on device/simulator.
