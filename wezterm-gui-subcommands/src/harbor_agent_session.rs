//! Registry of which agent session runs in which Terminal Harbor pane.
//!
//! An agent's own hook (`wezterm agent-session register`) writes one small
//! file per pane; the GUI reads it back when the mobile bridge is asked for the
//! agent's plan. One file per pane keeps concurrent hooks from racing on a
//! shared read-modify-write, and the write is atomic (temp file + rename).
//!
//! The records hold a transcript path and a working directory, so they are
//! local state only: nothing here is ever serialized into an API response.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const AGENT_CLAUDE: &str = "claude";

const RECORD_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionRecord {
    pub version: u32,
    /// The mux server's pane id, i.e. what the agent sees in `WEZTERM_PANE`.
    pub pane_id: u64,
    pub agent: String,
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub cwd: Option<PathBuf>,
    /// Unix seconds of the last hook call.
    pub updated_at: u64,
}

impl AgentSessionRecord {
    pub fn new(
        pane_id: u64,
        agent: &str,
        session_id: String,
        transcript_path: PathBuf,
        cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            version: RECORD_VERSION,
            pane_id,
            agent: agent.to_string(),
            session_id,
            transcript_path,
            cwd,
            updated_at: unix_now(),
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Same base directory as the mobile bridge's `state_dir()`.
pub fn default_registry_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("terminal-harbor")
        .join("agent-sessions")
}

fn record_path(dir: &Path, pane_id: u64) -> PathBuf {
    dir.join(format!("{pane_id}.json"))
}

/// Create or replace the record for `record.pane_id`.
pub fn register(dir: &Path, record: &AgentSessionRecord) -> std::io::Result<()> {
    create_private_dir(dir)?;
    let target = record_path(dir, record.pane_id);
    let tmp = dir.join(format!(".{}.{}.tmp", record.pane_id, std::process::id()));
    let result = write_private_file(&tmp, &serde_json::to_vec(record)?)
        .and_then(|()| fs::rename(&tmp, &target));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The record for `pane_id`, or `None` if there is none or it is unreadable.
/// A corrupt record is treated as absent so a bad file cannot wedge the API.
pub fn lookup(dir: &Path, pane_id: u64) -> Option<AgentSessionRecord> {
    let bytes = fs::read(record_path(dir, pane_id)).ok()?;
    let record: AgentSessionRecord = serde_json::from_slice(&bytes).ok()?;
    (record.version == RECORD_VERSION && record.pane_id == pane_id).then_some(record)
}

/// Remove the record for `pane_id` only if it still belongs to `session_id`.
///
/// A session that ends after another one took over the pane (for example a
/// resumed session starting before the old one's SessionEnd hook runs) must not
/// erase the newer registration. Returns whether a record was removed.
pub fn clear_if_session(dir: &Path, pane_id: u64, session_id: &str) -> std::io::Result<bool> {
    match lookup(dir, pane_id) {
        Some(record) if record.session_id == session_id => {
            fs::remove_file(record_path(dir, pane_id))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "harbor-agent-session-{name}-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn record(pane: u64, session: &str) -> AgentSessionRecord {
        AgentSessionRecord::new(
            pane,
            AGENT_CLAUDE,
            session.to_string(),
            PathBuf::from("/x/projects/t.jsonl"),
            Some(PathBuf::from("/x/repo")),
        )
    }

    #[test]
    fn registered_session_is_found_by_its_pane_only() {
        let dir = temp_dir("lookup");
        register(&dir, &record(7, "s-1")).unwrap();
        assert_eq!(lookup(&dir, 7).unwrap().session_id, "s-1");
        assert!(lookup(&dir, 8).is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_new_registration_replaces_the_previous_session_for_the_pane() {
        let dir = temp_dir("replace");
        register(&dir, &record(7, "old")).unwrap();
        register(&dir, &record(7, "new")).unwrap();
        assert_eq!(lookup(&dir, 7).unwrap().session_id, "new");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ending_an_older_session_keeps_the_newer_registration() {
        let dir = temp_dir("end");
        register(&dir, &record(7, "new")).unwrap();
        assert!(!clear_if_session(&dir, 7, "old").unwrap());
        assert!(lookup(&dir, 7).is_some());
        assert!(clear_if_session(&dir, 7, "new").unwrap());
        assert!(lookup(&dir, 7).is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_or_mismatched_records_read_as_absent() {
        let dir = temp_dir("corrupt");
        register(&dir, &record(7, "s")).unwrap();
        fs::write(record_path(&dir, 7), b"{not json").unwrap();
        assert!(lookup(&dir, 7).is_none());
        // A record copied to another pane's file must not answer for that pane.
        register(&dir, &record(7, "s")).unwrap();
        fs::copy(record_path(&dir, 7), record_path(&dir, 9)).unwrap();
        assert!(lookup(&dir, 9).is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn registry_files_are_private_and_leave_no_temp_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        register(&dir, &record(7, "s")).unwrap();
        let file_mode = fs::metadata(record_path(&dir, 7))
            .unwrap()
            .permissions()
            .mode();
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600);
        assert_eq!(dir_mode & 0o777, 0o700);
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["7.json".to_string()]);
        let _ = fs::remove_dir_all(dir);
    }
}
