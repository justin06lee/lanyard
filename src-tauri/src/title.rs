//! Terminal-title identity channel.
//!
//! Claude Code names every window `✳ Claude Code`, which is precisely why you
//! can't tell sessions apart. gru writes its own OSC title to each session's
//! tty, embedding a machine-readable token. Because Alacritty is configured
//! `decorations = "Buttonless"` the title is invisible in the window itself —
//! it only surfaces in Mission Control, where the leading human name is a bonus.

use std::fs::OpenOptions;
use std::io::Write;

const OPEN: &str = "\u{27E6}gru:";
const CLOSE: char = '\u{27E7}';

/// `my-project ⟦gru:29260⟧`
pub fn compose(name: &str, pid: i32) -> String {
    format!("{name} {OPEN}{pid}{CLOSE}")
}

/// Recovers the session pid from a window title, if gru stamped it.
pub fn parse_pid(title: &str) -> Option<i32> {
    let start = title.find(OPEN)? + OPEN.len();
    let rest = &title[start..];
    let end = rest.find(CLOSE)?;
    rest[..end].trim().parse().ok()
}

/// Writes an OSC-0 title sequence directly to a session's terminal device.
///
/// OSC is out-of-band as far as the screen buffer is concerned, so this cannot
/// scribble on Claude Code's TUI — worst case the emulator ignores it.
pub fn stamp(tty: &str, text: &str) -> std::io::Result<()> {
    let path = format!("/dev/{tty}");
    let mut dev = OpenOptions::new().write(true).open(path)?;
    write!(dev, "\u{1b}]0;{text}\u{7}")?;
    dev.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_pid() {
        let t = compose("hikmah.chat", 6202);
        assert_eq!(parse_pid(&t), Some(6202));
    }

    #[test]
    fn tolerates_names_containing_brackets() {
        let t = compose("weird ⟦name⟧", 42);
        assert_eq!(parse_pid(&t), Some(42));
    }

    #[test]
    fn ignores_untagged_titles() {
        assert_eq!(parse_pid("✳ Claude Code"), None);
        assert_eq!(parse_pid(""), None);
    }
}
