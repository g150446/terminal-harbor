use mux::pane::CachePolicy;
use mux::Mux;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 2;
const LEGACY_SCHEMA_VERSION: u32 = 1;
/// Previous default before the mobile pairing UI needed more room.
const LEGACY_SIDEBAR_DEFAULT_WIDTH: usize = 240;
pub const SIDEBAR_DEFAULT_WIDTH: usize = 480;
pub const SIDEBAR_MIN_WIDTH: usize = 360;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarborWorkspace {
    pub id: Uuid,
    pub name: String,
    pub root: Option<PathBuf>,
    pub mux_workspace: String,
    pub order: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedWorkspaceState {
    schema_version: u32,
    sidebar_visible: bool,
    sidebar_width: usize,
    workspaces: Vec<HarborWorkspace>,
}

impl Default for PersistedWorkspaceState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            sidebar_visible: true,
            sidebar_width: SIDEBAR_DEFAULT_WIDTH,
            workspaces: vec![],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceActivity {
    Error,
    Waiting,
    Running,
    Unread,
    Done,
    Idle,
}

impl WorkspaceActivity {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Error => "!",
            Self::Waiting => "?",
            Self::Running => "●",
            Self::Unread => "•",
            Self::Done => "✓",
            Self::Idle => "○",
        }
    }

    fn priority(self) -> u8 {
        match self {
            Self::Error => 6,
            Self::Waiting => 5,
            Self::Running => 4,
            Self::Unread => 3,
            Self::Done => 2,
            Self::Idle => 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HarborWorkspaceRow {
    pub workspace: HarborWorkspace,
    pub activity: WorkspaceActivity,
    pub directory: String,
    /// Display name of the AI agent running in this workspace, if any.
    /// Sidebar-only, like `directory`: not persisted and not in the mobile API.
    pub agent: Option<String>,
    /// One-line summary of what that agent is working on. Sidebar-only.
    pub summary: Option<String>,
    pub process: Option<String>,
    pub message: Option<String>,
    pub selected: bool,
}

/// AI coding agents the sidebar recognizes, matched against the foreground
/// process basename. Anything else counts as "no agent running" and keeps the
/// row down to its directory line.
const KNOWN_AGENTS: &[(&str, &str)] = &[
    ("claude", "Claude"),
    ("codex", "Codex"),
    ("opencode", "OpenCode"),
];

/// Pane titles that carry no task information; agents and shells both fall
/// back to these, and `LocalPane::get_title` substitutes the process name for
/// the default title.
const UNINFORMATIVE_TITLES: &[&str] = &["wezterm", "zsh", "bash", "fish", "sh", "nu", "pwsh"];

/// Leading decoration that agents put in front of the actual summary. Claude
/// Code animates a braille spinner here, so it has to come off before the
/// title can be compared for changes.
fn is_status_glyph(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(ch, '\u{2800}'..='\u{28FF}')
        || matches!(
            ch,
            '✳' | '✻' | '✽' | '✶' | '✢' | '·' | '●' | '○' | '◐' | '◓' | '▪' | '⏵' | '*' | '⠿'
        )
}

/// User var the mux server uses to relay the foreground process basename.
/// Must match `wezterm_mux_server_impl::sessionhandler::PANE_PROCESS_USER_VAR`.
pub const PANE_PROCESS_USER_VAR: &str = "TH_PANE_PROCESS";

/// Foreground process basename for a pane.
///
/// `Pane::get_foreground_process_name` is only implemented for local panes. A
/// GUI attached to the persistent mux server sees `ClientPane`s, which return
/// `None`, so fall back to the user var the server relays for us.
pub fn pane_process_name(
    pane: &std::sync::Arc<dyn mux::pane::Pane>,
    vars: &std::collections::HashMap<String, String>,
) -> Option<String> {
    pane.get_foreground_process_name(CachePolicy::AllowStale)
        .and_then(|name| {
            Path::new(&name)
                .file_name()
                .map(|part| part.to_string_lossy().into_owned())
        })
        .or_else(|| {
            vars.get(PANE_PROCESS_USER_VAR)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

pub fn agent_label(raw: &str) -> Option<&'static str> {
    let name = raw.trim().to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    KNOWN_AGENTS
        .iter()
        .find(|(exe, _)| *exe == name)
        .map(|(_, label)| *label)
}

/// Strip the animated spinner and collapse whitespace so repeated title
/// updates that only advance the spinner compare equal.
pub fn strip_status_glyphs(title: &str) -> String {
    let stripped = title.trim_start_matches(is_status_glyph);
    let mut out = String::with_capacity(stripped.len());
    let mut pending_space = false;
    for ch in stripped.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

/// Return the pane title only when it actually describes the agent's work.
fn summary_from_title(title: &str, agent: &str, process: Option<&str>) -> Option<String> {
    let summary = strip_status_glyphs(title);
    if summary.is_empty() || summary.eq_ignore_ascii_case(agent) {
        return None;
    }
    if process.is_some_and(|process| summary.eq_ignore_ascii_case(process)) {
        return None;
    }
    if UNINFORMATIVE_TITLES
        .iter()
        .any(|known| summary.eq_ignore_ascii_case(known))
    {
        return None;
    }
    Some(summary)
}

#[derive(Default)]
struct WorkspaceRegistry {
    state: PersistedWorkspaceState,
    loaded: bool,
}

lazy_static::lazy_static! {
    static ref REGISTRY: Mutex<WorkspaceRegistry> = Mutex::new(WorkspaceRegistry::default());
}

fn state_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("terminal-harbor")
}

fn state_path() -> PathBuf {
    state_dir().join("workspaces-v1.json")
}

fn load_if_needed(registry: &mut WorkspaceRegistry) {
    if registry.loaded {
        return;
    }
    registry.loaded = true;
    let path = state_path();
    let Ok(data) = fs::read(&path) else {
        return;
    };
    match serde_json::from_slice::<PersistedWorkspaceState>(&data) {
        Ok(mut state)
            if state.schema_version == SCHEMA_VERSION
                || state.schema_version == LEGACY_SCHEMA_VERSION =>
        {
            let mut migrated = state.schema_version == LEGACY_SCHEMA_VERSION;
            if migrated {
                migrate_legacy_workspaces(&mut state);
            }
            // Widen sidebars that still use the pre-pairing default.
            if state.sidebar_width == LEGACY_SIDEBAR_DEFAULT_WIDTH
                || state.sidebar_width < SIDEBAR_MIN_WIDTH
            {
                state.sidebar_width = SIDEBAR_DEFAULT_WIDTH;
                migrated = true;
            }
            state.sidebar_width = state.sidebar_width.max(SIDEBAR_MIN_WIDTH);
            state.workspaces.sort_by_key(|workspace| workspace.order);
            registry.state = state;
            if migrated {
                if let Err(err) = save(registry) {
                    log::error!("migrating Terminal Harbor workspace state: {err:#}");
                }
            }
        }
        Ok(_) | Err(_) => {
            let corrupt =
                path.with_extension(format!("corrupt-{}", chrono::Utc::now().timestamp()));
            let _ = fs::rename(path, corrupt);
        }
    }
}

fn migrate_legacy_workspaces(state: &mut PersistedWorkspaceState) {
    // Early development builds could persist multiple initial workspaces for
    // the same directory. Keep the first entry for each root while preserving
    // genuinely distinct project roots.
    let mut seen_roots = HashSet::new();
    state
        .workspaces
        .retain(|workspace| seen_roots.insert(workspace.root.clone()));
    for (order, workspace) in state.workspaces.iter_mut().enumerate() {
        workspace.order = order;
    }
    state.schema_version = SCHEMA_VERSION;
}

fn save(registry: &WorkspaceRegistry) -> anyhow::Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let path = state_path();
    let temp = dir.join("workspaces-v1.json.tmp");
    let data = serde_json::to_vec_pretty(&registry.state)?;
    fs::write(&temp, data)?;
    fs::rename(temp, path)?;
    Ok(())
}

fn pane_root(pane: &std::sync::Arc<dyn mux::pane::Pane>) -> Option<PathBuf> {
    pane.get_current_working_dir(CachePolicy::AllowStale)
        .and_then(|url| url.to_file_path().ok())
}

fn directory_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            let name = path.display().to_string();
            (path.has_root() && path.parent().is_none() && !name.is_empty()).then_some(name)
        })
}

