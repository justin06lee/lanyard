//! Discovery of live Claude Code sessions.
//!
//! Claude Code already maintains a registry at `~/.claude/sessions/<pid>.json`
//! containing the session id, cwd, a derived name and a live busy/idle status.
//! We treat that as the source of truth and enrich each entry with the two
//! things it lacks: the controlling tty, and the terminal window id exported
//! into the process environment.

use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub pid: i32,
    pub session_id: String,
    pub cwd: String,
    /// Repository name, derived from the nearest enclosing git root.
    pub repo: String,
    /// Path of `cwd` relative to the repo root ("" when at the root).
    pub subpath: String,
    /// The name Claude Code derived for itself, e.g. `myrepo-9d`.
    pub claude_name: Option<String>,
    /// `busy` / `idle` — whatever Claude Code last wrote.
    pub status: Option<String>,
    /// Controlling terminal, e.g. `ttys003`.
    pub tty: Option<String>,
    /// Terminal window id from the environment (Alacritty, kitty, WezTerm, …).
    pub window_id: Option<String>,
    /// True when this session was launched with CLAUDE_CODE_DISABLE_TERMINAL_TITLE
    /// set, meaning Lanyard owns the window title outright instead of contesting it.
    pub title_disabled: bool,
    /// Claude Code's own one-line summary of what this session is doing, lifted
    /// from the `ai-title` entries in its transcript.
    pub ai_title: Option<String>,
    pub started_at: Option<i64>,
}

/// The slice of a session's environment Lanyard cares about. Immutable for the
/// lifetime of the process, so it is read once and cached.
#[derive(Debug, Clone, Default)]
pub struct ProcEnv {
    pub window_id: Option<String>,
    pub title_disabled: bool,
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude"))
}

/// BSD-layer facts about one process, read straight from the kernel — no
/// subprocess, no parsing, a few microseconds per call.
pub struct ProcInfo {
    /// The kernel's immutable command name (`p_comm`, 15 chars). Claude Code
    /// repaints its visible title (`pbi_name`) with version info, so identity
    /// checks must look here.
    pub comm: String,
    /// The (possibly self-rewritten) long process name.
    pub name: String,
    pub ppid: i32,
    /// Controlling terminal, e.g. `ttys003`.
    pub tty: Option<String>,
}

impl ProcInfo {
    /// True when either name form marks this as a Claude Code process.
    ///
    /// Current Claude Code repaints both to its bare version number
    /// ("2.1.231"), so this fast path usually misses and identity falls back
    /// to the executable path — see the call in `scan`.
    pub fn is_claude(&self) -> bool {
        self.comm.contains("claude") || self.name.contains("claude")
    }
}

/// The executable path behind a pid, via `proc_pidpath`. The kernel answers
/// from the exec image, so a process that repaints its name can't hide here —
/// Claude Code's resolves to `…/claude/versions/<x.y.z>`.
pub fn exe_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

pub fn proc_info(pid: i32) -> Option<ProcInfo> {
    unsafe {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let read = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        );
        if read < size {
            return None; // no such process (or not ours to inspect)
        }
        let info = info.assume_init();
        Some(ProcInfo {
            comm: c_string(&info.pbi_comm),
            name: c_string(&info.pbi_name),
            ppid: info.pbi_ppid as i32,
            tty: tty_name(info.e_tdev),
        })
    }
}

/// The parent of a pid, for walking up to the terminal app hosting a session.
///
/// Deliberately *not* `proc_info`: the full `PROC_PIDTBSDINFO` flavor is
/// refused across users, and the walk from a session to its terminal app
/// crosses root-owned `/usr/bin/login` — stopping there would misidentify
/// `login` as the terminal. The short flavor answers for any process, the way
/// `ps` can.
pub fn parent_of(pid: i32) -> Option<i32> {
    unsafe {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        let read = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        );
        if read < size {
            return None;
        }
        Some(info.assume_init().pbsi_ppid as i32)
    }
}

fn c_string(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `ttys003` for a controlling terminal device, `None` when there isn't one.
fn tty_name(tdev: u32) -> Option<String> {
    if tdev == u32::MAX {
        return None; // NODEV
    }
    unsafe {
        let name = libc::devname(tdev as libc::dev_t, libc::S_IFCHR);
        if name.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(name).to_string_lossy().into_owned())
    }
}

/// Terminal emulators each export their own window handle; we accept any of them.
const WINDOW_ID_VARS: [&str; 5] = [
    "ALACRITTY_WINDOW_ID",
    "KITTY_WINDOW_ID",
    "WEZTERM_PANE",
    "ITERM_SESSION_ID",
    "WINDOWID",
];

