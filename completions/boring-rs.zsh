#compdef boring-rs
_boring_rs() {
    local -a values
    if (( CURRENT == 2 )); then
        values=(open close list init edit shutdown version help completions)
    elif [[ ${words[CURRENT-1]} == -g || ${words[CURRENT-1]} == --group ]]; then
        values=("${(@f)$(boring-rs __complete groups 2>/dev/null)}")
    elif [[ ${words[2]} == close || ${words[2]} == c ]]; then
        values=(--all --group "${(@f)$(boring-rs __complete names --running 2>/dev/null)}")
    elif [[ ${words[2]} == open || ${words[2]} == o ]]; then
        values=(--all --group --dry-run "${(@f)$(boring-rs __complete names 2>/dev/null)}")
    else
        values=(--help --json --group)
    fi
    compadd -- "${values[@]}"
}
compdef _boring_rs boring-rs
