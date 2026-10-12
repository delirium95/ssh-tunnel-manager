_boring_rs() {
    local current previous command candidates
    current="${COMP_WORDS[COMP_CWORD]}"
    previous="${COMP_WORDS[COMP_CWORD-1]}"
    command="${COMP_WORDS[1]}"
    if [[ $COMP_CWORD -eq 1 ]]; then
        candidates='open close list init edit shutdown version help completions'
    elif [[ "$previous" == '-g' || "$previous" == '--group' ]]; then
        candidates="$(boring-rs __complete groups 2>/dev/null)"
    elif [[ "$command" == 'close' || "$command" == 'c' ]]; then
        candidates="--all --group $(boring-rs __complete names --running 2>/dev/null)"
    elif [[ "$command" == 'open' || "$command" == 'o' ]]; then
        candidates="--all --group --dry-run $(boring-rs __complete names 2>/dev/null)"
    else
        candidates='--help --json --group'
    fi
    COMPREPLY=( $(compgen -W "$candidates" -- "$current") )
}
complete -F _boring_rs boring-rs