/// A same-user process's environment via `KERN_PROCARGS2` — the exact strings,
/// unlike `ps eww` whose whitespace-splitting mangled values with spaces.
fn proc_env_strings(pid: i32) -> Vec<String> {
    let raw = unsafe {
        let mut argmax: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        if libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut argmax as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return Vec::new();
        }
        let mut buf = vec![0u8; argmax.max(4096) as usize];
        let mut size = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return Vec::new();
        }
        buf.truncate(size);
        buf
    };

    // Layout: argc, the exec path (null-terminated, then null padding), argc
    // argv strings, then the environment strings until an empty one.
    if raw.len() < 4 {
        return Vec::new();
    }
    let argc = i32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]).max(0) as usize;
    let mut i = 4;
    while i < raw.len() && raw[i] != 0 {
        i += 1;
    }
    while i < raw.len() && raw[i] == 0 {
        i += 1;
    }
    for _ in 0..argc {
        while i < raw.len() && raw[i] != 0 {
            i += 1;
        }
        i += 1;
    }
    let mut out = Vec::new();
    while i < raw.len() {
        let start = i;
        while i < raw.len() && raw[i] != 0 {
            i += 1;
        }
        if i == start {
            break;
        }
        out.push(String::from_utf8_lossy(&raw[start..i]).into_owned());
        i += 1;
    }
    out
}

fn proc_env(pid: i32) -> ProcEnv {
    let mut env = ProcEnv::default();
    for entry in proc_env_strings(pid) {
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        if key == "CLAUDE_CODE_DISABLE_TERMINAL_TITLE" {
            env.title_disabled = !matches!(value, "" | "0" | "false");
        } else if env.window_id.is_none() && WINDOW_ID_VARS.contains(&key) && !value.is_empty() {
            env.window_id = Some(value.to_string());
        }
    }
    env
}

/// Remembers where a session's transcript lives and what we last read from it.
#[derive(Debug, Clone, Default)]
struct Transcript {
    path: Option<PathBuf>,
    /// File length at the last read, used to skip re-parsing an idle session.
    len: u64,
    title: Option<String>,
}

/// Transcripts are named `<sessionId>.jsonl` under a per-project directory whose
/// name is a mangled cwd. Rather than reproduce that mangling, just look for the
/// session id — it is unique across projects.
fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let projects = claude_dir().join("projects");
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        let candidate = entry.path().join(format!("{session_id}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Transcripts run to megabytes, so only the tail is read. `ai-title` is
/// rewritten on every turn, so the newest one is always near the end.
const TRANSCRIPT_TAIL: u64 = 128 * 1024;

fn read_ai_title(path: &Path) -> Option<(u64, Option<String>)> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TRANSCRIPT_TAIL)))
        .ok()?;

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);

    // The first line is usually a fragment; it simply fails to parse and is skipped.
    let mut title = None;
    for line in text.lines() {
        if !line.contains("\"ai-title\"") {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(found) = value.get("aiTitle").and_then(|v| v.as_str()) {
                title = Some(found.to_string());
            }
        }
    }
    Some((len, title))
}

