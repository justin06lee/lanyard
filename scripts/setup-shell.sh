#!/usr/bin/env bash
# Hands the terminal title over to Lanyard.
#
# Claude Code continuously rewrites the terminal title (first "✳ Claude Code",
# then a summary of whatever it is working on). Lanyard uses that same title as its
# identity channel, so without this the two overwrite each other every few
# seconds. Setting CLAUDE_CODE_DISABLE_TERMINAL_TITLE stops Claude Code from
# touching the title at all and leaves Lanyard in sole possession.
#
# Usage: ./scripts/setup-shell.sh [profile]   (defaults to your shell's rc file)

set -euo pipefail

LINE='export CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1  # Lanyard owns the terminal title'
MARKER='CLAUDE_CODE_DISABLE_TERMINAL_TITLE'

default_profile() {
  case "${SHELL##*/}" in
    zsh)  echo "${ZDOTDIR:-$HOME}/.zshrc" ;;
    bash) [ -f "$HOME/.bash_profile" ] && echo "$HOME/.bash_profile" || echo "$HOME/.bashrc" ;;
    fish) echo "$HOME/.config/fish/config.fish" ;;
    *)    echo "$HOME/.profile" ;;
  esac
}

PROFILE="${1:-$(default_profile)}"

# Follow symlinks so dotfiles repos get the edit, not the link.
if [ -L "$PROFILE" ]; then
  PROFILE="$(cd "$(dirname "$PROFILE")" && realpath "$PROFILE")"
fi

if [ ! -e "$PROFILE" ]; then
  printf 'Profile %s does not exist; creating it.\n' "$PROFILE"
  touch "$PROFILE"
fi

if grep -q "$MARKER" "$PROFILE"; then
  printf '✓ %s already sets %s — nothing to do.\n' "$PROFILE" "$MARKER"
  exit 0
fi

if [ "${PROFILE##*.}" = "fish" ]; then
  LINE='set -gx CLAUDE_CODE_DISABLE_TERMINAL_TITLE 1  # Lanyard owns the terminal title'
fi

printf '\n%s\n' "$LINE" >> "$PROFILE"
printf '✓ Added to %s\n' "$PROFILE"
printf '\nRestart your Claude Code sessions (or open new terminal windows) for it\n'
printf 'to take effect. Lanyard works before then too — it just has to keep\n'
printf 'reclaiming the title, which you may notice in Mission Control.\n'
