//! Orchestration loop and event channel.
//!
//! Filled in starting in phase 12. The runner emits events to a broadcast
//! channel; the plain CLI logger and the TUI both subscribe to it.
