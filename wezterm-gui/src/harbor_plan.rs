//! Agent plan retrieval for `GET /v1/workspaces/{id}/plan`.
//!
//! The plan is the file the agent itself wrote, read whole. Nothing here looks
//! at the terminal screen: a screen-derived plan would be cut off at the
//! visible rows and could not be told apart from ordinary output. When no file
//! can be identified the answer says so (`available: false` plus a reason)
//! instead of guessing.
//!
//! Errors and responses never carry local paths or session ids; both are
//! sensitive local state (see docs/agent-plans.md).

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use wezterm_gui_subcommands::harbor_agent_session::{self, AGENT_CLAUDE};

/// A success is always the whole file, so a file over this limit is reported as
/// too large rather than returned truncated.
pub const MAX_PLAN_BYTES: u64 = 1024 * 1024;
/// Only the tail of a session transcript is searched for the plan path: the
/// latest reference is what matters, and transcripts can reach hundreds of MB.
const MAX_TRANSCRIPT_SCAN_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// The agent keeps plans in files Harbor can read.
    AgentFile,
    /// The agent has no dedicated plan file.
    None,
    /// Unsupported, or no agent could be identified.
    Unknown,
}

impl Capability {
    fn as_str(self) -> &'static str {
        match self {
            Capability::AgentFile => "agent_file",
            Capability::None => "none",
            Capability::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    NoAgent,
    UnsupportedAgent,
    SessionUnidentified,
    PlanNotCreated,
    /// The recorded plan file is gone, or is not a file Harbor may read.
    PlanFileMissing,
    AgentHasNoPlanFile,
    StaleSession,
    PlanTooLarge,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Reason::NoAgent => "no_agent",
            Reason::UnsupportedAgent => "unsupported_agent",
            Reason::SessionUnidentified => "session_unidentified",
            Reason::PlanNotCreated => "plan_not_created",
            Reason::PlanFileMissing => "plan_file_missing",
            Reason::AgentHasNoPlanFile => "agent_has_no_plan_file",
            Reason::StaleSession => "stale_session",
            Reason::PlanTooLarge => "plan_too_large",
        }
    }
}

#[derive(Debug)]
pub struct PlanFile {
    pub source: &'static str,
    pub text: String,
    pub updated_at: SystemTime,
}

#[derive(Debug)]
pub enum PlanResult {
    Available(PlanFile),
    Unavailable(Capability, Reason),
}

/// Where plans and registrations live; injectable so tests need no real home.
pub struct PlanEnv {
    pub registry_dir: PathBuf,
    pub claude_config_dir: Option<PathBuf>,
    /// Unix seconds the mux server started, when known. Pane ids restart with
    /// the mux server, so a registration older than this cannot be trusted.
    pub mux_started_at: Option<u64>,
}

impl PlanEnv {
    pub fn from_system() -> Self {
        Self {
            registry_dir: harbor_agent_session::default_registry_dir(),
            claude_config_dir: match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty())
            {
                Some(dir) => Some(PathBuf::from(dir)),
                None => dirs_next::home_dir().map(|home| home.join(".claude")),
            },
            mux_started_at: fs::metadata(wezterm_gui_subcommands::harbor_mux_socket_path())
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
        }
    }
}

/// One agent's way of finding its plan. Codex has no plan file, so it has no
/// provider; a future agent with a stable file adds one here.
pub trait PlanProvider {
    fn resolve(&self, pane_id: u64, env: &PlanEnv) -> anyhow::Result<PlanResult>;
}

pub struct ClaudePlanProvider;

impl PlanProvider for ClaudePlanProvider {
    fn resolve(&self, pane_id: u64, env: &PlanEnv) -> anyhow::Result<PlanResult> {
        use Reason::*;
        let unavailable = |reason| Ok(PlanResult::Unavailable(Capability::AgentFile, reason));

        let Some(record) = harbor_agent_session::lookup(&env.registry_dir, pane_id)
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

        // The record was written by a hook, so re-validate it: the transcript
        // must be a real file under Claude's own projects directory.
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

        let Some(plan_path) = last_plan_file_path(&transcript)
            .map_err(|_| anyhow::anyhow!("failed to read the session transcript"))?
        else {
            return unavailable(PlanNotCreated);
        };
        read_plan_file(&plan_path, &config_dir.join("plans"))
    }
}

