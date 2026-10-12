complete -c boring-rs -f
complete -c boring-rs -n '__fish_use_subcommand' -a 'open close list init edit shutdown version help completions'
complete -c boring-rs -n '__fish_seen_subcommand_from open o close c' -s a -l all -d 'Select all tunnels'
complete -c boring-rs -n '__fish_seen_subcommand_from open o close c list l ls' -s g -l group -r -a '(boring-rs __complete groups 2>/dev/null)'
complete -c boring-rs -n '__fish_seen_subcommand_from open o' -a '(boring-rs __complete names 2>/dev/null)'
complete -c boring-rs -n '__fish_seen_subcommand_from open o' -l dry-run -d 'Print SSH arguments'
complete -c boring-rs -n '__fish_seen_subcommand_from close c' -a '(boring-rs __complete names --running 2>/dev/null)'
complete -c boring-rs -n '__fish_seen_subcommand_from list l ls' -l json -d 'JSON output'
