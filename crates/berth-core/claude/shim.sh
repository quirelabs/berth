#!/bin/sh
# Berth's `claude` shim (D48). Berth puts its directory first in PATH in its
# own panes, so starting Claude there loads Berth's hooks with --plugin-dir and
# nothing is ever installed into Claude's config. It finds the real `claude`
# further down PATH and runs it; subcommands such as `plugin`, `mcp` or
# `update` pass straight through untouched.

self_dir=$(cd "$(dirname "$0")" 2>/dev/null && pwd -P)
plugin_dir=$(cd "$self_dir/../claude-plugin" 2>/dev/null && pwd -P)

# Every shim directory already passed through, this one included. All of them
# are skipped, so two shims in PATH (two Berths, say) can never hand off to
# each other forever.
first_shim=${BERTH_CLAUDE_SHIM:+no}
BERTH_CLAUDE_SHIM="${BERTH_CLAUDE_SHIM:+$BERTH_CLAUDE_SHIM:}$self_dir"
export BERTH_CLAUDE_SHIM

real=""
old_ifs=$IFS
IFS=:
for dir in $PATH; do
	[ -n "$dir" ] || continue
	resolved=$(cd "$dir" 2>/dev/null && pwd -P) || continue
	skip=""
	for seen in $BERTH_CLAUDE_SHIM; do
		[ "$resolved" = "$seen" ] && skip=yes
	done
	[ -n "$skip" ] && continue
	if [ -f "$dir/claude" ] && [ -x "$dir/claude" ]; then
		real="$dir/claude"
		break
	fi
done
IFS=$old_ifs

if [ -z "$real" ]; then
	echo "claude: command not found" >&2
	exit 127
fi

# Only the first shim adds the plugin. Twice would load the hooks twice.
if [ -n "$first_shim" ] || [ -z "$plugin_dir" ]; then
	exec "$real" "$@"
fi

case "$1" in
agents | attach | auth | auto-mode | doctor | gateway | import | install | logs | mcp | \
	plugin | plugins | project | respawn | rm | setup-token | stop | kill | \
	ultrareview | update | upgrade | config | migrate-installer | help | --help | -h | \
	--version | -v)
	exec "$real" "$@"
	;;
esac

exec "$real" --plugin-dir "$plugin_dir" "$@"
