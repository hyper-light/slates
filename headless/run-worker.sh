#!/bin/bash
# Runs one headless slates worker on Ada's API key (read from ~/.zshrc inside this process; never echoed).
set -u
g="$1"
prompt="$(sed "s/GNUM/$g/g" ~/.local/state/slates-fanout/headless/prompt-template.txt)"
eval "$(command grep '^export ANTHROPIC_API_KEY=' ~/.zshrc)"
cd ~/slates-work || exit 1
exec claude -p --permission-mode bypassPermissions --model claude-opus-5-5 "$prompt"
