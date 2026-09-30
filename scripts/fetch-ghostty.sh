#!/bin/sh
# Fetch Ghostty's source at the commit the bindings are pinned to and check it against its
# SHA-256, then print the directory to hand the build as GHOSTTY_SOURCE_DIR.
# The build never clones anything itself (D49). From quirelabs' public mirror of
# upstream, so the pinned commit stays fetchable whatever happens upstream.
#
# Usage: scripts/fetch-ghostty.sh, once. .cargo/config.toml points the build at
# the directory it prints.
set -eu

commit=22d13172cde98a0a4dda05d3d6a3fcb0dd8ed018
sha256=40e794871d9b4482421662c4a0f0e6f42513b4c543f88e536a6829db7cafddde

here=$(cd "$(dirname "$0")/.." && pwd)
dest="$here/.ghostty/ghostty-$commit"
if [ -f "$dest/build.zig" ]; then
	echo "$dest"
	exit 0
fi

mkdir -p "$here/.ghostty"
archive="$here/.ghostty/ghostty-$commit.tar.gz"
curl -sSfL -o "$archive" "https://codeload.github.com/quirelabs/ghostty-mirror/tar.gz/$commit"

if command -v sha256sum >/dev/null 2>&1; then
	actual=$(sha256sum "$archive" | cut -d' ' -f1)
else
	actual=$(shasum -a 256 "$archive" | cut -d' ' -f1)
fi
if [ "$actual" != "$sha256" ]; then
	# Loud rather than silent: a changed archive is never built.
	echo "ghostty source checksum mismatch: expected $sha256, got $actual" >&2
	rm -f "$archive"
	exit 1
fi

# The archive's top directory is named after the repository; drop it.
mkdir -p "$dest"
tar -xzf "$archive" -C "$dest" --strip-components=1
rm -f "$archive"
echo "$dest"
