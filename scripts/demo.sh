#!/bin/sh
set -eu

if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ]; then
    printf 'run this demo as your normal user; it elevates only loop-device setup\n' >&2
    exit 1
fi

run_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
fixture="$project_dir/crates/defrag-service/tests/fixtures/ext4-fragmented.img.zst"
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
printf 'Preparing a disposable fragmented ext4 image...\n' >&2
zstd -q -d "$fixture" -o "$image"

printf 'Attaching the temporary image (sudo is used only for losetup)...\n' >&2
loop_device=$(run_root losetup --find --show "$image")
printf '\nDefragger demo device: %s\nSelect this device in the application.\n\n' "$loop_device" >&2

cd "$project_dir"
cargo run --release
