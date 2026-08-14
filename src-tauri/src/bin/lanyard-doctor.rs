//! Prints exactly what Lanyard sees, without the GUI in the way.
//!
//! Run it from a terminal when the floater shows the wrong session — or
//! nothing at all — to find out which half is misbehaving.
//!
//!     cargo run --manifest-path src-tauri/Cargo.toml --bin lanyard-doctor

use lanyard_lib::{ax, sessions, store::Config, title, tracker::host_app};

fn main() {
    let config = Config::load();

    println!("lanyard doctor");
    println!("──────────────");
    println!(
        "accessibility : {}",
        if ax::is_trusted() {
            "granted"
        } else {
            "NOT GRANTED — the floater will stay hidden"
        }
    );
    println!("config        : {}", store_path());
    println!();

    let mut scanner = sessions::Scanner::default();
    let found = scanner.scan();
    println!("sessions ({})", found.len());
    if found.is_empty() {
        println!("  none — is Claude Code running interactively?");
    }
    for s in &found {
        let name = config.name_for(&s.session_id, &s.cwd, &s.repo);
        println!(
            "  {:>6}  {:<24} {:<10} tty={:<8} title-disabled={:<5} win={}",
            s.pid,
            name,
            s.status.as_deref().unwrap_or("?"),
            s.tty.as_deref().unwrap_or("-"),
            s.title_disabled,
            s.window_id.as_deref().unwrap_or("-"),
        );
        println!("          {}", s.cwd);
        if let Some(summary) = &s.ai_title {
            println!("          “{summary}”");
        }
    }

    let contested = found.iter().filter(|s| !s.title_disabled).count();
    if contested > 0 {
        println!();
        println!(
            "  {contested} session(s) still rewrite their own title — run \
             scripts/setup-shell.sh"
        );
    }

    let drift = scanner.registry_errors_raw();
    if drift > 0 {
        println!();
        println!(
            "  {drift} registry file(s) in ~/.claude/sessions did not parse the way \
             Lanyard expects — a Claude Code update may have changed the format"
        );
    }

    println!();
    probe_extra();

    // Same candidate set the tracker uses: the terminal apps hosting sessions,
    // plus any extra pids given on the command line.
    let hosts: Vec<i32> = found
        .iter()
        .filter_map(|s| host_app(s.pid))
        .chain(std::env::args().skip(1).filter_map(|a| a.parse().ok()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    println!("terminal apps : {hosts:?}");

    match ax::focused_window_among(&hosts) {
        None => {
            println!("focused       : no terminal app is frontmost");
            println!("  system-wide : {}", ax::focus_diagnostic());
        }
        Some(win) => {
            println!("focused       : pid={} {:?}", win.pid, win.title);
            println!(
                "  rect        : {}×{} at ({}, {})",
                win.w, win.h, win.x, win.y
            );
            match title::parse_pid(&win.title) {
                Some(pid) if found.iter().any(|s| s.pid == pid) => {
                    println!("  resolved    : session pid {pid} ✓")
                }
                Some(pid) => println!("  resolved    : tagged pid {pid}, but no such live session"),
                None => println!(
                    "  resolved    : no Lanyard tag in this title — either it isn't a \
                     Claude terminal, or the title was overwritten"
                ),
            }
        }
    }
}

/// Reports the focused window of any app pid passed on the command line —
/// useful for checking a terminal that isn't frontmost right now.
fn probe_extra() {
    for arg in std::env::args().skip(1) {
        let Ok(pid) = arg.parse::<i32>() else { continue };
        match ax::probe_app(pid) {
            Err(why) => println!("probe {pid:<7} : {why}"),
            Ok(t) => {
                let tag = match title::parse_pid(&t) {
                    Some(session) => format!("Lanyard tag -> session {session}"),
                    None => "no Lanyard tag".into(),
                };
                println!("probe {pid:<7} : {t:?}\n                {tag}");
            }
        }
    }
}

fn store_path() -> String {
    lanyard_lib::store::config_path().display().to_string()
}
