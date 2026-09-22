# Agent conversations (`GET /v1/workspaces/{id}/transcript`)

The mobile bridge can return the conversation an agent is having in a pane —
what the person asked and what the agent answered — read from the agent's own
session log. This document owns that endpoint's contract, how a pane is matched
to a session, what is deliberately left out, and the privacy rules.

## Why it is separate from `/screen`

`/screen` mirrors the terminal. That is enough for a shell, whose output
accumulates in scrollback, but not for an AI agent: an agent's TUI repaints its
view in place, so the terminal keeps roughly one screenful of it no matter how
many rows a client asks for. Measured on a pane running Claude Code, `lines=500`
and `lines=20000` both return about 30 rows, with `truncated: false` — the older
exchange is not truncated, it was never in scrollback.

So a client that wants to go back to the instruction that produced a reply
cannot get there through `/screen`, and no amount of `scrollback_lines` helps.
The agent's own log has the whole conversation and is unaffected by repainting.

The two endpoints have different jobs and never substitute for each other:

- `/screen`: the terminal as the user sees it, including command output and
  build logs, which the conversation log does not have.
- `/transcript`: the exchange between the person and the agent, whole.

`/transcript` never touches the screen and never sends keys. When it reports
`available: false` the bridge does **not** fall back to `/screen`. A caller that
decides a screen snapshot is acceptable must request `/screen` itself and label
the result as a screen snapshot.

## What is returned, and what is not

Only prose: `user` messages the person typed, and `assistant` messages the agent
wrote. Left out on purpose:

- tool calls and their output (`tool_use` / `tool_result`, Codex
  `function_call` / `custom_tool_call`) — the agent talking to itself,
- reasoning (`thinking`, Codex `reasoning`) — internal, and often not meant to
  be read,
- sidechain and sub-agent records — a different conversation,
- Codex `developer` messages — the harness talking to the model,
- context the harness injects into a user turn. The wrappers
  (`<system-reminder>`, `<local-command-caveat>`, `<command-name>` and friends
  for Claude; `<recommended_plugins>`, `<app-context>`,
  `<environment_context>`, `<user_instructions>` for Codex) are removed, and a
  turn that was nothing but injected context is dropped.

A message longer than 8192 characters is cut with a visible marker and
`truncated: true`. A success is never silently shortened.

## Paging

Requests return a page ending at `before` (the newest messages when `before` is
absent), ordered oldest-first within the page:

```
GET /v1/workspaces/{id}/transcript?limit=20
GET /v1/workspaces/{id}/transcript?limit=20&before=<next_before from the last page>
```

`has_more` is true when older messages exist, and `next_before` is the cursor
for the page before this one. A cursor is the byte offset of a record in the
log: session logs are append-only, so an offset keeps pointing at the same
record, and it carries no path, id or content. A cursor past the end of the
current log — a log replaced or rotated — reads as `stale_cursor` rather than
being clamped to something that is not what the caller asked for.

One request reads at most 64 MiB from the end of the log, and a page is capped
at 512 KiB; a page that hits either limit still reports `has_more`.

## Response

Always `200` for a workspace that exists; `404` only when it does not.

Available:

```json
{
  "workspace_id": "…",
  "agent": "Claude",
  "capability": "agent_transcript",
  "available": true,
  "source": "claude_transcript",
  "messages": [
    { "role": "user", "text": "テストを直して", "at": "2026-09-22T10:00:00Z",
      "cursor": "40213", "truncated": false },
    { "role": "assistant", "text": "直しました", "at": "2026-09-22T10:00:31Z",
      "cursor": "41880", "truncated": false }
  ],
  "has_more": true,
  "next_before": "40213"
}
```

Unavailable:

```json
{ "workspace_id": "…", "agent": "Codex", "capability": "agent_transcript",
  "available": false, "reason": "ambiguous_session" }
```

| `capability` | Meaning |
| --- | --- |
| `agent_transcript` | The agent keeps a log Harbor can read. `available: false` means this pane's session could not be identified. |
| `unknown` | Unsupported agent, or no agent is running in the pane. |

