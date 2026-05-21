# hintkit shell integration for bash (requires bash 4.4+).
#
# Source this from your ~/.bashrc, typically by running:
#
#   hintkit init bash >> ~/.bashrc
#
# Like the zsh integration, this is a no-op outside hintkit and emits
# standard OSC 133 / OSC 7 sequences. macOS users on the system bash 3.2
# need to install a newer bash (Homebrew: `brew install bash`) — PS0 (used
# below for the OSC 133 C marker) was added in 4.4.

if [[ -z "$HINTKIT_WRAPPED" || "$TERM" == "dumb" ]]; then
    return 0 2>/dev/null || true
fi

if [[ -n "$HINTKIT_SHELL_INTEGRATION_LOADED" ]]; then
    return 0 2>/dev/null || true
fi

if [[ -z "${BASH_VERSINFO+x}" ]] \
    || (( BASH_VERSINFO[0] < 4 )) \
    || (( BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4 )); then
    printf 'hintkit: shell integration requires bash 4.4+ (current: %s)\n' \
        "${BASH_VERSION:-unknown}" >&2
    return 0 2>/dev/null || true
fi

# PROMPT_COMMAND hook: D for previous command's exit, A for new prompt,
# OSC 7 for cwd. $? at the top captures the previous command's exit;
# subsequent commands inside the hook clobber it, so save first.
hintkit_prompt_hook() {
    local _hintkit_status=$?
    printf '\e]133;D;%d\e\\' "$_hintkit_status"
    printf '\e]133;A\e\\'
    printf '\e]7;file://%s%s\e\\' "${HOSTNAME:-localhost}" "$PWD"
}

# Prepend our hook to any existing PROMPT_COMMAND so we run *before*
# user/framework hooks (starship, atuin, etc.) — they shouldn't see a
# stale OSC 133 D.
if [[ -n "$PROMPT_COMMAND" ]]; then
    PROMPT_COMMAND="hintkit_prompt_hook; $PROMPT_COMMAND"
else
    PROMPT_COMMAND="hintkit_prompt_hook"
fi

# PS0 is printed by readline after Enter is pressed but before the
# command runs — exactly when OSC 133 C should fire.
PS0="${PS0}"$'\e]133;C\e\\'

# PS1: append B marker delimiting end-of-prompt / start-of-input.
case "$PS1" in
    *$'\e]133;B'*) ;;
    *) PS1="${PS1}"$'\e]133;B\e\\' ;;
esac

export HINTKIT_SHELL_INTEGRATION_LOADED=1
export HINTKIT_SHELL_INTEGRATION=bash
