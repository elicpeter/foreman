//! `deferred.md` domain types.
//!
//! Phase 2 introduces the type vocabulary; phase 4 adds parsing, serialization,
//! and snapshot/verify utilities on top.

use serde::{Deserialize, Serialize};

use crate::plan::PhaseId;

/// A single checkbox item under `## Deferred items`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredItem {
    /// The text following the checkbox marker.
    pub text: String,
    /// `true` if the box is checked. Swept by [`DeferredDoc::sweep`] in phase 4.
    pub done: bool,
}

/// A `### From phase <id>: <title>` block under `## Deferred phases`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredPhase {
    /// Phase id this replan was emitted from.
    pub source_phase: PhaseId,
    /// Title text after the colon in the H3 heading.
    pub title: String,
    /// Raw markdown body following the heading line, preserved verbatim.
    pub body: String,
}

/// Parsed `deferred.md`: pending checklist items plus replanned phase blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredDoc {
    /// Checklist items from `## Deferred items`.
    pub items: Vec<DeferredItem>,
    /// Replan blocks from `## Deferred phases`.
    pub phases: Vec<DeferredPhase>,
}

impl DeferredDoc {
    /// An empty document — equivalent to a missing `deferred.md`.
    pub fn empty() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    #[test]
    fn empty_is_default() {
        assert_eq!(DeferredDoc::empty(), DeferredDoc::default());
        let doc = DeferredDoc::empty();
        assert!(doc.items.is_empty());
        assert!(doc.phases.is_empty());
    }

    #[test]
    fn serde_round_trip() {
        let doc = DeferredDoc {
            items: vec![
                DeferredItem {
                    text: "polish error message".into(),
                    done: false,
                },
                DeferredItem {
                    text: "remove unused stub".into(),
                    done: true,
                },
            ],
            phases: vec![DeferredPhase {
                source_phase: pid("07"),
                title: "rework agent trait".into(),
                body: "Some body text\n- bullet\n".into(),
            }],
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: DeferredDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, back);
    }
}
