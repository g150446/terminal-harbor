//! Agent conversation retrieval for `GET /v1/workspaces/{id}/transcript`.
//!
//! An AI agent's TUI repaints in place, so the terminal keeps roughly one
//! screenful of it: `/screen` cannot go back to the instruction that produced a
//! reply, however many rows are asked for. The agent's own session log can, and
//! this reads that log — never the screen.
//!
//! Only what a person said and what the agent answered is returned. Tool calls,
//! tool output, reasoning and injected context are the model's plumbing, and a
//! conversation view full of them is unreadable.
//!
//! Like `/plan`, an unidentifiable session answers `available: false` with a
//! reason rather than guessing, and responses never carry local paths or
//! session ids (see docs/agent-transcripts.md).

use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use wezterm_gui_subcommands::harbor_agent_session::{self, AGENT_CLAUDE};

/// Newest-first paging window. A page is a screenful on a phone, not a file.
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 200;

/// One message this long is already past what anyone reads on a phone; the rest
/// is cut with a visible marker rather than silently.
const MAX_MESSAGE_CHARS: usize = 8 * 1024;
const TRUNCATION_MARK: &str = "…（長いため以降を省略しました）";

/// Whole-response ceiling, so a page of long messages cannot blow up the phone.
const MAX_PAGE_BYTES: usize = 512 * 1024;

/// Read backwards in windows of this size.
const WINDOW_BYTES: u64 = 256 * 1024;

/// How far back a single request may scan. Session logs reach hundreds of MB.
const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// The agent keeps a conversation log Harbor can read.
    AgentTranscript,
    /// Unsupported, or no agent could be identified.
    Unknown,
}

impl Capability {
    fn as_str(self) -> &'static str {
        match self {
            Capability::AgentTranscript => "agent_transcript",
            Capability::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    NoAgent,
    UnsupportedAgent,
    /// No session could be tied to this pane.
    SessionUnidentified,
    /// A registration exists but cannot belong to a live session.
    StaleSession,
    /// The session is known but its log is gone or unreadable.
    TranscriptMissing,
    /// Several sessions could be the pane's; Harbor will not pick one.
    AmbiguousSession,
    /// The cursor does not belong to the log as it is now.
    StaleCursor,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Reason::NoAgent => "no_agent",
            Reason::UnsupportedAgent => "unsupported_agent",
            Reason::SessionUnidentified => "session_unidentified",
            Reason::StaleSession => "stale_session",
            Reason::TranscriptMissing => "transcript_missing",
            Reason::AmbiguousSession => "ambiguous_session",
            Reason::StaleCursor => "stale_cursor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
    /// The agent's own timestamp for the message, when it records one.
    pub at: Option<String>,
    /// Opaque paging position; pass it back as `before` to read older messages.
    pub cursor: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Claude Code's `~/.claude/projects/**/<session>.jsonl`.
    ClaudeJsonl,
    /// Codex's `~/.codex/sessions/**/rollout-*.jsonl`.
    CodexRollout,
}

impl Format {
    fn source(self) -> &'static str {
        match self {
            Format::ClaudeJsonl => "claude_transcript",
            Format::CodexRollout => "codex_rollout",
        }
    }
}

#[derive(Debug)]
pub struct Located {
    pub path: PathBuf,
    pub format: Format,
}

#[derive(Debug)]
pub enum LocateResult {
    Found(Located),
    Unavailable(Capability, Reason),
}

#[derive(Debug, Default)]
pub struct Page {
    pub messages: Vec<Message>,
    pub has_more: bool,
    pub next_before: Option<String>,
}

/// The pane whose conversation is wanted.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    /// The mux server's pane id, which is what agent hooks see.
    pub pane_id: u64,
    /// The pane's foreground process, when the mux could report one.
    pub process_pid: Option<u32>,
}

