#!/bin/sh
set -eu

if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ]; then
    printf 'run this script without sudo; it elevates only loop-device operations\n' >&2
    exit 1
fi

run_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

work_dir=$(mktemp -d)
loop_device=
mount_point="$work_dir/mount"
mounted=false
cleanup() {
    if [ "$mounted" = true ]; then
        run_root umount "$mount_point" 2>/dev/null || true
    fi
    if [ -n "$loop_device" ]; then
        run_root losetup -d "$loop_device" 2>/dev/null || true
    fi
    rm -rf "$work_dir"
}
trap cleanup EXIT HUP INT TERM

mkdir "$mount_point"

printf '[1/3] Building FAT fixture I/O helper and integration test\n' >&2
cargo build --package defrag-service --example fat-fixture-io
test_binary=$(
    cargo test --package defrag-service --lib --no-run --message-format=json |
        sed -n 's/.*"executable":"\([^"]*\/defrag_service-[^"]*\)".*/\1/p' |
        tail -n 1
)
if [ -z "$test_binary" ] || [ ! -x "$test_binary" ]; then
    printf 'could not locate the compiled defrag-service test binary\n' >&2
    exit 1
fi

printf '[2/3] Exercising FAT16/FAT32 defrag and compact on loop devices\n' >&2
for variant in fat16 fat32; do
    for mode in defrag compact; do
        image="$work_dir/$variant-$mode.img"
        case "$variant" in
            fat16) image_size=32M; fat_bits=16 ;;
            fat32) image_size=64M; fat_bits=32 ;;
        esac
        truncate -s "$image_size" "$image"
        loop_device=$(run_root losetup --find --show "$image")
        run_root mkfs.fat -F "$fat_bits" "$loop_device" >/dev/null
        run_root mount -t vfat "$loop_device" "$mount_point"
        mounted=true
        run_root target/debug/examples/fat-fixture-io populate "$mount_point"
        run_root umount "$mount_point"
        mounted=false
        run_root fsck.fat -n "$loop_device" >/dev/null
        printf '      %s %s on %s\n' "$variant" "$mode" "$loop_device" >&2
        run_root env DEFRAGGER_TEST_DEVICE="$loop_device" DEFRAGGER_TEST_MODE="$mode" \
            "$test_binary" loop_device_fat_optimization_is_consistent --ignored --nocapture
        run_root fsck.fat -n "$loop_device"
        run_root mount -t vfat -o ro "$loop_device" "$mount_point"
        mounted=true
        run_root target/debug/examples/fat-fixture-io verify "$mount_point"
        run_root umount "$mount_point"
        mounted=false
        run_root losetup -d "$loop_device"
        loop_device=
    done
done

printf '[3/3] PASS: FAT16/FAT32 defrag and compact preserved data and passed fsck.fat\n' >&2
