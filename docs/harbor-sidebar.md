# Terminal Harbor sidebar maintenance

This document records the behavior and maintenance rules for the workspace
rows in the macOS Terminal Harbor sidebar.

## Row contract

Each workspace row is one line of live directory, followed by two agent lines
that appear only while an AI agent is running:

```text
●  directory
  agent
  one-line task summary
```

```text
●  harbor
  Claude
  Removing the workspace folder name from the sidebar
```

- `directory` is always present. It is a basename such as `terminal-harbor`,
  never a full path.
- `agent` and the task summary are omitted entirely when no AI agent is
  running. A row for a plain shell is a single directory line; foreground
  process names such as `zsh` are never shown.
- The summary is omitted when the agent is running but publishes nothing
  usable, leaving a two-line row.
- Every line is truncated to one visual row with `…`. Width is measured in
  cells, so double-width CJK text cannot overflow the panel.

The creation-time workspace name (`HarborWorkspace.name`) is deliberately **not
shown**. It is captured once by `create_from_path()` and never follows `cd` or
tab switches, so displaying it alongside the live directory showed the same
folder twice with one copy permanently stale. The field is still persisted and
still used by `unique_name()`, the launcher palette
(`overlay/launcher.rs`), and the mobile JSON API.

These rules avoid leaking long local paths into the sidebar and keep the
workspace location visible independently of agent status.

## Agent detection and task summary

`harbor_workspace::rows()` resolves both values from the *same* pane, so the
summary always belongs to the agent named above it.

Agent name, in order:

1. A non-empty `TH_AGENT_NAME` user var, used verbatim. The
   `wezterm workspace status --agent <name>` protocol accepts any agent name,
   so this path is not restricted to the known list.
2. The foreground process name via `pane_process_name()`, but only when
   `agent_label()` matches `KNOWN_AGENTS` (`claude`, `codex`, `opencode`).
   Add new agents there.
3. Otherwise no agent is running and the row stays at one line.

### Why the process name comes from `argv[0]`

The name is `argv[0]`, not the executable image, and neither is a synonym for
the other. Claude Code installs itself as
`~/.local/share/claude/versions/<version>` with a `claude` symlink in front, so
the image basename is a version string such as `2.1.226`; the kernel's `p_comm`
is derived from the same path and reports the version too. Only `argv[0]`
carries the word `claude`, so matching on the image alone made Claude Code
invisible to `agent_label()` while Codex, whose image really is named `codex`,
matched.

`LocalProcessInfo::command_name()` reads it — `KERN_PROCARGS2` on macOS,
`/proc/<pid>/cmdline` on Linux, the PEB on Windows — and
`Pane::get_foreground_process_command_name` exposes it, cached in
`CachedLeaderInfo` next to the image path so it shares one `tcgetpgrp` and one
TTL. `process_label()` in the mux server strips the leading `-` of a login
shell and falls back to the image basename when `argv[0]` is unavailable.

### Why the foreground process needs the mux server

The process inspection behind both names is implemented only on `LocalPane`
(`mux/src/localpane.rs`). The GUI attaches to the persistent mux server as a
client, so every pane it sees is a `ClientPane`, which inherits the default
trait impls returning `None` (`mux/src/pane.rs`). Confirm the topology with
`wezterm cli list-clients`: the GUI's own pid appears as a connected client.

The server therefore relays the name as the `TH_PANE_PROCESS` user var from
`maybe_push_pane_changes()` in
`wezterm-mux-server-impl/src/sessionhandler.rs`, which already runs per pane on
activity. `PerPane::foreground_process` holds the last value so the alert is
only synthesized on change; the underlying proc inspection is separately rate
limited by `PROC_INFO_CACHE_TTL`. `ClientPane` stores incoming
`Alert::SetUserVar` into its user vars, so `pane_process_name()` in
`harbor_workspace.rs` reads it back as a fallback.

The constant is duplicated in both crates and pinned by the
`pane_process_user_var_matches_the_mux_server` test — keep them equal.

Note the asymmetry when shipping changes: GUI-only work reaches users through a
session-preserving `wezterm restart`, but anything in this path changes the mux
server and needs `wezterm restart --full`, which terminates every terminal
session.

