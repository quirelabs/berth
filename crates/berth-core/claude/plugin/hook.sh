#!/bin/sh
# Berth's Claude Code hook (D48). Tells Berth what Claude is doing in a Berth
# pane by writing one escape sequence to that pane's terminal:
#
#   ESC ] 7717 ; berth ; 1 ; <token> ; <event> ; <session-id> ; <kind> ; <cwd> BEL
#
# Outside Berth, BERTH_PANE is unset and this exits at once. It never sends
# the prompt, the reply, or any tool input or output, and it always exits 0 so
# it can never block Claude.

[ -n "$BERTH_PANE" ] || exit 0
payload=$(cat) || exit 0

field() {
	printf '%s' "$payload" | sed -n "s/.*\"$1\" *: *\"\\([A-Za-z0-9_-]*\\)\".*/\\1/p" | head -n 1
}

case "$(field hook_event_name)" in
SessionStart) event=start ;;
UserPromptSubmit) event=prompt ;;
PostToolUse) event=tool ;;
Notification) event=notify ;;
Stop) event=stop ;;
SessionEnd) event=end ;;
*) exit 0 ;;
esac

case "$(field notification_type)" in
permission_prompt) kind=permission ;;
idle_prompt) kind=idle ;;
*) kind=- ;;
esac

session=$(field session_id)
[ -n "$session" ] || exit 0

# The directory resume is scoped to, encoded so no character in a path can
# break the sequence apart.
dir=${CLAUDE_PROJECT_DIR:-$PWD}
cwd=$(printf '%s' "$dir" | base64 | tr -d '\n=' | tr '+/' '-_')

body="7717;berth;1;$BERTH_PANE;$event;$session;$kind;$cwd"

# Claude runs hooks with no terminal of their own, so the pane's device is
# named explicitly: by tmux inside tmux, by Berth outside it.
if [ -n "$TMUX_PANE" ]; then
	tty=$(tmux display-message -p -t "$TMUX_PANE" '#{pane_tty}' 2>/dev/null)
	[ -c "$tty" ] && [ -w "$tty" ] || exit 0
	# One layer of tmux passthrough: ESC doubled inside the wrapper.
	printf '\033Ptmux;\033\033]%s\007\033\\' "$body" >"$tty" 2>/dev/null
else
	tty=$BERTH_TTY
	[ -n "$tty" ] && [ -c "$tty" ] && [ -w "$tty" ] || exit 0
	printf '\033]%s\007' "$body" >"$tty" 2>/dev/null
fi
exit 0