/// Where logs and registrations live; injectable so tests need no real home.
pub struct TranscriptEnv {
    pub registry_dir: PathBuf,
    pub claude_config_dir: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    /// Unix seconds the mux server started, when known. Pane ids restart with
    /// the mux server, so a registration older than this cannot be trusted.
    pub mux_started_at: Option<u64>,
}

impl TranscriptEnv {
    pub fn from_system() -> Self {
        Self {
            registry_dir: harbor_agent_session::default_registry_dir(),
            claude_config_dir: match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty())
            {
                Some(dir) => Some(PathBuf::from(dir)),
                None => dirs_next::home_dir().map(|home| home.join(".claude")),
            },
            codex_home: match std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
                Some(dir) => Some(PathBuf::from(dir)),
                None => dirs_next::home_dir().map(|home| home.join(".codex")),
            },
            mux_started_at: fs::metadata(wezterm_gui_subcommands::harbor_mux_socket_path())
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
        }
    }
}

/// One agent's way of finding the log for a pane.
pub trait TranscriptProvider {
    fn locate(&self, target: &Target, env: &TranscriptEnv) -> anyhow::Result<LocateResult>;
}

pub struct ClaudeTranscriptProvider;

impl TranscriptProvider for ClaudeTranscriptProvider {
    fn locate(&self, target: &Target, env: &TranscriptEnv) -> anyhow::Result<LocateResult> {
        use Reason::*;
        let unavailable = |reason| {
            Ok(LocateResult::Unavailable(
                Capability::AgentTranscript,
                reason,
            ))
        };

        let Some(record) = harbor_agent_session::lookup(&env.registry_dir, target.pane_id)
            .filter(|record| record.agent == AGENT_CLAUDE)
        else {
            return unavailable(SessionUnidentified);
        };
        if env
            .mux_started_at
            .is_some_and(|started| record.updated_at < started)
        {
            return unavailable(StaleSession);
        }
        let Some(config_dir) = env
            .claude_config_dir
            .as_deref()
            .and_then(|dir| dir.canonicalize().ok())
        else {
            return unavailable(SessionUnidentified);
        };

        // The record was written by a hook, so re-validate it the same way
        // /plan does: a real file under Claude's own projects directory.
        let transcript = match record.transcript_path.canonicalize() {
            Ok(path) => path,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return unavailable(StaleSession)
            }
            Err(_) => return unavailable(SessionUnidentified),
        };
        if !transcript.starts_with(config_dir.join("projects")) || !transcript.is_file() {
            return unavailable(SessionUnidentified);
        }
        Ok(LocateResult::Found(Located {
            path: transcript,
            format: Format::ClaudeJsonl,
        }))
    }
}

/// Codex has no hook that could register a pane, so the session is identified
/// by the log the pane's own process is writing. Nothing is inferred from the
/// working directory: two Codex sessions in one repository would be
/// indistinguishable that way, and picking the newer one silently shows the
/// wrong conversation.
pub struct CodexTranscriptProvider;

impl TranscriptProvider for CodexTranscriptProvider {
    fn locate(&self, target: &Target, env: &TranscriptEnv) -> anyhow::Result<LocateResult> {
        use Reason::*;
        let unavailable = |reason| {
            Ok(LocateResult::Unavailable(
                Capability::AgentTranscript,
                reason,
            ))
        };

        let (Some(pid), Some(codex_home)) = (
            target.process_pid,
            env.codex_home
                .as_deref()
                .and_then(|dir| dir.canonicalize().ok()),
        ) else {
            return unavailable(SessionUnidentified);
        };
        let sessions_dir = codex_home.join("sessions");

        let mut found: Vec<PathBuf> = procinfo::LocalProcessInfo::open_files(pid)
            .into_iter()
            .filter(|path| is_codex_rollout(path, &sessions_dir))
            .collect();
        found.sort();
        found.dedup();

        match found.len() {
            0 => unavailable(SessionUnidentified),
            1 => Ok(LocateResult::Found(Located {
                path: found.remove(0),
                format: Format::CodexRollout,
            })),
            _ => unavailable(AmbiguousSession),
        }
    }
}

