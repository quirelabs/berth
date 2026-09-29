# Berth's zsh startup wrapper (D48). Berth points ZDOTDIR here so it can put its
# claude shim first in PATH after your own startup files have run. This loads
# your real ~/.zshrc unchanged, with ZDOTDIR set back to yours while it runs.
if [[ -n "$BERTH_USER_ZDOTDIR" && -f "$BERTH_USER_ZDOTDIR/.zshrc" ]]; then
	__berth_zdotdir=$ZDOTDIR
	ZDOTDIR=$BERTH_USER_ZDOTDIR
	source "$BERTH_USER_ZDOTDIR/.zshrc"
	# A .zshenv that moves ZDOTDIR is followed for the files after it.
	BERTH_USER_ZDOTDIR=$ZDOTDIR
	ZDOTDIR=$__berth_zdotdir
	unset __berth_zdotdir
fi

# Interactive shells that are not login shells end here: shim first, and
# ZDOTDIR back to what it was, so a zsh started from this one is ordinary.
if [[ ! -o login ]]; then
	[[ -n "$BERTH_SHIM_DIR" ]] && path=("$BERTH_SHIM_DIR" ${path:#$BERTH_SHIM_DIR})
	if [[ -n "$BERTH_ZDOTDIR_WAS_SET" ]]; then ZDOTDIR=$BERTH_USER_ZDOTDIR; else unset ZDOTDIR; fi
	unset BERTH_USER_ZDOTDIR BERTH_ZDOTDIR_WAS_SET
fi
