//! Agent abstraction. Concrete implementations live in submodules.
//!
//! The `Agent` trait, request/event/outcome types, and subprocess helpers are
//! defined in phase 7. `claude_code` and `dry_run` impls land in phases 7–8.

pub mod claude_code;
pub mod dry_run;