Both supported agents keep a readable log, so there is no "this agent has none"
capability; an agent that genuinely kept none would need one adding here.

| `reason` | Meaning |
| --- | --- |
| `no_agent` | No agent is identified in the workspace's active pane. |
| `unsupported_agent` | An agent Harbor has no provider for. |
| `session_unidentified` | The agent is running but its session could not be tied to this pane: for Claude, no valid hook registration; for Codex, no session log open in the pane's process. |
| `stale_session` | A registration exists but cannot belong to a live session: it predates the current mux server, or its log no longer exists. |
| `transcript_missing` | The session is known but its log is gone or unreadable. |
| `ambiguous_session` | Several sessions could be this pane's. Harbor does not pick one. |
| `stale_cursor` | The cursor does not belong to the log as it is now. |

The response never contains a local path, session id, or transcript path, and
neither do the error messages or logs.

`/transcript` resolves the same pane `/screen` and `/plan` do: the active pane
of the workspace's active tab.

## How a pane is matched to a session

**Claude Code** — the same registration `/plan` uses. Its hooks
(`SessionStart`, `UserPromptSubmit`, `SessionEnd`) run
`wezterm agent-session register|end --agent claude`, which stores one record per
pane keyed by the mux server's pane id. See
[`agent-plans.md`](agent-plans.md#how-a-pane-is-matched-to-a-session) for the
mechanism and [its enabling steps](agent-plans.md#enabling-the-hooks-opt-in);
the same installation serves both endpoints. The recorded transcript is
re-validated on every request: canonicalized, and required to be a regular file
under `<claude config dir>/projects`.

**Codex** — Codex has no hook that could register a pane, so the session is
identified by the log the pane's own process is writing: the foreground
process's open descriptors are read (`PROC_PIDLISTFDS`, then
`PROC_PIDFDVNODEPATHINFO`), and a descriptor pointing at
`<codex home>/sessions/**/rollout-*.jsonl` identifies the session.

Nothing is inferred from the working directory. Two Codex sessions in one
repository are indistinguishable that way, and silently picking the newer one
would show someone else's conversation. No candidate reads as
`session_unidentified`, more than one as `ambiguous_session`.

This requires the mux server and the GUI to be on the same machine, which is
how Harbor runs. If the pid cannot be inspected, the answer is
`session_unidentified` — never a guess.

## Privacy

This endpoint deliberately reverses the rule that session logs are local-only
state: their *content* is what the caller asked for. Everything else about them
stays local. Responses and errors carry no filesystem path, no session id and no
transcript path, and cursors are byte offsets rather than identifiers.

What the endpoint does return is the conversation itself, which can contain
anything that was said in it, including secrets that were pasted or read aloud
into the session. It is served over the same authenticated, device-paired
transport as every other bridge endpoint, and the bridge is not exposed beyond
the trusted network (see [`mobile-bridge.md`](mobile-bridge.md)).

## Adding another agent

Implement `TranscriptProvider` in `wezterm-gui/src/harbor_transcript.rs` and
select it in `resolve()`. A provider has two jobs: identify the pane's log
without guessing, and name the log's format. Parsing lives with the format
(`Format::ClaudeJsonl`, `Format::CodexRollout`), so an agent that writes one of
those needs only the locating half.

## Verifying

Unit tests cover the record filters, paging across read windows, the message cap
and every unavailable reason (`cargo +stable test -p wezterm-gui
harbor_transcript`), and the descriptor-path layout against the live kernel
(`cargo +stable test -p procinfo open_files`). Unit tests cannot see the
`ClientPane` topology, so also verify against the running app: with the hooks
installed, hold a short conversation with Claude in a Harbor pane, then check
that `GET /v1/workspaces/{id}/transcript` returns those turns and that walking
`next_before` back reaches the first one without gaps or repeats. Repeat with
two Claude sessions in the same directory in different panes, with one Codex
pane, and with two Codex panes in the same directory (which must read
`ambiguous_session`).

Restart mode: bridge and GUI only, so `wezterm restart`.
