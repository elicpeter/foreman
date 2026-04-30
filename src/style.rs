//! ANSI styling helpers for CLI output.
//!
//! All coloring is gated on a terminal-detection check so piped or redirected
//! output stays machine-readable. Set `NO_COLOR=1` to disable unconditionally.

use std::io::IsTerminal;

/// Returns `true` when stderr is a color-capable terminal.
pub fn use_color_stderr() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

/// Returns `true` when stdout is a color-capable terminal.
pub fn use_color_stdout() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const MAGENTA: &str = "\x1b[35m";
pub const CYAN: &str = "\x1b[36m";
pub const DARK_GRAY: &str = "\x1b[90m";
pub const BOLD_RED: &str = "\x1b[1;31m";
pub const BOLD_GREEN: &str = "\x1b[1;32m";
pub const BOLD_YELLOW: &str = "\x1b[1;33m";
pub const BOLD_CYAN: &str = "\x1b[1;36m";
pub const BOLD_WHITE: &str = "\x1b[1;37m";

/// Wrap `text` with an ANSI `code` and reset. Returns plain `text` when disabled.
pub fn col(enabled: bool, code: &str, text: &str) -> String {
    if enabled {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}
