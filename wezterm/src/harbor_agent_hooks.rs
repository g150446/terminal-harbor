//! `wezterm agent-session`: hook entry points that tell Terminal Harbor which
//! agent session runs in which pane, plus an opt-in installer for those hooks.
//!
//! The hooks run inside an agent's own process tree, so they must be quiet and
//! cheap: they print nothing on success (Claude Code adds hook stdout to the
//! model's context for some events) and never echo the hook payload.

use anyhow::{bail, Context};
use clap::{Parser, ValueEnum};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use wezterm_gui_subcommands::harbor_agent_session::{
    clear_if_session, default_registry_dir, register, AgentSessionRecord, AGENT_CLAUDE,
};

/// Hook payloads are a few hundred bytes; the cap only stops a runaway pipe.
const MAX_HOOK_INPUT_BYTES: u64 = 1024 * 1024;

/// Events whose hook keeps the registration fresh or removes it. SessionStart
/// covers new, resumed and cleared sessions; UserPromptSubmit re-registers a
/// pane whose record was lost (for example the state directory was cleaned).
const REGISTER_EVENTS: [&str; 2] = ["SessionStart", "UserPromptSubmit"];
const END_EVENTS: [&str; 1] = ["SessionEnd"];
const HOOK_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum HookAgent {
    Claude,
}

impl HookAgent {
    fn kind(self) -> &'static str {
        match self {
            HookAgent::Claude => AGENT_CLAUDE,
        }
    }
}

#[derive(Debug, Parser, Clone)]
pub struct AgentSessionCommand {
    #[command(subcommand)]
    command: AgentSessionSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum AgentSessionSubCommand {
    /// Record the session in the hook payload on stdin against this pane
    /// (`WEZTERM_PANE`). Intended to be run by an agent's SessionStart hook.
    #[command(name = "register")]
    Register(HookArgs),

    /// Remove this pane's record if it still belongs to the session in the
    /// hook payload on stdin. Intended for an agent's SessionEnd hook.
    #[command(name = "end")]
    End(HookArgs),

    /// Add the hooks above to the agent's settings file. Shows the change
    /// without writing unless --apply is given; an applied change first backs
    /// up the existing file.
    #[command(name = "install-hooks")]
    InstallHooks(InstallArgs),
}

#[derive(Debug, Parser, Clone)]
struct HookArgs {
    #[arg(long, value_enum)]
    agent: HookAgent,
}

#[derive(Debug, Parser, Clone)]
struct InstallArgs {
    #[arg(long, value_enum)]
    agent: HookAgent,