/// Read `path` if, after resolving symlinks, it is a regular file under
/// `plans_dir`. Everything else that is not a size problem reads as missing so
/// a caller cannot use the API to probe what exists elsewhere on disk.
fn read_plan_file(path: &Path, plans_dir: &Path) -> anyhow::Result<PlanResult> {
    use Reason::*;
    let missing = Ok(PlanResult::Unavailable(
        Capability::AgentFile,
        PlanFileMissing,
    ));
    let too_large = Ok(PlanResult::Unavailable(Capability::AgentFile, PlanTooLarge));

    if !path.is_absolute() {
        return missing;
    }
    let Ok(canonical) = path.canonicalize() else {
        return missing;
    };
    let Ok(plans_dir) = plans_dir.canonicalize() else {
        return missing;
    };
    if !canonical.starts_with(&plans_dir) {
        return missing;
    }
    let Ok(file) = File::open(&canonical) else {
        return missing;
    };
    let Ok(meta) = file.metadata() else {
        return missing;
    };
    if !meta.is_file() {
        return missing;
    }
    if meta.len() > MAX_PLAN_BYTES {
        return too_large;
    }
    let mut bytes = Vec::new();
    file.take(MAX_PLAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("failed to read the plan file"))?;
    // The file can grow between the metadata check and the read.
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return too_large;
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("the plan file is not valid UTF-8"))?;
    Ok(PlanResult::Available(PlanFile {
        source: "claude_plan_file",
        text,
        updated_at: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    }))
}

/// The plan file path most recently named by the session's own records.
///
/// Only two structured places count: the `plan_mode` attachment Claude Code
/// writes when plan mode is active, and the input of a tool call. Free text
/// that merely mentions `planFilePath` (for example a conversation about this
/// very feature) is not trusted, and neither are sub-agent records. The
/// `planContent` Claude Code also embeds is ignored on purpose: the file is
/// the source of truth.
fn last_plan_file_path(transcript: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut file = File::open(transcript)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(MAX_TRANSCRIPT_SCAN_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    if start > 0 {
        // Landed mid-line; drop the partial one.
        reader.read_until(b'\n', &mut line)?;
    }
    let mut found = None;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        if !text.contains("planFilePath") {
            continue;
        }
        if let Some(path) = plan_path_from_record(text) {
            found = Some(path);
        }
    }
    Ok(found)
}

fn plan_path_from_record(line: &str) -> Option<PathBuf> {
    let record: Value = serde_json::from_str(line).ok()?;
    if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    match record.get("type").and_then(Value::as_str)? {
        "attachment" => {
            let attachment = record.get("attachment")?;
            if attachment.get("type").and_then(Value::as_str) != Some("plan_mode")
                || attachment.get("isSubAgent").and_then(Value::as_bool) == Some(true)
            {
                return None;
            }
            attachment
                .get("planFilePath")
                .and_then(Value::as_str)
                .map(PathBuf::from)
        }
        "assistant" => record
            .pointer("/message/content")?
            .as_array()?
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter_map(|block| block.pointer("/input/planFilePath")?.as_str())
            .last()
            .map(PathBuf::from),
        _ => None,
    }
}

#[derive(Debug)]
pub struct PlanReply {
    pub status: u16,
    pub body: String,
}

/// Resolve the plan for the agent in one workspace pane.
///
/// `agent` is the sidebar's label for the pane's agent. There is deliberately no
/// fallback to the screen: the caller decides whether a screen snapshot is an
/// acceptable substitute, and labels it as one.
pub fn resolve(
    workspace_id: &str,
    agent: Option<&str>,
    pane_id: u64,
    env: &PlanEnv,
) -> anyhow::Result<PlanReply> {
    let result = match agent.map(|a| a.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") => PlanResult::Unavailable(Capability::Unknown, Reason::NoAgent),
        Some(AGENT_CLAUDE) => ClaudePlanProvider.resolve(pane_id, env)?,
        Some("codex") => PlanResult::Unavailable(Capability::None, Reason::AgentHasNoPlanFile),
        Some(_) => PlanResult::Unavailable(Capability::Unknown, Reason::UnsupportedAgent),
    };
    Ok(plan_reply(workspace_id, agent, result))
}

