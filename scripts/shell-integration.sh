# Berth shell integration. Sourced by the user's shell, opt in, never installed
# without asking. Emits OSC 133 markers so Berth knows where a prompt begins,
# where a command begins and ends, and what it exited with.
#
# All four markers are emitted, not only D. A alone would give no prompt row to
# style, B no boundary between prompt and input, and C no way to tell a
# command's output from its echo. The timeline needs D for exit codes; the
# command blocks in the UI need the other three.
#
#   A  prompt start
#   B  command start, the end of the prompt
#   C  output start, the command is now running
#   D  command end, with the exit status
#
# Nothing here writes to disk, starts a process, or talks to the network.

# tmux is a terminal emulator, not a pipe, and it drops OSC sequences it does
# not recognise. Measured on tmux 3.4: none of the four markers reach an
# attached client. Inside tmux they are therefore wrapped in tmux's passthrough
# sequence, which requires `allow-passthrough` on the pane; Berth sets that on
# its own sessions. Every ESC inside the wrapper must be doubled.
__berth_osc() {
  if [ -n "${TMUX:-}" ]; then
    printf '\033Ptmux;\033\033]133;%s\007\033\\' "$1"
  else
    printf '\033]133;%s\007' "$1"
  fi
}

# The same wrapping, as a string, for embedding in a prompt.
__berth_osc_str() {
  if [ -n "${TMUX:-}" ]; then
    printf '\033Ptmux;\033\033]133;%s\007\033\\' "$1"
  else
    printf '\033]133;%s\007' "$1"
  fi
}

if [ -n "${ZSH_VERSION:-}" ]; then

  __berth_precmd() {
    # Runs after a command finishes and before the next prompt: close the
    # previous command, then open the new prompt.
    local exit_code=$?
    if [ -n "${__berth_running:-}" ]; then
      __berth_osc "D;${exit_code}"
      unset __berth_running
    fi
    __berth_osc "A"
  }

  __berth_preexec() {
    # The prompt is over and the command is about to run.
    __berth_osc "C"
    __berth_running=1
  }

  # B goes at the very end of the prompt string, which is the only place that
  # marks where user input starts.
  if [ -z "${__berth_zsh_installed:-}" ]; then
    PS1="${PS1}%{$(__berth_osc_str B)%}"
    __berth_zsh_installed=1
  fi

  autoload -Uz add-zsh-hook 2>/dev/null
  if command -v add-zsh-hook >/dev/null 2>&1; then
    add-zsh-hook precmd __berth_precmd
    add-zsh-hook preexec __berth_preexec
  else
    precmd_functions+=(__berth_precmd)
    preexec_functions+=(__berth_preexec)
  fi

elif [ -n "${BASH_VERSION:-}" ]; then

  __berth_precmd() {
    local exit_code=$?
    if [ -n "${__berth_running:-}" ]; then
      __berth_osc "D;${exit_code}"
      unset __berth_running
    fi
    __berth_osc "A"
    # Re-arm: the DEBUG trap must fire for the next command, not for the
    # prompt machinery that runs between commands.
    __berth_at_prompt=1
  }

  __berth_preexec() {
    # DEBUG fires for every simple command, including the ones inside
    # PROMPT_COMMAND, so only the first one after a prompt counts.
    if [ -n "${__berth_at_prompt:-}" ]; then
      unset __berth_at_prompt
      __berth_osc "C"
      __berth_running=1
    fi
  }

  if [ -z "${__berth_bash_installed:-}" ]; then
    PS1="${PS1}\[$(__berth_osc_str B)\]"
    case "${PROMPT_COMMAND:-}" in
      *__berth_precmd*) ;;
      "") PROMPT_COMMAND="__berth_precmd" ;;
      *) PROMPT_COMMAND="__berth_precmd; ${PROMPT_COMMAND}" ;;
    esac
    trap '__berth_preexec' DEBUG
    __berth_bash_installed=1
  fi

fi