    /// Write the change. Without this flag nothing is modified.
    #[arg(long)]
    apply: bool,
}

impl AgentSessionCommand {
    pub fn run(&self) -> anyhow::Result<()> {
        match &self.command {
            AgentSessionSubCommand::Register(args) => run_register(args.agent),
            AgentSessionSubCommand::End(args) => run_end(args.agent),
            AgentSessionSubCommand::InstallHooks(args) => run_install(args),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct HookInput {
    session_id: String,
    transcript_path: Option<PathBuf>,
    cwd: Option<PathBuf>,
}

fn parse_hook_input(bytes: &[u8]) -> anyhow::Result<HookInput> {
    let value: Value = serde_json::from_slice(bytes).context("hook input is not valid JSON")?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let Some(session_id) = text("session_id") else {
        bail!("hook input has no session_id");
    };
    Ok(HookInput {
        session_id,
        transcript_path: text("transcript_path").map(PathBuf::from),
        cwd: text("cwd").map(PathBuf::from),
    })
}

fn pane_id_from_env(value: Option<&str>) -> Option<u64> {
    value.and_then(|v| v.trim().parse().ok())
}

fn read_hook_input() -> anyhow::Result<HookInput> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_HOOK_INPUT_BYTES)
        .read_to_end(&mut bytes)
        .context("failed to read hook input")?;
    parse_hook_input(&bytes)
}

fn run_register(agent: HookAgent) -> anyhow::Result<()> {
    // Not inside a Harbor pane (the agent was started elsewhere): nothing to do,
    // and not worth a hook error in the agent's UI.
    let Some(pane_id) = pane_id_from_env(std::env::var("WEZTERM_PANE").ok().as_deref()) else {
        return Ok(());
    };
    let input = read_hook_input()?;
    let Some(transcript_path) = input.transcript_path else {
        bail!("hook input has no transcript_path");
    };
    let record = AgentSessionRecord::new(
        pane_id,
        agent.kind(),
        input.session_id,
        transcript_path,
        input.cwd,
    );
    register(&default_registry_dir(), &record).context("failed to save the session record")
}

fn run_end(agent: HookAgent) -> anyhow::Result<()> {
    let Some(pane_id) = pane_id_from_env(std::env::var("WEZTERM_PANE").ok().as_deref()) else {
        return Ok(());
    };
    let input = read_hook_input()?;
    // The record is keyed by pane, so an end hook for another agent kind must
    // not remove whatever this pane's current record is.
    let _ = agent;
    clear_if_session(&default_registry_dir(), pane_id, &input.session_id)
        .context("failed to remove the session record")?;
    Ok(())
}

fn claude_config_dir() -> anyhow::Result<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(dirs_next::home_dir()
            .context("cannot determine the home directory")?
            .join(".claude")),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn hook_command(exe: &Path, subcommand: &str, agent: HookAgent) -> String {
    format!(
        "{} agent-session {subcommand} --agent {}",
        shell_quote(&exe.to_string_lossy()),
        agent.kind()
    )
}

fn is_harbor_hook_command(command: &str, subcommand: &str) -> bool {
    command.contains(&format!(
        "agent-session {subcommand} --agent {AGENT_CLAUDE}"
    ))
}

fn event_has_harbor_hook(settings: &Value, event: &str, subcommand: &str) -> bool {
    settings
        .pointer(&format!("/hooks/{event}"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .any(|command| is_harbor_hook_command(command, subcommand))
}

/// Add Harbor's hooks to a Claude Code settings object, leaving every existing
/// hook and setting in place. Returns the events that were added.
fn merge_claude_hooks(
    settings: &mut Value,
    register_command: &str,
    end_command: &str,
) -> anyhow::Result<Vec<&'static str>> {
    if !settings.is_object() {
        bail!("settings file is not a JSON object");
    }
    let mut added = Vec::new();
    let plan = REGISTER_EVENTS
        .iter()
        .map(|event| (*event, "register", register_command))
        .chain(END_EVENTS.iter().map(|event| (*event, "end", end_command)));
    for (event, subcommand, command) in plan {
        if event_has_harbor_hook(settings, event, subcommand) {
            continue;
        }
        let hooks = settings
            .as_object_mut()
            .expect("checked above")
            .entry("hooks")
            .or_insert_with(|| json!({}));
        let Some(hooks) = hooks.as_object_mut() else {
            bail!("\"hooks\" in the settings file is not an object");
        };
        let groups = hooks.entry(event).or_insert_with(|| json!([]));
        let Some(groups) = groups.as_array_mut() else {
            bail!("hooks.{event} in the settings file is not an array");
        };
        groups.push(json!({
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": HOOK_TIMEOUT_SECS,
            }]
        }));
        added.push(event);
    }
    Ok(added)
}

fn run_install(args: &InstallArgs) -> anyhow::Result<()> {
    let HookAgent::Claude = args.agent;
    let settings_path = claude_config_dir()?.join("settings.json");
    let exe = std::env::current_exe().context("cannot determine the wezterm executable path")?;

    let existing = match std::fs::read(&settings_path) {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).context("failed to read the Claude settings file"),
    };
    let mut settings: Value = match &existing {
        // Never replace a file this command cannot parse.
        Some(bytes) => serde_json::from_slice(bytes)
            .context("the Claude settings file is not valid JSON; not modifying it")?,
        None => json!({}),
    };

    let added = merge_claude_hooks(
        &mut settings,
        &hook_command(&exe, "register", args.agent),
        &hook_command(&exe, "end", args.agent),
    )?;
    if added.is_empty() {
        println!(
            "Terminal Harbor hooks are already present in the Claude settings; nothing to do."
        );
        return Ok(());
    }
    println!(
        "Hooks to add to the Claude settings ({}): {}",
        settings_path.display(),
        added.join(", ")
    );
    if !args.apply {
        println!("Nothing was written. Re-run with --apply to install them.");
        return Ok(());
    }