fn is_codex_rollout(path: &Path, sessions_dir: &Path) -> bool {
    path.starts_with(sessions_dir)
        && path.extension().is_some_and(|ext| ext == "jsonl")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-"))
}

#[derive(Debug)]
pub struct TranscriptReply {
    pub status: u16,
    pub body: String,
}

/// Resolve one page of the conversation for the agent in a workspace pane.
///
/// `agent` is the sidebar's label for the pane's agent. There is deliberately
/// no fallback to `/screen`: a screen snapshot is a different thing and the
/// caller has to ask for it, and label it, itself.
pub fn resolve(
    workspace_id: &str,
    agent: Option<&str>,
    target: &Target,
    limit: usize,
    before: Option<&str>,
    env: &TranscriptEnv,
) -> anyhow::Result<TranscriptReply> {
    let located = match agent.map(|a| a.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") => LocateResult::Unavailable(Capability::Unknown, Reason::NoAgent),
        Some(AGENT_CLAUDE) => ClaudeTranscriptProvider.locate(target, env)?,
        Some("codex") => CodexTranscriptProvider.locate(target, env)?,
        Some(_) => LocateResult::Unavailable(Capability::Unknown, Reason::UnsupportedAgent),
    };
    let located = match located {
        LocateResult::Found(located) => located,
        LocateResult::Unavailable(capability, reason) => {
            return Ok(unavailable_reply(workspace_id, agent, capability, reason))
        }
    };

    let before = match before.map(parse_cursor) {
        None => None,
        Some(Some(offset)) => Some(offset),
        Some(None) => {
            return Ok(unavailable_reply(
                workspace_id,
                agent,
                Capability::AgentTranscript,
                Reason::StaleCursor,
            ))
        }
    };

    let page = match read_page(&located.path, located.format, limit, before) {
        Ok(page) => page,
        Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => {
            return Ok(unavailable_reply(
                workspace_id,
                agent,
                Capability::AgentTranscript,
                Reason::StaleCursor,
            ))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(unavailable_reply(
                workspace_id,
                agent,
                Capability::AgentTranscript,
                Reason::TranscriptMissing,
            ))
        }
        // Deliberately not surfaced: the message would name the file.
        Err(_) => anyhow::bail!("failed to read the session log"),
    };

    Ok(available_reply(workspace_id, agent, located.format, page))
}

fn available_reply(
    workspace_id: &str,
    agent: Option<&str>,
    format: Format,
    page: Page,
) -> TranscriptReply {
    let agent = agent.map(str::trim).filter(|a| !a.is_empty());
    let messages: Vec<Value> = page
        .messages
        .iter()
        .map(|message| {
            json!({
                "role": message.role.as_str(),
                "text": message.text,
                "at": message.at,
                "cursor": message.cursor,
                "truncated": message.truncated,
            })
        })
        .collect();
    TranscriptReply {
        status: 200,
        body: json!({
            "workspace_id": workspace_id,
            "agent": agent,
            "capability": Capability::AgentTranscript.as_str(),
            "available": true,
            "source": format.source(),
            "messages": messages,
            "has_more": page.has_more,
            "next_before": page.next_before,
        })
        .to_string(),
    }
}

fn unavailable_reply(
    workspace_id: &str,
    agent: Option<&str>,
    capability: Capability,
    reason: Reason,
) -> TranscriptReply {
    let agent = agent.map(str::trim).filter(|a| !a.is_empty());
    TranscriptReply {
        status: 200,
        body: json!({
            "workspace_id": workspace_id,
            "agent": agent,
            "capability": capability.as_str(),
            "available": false,
            "reason": reason.as_str(),
        })
        .to_string(),
    }
}

