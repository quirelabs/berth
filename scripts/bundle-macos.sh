#!/bin/sh
# Assemble Berth.app around a built binary.
#
# A bare binary on macOS cannot be brought to the front, does not appear in the
# Dock or in Spotlight, and cannot be signed or notarised. None of that is a
# rendering problem and all of it looks like one: it is why the pixel check
# could not be judged from a development machine before this existed.
#
# Usage: scripts/bundle-macos.sh [debug|release]
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

# An ad-hoc signature, which is not a distribution signature: it is what lets
# the bundle keep a stable identity across rebuilds, so macOS does not treat
# every build as a new application and re-ask for every permission. Real
# signing and notarisation happen at release.
codesign --force --sign - --timestamp=none "$app" >/dev/null 2>&1 || \
	echo "note: ad-hoc signing failed, the bundle still runs" >&2

echo "$app"
