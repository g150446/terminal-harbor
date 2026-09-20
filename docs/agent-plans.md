# Agent plans (`GET /v1/workspaces/{id}/plan`)

The mobile bridge can return the exact plan file an agent wrote, separately from
the terminal screen. This document owns that endpoint's contract, how a pane is
matched to an agent session, the safety rules, and how to enable it.

## Why it is separate from `/screen`

`/screen` mirrors what the terminal currently shows, including scrollback. A plan
read from it is cut off at the visible rows, is mixed with unrelated output, and
inside an alternate-screen TUI may never have been drawn into scrollback at all.
Reading the agent's own file has none of those problems, so the two endpoints
have different jobs and never substitute for each other:

- `/screen`: the terminal as the user sees it.
- `/plan`: the whole plan file the agent saved.

`/plan` never touches the screen and never sends keys. When it reports
`available: false` the bridge does **not** fall back to `/screen`. A caller that
decides a screen snapshot is acceptable must request `/screen` itself and label
the result as a screen snapshot.

## Response

Always `200` for a workspace that exists, including "there is no plan", except
`413` for a plan over the size limit. `404` only when the workspace does not
exist. `capability` and `available` are separate on purpose.

Available:

```json
{
  "workspace_id": "…",
  "agent": "Claude",
  "capability": "agent_file",
  "available": true,
  "source": "claude_plan_file",
  "text": "# Plan\n…",
  "updated_at": "2026-09-20T12:34:56Z",
  "content_sha256": "…",
  "complete": true
}
```

`text` is the file's bytes as UTF-8, unmodified; `content_sha256` is over those
bytes and `updated_at` is the file's modification time. `complete` is always
`true`: there is no truncated success. A file over 1 MiB (`MAX_PLAN_BYTES`) is
`413` with `reason: "plan_too_large"`, and a file that is not valid UTF-8 is a
`500`.

Unavailable:

```json
{
  "workspace_id": "…",
  "agent": "Codex",
  "capability": "none",
  "available": false,
  "reason": "agent_has_no_plan_file"
}
```

| `capability` | Meaning |
| --- | --- |
| `agent_file` | The agent keeps plans in files Harbor can read (Claude Code). `available: false` means this session has none to show yet. |
| `none` | The agent has no dedicated plan file (Codex). |
| `unknown` | Unsupported agent, or no agent is running in the pane. |

| `reason` | Meaning |
| --- | --- |
| `no_agent` | No agent is identified in the workspace's active pane. |
| `unsupported_agent` | An agent Harbor has no plan provider for (OpenCode, Cursor, custom names). |
| `agent_has_no_plan_file` | Codex. Its plan is not extracted from the session log or screen. |
| `session_unidentified` | Claude is running but no valid hook registration exists for the pane (hooks not installed, or the registration failed validation). |
| `stale_session` | A registration exists but cannot belong to a live session: it predates the current mux server, or its transcript no longer exists. |
| `plan_not_created` | The session has not named a plan file. |
| `plan_file_missing` | The session named a plan file that is gone, or is not a regular file under Claude's `plans` directory. These are deliberately not distinguished so the API cannot be used to probe other paths. |
| `plan_too_large` | Over 1 MiB. Sent with status `413`. |

The response never contains a local path, session id, or transcript path, and
neither do the error messages or logs.

`/plan` resolves the same pane `/screen` does: the active pane of the
workspace's active tab.

## How a pane is matched to a session

Choosing "the newest transcript for the workspace's directory" is wrong when two
Claude sessions run in the same repository: one pane would show the other's
plan. Harbor instead asks the agent to say which session is in which pane.

1. Claude Code's hooks (`SessionStart`, `UserPromptSubmit`, `SessionEnd`) run
   `wezterm agent-session register|end --agent claude`.