The pane title cannot substitute for this. `LocalPane::get_title` falls back to
the process basename only when the program sets no title of its own, so a pane
either reports its process (shells) or its task (agents), never both.

Task summary, in order:

1. A non-empty `TH_AGENT_MESSAGE` user var.
2. The pane title, via `summary_from_title()`.

Agents publish their current task as the pane title (OSC 0/2), which is why no
transcript parsing or screen scraping is involved.

Whether an agent does publish one is the agent's own setting, and an agent that
stays silent gets a name line and no summary — the sidebar cannot invent what
was never sent. Claude Code writes its task to the title with no configuration.
Codex does not until `[tui] terminal_title` is set in `~/.codex/config.toml`,
which its `/terminal-title` command writes; `thread-title` and `task-progress`
are the items that carry the work, and `activity` adds the spinner
`strip_status_glyphs()` already removes.

`summary_from_title()`
rejects titles that carry no task information: empty strings, the agent's own
name (opencode titles itself `OpenCode`), the foreground process name, and
shell names. `LocalPane::get_title` substitutes the process name for the
default title, so those must be filtered here.

`strip_status_glyphs()` removes leading decoration before the title is used or
compared. Claude Code prefixes an animated braille spinner and rewrites the
title continuously; without stripping, the sidebar would relayout on every
spinner frame. `termwindow/mod.rs` therefore caches the stripped title per pane
in `harbor_pane_titles` and only calls `invalidate_harbor_sidebar()` when that
text actually changes. Keep the stripping and the comparison in agreement —
comparing raw titles reintroduces the relayout storm.

`agent` and `summary` are sidebar-only derived fields on `HarborWorkspaceRow`,
like `directory`. `process` and `message` are unchanged and remain the source
for the mobile JSON API.

## Directory selection

`harbor_workspace::rows()` resolves the label for every workspace using this
precedence:

1. Take the lowest-ID mux window in the workspace. Workspaces normally have
   one window; this makes the multi-window fallback deterministic.
2. Read that window's active tab.
3. Read the active pane in that tab.
4. Call `get_current_working_dir(CachePolicy::AllowStale)` and keep only the
   final path component.
5. If live CWD is unavailable, use the basename of the persisted workspace
   root.
6. If neither exists, display `Session workspace`.

For a split tab, the selected pane controls the label. For a workspace with
multiple tabs, switching tabs must change the label to the newly active tab's
active-pane directory. Filesystem root is displayed as `/` rather than falling
through to `Session workspace`.

The live CWD comes from the pane URL. Local file URLs are converted to a path;
non-file URLs fall back to their last non-empty path segment. Keep the persisted
workspace root only as a fallback—using it as the primary label would make
`cd` and tab switching appear stale.

## Refresh lifecycle

The sidebar layout is cached in `TermWindow.harbor_sidebar`. Any event that can
change the selected pane or its CWD must call `invalidate_harbor_sidebar()` and
invalidate the window:

- `Alert::CurrentWorkingDirectoryChanged` after OSC 7 or local process CWD
  detection;
- `MuxNotification::PaneFocused` after selecting another split pane;
- `MuxNotification::WindowInvalidated`, which covers active-tab and structural
  window changes;
- `TermWindowNotif::SwitchToMuxWindow`, because the physical window is reused
  for the adjacent workspace after the final tab closes;
- `Alert::WindowTitleChanged`, but only when the stripped title differs from
  the cached one for that pane. `MuxNotification::PaneRemoved` drops the pane's
  cache entry.

If a new tab-activation path is introduced without one of these notifications,
explicitly invalidate the Harbor sidebar there. Do not rebuild the sidebar on
every paint frame; the cached layout is intentional.

## Tab and workspace close lifecycle

Closing a tab is immediate and never opens a confirmation overlay. When it is
the workspace's final tab, Terminal Harbor removes the persisted workspace
entry, activates the following row (or the preceding row at the end), and then
terminates the tab. The workspace owning a tab is resolved from its mux window,
not from the frontend's transient active-workspace value during a switch.

