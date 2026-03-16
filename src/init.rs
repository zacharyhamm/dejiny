pub fn init(shell: &str) {
    match shell {
        "zsh" => print!("{}", zsh_hook()),
        "bash" => print!("{}", bash_hook()),
        _ => {
            eprintln!("dejiny: unsupported shell: {shell}");
            std::process::exit(1);
        }
    }
}

fn zsh_hook() -> &'static str {
    r#"zmodload zsh/datetime

__dejiny_preexec() {
    __dejiny_cmd="$1"
    __dejiny_start="$EPOCHREALTIME"
}

__dejiny_precmd() {
    local exit_code=$?
    if [[ -n "$__dejiny_recording" ]]; then
        print -s -- "$__dejiny_original_cmd"
        unset __dejiny_recording __dejiny_original_cmd __dejiny_cmd __dejiny_start
        return
    fi
    [ -z "$__dejiny_cmd" ] && return
    local end="$EPOCHREALTIME"
    dejiny store \
        --command "$__dejiny_cmd" \
        --exit-code "$exit_code" \
        --start "$__dejiny_start" \
        --end "$end" \
        --cwd "$PWD"
    unset __dejiny_cmd
    unset __dejiny_start
}

__dejiny_zshaddhistory() {
    [[ -n "$__dejiny_recording" ]] && return 2
    return 0
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec __dejiny_preexec
add-zsh-hook precmd __dejiny_precmd
add-zsh-hook zshaddhistory __dejiny_zshaddhistory

__dejiny_accept_line() {
    if [[ "$DEJINY_RECORD_ALL" == 1 || "$DEJINY_RECORD_ALL" == true ]] \
       && [[ -n "${BUFFER// /}" \
          && "$BUFFER" != dejiny\ record\ * && "$BUFFER" != dejiny\ replay\ * \
          && "$BUFFER" != \ * ]]; then
        local words=(${=BUFFER})
        case "${words[1]}" in
            fg|bg|jobs|disown|wait|\
            cd|pushd|popd|dirs|\
            export|unset|source|.|\
            set|setopt|unsetopt|\
            alias|unalias|\
            typeset|declare|local|readonly|\
            exec|exit|logout|\
            eval|builtin|hash|rehash|\
            trap|history|fc) ;;
            *)
                __dejiny_original_cmd="$BUFFER"
                __dejiny_recording=1
                BUFFER="dejiny record -- ${(q)__dejiny_original_cmd}"
                ;;
        esac
    fi
    zle .accept-line
}
zle -N accept-line __dejiny_accept_line

__dejiny_search_widget() {
    local selected
    selected="$(dejiny search -- "$BUFFER" </dev/tty)"
    if [[ "$selected" == __DEJINY_REPLAY__* ]]; then
        BUFFER="dejiny replay ${selected#__DEJINY_REPLAY__}"
        zle accept-line
        return
    elif [ -n "$selected" ]; then
        BUFFER="$selected"
        CURSOR=$#BUFFER
    fi
    zle reset-prompt
}
zle -N __dejiny_search_widget
bindkey '^R' __dejiny_search_widget
"#
}

fn bash_hook() -> &'static str {
    r#"__dejiny_preexec() {
    case "$BASH_COMMAND" in
        __dejiny_*) return ;;
    esac
    if [ -z "$__dejiny_preexec_fired" ]; then
        __dejiny_preexec_fired=1
        __dejiny_cmd="$(HISTTIMEFORMAT= history 1 | sed 's/^[[:space:]]*[0-9]*[[:space:]]*//')"
        __dejiny_start="${EPOCHREALTIME:-$(date +%s)}"
    fi
}

__dejiny_precmd() {
    local exit_code=$?
    if [[ -n "$__dejiny_recording" ]]; then
        history -d "$(history 1 | sed 's/^[[:space:]]*\([0-9]*\).*/\1/')" 2>/dev/null
        history -s -- "$__dejiny_original_cmd"
        unset __dejiny_recording __dejiny_original_cmd __dejiny_preexec_fired __dejiny_cmd __dejiny_start
        return
    fi
    [ -z "$__dejiny_preexec_fired" ] && return
    unset __dejiny_preexec_fired
    [ -z "$__dejiny_cmd" ] && return
    local end="${EPOCHREALTIME:-$(date +%s)}"
    dejiny store \
        --command "$__dejiny_cmd" \
        --exit-code "$exit_code" \
        --start "$__dejiny_start" \
        --end "$end" \
        --cwd "$PWD"
    unset __dejiny_cmd
    unset __dejiny_start
}

__dejiny_search_widget() {
    local selected
    selected="$(dejiny search -- "$READLINE_LINE" </dev/tty)"
    if [[ "$selected" == __DEJINY_REPLAY__* ]]; then
        READLINE_LINE="dejiny replay ${selected#__DEJINY_REPLAY__}"
        READLINE_POINT=${#READLINE_LINE}
        return
    elif [ -n "$selected" ]; then
        READLINE_LINE="$selected"
        READLINE_POINT=${#selected}
    fi
}

__dejiny_maybe_record() {
    [[ "$DEJINY_RECORD_ALL" != 1 && "$DEJINY_RECORD_ALL" != true ]] && return
    [[ -z "${READLINE_LINE// /}" ]] && return
    [[ "$READLINE_LINE" == dejiny\ record\ * || "$READLINE_LINE" == dejiny\ replay\ * ]] && return
    [[ "$READLINE_LINE" == \ * ]] && return
    local trimmed="${READLINE_LINE#"${READLINE_LINE%%[![:space:]]*}"}"
    local first_word="${trimmed%% *}"
    case "$first_word" in
        fg|bg|jobs|disown|wait|\
        cd|pushd|popd|dirs|\
        export|unset|source|.|\
        set|shopt|\
        alias|unalias|\
        typeset|declare|local|readonly|\
        exec|exit|logout|\
        eval|builtin|hash|rehash|\
        trap|history|fc) return ;;
    esac
    __dejiny_recording=1
    __dejiny_original_cmd="$READLINE_LINE"
    READLINE_LINE="dejiny record -- $(printf '%q' "$READLINE_LINE")"
    READLINE_POINT=${#READLINE_LINE}
}

trap '__dejiny_preexec' DEBUG
PROMPT_COMMAND+=(__dejiny_precmd)
bind -x '"\C-r": __dejiny_search_widget'
bind -x '"\C-x\C-d": __dejiny_maybe_record'
bind '"\C-m": "\C-x\C-d\C-j"'
"#
}