/// Walks up from `cwd` looking for a `.git` entry (dir for repos, file for worktrees).
fn git_root(cwd: &Path) -> Option<PathBuf> {
    let mut cursor = Some(cwd);
    while let Some(dir) = cursor {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    None
}

fn basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Owns the caches that keep repeated scans cheap.
///
/// A process's environment never changes and a transcript only needs re-reading
/// when it grows, so neither is worth redoing on every tick.
#[derive(Default)]
pub struct Scanner {
    env: HashMap<i32, ProcEnv>,
    transcripts: HashMap<String, Transcript>,
    /// Registry files that didn't parse the way we expect, per scan. Two
    /// consecutive counts are kept so a file caught mid-write (unparseable for
    /// one tick) doesn't read as format drift.
    errors_now: usize,
    errors_prev: usize,
}

impl Scanner {
    /// Registry files that have looked wrong for two consecutive scans — the
    /// signal that Claude Code changed its registry format under us.
    pub fn registry_errors(&self) -> usize {
        self.errors_now.min(self.errors_prev)
    }

    /// This scan's count alone, for one-shot tools like `lanyard-doctor`.
    pub fn registry_errors_raw(&self) -> usize {
        self.errors_now
    }

    /// Reads the current set of live interactive sessions.
    pub fn scan(&mut self) -> Vec<Session> {
        let dir = claude_dir().join("sessions");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };

        let mut errors = 0;
        let mut sessions = Vec::new();
        // Directory enclosing each session's repo, kept to disambiguate collisions.
        let mut enclosing = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                errors += 1;
                continue;
            };

            let pid = value.get("pid").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            if pid <= 0 {
                // A registry entry without a usable pid isn't a stale file,
                // it's a shape we don't understand.
                errors += 1;
                continue;
            }

            // Guard against both stale files and pid reuse.
            let Some(info) = proc_info(pid) else {
                continue;
            };
            if !info.is_claude() && !exe_path(pid).is_some_and(|p| p.contains("claude")) {
                continue;
            }
            // Background agents (`claude --bg`) are excluded by design; the
            // registry has called them both "background" and "bg" over time.
            if value
                .get("kind")
                .and_then(|v| v.as_str())
                .is_some_and(|kind| kind != "interactive")
            {
                continue;
            }

            let session_id = value
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if session_id.is_empty() {
                errors += 1;
                continue;
            }
            let cwd = value
                .get("cwd")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let cwd_path = PathBuf::from(&cwd);
            let root = git_root(&cwd_path);
            let repo = root
                .as_deref()
                .map(basename)
                .unwrap_or_else(|| basename(&cwd_path));
            let subpath = root
                .as_deref()
                .and_then(|r| cwd_path.strip_prefix(r).ok())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();

            let env = self.env.entry(pid).or_insert_with(|| proc_env(pid)).clone();
            let ai_title = self.ai_title(&session_id);

            enclosing.push(
                root.as_deref()
                    .and_then(Path::parent)
                    .map(basename)
                    .unwrap_or_default(),
            );

            sessions.push(Session {
                pid,
                session_id,
                cwd,
                repo,
                subpath,
                claude_name: value
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                status: value
                    .get("status")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                tty: info.tty,
                window_id: env.window_id,
                title_disabled: env.title_disabled,
                ai_title,
                started_at: value.get("startedAt").and_then(|v| v.as_i64()),
            });
        }

        // Drop cache entries for sessions that have exited.
        self.env
            .retain(|pid, _| sessions.iter().any(|s| s.pid == *pid));
        self.transcripts
            .retain(|id, _| sessions.iter().any(|s| &s.session_id == id));

        self.errors_prev = self.errors_now;
        self.errors_now = errors;

        // A bare repo name is useless when two checkouts share it — `web-client`
        // says nothing when you have three. Qualify only the ones that collide.
        let mut counts: HashMap<String, usize> = HashMap::new();
        for session in &sessions {
            *counts.entry(session.repo.clone()).or_default() += 1;
        }
        for (session, parent) in sessions.iter_mut().zip(&enclosing) {
            if counts.get(&session.repo).copied().unwrap_or(0) > 1 && !parent.is_empty() {
                session.repo = format!("{parent}/{}", session.repo);
            }
        }

        sessions.sort_by_key(|s| (s.started_at.unwrap_or(0), s.pid));
        sessions
    }

    /// Claude Code's own summary for a session, re-read only when the
    /// transcript has grown since last time.
    fn ai_title(&mut self, session_id: &str) -> Option<String> {
        let entry = self.transcripts.entry(session_id.to_string()).or_default();
        if entry.path.is_none() {
            entry.path = find_transcript(session_id);
        }
        let path = entry.path.clone()?;

        match read_ai_title(&path) {
            Some((len, title)) if len != entry.len => {
                entry.len = len;
                entry.title = title;
            }
            None => {
                // Transcript vanished (session ended, or was moved) — re-find next tick.
                entry.path = None;
            }
            _ => {}
        }
        entry.title.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_info_reads_the_current_process() {
        let me = std::process::id() as i32;
        let info = proc_info(me).expect("proc_pidinfo should work on ourselves");
        assert!(info.ppid > 0);
        assert!(!info.name.is_empty());
    }

    #[test]
    fn parent_of_crosses_user_boundaries() {
        // launchd is root-owned; a same-user-only API would refuse it. Its
        // parent is the kernel, pid 0. The terminal-app walk depends on this
        // working, because it passes through root-owned /usr/bin/login.
        assert_eq!(parent_of(1), Some(0));
    }

    #[test]
    fn proc_env_reads_the_current_process() {
        // The test runner always carries PATH; finding it proves the
        // KERN_PROCARGS2 walk lands on the environment block.
        let me = std::process::id() as i32;
        let env = proc_env_strings(me);
        assert!(
            env.iter().any(|e| e.starts_with("PATH=")),
            "no PATH in {} strings",
            env.len()
        );
    }
}


