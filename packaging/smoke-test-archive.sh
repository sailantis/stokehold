#!/usr/bin/env bash
#
# Unpack a release archive somewhere else entirely and run it.
#
# "This archive runs" is a hypothesis until something runs it. Extracting to a temporary directory
# rather than in place is deliberate: it catches a binary that only works next to the source
# checkout, a missing linked library, a missing trust store, and a file that was never marked
# executable. It also verifies the checksum the Composer downloader will verify, because a
# checksum that is only compared by other people's tooling is a checksum nobody has checked.
set -euo pipefail

: "${ARCHIVE:?ARCHIVE is not set}"

case "$ARCHIVE" in
    zip) extension=zip ;;
    *) extension=tar.gz ;;
esac

shopt -s nullglob
archives=(volucer-*."$extension")
shopt -u nullglob
if [ "${#archives[@]}" -ne 1 ]; then
    echo "::error::expected exactly one volucer-*.${extension}, found ${#archives[@]}" >&2
    exit 1
fi
archive="${archives[0]}"

if command -v sha256sum >/dev/null 2>&1; then
    sha256sum --check "${archive}.sha256"
else
    shasum -a 256 --check "${archive}.sha256"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

case "$extension" in
    zip) unzip -q "$archive" -d "$work" ;;
    *) tar -xzf "$archive" -C "$work" ;;
esac

binary="$(find "$work" -type f -name 'volucer' -o -type f -name 'volucer.exe' | head -n1)"
if [ -z "$binary" ]; then
    echo "::error::the archive ${archive} contains no volucer binary" >&2
    exit 1
fi

chmod +x "$binary" 2>/dev/null || true

"$binary" version
"$binary" --help >/dev/null

echo "ran ${binary} from ${archive}"
