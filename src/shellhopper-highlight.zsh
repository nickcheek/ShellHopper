[[ -o interactive ]] || return

autoload -Uz add-zle-hook-widget add-zsh-hook

_shellhopper_record_command() {
  local command=$1
  command=${command//\\/\\\\}
  command=${command//$'\e'/\\e}
  command=${command//$'\a'/\\a}
  command=${command//$'\n'/\\n}
  command=${command//$'\r'/\\r}
  printf '\e]1337;ShellHopperCommand=%s\a' "$command"
}

_shellhopper_syntax_highlight() {
  region_highlight=()

  local remaining=$BUFFER
  local token style command=''
  local offset=0 position=0 start end

  while [[ $remaining =~ '[^[:space:]]+' ]]; do
    token=$MATCH
    start=$(( offset + MBEGIN - 1 ))
    end=$(( offset + MEND ))
    style='fg=white'

    if (( position == 0 )); then
      command=$token
      style='fg=10,bold'
    elif [[ $token == --* || $token == -?* ]]; then
      style='fg=229'
    elif [[ $command == aws && position -le 2 ]]; then
      style='fg=117,bold'
    elif (( position == 1 )) && [[ $command == git || $command == gh || $command == docker || $command == kubectl || $command == cargo || $command == npm || $command == brew ]]; then
      style='fg=117,bold'
    fi

    region_highlight+=("$start $end $style")
    offset=$(( offset + MEND ))
    remaining=${remaining[MEND+1,-1]}
    (( position++ ))
  done
}

add-zle-hook-widget line-init _shellhopper_syntax_highlight
add-zle-hook-widget line-pre-redraw _shellhopper_syntax_highlight
add-zsh-hook preexec _shellhopper_record_command
