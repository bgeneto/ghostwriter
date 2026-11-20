#!/usr/bin/env bash
# Builds the GTK3 target used by the Linux E2E test.
#
# Kept as a script (rather than a build.rs) because it must not run during a
# normal `cargo build`: it needs gtk+-3.0 development headers, which a user
# building the app does not.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
src="$here/gtk_target.c"
out="${1:-$here/gtk_target}"

if ! pkg-config --exists gtk+-3.0; then
    echo "error: gtk+-3.0 development files not found." >&2
    echo "       Debian/Ubuntu: sudo apt install libgtk-3-dev" >&2
    exit 1
fi

# shellcheck disable=SC2046
cc -O1 -Wall -Wextra -o "$out" "$src" $(pkg-config --cflags --libs gtk+-3.0)
echo "built $out"