`ensure_current_workspace()` must not persist an empty mux workspace. Teardown
notifications can repaint the old physical window before workspace
reconciliation completes; registering that empty name would resurrect the
workspace that was just closed. The switch notification invalidates the cached
sidebar so every surviving persisted row is rebuilt in its new order. When the
adjacent workspace has no live window, its replacement pane is spawned through
the persistent default domain rather than depending on the pane being closed.

A workspace-restoring session restart starts with an empty mux server. When no explicit
`--workspace` was requested, startup uses the configured default only if it is
still present in the Harbor registry, otherwise it opens the first surviving
row. Falling back unconditionally to the mux default would create a live pane
under the name of a closed default workspace, which the next sidebar paint
would legitimately register again.

## OSC 7 and fallbacks

Shell integration should emit OSC 7 whenever the prompt directory changes.
Without it, local process inspection may still provide a CWD, but it can be
stale or unavailable, especially for remote domains. In that case the sidebar
uses the persisted workspace root by design.

When troubleshooting a stale label:

1. Confirm the active pane reports the expected CWD with `wezterm cli list`.
2. Confirm the shell integration emits OSC 7 after `cd`.
3. Switch panes and tabs and verify the focus/window notifications are
   delivered.
4. Check that the sidebar cache is invalidated in the notification handler.
5. If only one workspace is wrong, compare its live pane CWD with its persisted
   root; do not edit `workspaces-v1.json` as a first-line fix.

## Code ownership

| File | Responsibility |
|---|---|
| `wezterm-gui/src/harbor_workspace.rs` | Directory resolution, fallback order, agent/activity aggregation, `KNOWN_AGENTS`, title normalization, and `HarborWorkspaceRow` |
| `wezterm-gui/src/termwindow/harbor_sidebar.rs` | Detail-line formatting, truncation, wrapping, colors, row rendering, and remote peer rows |
| `wezterm-gui/src/harbor_peer.rs` | HMAC client, clipboard pairing, and `paired-desktops.json` |
| `wezterm-gui/src/overlay/harbor_remote.rs` | Remote screen overlay and allowlisted key forwarding |
| `wezterm-gui/src/termwindow/mod.rs` | Mux notification handling, `harbor_pane_titles`, and sidebar cache invalidation |
| `README.md` | User-facing display behavior |

The persisted registry still stores `root`. API version 1.4.0 adds an additive
`directory` basename on each workspace record so a desktop peer can label
remote rows without a full path. Local sidebar rows still compute that
basename from the live pane; they do not persist `directory`. No data
migration is required.

## Verification

Run:

```sh
cargo test -p wezterm-gui harbor
cargo check -p wezterm-gui
cargo build -p wezterm-gui
```

Manual acceptance checks on macOS. `wezterm cli list --format json` reports the
pane titles and CWDs the sidebar derives from, so expected values can be
checked against it:

- no agent: a single row line with the active directory basename, and no
  `zsh`-style process name;
- Claude: `Claude` on line 2 and its current task on line 3, with no visible
  relayout while only the spinner advances;
- opencode: `OpenCode` on line 2 and no line 3, since it titles itself;
- `wezterm workspace status --agent codex --message "Running tests"` still wins
  over the pane title;
- the creation-time folder name appears nowhere, including after `cd` into an
  unrelated directory;
- `cd` updates line 1 after the next prompt;
- tabs with different CWDs show the selected tab's directory;
- split panes with different CWDs show the selected pane's directory;
- closing a non-final tab shows no confirmation and keeps the workspace row;
- closing the final tab shows no confirmation, removes that workspace row, and
  leaves every other row visible in order, including after a workspace-restoring session restart;
- long and non-ASCII summaries truncate to one row so row heights stay even,
  and no full path is exposed.

## Remote Harbor peers

A paired Mac appears as a host heading under **Pair another Harbor**. Its
workspace rows use the same activity glyph and directory basename as local
rows. They must not run `SwitchToWorkspace` locally: a click opens the remote
screen overlay and sends input over the mobile bridge. Full remote paths stay
off the sidebar. Pair URIs, tokens, and secrets stay out of the sidebar, logs,
and commits. See [`harbor-peers.md`](harbor-peers.md).

Unit tests should continue covering basename extraction (including `/` and
non-ASCII paths), `KNOWN_AGENTS` matching, spinner stripping, uninformative
title rejection, and cell-width truncation.
