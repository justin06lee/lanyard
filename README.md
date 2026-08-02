# gru

A floating name tag for whichever Claude Code session you're currently looking at.

When you run a dozen Claude Code sessions across a dozen Desktops, every window
looks identical — Claude Code titles them all `✳ Claude Code`. gru puts a small
always-on-top pill over the focused terminal telling you *which* session it is,
named after its repository and renameable to whatever you like.

```
   ╭──────────────────────────────────╮
   │ ● hikmah.chat        web-client  │
   ╰──────────────────────────────────╯
```

- **Follows you across Desktops.** One floater, pinned to every Space.
- **Follows focus within a Desktop.** Switch between two terminals on the same
  screen and the tag updates.
- **Names itself from the repo**, and remembers renames per session *and* per
  directory, so tomorrow's session in the same checkout keeps the name.
- **Live status.** The dot pulses while Claude is working and rests when idle.

---

## Install

Requires Rust and Node.

```bash
npm install
npm run build          # produces src-tauri/target/release/bundle/macos/gru.app
cp -r src-tauri/target/release/bundle/macos/gru.app /Applications/
./scripts/setup-shell.sh
```

Then launch gru from /Applications. It lives in the menu bar — there is no Dock
icon.

On first launch macOS will ask for **Accessibility** access. gru needs it to see
which window has focus; without it the floater stays hidden. If you miss the
prompt: System Settings › Privacy & Security › Accessibility, then add gru. The
tray menu's *Accessibility access…* item reopens the prompt.

`setup-shell.sh` appends `CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` to your shell
profile — see [How it works](#how-it-works) for why that matters. Restart your
Claude Code sessions afterwards.

---

## Using it

| Action | How |
| --- | --- |
| Rename the focused session | Double-click the name on the floater |
| Reset to the repo name | Rename it to an empty string |
| Move the floater | Click ⤡ on the pill, or tray › *Move floater* — cycles the six corners |
| See every session | Tray › *Sessions…* |
| Hide/show the floater | Tray › *Toggle floater* |

The floater only appears when a Claude Code session has focus. Switch to your
browser and it gets out of the way.

Settings live in `~/.config/gru/config.json`.

---

## How it works

Claude Code already keeps a registry at `~/.claude/sessions/<pid>.json` with each
session's id, working directory, a derived name and a live busy/idle status. gru
reads it rather than inventing its own tracking, and supplies the two things it
lacks: a link from session to *window*, and somewhere to display it.

**Identity travels in the terminal title.** This is the only channel that works
for terminals like Alacritty, where every window belongs to one shared process —
so process ancestry can't tell windows apart. gru writes an OSC-0 title to each
session's tty carrying a machine-readable token:

```
hikmah.chat ⟦gru:6202⟧
```

**Focus resolution uses the macOS Accessibility API** to read the focused
window's title and geometry. AX deliberately reports only windows on the *active*
Space, which is exactly right here: whatever it can see is what you're looking
at. The floater is marked `visibleOnAllWorkspaces`, so a single window follows
you across every Desktop instead of one per Space.

### Why `CLAUDE_CODE_DISABLE_TERMINAL_TITLE`

Claude Code rewrites the terminal title as it works — `✳ Claude Code` at rest,
then a summary of the current task. Since gru uses the title as its identity
channel, the two overwrite each other every few seconds. gru copes (it notices
an untagged title and re-stamps, throttled to once per 750 ms) but the title
visibly flickers in Mission Control.

Setting `CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` stops Claude Code touching the
title, leaving gru in sole possession. The Sessions panel tells you how many
running sessions still need this.

This is invisible in normal use: Alacritty's `decorations = "Buttonless"` hides
window titles entirely, so the title bar is free real estate. Where it *does*
show — Mission Control — the human-readable name comes first, which makes
picking the right Desktop easier.

---

## Terminal support

Developed against **Alacritty**. The mechanism is emulator-agnostic — it needs
only OSC title support and AX-visible windows — so Terminal.app, WezTerm, kitty
and Ghostty should work.

One caveat for **tabbed** terminals (iTerm2, Terminal.app, WezTerm): macOS
exposes a tabbed window as a single AX window, so gru resolves the tab that owns
the title, which is the foreground one. Split panes within a single tab can't be
distinguished. Alacritty's one-window-per-session model has no such ambiguity.

Window ids are read from whichever of `ALACRITTY_WINDOW_ID`, `KITTY_WINDOW_ID`,
`WEZTERM_PANE`, `ITERM_SESSION_ID` or `WINDOWID` the emulator exports.

---

## Development

```bash
npm run dev                          # hot-reloading dev build
cargo test --manifest-path src-tauri/Cargo.toml
```

| Path | Purpose |
| --- | --- |
| `src-tauri/src/sessions.rs` | Reads Claude Code's registry, enriches with tty + window id |
| `src-tauri/src/ax.rs` | Accessibility bridge: focused app, window title, rect |
| `src-tauri/src/title.rs` | OSC title stamping and token parsing |
| `src-tauri/src/tracker.rs` | Polling loop: focus → session → floater placement |
| `src-tauri/src/store.rs` | Persisted names and floater anchor |
| `ui/` | Floater and sessions panel (plain HTML/CSS/JS, no build step) |

The dev build is a different binary from the bundled app, so macOS treats it as
a separate Accessibility entry — you'll be asked to grant access twice.

---

## Troubleshooting

**The floater never appears.** Check Accessibility access is granted to the
binary you're actually running. Tray › *Sessions…* shows a banner when it isn't.

**It shows the wrong session, or lags a moment behind.** Almost always the title
tug-of-war — run `./scripts/setup-shell.sh` and restart those sessions.

**A session is missing from the panel.** gru lists interactive sessions with a
live pid and a controlling tty; background agents (`--bg`) are excluded by
design.
