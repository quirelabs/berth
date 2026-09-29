# Berth's zsh startup wrapper (D48). Berth points ZDOTDIR here so it can put its
# claude shim first in PATH after your own startup files have run. This loads
# your real ~/.zlogin unchanged, with ZDOTDIR set back to yours while it runs.
if [[ -n "$BERTH_USER_ZDOTDIR" && -f "$BERTH_USER_ZDOTDIR/.zlogin" ]]; then
	__berth_zdotdir=$ZDOTDIR
	ZDOTDIR=$BERTH_USER_ZDOTDIR
	source "$BERTH_USER_ZDOTDIR/.zlogin"
	# A .zshenv that moves ZDOTDIR is followed for the files after it.
	BERTH_USER_ZDOTDIR=$ZDOTDIR
	ZDOTDIR=$__berth_zdotdir
	unset __berth_zdotdir
fi

# Login shells end here, after every startup file, including the macOS
# path_helper in /etc/zprofile that would otherwise have moved the shim behind
# the system paths.
[[ -n "$BERTH_SHIM_DIR" ]] && path=("$BERTH_SHIM_DIR" ${path:#$BERTH_SHIM_DIR})
if [[ -n "$BERTH_ZDOTDIR_WAS_SET" ]]; then ZDOTDIR=$BERTH_USER_ZDOTDIR; else unset ZDOTDIR; fi
unset BERTH_USER_ZDOTDIR BERTH_ZDOTDIR_WAS_SET

# A restored tab resumes its Claude conversation here, after every startup file
# and before the first prompt, so nothing is typed or echoed (D48). Berth only
# ever sets a strict UUID; it is checked again here and passed as one quoted
# argument, never read as a command.
if [[ -n "$BERTH_RESUME" ]]; then
	__berth_resume=$BERTH_RESUME
	unset BERTH_RESUME
	if [[ "$__berth_resume" =~ '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$' ]]; then
		claude --resume "$__berth_resume"
	fi
	unset __berth_resume
fi
