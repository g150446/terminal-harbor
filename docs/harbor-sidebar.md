# Terminal Harbor sidebar maintenance

This document records the behavior and maintenance rules for the workspace
rows in the macOS Terminal Harbor sidebar.

## Row contract

Each workspace row has a title and one or more detail lines:

```text
●  Workspace name
  directory · agent
  optional status message
```

- `directory` is always present. It is a basename such as `terminal-harbor`,
  never a full path.
- `agent` is the existing agent or foreground-process label and is omitted
  when unavailable.
- A non-empty `TH_AGENT_MESSAGE` is rendered on the following line. It must
  not replace or hide the directory and agent line.
- The directory remains visible when no AI agent is running.

These rules avoid leaking long local paths into the sidebar and keep the
workspace location visible independently of agent status.

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
  window changes.

If a new tab-activation path is introduced without one of these notifications,
explicitly invalidate the Harbor sidebar there. Do not rebuild the sidebar on
every paint frame; the cached layout is intentional.

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
| `wezterm-gui/src/harbor_workspace.rs` | Directory resolution, fallback order, agent/activity aggregation, and `HarborWorkspaceRow` |
| `wezterm-gui/src/termwindow/harbor_sidebar.rs` | Detail-line formatting, wrapping, colors, and row rendering |
| `wezterm-gui/src/termwindow/mod.rs` | Mux notification handling and sidebar cache invalidation |
| `README.md` | User-facing display behavior |

The mobile workspace API continues to expose the persisted `root`; the
sidebar-only `directory` field is not part of the JSON API or persistence
schema. Changes to this display therefore require no data migration.

## Verification

Run:

```sh
cargo test -p wezterm-gui harbor
cargo check -p wezterm-gui
cargo build -p wezterm-gui
```

Manual acceptance checks on macOS:

- no agent: the active directory basename is visible;
- active agent: `directory · agent` is visible;
- agent message: the first detail line remains and the message appears below;
- `cd` updates the label after the next prompt;
- tabs with different CWDs show the selected tab's directory;
- split panes with different CWDs show the selected pane's directory;
- long and non-ASCII directory names wrap without exposing their full path.

Unit tests should continue covering basename extraction (including `/` and
non-ASCII paths) and all directory/agent/message formatting combinations.
