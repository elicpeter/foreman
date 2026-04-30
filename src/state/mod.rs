//! Runner-owned state stored at `.foreman/state.json`.
//!
//! Phase 2 introduces the type vocabulary; phase 5 wires the atomic load/save
//! helpers that read and write this struct from disk.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::plan::PhaseId;

/// Per-role token counters. Aggregated into [`TokenUsage::by_role`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleUsage {
    /// Total input tokens billed for this role.
    pub input: u64,
    /// Total output tokens billed for this role.
    pub output: u64,
}

/// Aggregated token usage for a run, broken down by agent role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Total input tokens across all roles.
    pub input: u64,
    /// Total output tokens across all roles.
    pub output: u64,
    /// Per-role breakdown keyed by the role name (e.g., `"implementer"`).
    pub by_role: HashMap<String, RoleUsage>,
}

/// Persistent runner state stored at `.foreman/state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    /// Stable identifier for this run (typically a UTC timestamp slug).
    pub run_id: String,
    /// Git branch the runner is committing to.
    pub branch: String,
    /// When the run was first started.
    pub started_at: DateTime<Utc>,
    /// The `current_phase` at the moment the run began.
    pub started_phase: PhaseId,
    /// Phases the runner has finished and committed.
    pub completed: Vec<PhaseId>,
    /// Number of attempts made per phase, summed across roles.
    pub attempts: HashMap<PhaseId, u32>,
    /// Aggregated token usage so far.
    pub token_usage: TokenUsage,
}

impl RunState {
    /// Build a fresh `RunState` with no completed phases and zero usage.
    pub fn new(
        run_id: impl Into<String>,
        branch: impl Into<String>,
        started_phase: PhaseId,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            branch: branch.into(),
            started_at: Utc::now(),
            started_phase,
            completed: Vec::new(),
            attempts: HashMap::new(),
            token_usage: TokenUsage::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    #[test]
    fn round_trips_through_json() {
        let mut by_role = HashMap::new();
        by_role.insert(
            "implementer".to_string(),
            RoleUsage {
                input: 1234,
                output: 567,
            },
        );
        by_role.insert(
            "auditor".to_string(),
            RoleUsage {
                input: 200,
                output: 50,
            },
        );

        let mut attempts = HashMap::new();
        attempts.insert(pid("02"), 1);
        attempts.insert(pid("10b"), 3);

        let state = RunState {
            run_id: "20260429T143022Z".into(),
            branch: "foreman/run-20260429T143022Z".into(),
            started_at: DateTime::parse_from_rfc3339("2026-04-29T14:30:22Z")
                .unwrap()
                .with_timezone(&Utc),
            started_phase: pid("02"),
            completed: vec![pid("01")],
            attempts,
            token_usage: TokenUsage {
                input: 1434,
                output: 617,
                by_role,
            },
        };

        let json = serde_json::to_string(&state).unwrap();
        let back: RunState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn new_initializes_empty_aggregates() {
        let s = RunState::new("rid", "branch", pid("01"));
        assert_eq!(s.run_id, "rid");
        assert_eq!(s.branch, "branch");
        assert!(s.completed.is_empty());
        assert!(s.attempts.is_empty());
        assert_eq!(s.token_usage.input, 0);
        assert_eq!(s.token_usage.output, 0);
        assert!(s.token_usage.by_role.is_empty());
    }

    #[test]
    fn phase_id_is_usable_as_map_key_through_serde() {
        // Regression guard: HashMap<PhaseId, _> must round-trip through JSON.
        let mut attempts = HashMap::new();
        attempts.insert(pid("01"), 2);
        let json = serde_json::to_string(&attempts).unwrap();
        let back: HashMap<PhaseId, u32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get(&pid("01")), Some(&2));
    }
}
