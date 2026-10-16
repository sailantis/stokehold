#!/usr/bin/env bash
#
# Stage and pack one target's release archive, and write the checksum beside it.
#
# A script rather than an inline `run:` block for one reason: this is the half of the release that
# can be tested without publishing anything, and `packaging/release-selftest.sh` runs it against a
# fake tree. An inline block in a workflow is only testable by pushing a tag.
#
# Inputs, all from the environment:
#   TARGET       the rust target triple, e.g. x86_64-unknown-linux-musl
#   ARCHIVE      `tar.gz` or `zip`
#   EXE_SUFFIX   `.exe` on Windows, empty elsewhere
#   GITHUB_REF_NAME  the tag, e.g. `v0.1.0`
#   BIN          (optional, self-test only) the binary to package, instead of the build output
#   OUT_DIR      (optional, self-test only) where the archive goes, instead of the repo root
set -euo pipefail

: "${TARGET:?TARGET is not set}"
: "${ARCHIVE:?ARCHIVE is not set}"
: "${EXE_SUFFIX:=}"
: "${GITHUB_REF_NAME:?GITHUB_REF_NAME is not set}"
: "${OUT_DIR:=.}"

version="${GITHUB_REF_NAME#v}"
name="volucer-${version}-${TARGET}"
binary="${BIN:-target/${TARGET}/release/volucer${EXE_SUFFIX}}"

if [ ! -f "$binary" ]; then
    echo "::error::no binary at ${binary} — was the build for ${TARGET} actually run?" >&2
    exit 1
fi

# Staged in a temporary directory, not in the repo root: a half-built archive left beside the
# source is the kind of thing a later `git add -A` picks up.
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
mkdir -p "${staging}/${name}/packaging/systemd"

cp "$binary" "${staging}/${name}/"
cp README.md LICENSE "${staging}/${name}/"
cp packaging/systemd/volucer.service "${staging}/${name}/packaging/systemd/"

archive="${OUT_DIR}/${name}.${ARCHIVE}"
if [ "$ARCHIVE" = "zip" ]; then
    # `7z` is on the Windows image but is not reachable as `7z` under Git Bash, so it is resolved
    # rather than assumed.
    if command -v 7z >/dev/null 2>&1; then
        sevenzip=7z
    elif [ -x "/c/Program Files/7-Zip/7z.exe" ]; then
        sevenzip="/c/Program Files/7-Zip/7z.exe"
    else
        echo "::error::7-Zip is not available to build ${name}.zip" >&2
        exit 1
    fi
    # Archiving `$name` rather than its contents, so the zip has the same single top-level
    # directory the tarballs have. Zipping `.` from inside would scatter the files into whatever
    # directory an operator happens to be standing in.
    (cd "$staging" && "$sevenzip" a -tzip "${archive}" "$name" >/dev/null)
else
    tar -zcf "$archive" -C "$staging" "$name"
fi

# The file the Composer downloader verifies against, written in the same step as the archive so
# the two cannot come to describe different bytes.
(
    cd "$OUT_DIR"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "${name}.${ARCHIVE}" >"${name}.${ARCHIVE}.sha256"
    else
        shasum -a 256 "${name}.${ARCHIVE}" >"${name}.${ARCHIVE}.sha256"
    fi
)

echo "ARCHIVE_NAME=${name}.${ARCHIVE}" >>"${GITHUB_ENV:-/dev/null}"
echo "staged ${name}.${ARCHIVE}"
