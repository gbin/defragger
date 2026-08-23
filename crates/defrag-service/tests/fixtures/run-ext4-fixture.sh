#!/bin/sh
set -eu

if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ]; then
    printf 'run this script without sudo; it elevates only the required operations\n' >&2
    exit 1
fi

run_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
work_dir=$(mktemp -d)
loop_device=
cleanup() {
    if [ -n "$loop_device" ]; then
        run_root losetup -d "$loop_device" 2>/dev/null || true
    fi
    rm -rf "$work_dir"
}
trap cleanup EXIT HUP INT TERM

image="$work_dir/ext4-fragmented.img"
printf '[1/5] Decompressing fixture to %s\n' "$image" >&2
zstd -q -d "$fixture_dir/ext4-fragmented.img.zst" -o "$image"

printf '[2/5] Building the integration test as %s\n' "$(id -un)" >&2
test_binary=$(
    cargo test --package defrag-service --lib --no-run --message-format=json |
        sed -n 's/.*"executable":"\([^"]*\/defrag_service-[^"]*\)".*/\1/p' |
        tail -n 1
)
if [ -z "$test_binary" ] || [ ! -x "$test_binary" ]; then
    printf 'could not locate the compiled defrag-service test binary\n' >&2
    exit 1
fi

printf '[3/5] Attaching the temporary image to a loop device (sudo)\n' >&2
loop_device=$(run_root losetup --find --show "$image")
printf '      loop device: %s\n' "$loop_device" >&2

printf '[4/5] Running analysis, defragmentation, and content verification (sudo)\n' >&2
run_root env DEFRAGGER_TEST_DEVICE="$loop_device" \
    "$test_binary" committed_fixture_defragments_without_changing_file_bytes \
    --ignored --nocapture

printf '[5/5] Checking ext4 consistency with e2fsck (sudo)\n' >&2
run_root e2fsck -fn "$loop_device"
printf 'PASS: fixture is fully defragmented, byte-identical, and filesystem-consistent\n' >&2
