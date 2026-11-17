#!/bin/sh
set -eu

fail() { printf '%s\n' "herdr-wt install: $*" >&2; exit 1; }

tag=$(sed -n '2p' release-pin)
[ "$tag" != unpublished ] || fail "no prebuilt release published yet"
printf '%s\n' "$tag" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || fail "invalid release pin"
repo=$(sed -n '3p' release-pin)
printf '%s\n' "$repo" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9_.-]+$' || fail "invalid release repository"

case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) target=linux-x86_64 ;;
    Linux/aarch64|Linux/arm64) target=linux-aarch64 ;;
    Darwin/arm64) target=macos-aarch64 ;;
    Darwin/x86_64)
        if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || :)" = 1 ]; then
            target=macos-aarch64
        else
            target=macos-x86_64
        fi ;;
    *) fail "unsupported OS/architecture: $(uname -s)/$(uname -m)" ;;
esac

asset="herdr-wt-$target.tar.gz"
expected=$(awk -v asset="$asset" '$2 == asset { print $1 }' release-pin)
[ "${#expected}" -eq 64 ] && printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$' || fail "missing or invalid SHA-256 for $asset"
for tool in curl tar; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
done

scratch=$(mktemp -d "./.herdr-wt-install.XXXXXXXX")
trap 'rm -rf "$scratch"' EXIT
if [ -n "${HERDR_WT_TEST_ASSET_DIR:-}" ]; then
    cp "$HERDR_WT_TEST_ASSET_DIR/$asset" "$scratch/package.tar.gz" || fail "cannot read $asset"
else
    curl -fLsS --retry 3 --proto '=https' --proto-redir '=https' \
        -o "$scratch/package.tar.gz" "https://github.com/$repo/releases/download/$tag/$asset" || fail "cannot download $tag/$asset"
fi

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$scratch/package.tar.gz" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$scratch/package.tar.gz" | awk '{print $1}')
else
    fail "sha256sum or shasum is required"
fi
[ "$actual" = "$expected" ] || fail "checksum mismatch for $asset"

tar -xOzf "$scratch/package.tar.gz" bin/herdr-wt > "$scratch/herdr-wt" || fail "archive has no usable binary"
chmod 755 "$scratch/herdr-wt"
mkdir -p bin
mv -f "$scratch/herdr-wt" bin/herdr-wt
