# hintkit shell integration for zsh.
#
# Source this from your ~/.zshrc, typically by running:
#
#   hintkit init zsh >> ~/.zshrc
#
# The integration is a no-op outside hintkit (HINTKIT_WRAPPED guard), so
# leaving it sourced when you're not running under hintkit is harmless.
#
# It emits standard OSC 133 (semantic prompt) and OSC 7 (current
# directory) escape sequences, the same vocabulary used by iTerm2,
# WezTerm, VS Code's terminal, kitty, and Ghostty. That means many other
# tools will read these too and Just Work — there's nothing hintkit-
# proprietary about the protocol.

if [[ -z "$HINTKIT_WRAPPED" || "$TERM" == "dumb" ]]; then
    return 0
fi

# Idempotency: re-sourcing must not stack hooks or PROMPT markers.
if [[ -n "$HINTKIT_SHELL_INTEGRATION_LOADED" ]]; then
    return 0
fi

# precmd: emit D for the previous command's exit code, then A to mark
# the start of the new prompt, then OSC 7 with the current cwd.
hintkit_precmd() {
    local _hintkit_status=$?
    printf '\e]133;D;%d\e\\' "$_hintkit_status"
    printf '\e]133;A\e\\'
    printf '\e]7;file://%s%s\e\\' "${HOST:-localhost}" "$PWD"
}

# preexec: emit C right before the command runs.
hintkit_preexec() {
    printf '\e]133;C\e\\'
}

# Append B marker to PROMPT (zero visible width — pure control sequence
# delimiting end-of-prompt / start-of-input). Idempotent: skip if
# already present from a prior source.
case "$PROMPT" in
    *$'\e]133;B'*) ;;
    *) PROMPT="${PROMPT}"$'\e]133;B\e\\' ;;
esac

# Register hooks. typeset -ga keeps them global arrays even if a function
# scope is active when this file is sourced.
typeset -ga precmd_functions preexec_functions
precmd_functions+=(hintkit_precmd)
preexec_functions+=(hintkit_preexec)

export HINTKIT_SHELL_INTEGRATION_LOADED=1
export HINTKIT_SHELL_INTEGRATION=zsh