/// The cursor is the byte offset of a record's line. Logs are append-only, so
/// an offset keeps pointing at the same record; it carries no path or id.
fn format_cursor(offset: u64) -> String {
    offset.to_string()
}

fn parse_cursor(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

/// Read up to `limit` messages ending just before `before`, oldest first.
///
/// Reads backwards in windows so a page near the end of a hundred-megabyte log
/// costs a few hundred kilobytes, and stops at [`MAX_SCAN_BYTES`] so one
/// request cannot walk an entire history.
pub fn read_page(
    path: &Path,
    format: Format,
    limit: usize,
    before: Option<u64>,
) -> std::io::Result<Page> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut end = match before {
        Some(offset) if offset > len => {
            // The log was replaced or rotated; the cursor is not ours.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cursor is past the end of the log",
            ));
        }
        Some(offset) => offset,
        None => len,
    };
    let floor = len.saturating_sub(MAX_SCAN_BYTES);

    // One extra message proves there is an older page and supplies its cursor.
    let wanted = limit + 1;
    let mut collected: Vec<Message> = Vec::new();
    let mut bytes = 0usize;
    let mut carry: Vec<u8> = Vec::new();
    let mut carry_end = end;

    while collected.len() < wanted && end > floor {
        let start = end.saturating_sub(WINDOW_BYTES).max(floor);
        let mut window = vec![0u8; (end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut window)?;

        // The carried bytes are the head of the line this window ends inside.
        if !carry.is_empty() && carry_end == end {
            window.extend_from_slice(&carry);
            carry.clear();
        }

        let mut lines = line_offsets(&window, start);
        // The first line of a window that does not start the file is partial:
        // hold it for the next (earlier) window.
        if start > floor || (start > 0 && start == floor) {
            if let Some((offset, line)) = lines.first().cloned() {
                if offset == start {
                    carry = line.to_vec();
                    carry_end = start;
                    lines.remove(0);
                }
            }
        }

        for (offset, line) in lines.into_iter().rev() {
            let Some((role, text, at)) = parse_record(line, format) else {
                continue;
            };
            let (text, truncated) = cap_message(text);
            if bytes + text.len() > MAX_PAGE_BYTES && !collected.is_empty() {
                // Stop here rather than overrun. There is definitely more before
                // this point, whatever the count says, so say so outright.
                return Ok(finish_page(collected, limit, true));
            }
            bytes += text.len();
            collected.push(Message {
                role,
                text,
                at,
                cursor: format_cursor(offset),
                truncated,
            });
            if collected.len() >= wanted {
                break;
            }
        }
        end = start;
    }

    // The extra message is the proof that an older page exists.
    Ok(finish_page(collected, limit, false))
}

/// `collected` is newest-first; the reply is oldest-first.
///
/// `more_regardless` is for callers that stopped early and know there is more
/// even though they did not collect the extra message that normally proves it.
fn finish_page(mut collected: Vec<Message>, limit: usize, more_regardless: bool) -> Page {
    let has_more = more_regardless || collected.len() > limit;
    collected.truncate(limit);
    let next_before = collected.last().map(|message| message.cursor.clone());
    collected.reverse();
    Page {
        messages: collected,
        has_more,
        next_before: if has_more { next_before } else { None },
    }
}

/// Offsets and bytes of every complete line in `window`, which starts at
/// `base` in the file.
fn line_offsets(window: &[u8], base: u64) -> Vec<(u64, &[u8])> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (index, byte) in window.iter().enumerate() {
        if *byte == b'\n' {
            out.push((base + start as u64, &window[start..index]));
            start = index + 1;
        }
    }
    if start < window.len() {
        out.push((base + start as u64, &window[start..]));
    }
    out
}

fn cap_message(mut text: String) -> (String, bool) {
    if text.chars().count() <= MAX_MESSAGE_CHARS {
        return (text, false);
    }
    let cut = text
        .char_indices()
        .nth(MAX_MESSAGE_CHARS)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(cut);
    text.push_str(TRUNCATION_MARK);
    (text, true)
}

