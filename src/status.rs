//! The daemon's agent status cache, `<runtime>/status.json` (R7.3). The
//! daemon writes it every pass; the CLI and the switcher read it instead of
//! asking every session. A cache older than 15 s means the daemon is not
//! running, and a UI must say so instead of showing nothing waiting.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const MAX_AGE: i64 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    Done,
    #[serde(other)]
    Unknown,
}

impl AgentState {
    pub fn name(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Done => "done",
            AgentState::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub agent: String,
    pub status: AgentState,
    /// The agent's terminal title (for Claude Code, the conversation's topic).
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pane_id: Option<String>,
    /// When the daemon first saw this wait (blocked agents only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    #[serde(default)]
    pub blocked: usize,
    #[serde(default)]
    pub agents: Vec<AgentStatus>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Cache {
    pub updated_at: i64,
    pub sessions: BTreeMap<String, SessionStatus>,
}

/// Agent status per task session, as of the daemon's last pass.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub sessions: BTreeMap<String, SessionStatus>,
    /// Whether the daemon updated the cache lately. When not, `sessions` is empty
    /// and waits are unknown, not zero.
    pub fresh: bool,
}

impl Status {
    pub fn read(path: &Path, now: i64) -> Status {
        let cache = fs::read_to_string(path).ok().and_then(|text| serde_json::from_str::<Cache>(&text).ok());
        match cache {
            Some(cache) if cache.updated_at >= now - MAX_AGE => Status { sessions: cache.sessions, fresh: true },
            _ => Status::default(),
        }
    }

    pub fn agents(&self, session: &str) -> &[AgentStatus] {
        self.sessions.get(session).map_or(&[], |s| &s.agents)
    }

    /// How many agents in `session` wait for input.
    pub fn waiting(&self, session: &str) -> usize {
        self.sessions.get(session).map_or(0, |s| s.blocked)
    }

    /// The blocked agents of `session`.
    pub fn blocked(&self, session: &str) -> impl Iterator<Item = &AgentStatus> {
        self.agents(session).iter().filter(|a| a.status == AgentState::Blocked)
    }

    /// When the longest wait in `session` started.
    pub fn waiting_since(&self, session: &str, now: i64) -> Option<i64> {
        self.blocked(session).map(|a| a.since.unwrap_or(now)).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_or_missing_cache_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("status.json");
        assert!(!Status::read(&path, 100).fresh);
        fs::write(
            &path,
            r#"{"updated_at": 100, "sessions": {"task-a": {"blocked": 1, "agents": [
            {"agent": "claude", "status": "blocked", "title": "", "pane_id": "w1:p1", "since": 90},
            {"agent": "codex", "status": "thinking", "title": "", "pane_id": null}]}}}"#,
        )
        .unwrap();
        let status = Status::read(&path, 100 + MAX_AGE);
        assert!(status.fresh);
        assert_eq!(status.waiting("task-a"), 1);
        assert_eq!(status.waiting_since("task-a", 200), Some(90));
        assert_eq!(status.agents("task-a")[1].status, AgentState::Unknown);
        assert!(!Status::read(&path, 101 + MAX_AGE).fresh);
    }
}
