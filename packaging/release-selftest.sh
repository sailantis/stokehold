#!/usr/bin/env bash
#
# Run the release packaging against a fake tree, so the part of `release.yml` that can be wrong
# without anyone noticing is exercised on every platform it will ever run on.
#
# The workflow's staging and smoke-test halves were inline `run:` blocks until it was pointed out
# that the only way to test an inline block is to push a tag. This runs them for real: a real
# binary, a real archive, a real extraction, and the checksum compared by the same command the
# downloader will use.
#
# Usage: packaging/release-selftest.sh [path-to-volucer-binary]
#
# The binary defaults to the release build for this host. Nothing here needs the network, a tag,
# or a Docker login.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "${here}/.." && pwd)"
cd "$repo"

case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) os=windows ;;
    Darwin*) os=macos ;;
    *) os=linux ;;
esac

# One target and one archive format per host: the point is that the *script* runs and produces a
# correct archive, not to re-test the matrix that the workflow already fans out.
#
# `SELFTEST_ARCHIVE` and `SELFTEST_TARGET` override the host's choice, so the tarball path — which
# is what Linux and macOS actually release — is checkable from a Windows machine too, and the zip
# path from a Unix one. Both branches of the packing code are otherwise only exercised on the
# platform that ships them.
case "$os" in
    windows)
        target=${SELFTEST_TARGET:-x86_64-pc-windows-msvc}
        archive=${SELFTEST_ARCHIVE:-zip}
        exe=.exe
        ;;
    macos)
        target=${SELFTEST_TARGET:-aarch64-apple-darwin}
        archive=${SELFTEST_ARCHIVE:-tar.gz}
        exe=
        ;;
    *)
        target=${SELFTEST_TARGET:-x86_64-unknown-linux-musl}
        archive=${SELFTEST_ARCHIVE:-tar.gz}
        exe=
        ;;
esac

default_binary="target/release/volucer${exe}"
binary="${1:-$default_binary}"
if [ ! -f "$binary" ]; then
    echo "no binary at ${binary}; run 'cargo build --release -p volucer' first, or pass a path" >&2
    exit 1
fi

out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT

echo "== staging ${target} (${archive})"
TARGET="$target" \
    ARCHIVE="$archive" \
    EXE_SUFFIX="$exe" \
    GITHUB_REF_NAME="v0.1.0" \
    BIN="$binary" \
    OUT_DIR="$out" \
    bash packaging/stage-archive.sh

expected="volucer-0.1.0-${target}.${archive}"
if [ ! -f "${out}/${expected}" ]; then
    echo "expected ${out}/${expected} to exist" >&2
    exit 1
fi
if [ ! -f "${out}/${expected}.sha256" ]; then
    echo "expected a checksum beside ${expected}" >&2
    exit 1
fi

# The archive has to hold exactly what an operator is promised, and the binary has to be in it —
# under one top-level directory, so extracting it in a directory of other things does not scatter
# the files. The zip and the tarballs have to agree about that, which is why the prefix is stated.
echo "== contents"
listing="$(tar -tzf "${out}/${expected}" 2>/dev/null || unzip -Z1 "${out}/${expected}")"
echo "$listing"
name="volucer-0.1.0-${target}"
for want in "volucer${exe}" README.md LICENSE packaging/systemd/volucer.service; do
    if ! grep -q "^${name}/${want}$" <<<"$listing"; then
        echo "the archive does not contain ${name}/${want}" >&2
        exit 1
    fi
done

# ...and the smoke test has to accept it, which is the half that actually runs the binary.
echo "== smoke-testing"
(
    cd "$out"
    ARCHIVE="$archive" bash "${here}/smoke-test-archive.sh"
)

# The negative case, because a smoke test that passes on anything proves nothing. A corrupted
# archive must be *refused* by the checksum rather than run.
echo "== a corrupted archive is refused"
cp "${out}/${expected}" "${out}/corrupt.${archive}"
printf 'x' >>"${out}/corrupt.${archive}"
if (cd "$out" && ARCHIVE="$archive" GITHUB_REF_NAME="v0.1.0" OUT_DIR="$out" TARGET="$target" \
    EXE_SUFFIX="$exe" BIN="$binary" bash -c "
        set -euo pipefail
        mv '${expected}' '${expected}.keep'
        mv 'corrupt.${archive}' '${expected}'
        if bash '${here}/smoke-test-archive.sh'; then
            echo 'the smoke test accepted a corrupted archive'
            exit 1
        fi
        mv '${expected}.keep' '${expected}'
    "); then
    echo "a corrupted archive was refused, as it should be"
else
    echo "the corrupted-archive check failed" >&2
    exit 1
fi

echo "== ok: ${expected} stages, verifies and runs"