fn pane_directory_name(pane: &std::sync::Arc<dyn mux::pane::Pane>) -> Option<String> {
    // Sidebar layouts are cached, so accepting a stale process-inspected CWD here
    // can leave the old directory visible indefinitely when OSC 7 isn't available.
    let cwd = pane.get_current_working_dir(CachePolicy::FetchImmediate)?;
    cwd.to_file_path()
        .ok()
        .and_then(|path| directory_name(&path))
        .or_else(|| {
            cwd.path_segments()
                .and_then(|segments| segments.filter(|part| !part.is_empty()).last())
                .map(str::to_string)
        })
}

fn active_directory_for_workspace(mux: &Mux, workspace: &HarborWorkspace) -> String {
    let active = mux
        .iter_windows_in_workspace(&workspace.mux_workspace)
        .into_iter()
        .next()
        .and_then(|window_id| mux.get_active_tab_for_window(window_id))
        .and_then(|tab| tab.get_active_pane())
        .and_then(|pane| pane_directory_name(&pane));
    active
        .or_else(|| workspace.root.as_deref().and_then(directory_name))
        .unwrap_or_else(|| "Session workspace".to_string())
}

fn root_for_workspace(mux: &Mux, workspace: &str) -> Option<PathBuf> {
    for window_id in mux.iter_windows_in_workspace(workspace) {
        if let Some(tab) = mux.get_active_tab_for_window(window_id) {
            if let Some(pane) = tab.get_active_pane() {
                if let Some(root) = pane_root(&pane) {
                    return Some(root);
                }
            }
        }
    }
    None
}

