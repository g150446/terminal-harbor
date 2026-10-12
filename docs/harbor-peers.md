# Harbor-to-Harbor peers

One Terminal Harbor instance can control another over the same HMAC JSON
bridge the mobile app uses. The WezTerm mux protocol is not used.

## Pairing

Two routes lead to the same HMAC device credential: a one-time pairing code, or
a Tailscale device id when the target opts in to no-code pairing.

### With the one-time pairing code

1. On the host machine, open **Pair mobile** so a one-time URI is copied to the
   clipboard (and a QR is shown for phones).
2. On the client machine, copy that URI if needed, then click **Pair another
   Harbor**. The client reads the clipboard and completes HMAC pairing.

The URI `host=` field is a LAN address. Typed `endpoint=` values also list
Tailscale HTTPS, Tailscale direct (`http://<magicdns-or-ip>:7780`), and LAN.
The desktop client tries them in that order, matching the mobile app. Pairing
and later polls use the first endpoint that answers a signed `/v1/identity`
with the saved `server_id`.

### By Tailscale device id, without a pairing code

For a peer that is reachable over Tailscale and owned by the same Tailscale
user, the code can be skipped.

1. On the machine that will be controlled, click **Tailscale auto-pair: off**
   until it reads **on**. This is the opt-in switch; it is off by default and
   persists as `allow_tailnet_pairing` in `settings-v1.json` (the file keeps its
   name while the stored `schema_version` moves to 2, and version 1 files still
   load with the switch off).
2. On the controlling machine, copy the peer's MagicDNS name
   (`pc-office.tailnet-abc123.ts.net`; its Tailscale machine name or one of its
   tailnet addresses also resolve) and click **Pair via Tailscale (clipboard)**.
   A `harbor://pair` URI on the clipboard still works from this row.

The client resolves the id from its own `tailscale status --json` and posts
`/v1/pair/tailnet` to port 7780 on each address of that node. The request
carries no secret. The host accepts it only when all of these hold:

- the opt-in switch is on;
- the TCP source is a Tailscale address — `100.64.0.0/10` or the `fc00::/7`
  range Tailscale assigns — so a LAN or loopback source is refused before any
  Tailscale lookup happens;
- the node owning that address reports the same tailnet user ID as this
  machine's own node. Identity comes from the local `tailscale status --json`,
  with `tailscale whois --json` as the fallback when that document is
  unavailable in the GUI launch environment.

The host then mints the one-time token itself, the way loopback pairing does,
derives the device secret with the same HKDF as the pairing-code flow, signs
the response, and stores a device record tagged with the peer's Tailscale node
id. The client derives the same key from the `server_id` it read from that
node's unsigned `/v1/identity` plus the returned token, and requires the
response to be signed with it. That signature is a consistency check rather than
the proof of who the peer is: the address the client dialed came from its own
`tailscale status` data, and Tailscale's own WireGuard authentication is what
makes the node that owns the address the one that answers. The host-side
same-user check is the authorization on the other side, and the signed
`/v1/identity` and `/v1/session` polls keep the credential bound to that
machine's `server_id` afterwards. Re-pairing the same tailnet node replaces its
record instead of stacking credentials, and leaves any pairing made with a
pairing code alone.

**What this trades away.** Trust moves from a secret you carried across to
Tailscale account membership. While the switch is on, every device owned by
the same Tailscale user can obtain full control with no prompt on the host.
Turn the switch off when pairing is done. HMAC still protects integrity after
pairing; the bridge remains plain HTTP on `:7780`, so nothing here hides
request or response bodies from another node on the tailnet.

If only Tailscale routes fail, the sidebar shows **Use local network**. LAN
HTTP is used only after that click, and a tailnet pairing never drops to LAN on
its own.

Mutual access is two pairings: each machine is a client of the other.

Credentials live in
`~/Library/Application Support/terminal-harbor/paired-desktops.json` next to
`mobile-devices.json`. Treat both as secrets. Do not log pair URIs, tokens,
Tailscale node ids, or endpoint lists that include tokens.

## Using a remote workspace

A remote row shows the live directory basename from the additive `directory`
field (falling back to `name`). Clicking it:

1. `POST /v1/workspaces/{id}/activate` on the host machine
2. Opens a local overlay that polls `GET /screen` about once a second
3. Sends typed lines with `POST /instruction` and allowlisted keys with
   `POST /key`

The overlay is not a native remote pane. Space, Tab, Shift+Tab, arrows, Escape,
and Ctrl-C are forwarded as keys; other typing goes through the instruction line.
Esc on an empty line closes the overlay. Unpair removes only the local
credential; it does not change the host machine.

## Code ownership

| File | Responsibility |
|---|---|
| `wezterm-gui/src/harbor_peer.rs` | HMAC HTTP client, clipboard pairing, Tailscale device-id resolution, endpoint order, `paired-desktops.json` |
| `wezterm-gui/src/overlay/harbor_remote.rs` | Screen overlay; forwards Space, Tab, Shift+Tab, arrows, Escape, Ctrl-C |
| `wezterm-gui/src/termwindow/harbor_sidebar.rs` | **Pair another Harbor**, **Pair via Tailscale (clipboard)**, **Tailscale auto-pair**, remote host heading, remote workspace rows |
| `wezterm-gui/src/harbor_mobile.rs` | Host-side `/key` allowlist, additive workspace `directory`, Tailscale status parsing, `/v1/pair/tailnet` |
| `wezterm-gui/src/harbor_settings.rs` | `allow_tailnet_pairing` opt-in (schema v2) |

## Verification

- Pair while both machines are on Tailscale and not the same LAN, then open a
  remote workspace and send Enter.
- Confirm a Tailscale-only failure shows **Use local network** before LAN is
  used.
- With **Tailscale auto-pair** off, `Pair via Tailscale (clipboard)` fails and
  the host answers `/v1/pair/tailnet` with `403`. With it on, the pairing
  succeeds and `paired-desktops.json` records a `tailscale_direct` endpoint as
  `last_successful_url`.
- Refusals worth checking by hand: a different Tailscale user's device, and any
  LAN or loopback source. Both must be `403` and must not store a credential.
- `cargo test -p wezterm-gui harbor` covers the address ranges, the same-user
  policy, device-id resolution, and the pairing request shape.
- Connecting to a Windows peer requires that machine to run this build, whose
  bridge listens on `:7780` and whose Tailscale CLI is found at
  `C:\Program Files\Tailscale\tailscale.exe`.
- Restart required after shipping the GUI: `wezterm restart` (mux unchanged).
