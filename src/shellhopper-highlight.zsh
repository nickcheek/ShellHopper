[[ -o interactive ]] || return

autoload -Uz add-zle-hook-widget add-zsh-hook

if [[ -n ${SHELLHOPPER_MARKER_TOKEN-} ]]; then
  typeset -g _shellhopper_token=$SHELLHOPPER_MARKER_TOKEN
  unset SHELLHOPPER_MARKER_TOKEN
fi

_shellhopper_record_command() {
  local command=$1
  command=${command//\\/\\\\}
  command=${command//$'\e'/\\e}
  command=${command//$'\a'/\\a}
  command=${command//$'\n'/\\n}
  command=${command//$'\r'/\\r}
  [[ -n $_shellhopper_token ]] || return
  printf '\e]1337;ShellHopper%s;Command=%s\a' "$_shellhopper_token" "$command"
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

# A pane can close without a normal shell exit. Use zsh's incremental writes
# unless the user already has a different incremental history mode enabled.
if [[ -n $HISTFILE && $SAVEHIST -gt 0 && ! -o sharehistory && ! -o incappendhistorytime ]]; then
  setopt incappendhistory
fi

_shellhopper_load_history() {
  emulate -L zsh
  add-zsh-hook -d precmd _shellhopper_load_history
  [[ -n $_shellhopper_token ]] || return 0
  zmodload zsh/parameter || return 0
  local -a events
  local event
  events=( ${(onk)history} )
  # Send the newest COMMAND_HISTORY_LIMIT entries oldest first, before Prompt.
  local -i offset=$(( ${#events} > 1000 ? ${#events} - 1000 : 0 ))
  for event in "${events[@]:$offset}"; do
    _shellhopper_record_command "${history[$event]}"
  done
}
add-zsh-hook precmd _shellhopper_load_history

_shellhopper_set_buffer() {
  local text
  zle .bracketed-paste text
  BUFFER=$text
  CURSOR=${#BUFFER}
}

_shellhopper_accept_line() {
  zle .accept-line
}

zle -N _shellhopper_set_buffer
zle -N _shellhopper_accept_line

_shellhopper_prompt() {
  [[ -n $_shellhopper_token ]] || return
  local keymap
  for keymap in emacs viins vicmd; do
    bindkey -M $keymap '^[[5733~' _shellhopper_set_buffer
    bindkey -M $keymap '^[[5734~' _shellhopper_accept_line
  done
  printf '\e]1337;ShellHopper%s;Prompt\a' "$_shellhopper_token"
}
add-zsh-hook precmd _shellhopper_prompt