fn unique_name(registry: &WorkspaceRegistry, base: &str) -> String {
    let base = if base.trim().is_empty() {
        "Workspace"
    } else {
        base.trim()
    };
    if !registry
        .state
        .workspaces
        .iter()
        .any(|item| item.name == base)
    {
        return base.to_string();
    }
    for suffix in 2.. {
        let candidate = format!("{base} {suffix}");
        if !registry
            .state
            .workspaces
            .iter()
            .any(|item| item.name == candidate)
        {
            return candidate;
        }
    }
    unreachable!()
}

pub fn ensure_current_workspace(mux_window_id: mux::window::WindowId) {
    let mux = Mux::get();
    let active = crate::frontend::try_front_end()
        .map(|front_end| front_end.active_workspace())
        .unwrap_or_else(|| mux.active_workspace());
    let mut registry = REGISTRY.lock();
    load_if_needed(&mut registry);
    if registry
        .state
        .workspaces
        .iter()
        .any(|workspace| workspace.mux_workspace == active)
    {
        return;
    }

    let root = mux
        .get_active_tab_for_window(mux_window_id)
        .and_then(|tab| tab.get_active_pane())
        .and_then(|pane| pane_root(&pane))
        .or_else(|| root_for_workspace(&mux, &active));
    let fallback = active.clone();
    let base = root
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or(&fallback);
    let name = unique_name(&registry, base);
    let order = registry.state.workspaces.len();
    registry.state.workspaces.push(HarborWorkspace {
        id: Uuid::new_v4(),
        name,
        root,
        mux_workspace: active,
        order,
    });
    if let Err(err) = save(&registry) {
        log::error!("saving Terminal Harbor workspace state: {err:#}");
    }
}