fn parse_record(line: &[u8], format: Format) -> Option<(Role, String, Option<String>)> {
    let text = std::str::from_utf8(line).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    let record: Value = serde_json::from_str(text).ok()?;
    match format {
        Format::ClaudeJsonl => parse_claude_record(&record),
        Format::CodexRollout => parse_codex_record(&record),
    }
}

/// Tags Claude Code wraps around text it injects into a user turn. What is left
/// after removing them is what the person actually typed.
const CLAUDE_INJECTED_TAGS: &[&str] = &[
    "system-reminder",
    "local-command-caveat",
    "local-command-stdout",
    "command-name",
    "command-message",
    "command-args",
];

/// The same idea for Codex, whose user turns carry environment and plugin
/// context.
const CODEX_INJECTED_TAGS: &[&str] = &[
    "recommended_plugins",
    "app-context",
    "environment_context",
    "user_instructions",
];

fn parse_claude_record(record: &Value) -> Option<(Role, String, Option<String>)> {
    if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let at = record
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    let content = record.pointer("/message/content")?;
    match record.get("type").and_then(Value::as_str)? {
        "user" => {
            let text = match content {
                Value::String(text) => text.clone(),
                Value::Array(blocks) => {
                    // A turn carrying tool output is the loop feeding itself,
                    // not something the person said.
                    if blocks.iter().any(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    }) {
                        return None;
                    }
                    join_text_blocks(blocks, &["text"])
                }
                _ => return None,
            };
            finish_message(Role::User, text, CLAUDE_INJECTED_TAGS, at)
        }
        "assistant" => {
            let blocks = content.as_array()?;
            // `thinking` is internal and `tool_use` is machinery; only prose.
            let text = join_text_blocks(blocks, &["text"]);
            finish_message(Role::Assistant, text, CLAUDE_INJECTED_TAGS, at)
        }
        _ => None,
    }
}

fn parse_codex_record(record: &Value) -> Option<(Role, String, Option<String>)> {
    if record.get("type").and_then(Value::as_str)? != "response_item" {
        return None;
    }
    let at = record
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    let payload = record.get("payload")?;
    if payload.get("type").and_then(Value::as_str)? != "message" {
        return None;
    }
    let role = match payload.get("role").and_then(Value::as_str)? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        // `developer` is the harness talking to the model.
        _ => return None,
    };
    let blocks = payload.get("content")?.as_array()?;
    let text = join_text_blocks(blocks, &["input_text", "output_text", "text"]);
    finish_message(role, text, CODEX_INJECTED_TAGS, at)
}

fn join_text_blocks(blocks: &[Value], types: &[&str]) -> String {
    blocks
        .iter()
        .filter(|block| {
            block
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| types.contains(&kind))
        })
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn finish_message(
    role: Role,
    text: String,
    tags: &[&str],
    at: Option<String>,
) -> Option<(Role, String, Option<String>)> {
    let text = strip_injected_blocks(&text, tags);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some((role, text.to_string(), at))
}

