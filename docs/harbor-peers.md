# Harbor-to-Harbor peers

One Terminal Harbor instance can control another over the same HMAC JSON
bridge the mobile app uses. The WezTerm mux protocol is not used.

## Pairing

1. On the host Mac, open **Pair mobile** so a one-time URI is copied to the
   clipboard (and a QR is shown for phones).
2. On the client Mac, copy that URI if needed, then click **Pair another
   Harbor**. The client reads the clipboard and completes HMAC pairing.

The URI `host=` field is a LAN address. Typed `endpoint=` values also list
Tailscale HTTPS, Tailscale direct (`http://<magicdns-or-ip>:7780`), and LAN.
The desktop client tries them in that order, matching the mobile app. Pairing
and later polls use the first endpoint that answers a signed `/v1/identity`
with the saved `server_id`.

If only Tailscale routes fail, the sidebar shows **Use local network**. LAN
HTTP is used only after that click. HMAC protects integrity; it does not hide
request or response bodies on the local network.

Mutual access is two pairings: each Mac is a client of the other.

Credentials live in
`~/Library/Application Support/terminal-harbor/paired-desktops.json` next to
`mobile-devices.json`. Treat both as secrets. Do not log pair URIs, tokens, or
endpoint lists that include tokens.

## Using a remote workspace

A remote row shows the live directory basename from the additive `directory`
field (falling back to `name`). Clicking it:

1. `POST /v1/workspaces/{id}/activate` on the host Mac
2. Opens a local overlay that polls `GET /screen` about once a second
3. Sends typed lines with `POST /instruction` and allowlisted keys with
   `POST /key`

The overlay is not a native remote pane. Space, Tab, Shift+Tab, arrows, Escape,
and Ctrl-C are forwarded as keys; other typing goes through the instruction line.
Esc on an empty line closes the overlay. Unpair removes only the local
credential; it does not change the host Mac.

## Code ownership

| File | Responsibility |
|---|---|
| `wezterm-gui/src/harbor_peer.rs` | HMAC HTTP client, clipboard pairing, endpoint order, `paired-desktops.json` |
| `wezterm-gui/src/overlay/harbor_remote.rs` | Screen overlay; forwards Space, Tab, Shift+Tab, arrows, Escape, Ctrl-C |
| `wezterm-gui/src/termwindow/harbor_sidebar.rs` | **Pair another Harbor**, remote host heading, remote workspace rows |
| `wezterm-gui/src/harbor_mobile.rs` | Host-side `/key` allowlist and additive workspace `directory` |

## Verification

- Pair while both Macs are on Tailscale and not the same LAN, then open a
  remote workspace and send Enter.
- Confirm a Tailscale-only failure shows **Use local network** before LAN is
  used.
- Restart required after shipping the GUI: `wezterm restart` (mux unchanged).