pub fn create_from_path(root: PathBuf) -> HarborWorkspace {
    let mut registry = REGISTRY.lock();
    load_if_needed(&mut registry);
    let base = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Workspace");
    let name = unique_name(&registry, base);
    let id = Uuid::new_v4();
    let order = registry.state.workspaces.len();
    let workspace = HarborWorkspace {
        id,
        name: name.clone(),
        root: Some(root),
        mux_workspace: format!("harbor-{}-{name}", &id.simple().to_string()[..8]),
        order,
    };
    registry.state.workspaces.push(workspace.clone());
    if let Err(err) = save(&registry) {
        log::error!("saving Terminal Harbor workspace state: {err:#}");
    }
    workspace
}

pub fn workspaces() -> Vec<HarborWorkspace> {
    let mut registry = REGISTRY.lock();
    load_if_needed(&mut registry);
    registry.state.workspaces.clone()
}

pub fn workspace_at(index: usize) -> Option<HarborWorkspace> {
    workspaces().get(index).cloned()
}

pub fn workspace_with_mux_name(mux_workspace: &str) -> Option<HarborWorkspace> {
    workspaces()
        .into_iter()
        .find(|workspace| workspace.mux_workspace == mux_workspace)
}

pub fn remove_workspace(mux_workspace: &str) -> Option<HarborWorkspace> {
    let mut registry = REGISTRY.lock();
    load_if_needed(&mut registry);
    let index = registry
        .state
        .workspaces
        .iter()
        .position(|workspace| workspace.mux_workspace == mux_workspace)?;
    registry.state.workspaces.remove(index);
    for (order, workspace) in registry.state.workspaces.iter_mut().enumerate() {
        workspace.order = order;
    }
    let next = registry
        .state
        .workspaces
        .get(index)
        .or_else(|| {
            index
                .checked_sub(1)
                .and_then(|index| registry.state.workspaces.get(index))
        })
        .cloned();
    if let Err(err) = save(&registry) {
        log::error!("saving Terminal Harbor workspace removal: {err:#}");
    }
    next
}

pub fn relative_workspace(delta: isize, active_mux_workspace: &str) -> Option<HarborWorkspace> {
    let workspaces = workspaces();
    if workspaces.is_empty() {
        return None;
    }
    let current = workspaces
        .iter()
        .position(|workspace| workspace.mux_workspace == active_mux_workspace)
        .unwrap_or(0);
    let target = relative_index(current, delta, workspaces.len());
    workspaces.get(target).cloned()
}

fn relative_index(current: usize, delta: isize, count: usize) -> usize {
    (current as isize + delta).rem_euclid(count as isize) as usize
}

pub fn sidebar_visible() -> bool {
    let mut registry = REGISTRY.lock();
    load_if_needed(&mut registry);
    registry.state.sidebar_visible
}

pub fn sidebar_width() -> usize {
    let mut registry = REGISTRY.lock();
    load_if_needed(&mut registry);
    registry.state.sidebar_width.max(SIDEBAR_MIN_WIDTH)
}

pub fn toggle_sidebar() -> bool {
    let mut registry = REGISTRY.lock();
    load_if_needed(&mut registry);
    registry.state.sidebar_visible = !registry.state.sidebar_visible;
    if let Err(err) = save(&registry) {
        log::error!("saving Terminal Harbor sidebar state: {err:#}");
    }
    registry.state.sidebar_visible
}

fn explicit_activity(value: Option<&String>) -> Option<WorkspaceActivity> {
    match value.map(String::as_str) {
        Some("error") => Some(WorkspaceActivity::Error),
        Some("waiting") => Some(WorkspaceActivity::Waiting),
        Some("running") => Some(WorkspaceActivity::Running),
        Some("done") => Some(WorkspaceActivity::Done),
        Some("idle") => Some(WorkspaceActivity::Idle),
        _ => None,
    }
}

