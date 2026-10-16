#!/bin/sh
# Assemble Berth.app around a built binary.
#
# A bare binary on macOS cannot be brought to the front, does not appear in the
# Dock or in Spotlight, and cannot be signed or notarised. None of that is a
# rendering problem and all of it looks like one: it is why the pixel check
# could not be judged from a development machine before this existed.
#
# Usage: scripts/bundle-macos.sh [debug|release] [servers-dir]
#
# servers-dir holds the four berth-server builds as CI names them
# (berth-server-<target>, each with its .sha256). Given it, the bundle carries
# them gzip-compressed and their hashes are compiled into the app, so it can
# install Berth's server on remote hosts (D55). Without it the app carries
# none and every host stays on tmux: fine for development, never a release.
set -eu

profile="${1:-release}"
case "$profile" in
	debug) flags="" ;;
	release) flags="--release" ;;
	*) echo "usage: $0 [debug|release]" >&2; exit 2 ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

# The version the workspace declares, so the bundle cannot claim a different
# one from the crate.
version=$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)
if [ -z "$version" ]; then
	echo "could not read the version from Cargo.toml" >&2
	exit 1
fi

servers="${2:-}"
staged=""
if [ -n "$servers" ]; then
	staged=$(mktemp -d)
	trap 'rm -rf "$staged"' EXIT
	manifest=""
	for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
		aarch64-apple-darwin x86_64-apple-darwin; do
		name="berth-server-$target"
		if [ ! -f "$servers/$name" ]; then
			echo "no $name in $servers: a bundle carries all four or none" >&2
			exit 1
		fi
		sum=$(shasum -a 256 "$servers/$name" | cut -d' ' -f1)
		# CI's checksum, when it came along: the file is the one CI built.
		if [ -f "$servers/$name.sha256" ]; then
			want=$(cut -d' ' -f1 "$servers/$name.sha256")
			if [ "$sum" != "$want" ]; then
				echo "$name does not match its .sha256" >&2
				exit 1
			fi
		fi
		# -n: no name or time in the file, so the same binary packs the same.
		gzip -9 -n -c "$servers/$name" > "$staged/$name.gz"
		size=$(wc -c < "$servers/$name" | tr -d ' ')
		gz_size=$(wc -c < "$staged/$name.gz" | tr -d ' ')
		gz_sum=$(shasum -a 256 "$staged/$name.gz" | cut -d' ' -f1)
		manifest="$manifest$target $size $sum $gz_size $gz_sum
"
	done
	# Compiled in: what the app will accept beside itself, and nothing else.
	BERTH_SERVER_MANIFEST="$manifest"
	export BERTH_SERVER_MANIFEST
else
	echo "note: no servers given, this bundle installs none on hosts" >&2
fi

# shellcheck disable=SC2086
cargo build -p berth-app $flags

binary="target/$profile/berth"
if [ ! -x "$binary" ]; then
	echo "no binary at $binary" >&2
	exit 1
fi

app="target/$profile/Berth.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

sed "s/@VERSION@/$version/g" crates/berth-app/macos/Info.plist > "$app/Contents/Info.plist"
cp "$binary" "$app/Contents/MacOS/berth"

# The icon is optional so this works before there is any art. macOS shows the
# generic application icon when the file named by CFBundleIconFile is absent.
if [ -f crates/berth-app/macos/berth.icns ]; then
	cp crates/berth-app/macos/berth.icns "$app/Contents/Resources/berth.icns"
fi

if [ -n "$staged" ]; then
	mkdir -p "$app/Contents/Resources/servers"
	cp "$staged"/*.gz "$app/Contents/Resources/servers/"
fi

# An ad-hoc signature, which is not a distribution signature: it is what lets
# the bundle keep a stable identity across rebuilds, so macOS does not treat
# every build as a new application and re-ask for every permission. Real
# signing and notarisation happen at release.
codesign --force --sign - --timestamp=none "$app" >/dev/null 2>&1 || \
	echo "note: ad-hoc signing failed, the bundle still runs" >&2

echo "$app"