    if existing.is_some() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup = settings_path.with_file_name(format!("settings.json.harbor-backup-{stamp}"));
        std::fs::copy(&settings_path, &backup).context("failed to back up the settings file")?;
        println!("Backup: {}", backup.display());
    } else if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).context("failed to create the Claude config directory")?;
    }
    // Same-directory temp file + rename, so a crash cannot leave a half-written
    // settings file that would break Claude Code's startup.
    let tmp =
        settings_path.with_file_name(format!("settings.json.harbor-tmp-{}", std::process::id()));
    let mut serialized = serde_json::to_string_pretty(&settings)?;
    serialized.push('\n');
    std::fs::write(&tmp, serialized).context("failed to write the new settings file")?;
    if let Some(perms) = std::fs::metadata(&settings_path)
        .ok()
        .map(|m| m.permissions())
    {
        let _ = std::fs::set_permissions(&tmp, perms);
    }
    std::fs::rename(&tmp, &settings_path).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::Error::new(err).context("failed to replace the settings file")
    })?;
    println!("Installed. Restart running Claude Code sessions to pick up the hooks.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REG: &str = "'/App/wezterm' agent-session register --agent claude";
    const END: &str = "'/App/wezterm' agent-session end --agent claude";

    #[test]
    fn hook_input_requires_a_session_id_and_ignores_unknown_fields() {
        let input = parse_hook_input(
            br#"{"session_id":"abc","transcript_path":"/t.jsonl","cwd":"/r","hook_event_name":"SessionStart","x":1}"#,
        )
        .unwrap();
        assert_eq!(input.session_id, "abc");
        assert_eq!(input.transcript_path, Some(PathBuf::from("/t.jsonl")));
        assert!(parse_hook_input(br#"{"transcript_path":"/t"}"#).is_err());
        assert!(parse_hook_input(br#"{"session_id":"  "}"#).is_err());
        assert!(parse_hook_input(b"not json").is_err());
    }

    #[test]
    fn pane_id_comes_only_from_a_numeric_wezterm_pane() {
        assert_eq!(pane_id_from_env(Some("12")), Some(12));
        assert_eq!(pane_id_from_env(Some(" 3\n")), Some(3));
        assert_eq!(pane_id_from_env(Some("abc")), None);
        assert_eq!(pane_id_from_env(Some("")), None);
        assert_eq!(pane_id_from_env(None), None);
    }

    #[test]
    fn merge_keeps_existing_settings_and_hooks() {
        let mut settings = json!({
            "model": "opus",
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": "echo mine"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "notify"}]}]
            }
        });
        let added = merge_claude_hooks(&mut settings, REG, END).unwrap();
        assert_eq!(
            added,
            vec!["SessionStart", "UserPromptSubmit", "SessionEnd"]
        );
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 1);
        let start = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 2);
        assert_eq!(start[0]["hooks"][0]["command"], "echo mine");
        assert_eq!(start[1]["hooks"][0]["command"], REG);
        assert_eq!(
            settings["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
            END
        );
    }

    #[test]
    fn merge_is_idempotent() {
        let mut settings = json!({});
        merge_claude_hooks(&mut settings, REG, END).unwrap();
        let once = settings.clone();
        assert!(merge_claude_hooks(&mut settings, REG, END)
            .unwrap()
            .is_empty());
        assert_eq!(settings, once);
    }

    #[test]
    fn merge_refuses_settings_of_an_unexpected_shape() {
        assert!(merge_claude_hooks(&mut json!([]), REG, END).is_err());
        assert!(merge_claude_hooks(&mut json!({"hooks": []}), REG, END).is_err());
        assert!(merge_claude_hooks(&mut json!({"hooks": {"SessionStart": {}}}), REG, END).is_err());
    }

    #[test]
    fn hook_commands_quote_paths_with_spaces_and_quotes() {
        let command = hook_command(
            Path::new("/Applications/Terminal Harbor.app/it's/wezterm"),
            "register",
            HookAgent::Claude,
        );
        assert_eq!(
            command,
            "'/Applications/Terminal Harbor.app/it'\\''s/wezterm' agent-session register --agent claude"
        );
        assert!(is_harbor_hook_command(&command, "register"));
        assert!(!is_harbor_hook_command(&command, "end"));
    }
}