pub fn rows() -> Vec<HarborWorkspaceRow> {
    let mux = Mux::get();
    let active = crate::frontend::try_front_end()
        .map(|front_end| front_end.active_workspace())
        .unwrap_or_else(|| mux.active_workspace());
    let workspaces = {
        let mut registry = REGISTRY.lock();
        load_if_needed(&mut registry);
        registry.state.workspaces.clone()
    };

    workspaces
        .into_iter()
        .map(|workspace| {
            let mut activity = WorkspaceActivity::Idle;
            let directory = active_directory_for_workspace(&mux, &workspace);
            let mut process = None;
            let mut message = None;
            // Agent name and summary must describe the same pane, so they are
            // resolved together rather than as independent first-hit scans.
            let mut agent_info: Option<(String, Option<String>)> = None;
            for window_id in mux.iter_windows_in_workspace(&workspace.mux_workspace) {
                let Some(window) = mux.get_window(window_id) else {
                    continue;
                };
                for tab in window.iter() {
                    for positioned in tab.iter_panes() {
                        let pane = positioned.pane;
                        let vars = pane.copy_user_vars();
                        let candidate = explicit_activity(vars.get("TH_AGENT_STATE"))
                            .unwrap_or_else(|| {
                                if pane.has_unseen_output() {
                                    WorkspaceActivity::Unread
                                } else {
                                    WorkspaceActivity::Idle
                                }
                            });
                        if candidate.priority() > activity.priority() {
                            activity = candidate;
                        }
                        let pane_process = pane_process_name(&pane, &vars);
                        if process.is_none() {
                            process = vars
                                .get("TH_AGENT_NAME")
                                .cloned()
                                .or_else(|| pane_process.clone());
                        }
                        if message.is_none() {
                            message = vars.get("TH_AGENT_MESSAGE").cloned();
                        }
                        if agent_info.is_none() {
                            // An explicit TH_AGENT_NAME wins and may be any
                            // agent; otherwise only a recognized AI agent
                            // counts as running.
                            let label = vars
                                .get("TH_AGENT_NAME")
                                .map(|name| name.trim())
                                .filter(|name| !name.is_empty())
                                .map(str::to_string)
                                .or_else(|| {
                                    pane_process
                                        .as_deref()
                                        .and_then(agent_label)
                                        .map(str::to_string)
                                });
                            if let Some(label) = label {
                                let summary = vars
                                    .get("TH_AGENT_MESSAGE")
                                    .map(|value| value.trim())
                                    .filter(|value| !value.is_empty())
                                    .map(str::to_string)
                                    .or_else(|| {
                                        summary_from_title(
                                            &pane.get_title(),
                                            &label,
                                            pane_process.as_deref(),
                                        )
                                    });
                                agent_info = Some((label, summary));
                            }
                        }
                    }
                }
            }
            let (agent, summary) = match agent_info {
                Some((agent, summary)) => (Some(agent), summary),
                None => (None, None),
            };
            HarborWorkspaceRow {
                selected: workspace.mux_workspace == active,
                workspace,
                activity,
                directory,
                agent,
                summary,
                process,
                message,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_process_user_var_matches_the_mux_server() {
        // The GUI only learns the foreground process of a client pane through
        // this user var, so the two constants must not drift apart.
        assert_eq!(
            PANE_PROCESS_USER_VAR,
            wezterm_mux_server_impl::sessionhandler::PANE_PROCESS_USER_VAR
        );
    }

    #[test]
    fn agent_label_matches_known_agents_only() {
        assert_eq!(agent_label("claude"), Some("Claude"));
        assert_eq!(agent_label("codex"), Some("Codex"));
        assert_eq!(agent_label("opencode"), Some("OpenCode"));
        assert_eq!(agent_label("Codex.exe"), Some("Codex"));
        // Shells and generic runtimes must not count as a running agent.
        assert_eq!(agent_label("zsh"), None);
        assert_eq!(agent_label("node"), None);
        assert_eq!(agent_label("cargo"), None);
        assert_eq!(agent_label(""), None);
    }

    #[test]
    fn strip_status_glyphs_ignores_spinner_frames() {
        // Claude Code advances a braille spinner in place; both frames must
        // normalize to the same text so the sidebar is not rebuilt each frame.
        assert_eq!(
            strip_status_glyphs("⠂ サイドバーの表示を修正"),
            "サイドバーの表示を修正"
        );
        assert_eq!(
            strip_status_glyphs("⠐ サイドバーの表示を修正"),
            "サイドバーの表示を修正"
        );
        assert_eq!(strip_status_glyphs("✳ Running  tests"), "Running tests");
        assert_eq!(strip_status_glyphs("  ⠿  "), "");
    }

    #[test]
    fn summary_rejects_uninformative_titles() {
        // opencode reports only its own name as the title.
        assert_eq!(summary_from_title("OpenCode", "OpenCode", None), None);
        assert_eq!(summary_from_title("zsh", "Claude", None), None);
        assert_eq!(summary_from_title("wezterm", "Claude", None), None);
        assert_eq!(summary_from_title("", "Claude", None), None);
        assert_eq!(summary_from_title("claude", "Claude", Some("claude")), None);
        assert_eq!(
            summary_from_title("⠂ Fixing the sidebar", "Claude", Some("claude")),
            Some("Fixing the sidebar".to_string())
        );
    }

    #[test]
    fn activity_priority_keeps_attention_states_visible() {
        assert!(WorkspaceActivity::Error.priority() > WorkspaceActivity::Waiting.priority());
        assert!(WorkspaceActivity::Waiting.priority() > WorkspaceActivity::Running.priority());
        assert!(WorkspaceActivity::Running.priority() > WorkspaceActivity::Unread.priority());
    }

    #[test]
    fn explicit_state_parser_rejects_unknown_values() {
        assert_eq!(
            explicit_activity(Some(&"waiting".into())),
            Some(WorkspaceActivity::Waiting)
        );
        assert_eq!(explicit_activity(Some(&"surprise".into())), None);
    }

    #[test]
    fn legacy_migration_collapses_duplicate_initial_roots() {
        let workspace = |name: &str, root: &str, order| HarborWorkspace {
            id: Uuid::new_v4(),
            name: name.to_string(),
            root: Some(PathBuf::from(root)),
            mux_workspace: name.to_string(),
            order,
        };
        let mut state = PersistedWorkspaceState {
            schema_version: LEGACY_SCHEMA_VERSION,
            sidebar_visible: true,
            sidebar_width: SIDEBAR_DEFAULT_WIDTH,
            workspaces: vec![
                workspace("home", "/Users/example", 0),
                workspace("home 2", "/Users/example/", 1),
                workspace("project", "/Users/example/project", 2),
            ],
        };

        migrate_legacy_workspaces(&mut state);

        assert_eq!(state.schema_version, SCHEMA_VERSION);
        assert_eq!(state.workspaces.len(), 2);
        assert_eq!(state.workspaces[0].name, "home");
        assert_eq!(state.workspaces[0].order, 0);
        assert_eq!(state.workspaces[1].name, "project");
        assert_eq!(state.workspaces[1].order, 1);
    }

    #[test]
    fn relative_workspace_index_wraps_in_display_order() {
        assert_eq!(relative_index(0, -1, 3), 2);
        assert_eq!(relative_index(2, 1, 3), 0);
        assert_eq!(relative_index(1, 1, 3), 2);
    }

    #[test]
    fn directory_name_uses_only_the_final_component() {
        assert_eq!(
            directory_name(Path::new("/Users/example/projects/terminal-harbor")),
            Some("terminal-harbor".to_string())
        );
        assert_eq!(
            directory_name(Path::new("/Users/example/日本語/")),
            Some("日本語".to_string())
        );
        assert_eq!(directory_name(Path::new("/")), Some("/".to_string()));
    }
}