2. The command reads the hook payload on stdin (`session_id`, `transcript_path`,
   `cwd`) and the pane id from `WEZTERM_PANE`, which is the **mux server's** pane
   id, and stores one record per pane at
   `<data dir>/terminal-harbor/agent-sessions/<pane id>.json` (directory `0700`,
   files `0600`, written atomically). The implementation is shared in
   `wezterm-gui-subcommands/src/harbor_agent_session.rs`.
3. The GUI's panes are `ClientPane`s, so the bridge uses `ClientPane::remote_pane_id`
   to look the record up. This needs no mux wire-protocol change.
4. `SessionEnd` removes the record only if it still names the ending session, so
   a resumed session that already took over the pane keeps its registration.
5. If the hook runs outside a Harbor pane (`WEZTERM_PANE` unset), it does nothing.

Pane ids restart when the mux server restarts, so a record older than the mux
server's start (the modification time of `terminal-harbor-mux.sock`) reads as
`stale_session` rather than being applied to an unrelated new pane.

## Claude provider

1. The registered transcript is canonicalized and must be a regular file under
   `<claude config dir>/projects`.
2. The tail of the transcript (last 64 MiB) is scanned for the **latest**
   `planFilePath`, accepted only from a `plan_mode` attachment or a tool call's
   input, and only from the main session: sidechain and sub-agent records, and
   free text that merely mentions the key, are ignored. The embedded
   `planContent` is never used.
3. The path must be absolute; it is canonicalized, and after resolving symlinks
   must be a regular file under `<claude config dir>/plans`. `..` traversal,
   symlinks pointing out, and other directories are rejected.
4. The file is read whole from the opened handle, capped at 1 MiB.

`<claude config dir>` is `CLAUDE_CONFIG_DIR` of the **GUI process** if set,
otherwise `~/.claude`. A Claude launched with a different `CLAUDE_CONFIG_DIR`
than the GUI's will read as `session_unidentified`.

## Adding another agent

Implement `PlanProvider` in `wezterm-gui/src/harbor_plan.rs` and select it in
`resolve()`. Codex has no provider on purpose: it has no dedicated plan file,
and reconstructing a plan from its session log or the screen is not done.

## Enabling the hooks (opt-in)

Nothing changes in the agent's configuration unless you ask. From the installed
app's `wezterm`:

```sh
"/Applications/Terminal Harbor.app/Contents/MacOS/wezterm" agent-session install-hooks --agent claude
# review the output, then:
"/Applications/Terminal Harbor.app/Contents/MacOS/wezterm" agent-session install-hooks --agent claude --apply
```

Without `--apply` nothing is written. With it, the existing
`<claude config dir>/settings.json` is copied to
`settings.json.harbor-backup-<unix seconds>`, the Harbor hooks are **merged**
(existing hooks and settings are kept; a file that is not valid JSON or has an
unexpected shape is refused), and the result is written atomically. Re-running is
a no-op. JSON key order in the file is normalized (sorted) by the rewrite; the
backup keeps the original. Restart running Claude Code sessions afterwards.

The hook command embeds the absolute path of the `wezterm` that ran the
installer. Run it from the installed bundle, not from `target/`. To remove the
hooks, delete the three entries containing `agent-session` from `settings.json`
or restore the backup.

There is no Codex installer: Codex needs no registration for `/plan`.

## Verifying

Unit tests cover the resolver (`cargo +stable test -p wezterm-gui harbor_plan`),
the record store (`-p wezterm-gui-subcommands harbor_agent_session`), and the
hook CLI and merge (`-p wezterm harbor_agent_hooks`). Unit tests cannot see the
`ClientPane` topology, so also verify against the running app: with the hooks
installed, start Claude in a Harbor pane, create a plan, and compare
`GET /v1/workspaces/{id}/plan` `text` with the file byte for byte
(`content_sha256` should equal `shasum -a 256` of it). Repeat with two Claude
sessions in the same directory in different panes, and once inside an alternate
screen.

Restart mode: bridge and GUI only, so `wezterm restart`. The mux server and wire
protocol are unchanged.
