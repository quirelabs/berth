# Berth's bash startup wrapper (D48). Berth starts bash with --init-file
# pointing here instead of as a login shell, so this does what a login shell
# would, with your own files unchanged, and then puts the claude shim first in
# PATH.
[ -r /etc/profile ] && . /etc/profile
for __berth_profile in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
	if [ -r "$__berth_profile" ]; then
		. "$__berth_profile"
		break
	fi
done
unset __berth_profile
if [ -n "$BERTH_SHIM_DIR" ]; then
	case ":$PATH:" in
	*":$BERTH_SHIM_DIR:"*) PATH=$(printf '%s' ":$PATH:" | sed "s#:$BERTH_SHIM_DIR:#:#g; s#^:##; s#:\$##") ;;
	esac
	PATH="$BERTH_SHIM_DIR:$PATH"
	export PATH
fi
