# Restarting Terminal Harbor

Terminal Harbor provides two restart modes from the **Terminal Harbor** app
menu and from the command line.

```console
# Restart the GUI and keep shells, Codex agents, and scrollback alive.
wezterm restart

# Restart both the GUI and session host. This ends all terminal sessions.
wezterm restart --full
```

The session-preserving mode is the default so that an AI agent can rebuild and
restart Terminal Harbor without terminating itself. Terminal sessions run in a
private local mux server; the old GUI disconnects and the new GUI reconnects to
that server. The CLI fails instead of starting a new application when no GUI is
running.

## Tradeoffs

Keeping sessions requires a background `wezterm-mux-server` process. A GUI-only
restart applies frontend and menu changes, but it does not replace mux, PTY, or
process-inspection code, nor refresh the session host's startup environment.
Use `wezterm restart --full` when testing those changes. A preserving restart is
also rejected when the installed GUI and running mux server are protocol
incompatible.

The first build that introduces persistent sessions cannot migrate terminals
that were already owned by an older GUI process. Perform one complete restart;
sessions opened by subsequent builds can then survive GUI restarts.

A complete restart and a normal application quit still end the processes,
tabs, split panes, scrollback, and other terminal state. Before stopping the
session host, Terminal Harbor records the active pane's working directory for
each workspace. Opening those workspaces after relaunch creates one terminal in
the recorded directory. If that directory is no longer available, Terminal
Harbor falls back to the workspace root and then the home directory.

The workspace the application opens into is restored by the startup path rather
than by the sidebar, because its window already exists by the time the sidebar
is drawn and activating a workspace that has a window only switches to it. That
window is spawned through `startup_cwd()` in `harbor_workspace.rs`. A directory
missed there is missed until the next restart, so both paths have to stay
wired: `resume_cwd()` for a workspace activated from the sidebar, `startup_cwd()`
for the one the application lands in. `startup_cwd()` deliberately has no home
directory fallback, so a first ever launch still opens where the domain would
have put it.

With no explicit `--workspace`, a fresh GUI uses the configured default only
when that mux workspace is still in the persisted Harbor registry; otherwise it
starts in the first surviving row. This makes the registry authoritative after
a workspace is closed. In particular, an empty mux server after a complete
restart must not recreate a configured default that the user removed.

Launching with an explicit program or `--cwd` keeps that directory instead; only
a plain launch, which is what the restart helper performs, is resumed.

## Releases that require a full restart

Sidebar agent activity relies on the session host publishing `TH_PANE_PROCESS`
(see [`harbor-sidebar.md`](harbor-sidebar.md)). That code lives in the mux
server, so installing the bundle and running a preserving restart leaves the
agent and task lines blank: the old host keeps running and never sends the
variable. Run `wezterm restart --full` once after installing that build.

This is the general rule rather than a one-off. Any release touching the mux
server, the wire protocol, or process-inspection code must state the required
restart mode, because a preserving restart silently keeps the old host.