/// Remove `<tag>…</tag>` spans for the named tags, including an unclosed one
/// that runs to the end. Everything else, including the person's own angle
/// brackets, is left alone.
fn strip_injected_blocks(text: &str, tags: &[&str]) -> String {
    let mut out = text.to_string();
    for tag in tags {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        // Search forward from `from` so a near-miss cannot be reconsidered
        // forever. Both bounds land on ASCII, so they stay char boundaries.
        let mut from = 0usize;
        while let Some(offset) = out[from..].find(&open) {
            let start = from + offset;
            // `<tagged-thing>` must not match `<tag>`.
            let after = out[start + open.len()..].chars().next();
            if !matches!(
                after,
                Some('>') | Some(' ') | Some('\n') | Some('\t') | Some('\r') | None
            ) {
                from = start + open.len();
                continue;
            }
            let end = match out[start..].find(&close) {
                Some(offset) => start + offset + close.len(),
                None => out.len(),
            };
            out.replace_range(start..end, "");
            from = start;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use harbor_agent_session::{register, AgentSessionRecord};
    use std::io::Write;

    const WS: &str = "11111111-1111-1111-1111-111111111111";

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "harbor-transcript-{name}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(root.join("claude/projects")).unwrap();
            fs::create_dir_all(root.join("codex/sessions/2026/09/22")).unwrap();
            fs::create_dir_all(root.join("registry")).unwrap();
            Self { root }
        }

        fn env(&self) -> TranscriptEnv {
            TranscriptEnv {
                registry_dir: self.root.join("registry"),
                claude_config_dir: Some(self.root.join("claude")),
                codex_home: Some(self.root.join("codex")),
                mux_started_at: None,
            }
        }

        fn claude_transcript(&self, session: &str, lines: &[String]) -> PathBuf {
            let path = self
                .root
                .join("claude/projects")
                .join(format!("{session}.jsonl"));
            let mut file = File::create(&path).unwrap();
            for line in lines {
                writeln!(file, "{line}").unwrap();
            }
            path
        }

        fn codex_rollout(&self, lines: &[String]) -> PathBuf {
            let path = self
                .root
                .join("codex/sessions/2026/09/22")
                .join("rollout-2026-09-22T10-00-00-abc.jsonl");
            let mut file = File::create(&path).unwrap();
            for line in lines {
                writeln!(file, "{line}").unwrap();
            }
            path
        }

        fn register(&self, pane: u64, session: &str, transcript: &Path) {
            let record = AgentSessionRecord::new(
                pane,
                AGENT_CLAUDE,
                session.to_string(),
                transcript.to_path_buf(),
                None,
            );
            register(&self.root.join("registry"), &record).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn claude_user(text: &str) -> String {
        json!({"type": "user", "timestamp": "2026-09-22T10:00:00Z",
               "message": {"role": "user", "content": text}})
        .to_string()
    }

    fn claude_assistant(text: &str) -> String {
        json!({"type": "assistant", "timestamp": "2026-09-22T10:00:01Z",
               "message": {"role": "assistant",
                           "content": [{"type": "text", "text": text}]}})
        .to_string()
    }

    fn claude_tool_result() -> String {
        json!({"type": "user", "message": {"role": "user",
               "content": [{"type": "tool_result", "tool_use_id": "x", "content": "output"}]}})
        .to_string()
    }

    fn page_of(path: &Path, format: Format, limit: usize, before: Option<u64>) -> Page {
        read_page(path, format, limit, before).unwrap()
    }

    #[test]
    fn claude_prose_survives_and_machinery_does_not() {
        let fixture = Fixture::new("claude-prose");
        let path = fixture.claude_transcript(
            "s1",
            &[
                claude_user("テストを直して"),
                json!({"type": "assistant", "message": {"role": "assistant",
                       "content": [{"type": "thinking", "thinking": "internal"}]}})
                .to_string(),
                json!({"type": "assistant", "message": {"role": "assistant",
                       "content": [{"type": "tool_use", "name": "Bash", "input": {}}]}})
                .to_string(),
                claude_tool_result(),
                claude_assistant("直しました"),
                json!({"type": "user", "isSidechain": true,
                       "message": {"role": "user", "content": "subagent turn"}})
                .to_string(),
            ],
        );

        let page = page_of(&path, Format::ClaudeJsonl, 20, None);

        assert_eq!(
            page.messages
                .iter()
                .map(|m| (m.role, m.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (Role::User, "テストを直して"),
                (Role::Assistant, "直しました"),
            ]
        );
        assert!(!page.has_more);
        assert_eq!(page.next_before, None);
    }

    #[test]
    fn claude_injected_context_is_stripped_and_empty_turns_dropped() {
        let fixture = Fixture::new("claude-injected");
        let path = fixture.claude_transcript(
            "s1",
            &[
                claude_user("<system-reminder>be nice</system-reminder>本題です"),
                claude_user("<local-command-caveat>noise</local-command-caveat>"),
            ],
        );

        let page = page_of(&path, Format::ClaudeJsonl, 20, None);

        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].text, "本題です");
    }

    #[test]
    fn paging_walks_back_without_gaps_or_repeats() {
        let fixture = Fixture::new("paging");
        let lines: Vec<String> = (0..10).map(|i| claude_user(&format!("m{i}"))).collect();
        let path = fixture.claude_transcript("s1", &lines);

        let first = page_of(&path, Format::ClaudeJsonl, 4, None);
        assert_eq!(
            first
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["m6", "m7", "m8", "m9"]
        );
        assert!(first.has_more);

        let cursor = parse_cursor(first.next_before.as_ref().unwrap()).unwrap();
        let second = page_of(&path, Format::ClaudeJsonl, 4, Some(cursor));
        assert_eq!(
            second
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["m2", "m3", "m4", "m5"]
        );
        assert!(second.has_more);

        let cursor = parse_cursor(second.next_before.as_ref().unwrap()).unwrap();
        let third = page_of(&path, Format::ClaudeJsonl, 4, Some(cursor));
        assert_eq!(
            third
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["m0", "m1"]
        );
        assert!(!third.has_more);
        assert_eq!(third.next_before, None);
    }

    #[test]
    fn paging_survives_a_page_larger_than_one_read_window() {
        let fixture = Fixture::new("paging-window");
        // Each message is far bigger than WINDOW_BYTES/4, forcing several reads.
        let big = "あ".repeat(60_000);
        let lines: Vec<String> = (0..8).map(|i| claude_user(&format!("{i}{big}"))).collect();
        let path = fixture.claude_transcript("s1", &lines);

        let page = page_of(&path, Format::ClaudeJsonl, 6, None);

        let seen: Vec<char> = page
            .messages
            .iter()
            .map(|m| m.text.chars().next().unwrap())
            .collect();
        assert_eq!(seen, vec!['2', '3', '4', '5', '6', '7']);
    }

    #[test]
    fn a_tag_that_merely_starts_like_an_injected_one_is_kept() {
        let text = strip_injected_blocks(
            "<command-name-ish>keep</command-name-ish> <command-name>drop</command-name> tail",
            CLAUDE_INJECTED_TAGS,
        );

        assert_eq!(
            text.trim(),
            "<command-name-ish>keep</command-name-ish>  tail"
        );
    }

    #[test]
    fn an_unclosed_injected_block_is_removed_to_the_end() {
        let text = strip_injected_blocks("本文<system-reminder>note", CLAUDE_INJECTED_TAGS);

        assert_eq!(text, "本文");
    }

    #[test]
    fn a_long_message_is_cut_visibly() {
        let fixture = Fixture::new("long");
        let path =
            fixture.claude_transcript("s1", &[claude_user(&"x".repeat(MAX_MESSAGE_CHARS * 2))]);

        let page = page_of(&path, Format::ClaudeJsonl, 20, None);

        assert!(page.messages[0].truncated);
        assert!(page.messages[0].text.ends_with(TRUNCATION_MARK));
    }

    #[test]
    fn codex_keeps_user_and_assistant_only() {
        let fixture = Fixture::new("codex");
        let path = fixture.codex_rollout(&[
            json!({"type": "session_meta", "payload": {"session_id": "x"}}).to_string(),
            json!({"type": "response_item", "timestamp": "2026-09-22T10:00:00Z",
                   "payload": {"type": "message", "role": "developer",
                               "content": [{"type": "input_text", "text": "harness"}]}})
            .to_string(),
            json!({"type": "response_item", "timestamp": "2026-09-22T10:00:01Z",
                   "payload": {"type": "message", "role": "user",
                               "content": [{"type": "input_text",
                                            "text": "<recommended_plugins>x</recommended_plugins>本題"}]}})
            .to_string(),
            json!({"type": "response_item",
                   "payload": {"type": "reasoning", "summary": []}})
            .to_string(),
            json!({"type": "response_item", "timestamp": "2026-09-22T10:00:02Z",
                   "payload": {"type": "message", "role": "assistant",
                               "content": [{"type": "output_text", "text": "承知しました"}]}})
            .to_string(),
        ]);

        let page = page_of(&path, Format::CodexRollout, 20, None);

        assert_eq!(
            page.messages
                .iter()
                .map(|m| (m.role, m.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(Role::User, "本題"), (Role::Assistant, "承知しました")]
        );
    }

    #[test]
    fn a_cursor_past_the_end_is_refused_rather_than_clamped() {
        let fixture = Fixture::new("stale-cursor");
        let path = fixture.claude_transcript("s1", &[claude_user("hi")]);

        let err = read_page(&path, Format::ClaudeJsonl, 20, Some(u64::MAX)).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn an_unregistered_pane_is_unidentified_not_guessed() {
        let fixture = Fixture::new("unregistered");
        let target = Target {
            pane_id: 99,
            process_pid: None,
        };

        let reply = resolve(WS, Some("Claude"), &target, 20, None, &fixture.env()).unwrap();

        assert_eq!(reply.status, 200);
        assert!(reply.body.contains("\"available\":false"));
        assert!(reply.body.contains("session_unidentified"));
    }

    #[test]
    fn a_registration_older_than_the_mux_is_stale() {
        let fixture = Fixture::new("stale");
        let path = fixture.claude_transcript("s1", &[claude_user("hi")]);
        fixture.register(7, "s1", &path);
        let mut env = fixture.env();
        env.mux_started_at = Some(u64::MAX);
        let target = Target {
            pane_id: 7,
            process_pid: None,
        };

        let reply = resolve(WS, Some("Claude"), &target, 20, None, &env).unwrap();

        assert!(reply.body.contains("stale_session"));
    }

    #[test]
    fn a_pane_with_no_agent_says_so() {
        let fixture = Fixture::new("no-agent");
        let target = Target {
            pane_id: 1,
            process_pid: None,
        };

        let reply = resolve(WS, None, &target, 20, None, &fixture.env()).unwrap();

        assert!(reply.body.contains("no_agent"));
        assert!(reply.body.contains("\"capability\":\"unknown\""));
    }

    #[test]
    fn a_reply_carries_no_local_path_or_session_id() {
        let fixture = Fixture::new("privacy");
        let path = fixture.claude_transcript("secret-session-id", &[claude_user("hi")]);
        fixture.register(3, "secret-session-id", &path);
        let target = Target {
            pane_id: 3,
            process_pid: None,
        };

        let reply = resolve(WS, Some("Claude"), &target, 20, None, &fixture.env()).unwrap();

        assert!(reply.body.contains("\"available\":true"));
        assert!(!reply.body.contains("secret-session-id"));
        assert!(!reply
            .body
            .contains(&fixture.root.to_string_lossy().to_string()));
    }

    #[test]
    fn codex_rollout_paths_outside_the_sessions_dir_are_rejected() {
        let sessions = PathBuf::from("/home/u/.codex/sessions");
        assert!(is_codex_rollout(
            &sessions.join("2026/09/22/rollout-x.jsonl"),
            &sessions
        ));
        assert!(!is_codex_rollout(
            &PathBuf::from("/home/u/elsewhere/rollout-x.jsonl"),
            &sessions
        ));
        assert!(!is_codex_rollout(
            &sessions.join("2026/09/22/notes.jsonl"),
            &sessions
        ));
    }
}
