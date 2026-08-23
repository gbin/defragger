#!/bin/sh
set -eu

fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM

image="$work_dir/ext4-fragmented.img"
output="$fixture_dir/ext4-fragmented.img.zst"

truncate -s 32M "$image"
E2FSPROGS_FAKE_TIME=1700000000 mkfs.ext4 -q -F -b 4096 \
    -U 11111111-2222-3333-4444-555555555555 \
    -E lazy_itable_init=0,lazy_journal_init=0 "$image"

{
    file=0
    while [ "$file" -lt 4 ]; do
        printf 'write /dev/null /target-%s.bin\n' "$file"
        file=$((file + 1))
    done

    block=3000
    while [ "$block" -lt 3128 ]; do
        printf 'setb %s\n' "$block"
        block=$((block + 1))
    done

    file=0
    while [ "$file" -lt 4 ]; do
        printf 'eo /target-%s.bin\n' "$file"
        logical=0
        while [ "$logical" -lt 32 ]; do
            physical=$((3000 + logical * 4 + file))
            pattern=$((file * 32 + logical + 1))
            printf 'zap -p %s %s\n' "$pattern" "$physical"
            printf 'insert --after %s 1 %s\n' "$logical" "$physical"
            logical=$((logical + 1))
        done
        printf 'ec\n'
        printf 'sif /target-%s.bin size 131072\n' "$file"
        file=$((file + 1))
    done
} | E2FSPROGS_FAKE_TIME=1700000000 debugfs -w "$image" >/dev/null 2>&1

set +e
E2FSPROGS_FAKE_TIME=1700000000 e2fsck -fy "$image" >/dev/null
fsck_status=$?
set -e
if [ "$fsck_status" -gt 1 ]; then
    exit "$fsck_status"
fi
e2fsck -fn "$image" >/dev/null
zstd -q -f -19 "$image" -o "$output"
printf 'generated %s\n' "$output"