fn plan_reply(workspace_id: &str, agent: Option<&str>, result: PlanResult) -> PlanReply {
    let agent = agent.map(str::trim).filter(|a| !a.is_empty());
    match result {
        PlanResult::Available(plan) => {
            let updated_at = chrono::DateTime::<chrono::Utc>::from(plan.updated_at)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let digest = Sha256::digest(plan.text.as_bytes());
            PlanReply {
                status: 200,
                body: json!({
                    "workspace_id": workspace_id,
                    "agent": agent,
                    "capability": Capability::AgentFile.as_str(),
                    "available": true,
                    "source": plan.source,
                    "text": plan.text,
                    "updated_at": updated_at,
                    "content_sha256": digest.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                    "complete": true,
                })
                .to_string(),
            }
        }
        PlanResult::Unavailable(capability, reason) => PlanReply {
            status: if reason == Reason::PlanTooLarge {
                413
            } else {
                200
            },
            body: json!({
                "workspace_id": workspace_id,
                "agent": agent,
                "capability": capability.as_str(),
                "available": false,
                "reason": reason.as_str(),
            })
            .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harbor_agent_session::{register, AgentSessionRecord};

    const WS: &str = "11111111-1111-1111-1111-111111111111";

    /// A throwaway Claude config dir, plans dir, projects dir and registry.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("harbor-plan-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            for dir in ["config/projects/p", "config/plans", "registry", "outside"] {
                fs::create_dir_all(root.join(dir)).unwrap();
            }
            let root = root.canonicalize().unwrap();
            Self { root }
        }

        fn env(&self) -> PlanEnv {
            PlanEnv {
                registry_dir: self.root.join("registry"),
                claude_config_dir: Some(self.root.join("config")),
                mux_started_at: None,
            }
        }

        fn transcript(&self, session: &str, lines: &[String]) -> PathBuf {
            let path = self.root.join(format!("config/projects/p/{session}.jsonl"));
            fs::write(&path, lines.join("\n") + "\n").unwrap();
            path
        }

        fn register(&self, pane: u64, session: &str, transcript: &Path) {
            register(
                &self.root.join("registry"),
                &AgentSessionRecord::new(
                    pane,
                    AGENT_CLAUDE,
                    session.to_string(),
                    transcript.to_path_buf(),
                    None,
                ),
            )
            .unwrap();
        }

        fn plan(&self, name: &str, text: &str) -> PathBuf {
            let path = self.root.join("config/plans").join(name);
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn attachment(path: &Path) -> String {
        json!({"type": "attachment", "attachment": {
            "type": "plan_mode", "isSubAgent": false,
            "planFilePath": path.to_string_lossy(),
            "planContent": "STALE COPY THAT MUST NOT BE USED",
        }})
        .to_string()
    }

    fn reply_json(reply: &PlanReply) -> Value {
        serde_json::from_str(&reply.body).unwrap()
    }

    fn claude(f: &Fixture, pane: u64) -> PlanReply {
        resolve(WS, Some("Claude"), pane, &f.env()).unwrap()
    }

    fn assert_unavailable(reply: &PlanReply, capability: &str, reason: &str) {
        let body = reply_json(reply);
        assert_eq!(body["available"], false, "{body}");
        assert_eq!(body["capability"], capability, "{body}");
        assert_eq!(body["reason"], reason, "{body}");
        assert!(body.get("text").is_none());
    }

    #[test]
    fn returns_the_whole_plan_file_byte_for_byte_including_cjk() {
        let f = Fixture::new("full");
        // Far taller than any terminal, with double-width text and no final newline.
        let text: String = (0..400)
            .map(|n| format!("## 手順 {n}\n- 端末の画面に収まらない計画 ✅\n"))
            .collect::<String>()
            + "end";
        let plan = f.plan("big-plan.md", &text);
        let transcript = f.transcript("s1", &[attachment(&plan)]);
        f.register(3, "s1", &transcript);

        let reply = claude(&f, 3);
        assert_eq!(reply.status, 200);
        let body = reply_json(&reply);
        assert_eq!(body["available"], true);
        assert_eq!(body["capability"], "agent_file");
        assert_eq!(body["source"], "claude_plan_file");
        assert_eq!(body["complete"], true);
        assert_eq!(body["agent"], "Claude");
        assert_eq!(body["text"].as_str().unwrap().as_bytes(), text.as_bytes());
        let expected: String = Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(body["content_sha256"], expected);
        assert!(body["updated_at"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn same_directory_sessions_in_different_panes_are_not_mixed() {
        let f = Fixture::new("panes");
        let plan_a = f.plan("a.md", "plan A");
        let plan_b = f.plan("b.md", "plan B");
        let ta = f.transcript("sa", &[attachment(&plan_a)]);
        let tb = f.transcript("sb", &[attachment(&plan_b)]);
        f.register(1, "sa", &ta);
        f.register(2, "sb", &tb);

        assert_eq!(reply_json(&claude(&f, 1))["text"], "plan A");
        assert_eq!(reply_json(&claude(&f, 2))["text"], "plan B");
    }

    #[test]
    fn the_latest_plan_named_by_the_session_wins() {
        let f = Fixture::new("latest");
        let old = f.plan("old.md", "old");
        let new = f.plan("new.md", "new");
        let exit_plan = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "planFilePath is mentioned here"},
            {"type": "tool_use", "name": "ExitPlanMode",
             "input": {"planFilePath": new.to_string_lossy()}},
        ]}})
        .to_string();
        let transcript = f.transcript("s", &[attachment(&old), exit_plan]);
        f.register(1, "s", &transcript);
        assert_eq!(reply_json(&claude(&f, 1))["text"], "new");
    }

    #[test]
    fn free_text_and_subagent_records_do_not_name_a_plan() {
        let f = Fixture::new("untrusted");
        let secret = f.plan("secret.md", "not this session's plan");
        let chatter = json!({"type": "user", "message": {"content":
            format!("look at \"planFilePath\":\"{}\"", secret.display())}})
        .to_string();
        let sidechain = json!({"type": "attachment", "isSidechain": true, "attachment": {
            "type": "plan_mode", "planFilePath": secret.to_string_lossy()}})
        .to_string();
        let subagent = json!({"type": "attachment", "attachment": {
            "type": "plan_mode", "isSubAgent": true,
            "planFilePath": secret.to_string_lossy()}})
        .to_string();
        let transcript = f.transcript("s", &[chatter, sidechain, subagent]);
        f.register(1, "s", &transcript);
        assert_unavailable(&claude(&f, 1), "agent_file", "plan_not_created");
    }

    #[test]
    fn unregistered_pane_is_session_unidentified() {
        let f = Fixture::new("unregistered");
        assert_unavailable(&claude(&f, 9), "agent_file", "session_unidentified");
    }

    #[test]
    fn session_without_a_plan_yet_is_plan_not_created() {
        let f = Fixture::new("noplan");
        let transcript = f.transcript("s", &[json!({"type": "user"}).to_string()]);
        f.register(1, "s", &transcript);
        assert_unavailable(&claude(&f, 1), "agent_file", "plan_not_created");
    }

    #[test]
    fn deleted_plan_file_is_plan_file_missing() {
        let f = Fixture::new("deleted");
        let plan = f.plan("gone.md", "x");
        let transcript = f.transcript("s", &[attachment(&plan)]);
        f.register(1, "s", &transcript);
        fs::remove_file(&plan).unwrap();
        assert_unavailable(&claude(&f, 1), "agent_file", "plan_file_missing");
    }

    #[test]
    fn registration_from_before_the_mux_started_is_stale() {
        let f = Fixture::new("stale");
        let plan = f.plan("p.md", "x");
        let transcript = f.transcript("s", &[attachment(&plan)]);
        f.register(1, "s", &transcript);
        let mut env = f.env();
        env.mux_started_at = Some(unix_now() + 60);
        let reply = resolve(WS, Some("Claude"), 1, &env).unwrap();
        assert_unavailable(&reply, "agent_file", "stale_session");
    }

    #[test]
    fn registration_whose_transcript_was_removed_is_stale() {
        let f = Fixture::new("notranscript");
        let plan = f.plan("p.md", "x");
        let transcript = f.transcript("s", &[attachment(&plan)]);
        f.register(1, "s", &transcript);
        fs::remove_file(&transcript).unwrap();
        assert_unavailable(&claude(&f, 1), "agent_file", "stale_session");
    }

    #[test]
    fn transcript_outside_the_projects_directory_is_rejected() {
        let f = Fixture::new("transcript-outside");
        let plan = f.plan("p.md", "x");
        let outside = f.root.join("outside/t.jsonl");
        fs::write(&outside, attachment(&plan) + "\n").unwrap();
        f.register(1, "s", &outside);
        assert_unavailable(&claude(&f, 1), "agent_file", "session_unidentified");
    }

    #[cfg(unix)]
    #[test]
    fn plan_paths_that_escape_the_plans_directory_are_rejected() {
        let f = Fixture::new("escape");
        let secret = f.root.join("outside/secret.md");
        fs::write(&secret, "SECRET").unwrap();
        std::os::unix::fs::symlink(&secret, f.root.join("config/plans/link.md")).unwrap();
        std::os::unix::fs::symlink(f.root.join("outside"), f.root.join("config/plans/dir"))
            .unwrap();

        let candidates = [
            // Direct path outside the plans directory.
            secret.clone(),
            // Symlink inside plans/ pointing out.
            f.root.join("config/plans/link.md"),
            f.root.join("config/plans/dir/secret.md"),
            // Traversal that lexically starts inside plans/.
            f.root.join("config/plans/../../outside/secret.md"),
            // Relative paths are never resolved against the GUI's cwd.
            PathBuf::from("plans/link.md"),
        ];
        for (n, candidate) in candidates.iter().enumerate() {
            let session = format!("s{n}");
            let transcript = f.transcript(&session, &[attachment(candidate)]);
            f.register(n as u64, &session, &transcript);
            let reply = claude(&f, n as u64);
            assert_unavailable(&reply, "agent_file", "plan_file_missing");
            assert!(!reply.body.contains("SECRET"));
        }
    }

    #[test]
    fn a_directory_named_like_a_plan_is_not_a_plan() {
        let f = Fixture::new("dir");
        let dir = f.root.join("config/plans/looks-like-a-file.md");
        fs::create_dir(&dir).unwrap();
        let transcript = f.transcript("s", &[attachment(&dir)]);
        f.register(1, "s", &transcript);
        assert_unavailable(&claude(&f, 1), "agent_file", "plan_file_missing");
    }

    #[test]
    fn oversized_plan_is_413_and_never_truncated() {
        let f = Fixture::new("large");
        let plan = f.plan("huge.md", &"a".repeat(MAX_PLAN_BYTES as usize + 1));
        let transcript = f.transcript("s", &[attachment(&plan)]);
        f.register(1, "s", &transcript);
        let reply = claude(&f, 1);
        assert_eq!(reply.status, 413);
        assert_unavailable(&reply, "agent_file", "plan_too_large");

        // Exactly the limit is still returned whole.
        let plan = f.plan("edge.md", &"a".repeat(MAX_PLAN_BYTES as usize));
        let transcript = f.transcript("s2", &[attachment(&plan)]);
        f.register(2, "s2", &transcript);
        let reply = claude(&f, 2);
        assert_eq!(reply.status, 200);
        assert_eq!(
            reply_json(&reply)["text"].as_str().unwrap().len(),
            MAX_PLAN_BYTES as usize
        );
    }

    #[test]
    fn plan_that_is_not_utf8_is_an_error_not_a_lossy_success() {
        let f = Fixture::new("utf8");
        let plan = f.root.join("config/plans/bin.md");
        fs::write(&plan, [0xff, 0xfe, 0x00]).unwrap();
        let transcript = f.transcript("s", &[attachment(&plan)]);
        f.register(1, "s", &transcript);
        let err = resolve(WS, Some("Claude"), 1, &f.env()).unwrap_err();
        assert!(!format!("{err:#}").contains(&*f.root.to_string_lossy()));
    }

    #[test]
    fn codex_has_no_plan_file_and_other_agents_are_unsupported() {
        let f = Fixture::new("agents");
        let env = f.env();
        assert_unavailable(
            &resolve(WS, Some("Codex"), 1, &env).unwrap(),
            "none",
            "agent_has_no_plan_file",
        );
        assert_unavailable(
            &resolve(WS, Some("OpenCode"), 1, &env).unwrap(),
            "unknown",
            "unsupported_agent",
        );
        assert_unavailable(&resolve(WS, None, 1, &env).unwrap(), "unknown", "no_agent");
        assert_unavailable(
            &resolve(WS, Some("  "), 1, &env).unwrap(),
            "unknown",
            "no_agent",
        );
        assert_eq!(
            reply_json(&resolve(WS, None, 1, &env).unwrap())["agent"],
            Value::Null
        );
    }

    #[test]
    fn responses_never_contain_paths_or_session_ids() {
        let f = Fixture::new("leak");
        let session = "very-secret-session-id";
        let plan = f.plan("p.md", "body");
        let transcript = f.transcript(session, &[attachment(&plan)]);
        f.register(1, session, &transcript);
        let root = f.root.to_string_lossy().into_owned();

        let mut bodies = vec![claude(&f, 1).body, claude(&f, 8).body];
        fs::remove_file(&plan).unwrap();
        bodies.push(claude(&f, 1).body);
        for body in bodies {
            assert!(!body.contains(&root), "{body}");
            assert!(!body.contains(session), "{body}");
            assert!(!body.contains("jsonl"), "{body}");
        }
    }

    fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
}
