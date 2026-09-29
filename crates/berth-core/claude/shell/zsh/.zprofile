# Berth's zsh startup wrapper (D48). Berth points ZDOTDIR here so it can put its
# claude shim first in PATH after your own startup files have run. This loads
# your real ~/.zprofile unchanged, with ZDOTDIR set back to yours while it runs.
if [[ -n "$BERTH_USER_ZDOTDIR" && -f "$BERTH_USER_ZDOTDIR/.zprofile" ]]; then
	__berth_zdotdir=$ZDOTDIR
	ZDOTDIR=$BERTH_USER_ZDOTDIR
	source "$BERTH_USER_ZDOTDIR/.zprofile"
	# A .zshenv that moves ZDOTDIR is followed for the files after it.
	BERTH_USER_ZDOTDIR=$ZDOTDIR
	ZDOTDIR=$__berth_zdotdir
	unset __berth_zdotdir
fi
