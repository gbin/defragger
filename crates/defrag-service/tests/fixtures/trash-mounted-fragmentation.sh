#!/usr/bin/env bash
set -Eeuo pipefail

readonly fixture_name=.defragger-fragmentation-fixture
readonly mib=$((1024 * 1024))
readonly max_fat_payload_bytes=$((3 * 1024 * 1024 * 1024))

usage() {
    cat >&2 <<EOF
usage: $0 MOUNT_POINT

Creates a deliberately fragmented fixture on a disposable mounted ext4, FAT,
or exFAT filesystem. Existing files are not modified or removed.
EOF
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

if [[ $# -ne 1 ]]; then
    usage
    exit 2
fi

input_mount=$1

for command in realpath mountpoint findmnt df stat dd sync; do
    command -v "$command" >/dev/null || die "required command is missing: $command"
done

mount_path=$(realpath -e -- "$input_mount") || die "mount point does not exist: $input_mount"
[[ $mount_path != / ]] || die 'refusing to operate on the root filesystem'
[[ -d $mount_path ]] || die "not a directory: $mount_path"
mountpoint -q -- "$mount_path" || die "path is not an exact mount point: $mount_path"

mount_source=$(findmnt --raw --noheadings --mountpoint "$mount_path" --output SOURCE) ||
    die "could not resolve the mount source for $mount_path"
filesystem=$(findmnt --raw --noheadings --mountpoint "$mount_path" --output FSTYPE) ||
    die "could not resolve the filesystem type for $mount_path"
mount_options=$(findmnt --raw --noheadings --mountpoint "$mount_path" --output OPTIONS) ||
    die "could not resolve mount options for $mount_path"

case "$filesystem" in
    ext4 | vfat | exfat) ;;
    *) die "unsupported test filesystem '$filesystem' (expected ext4, vfat, or exfat)" ;;
esac
case ",$mount_options," in
    *,ro,*) die "filesystem is mounted read-only: $mount_path" ;;
esac

fill_percent=${DEFRAGGER_FRAGMENT_FILL_PERCENT:-96}
requested_slots=${DEFRAGGER_FRAGMENT_SLOT_COUNT:-2048}
[[ $fill_percent =~ ^[0-9]+$ ]] || die 'DEFRAGGER_FRAGMENT_FILL_PERCENT must be an integer'
[[ $requested_slots =~ ^[0-9]+$ ]] || die 'DEFRAGGER_FRAGMENT_SLOT_COUNT must be an integer'
((fill_percent >= 80 && fill_percent <= 98)) ||
    die 'DEFRAGGER_FRAGMENT_FILL_PERCENT must be between 80 and 98'
((requested_slots >= 128 && requested_slots <= 16384 && requested_slots % 2 == 0)) ||
    die 'DEFRAGGER_FRAGMENT_SLOT_COUNT must be an even integer from 128 through 16384'

fixture_path=$mount_path/$fixture_name
[[ ! -e $fixture_path && ! -L $fixture_path ]] ||
    die "fixture path already exists; refusing to overwrite it: $fixture_path"
[[ -w $mount_path ]] ||
    die "mount point is not writable by $(id -un); rerun this recipe with sufficient privileges"

available_bytes=$(df --block-size=1 --output=avail -- "$mount_path" | tail -n 1)
available_bytes=${available_bytes//[[:space:]]/}
[[ $available_bytes =~ ^[0-9]+$ ]] || die 'could not determine available filesystem space'
((available_bytes >= 16 * mib)) || die 'at least 16 MiB of available space is required'

block_size=$(stat --file-system --format='%S' -- "$mount_path")
[[ $block_size =~ ^[0-9]+$ ]] || die 'could not determine filesystem block size'
((block_size > 0)) || die 'filesystem reported a zero block size'

target_bytes=$((available_bytes * fill_percent / 100))
# Slots occupy 80% of the high-water mark. Half become holes and are replaced
# by round-robin payload writes. The anchor occupies the other 20%, preventing
# payload allocation in one large free run until it is removed at the end.
slot_region_bytes=$((target_bytes * 80 / 100))
minimum_slot_bytes=$((block_size * 4))
maximum_slots=$((slot_region_bytes / minimum_slot_bytes))
slot_count=$requested_slots
((slot_count <= maximum_slots)) || slot_count=$maximum_slots
((slot_count % 2 == 0)) || slot_count=$((slot_count - 1))
((slot_count >= 128)) || die 'filesystem is too small for a useful fragmentation pattern'

slot_bytes=$((slot_region_bytes / slot_count / block_size * block_size))
((slot_bytes >= minimum_slot_bytes)) || die 'calculated allocation slots are too small'
slots_bytes=$((slot_bytes * slot_count))
anchor_bytes=$((target_bytes - slots_bytes))
hole_count=$((slot_count / 2))
payload_total_bytes=$((hole_count * slot_bytes))
payload_count=32
required_payloads=$(((payload_total_bytes + max_fat_payload_bytes - 1) / max_fat_payload_bytes))
((payload_count >= required_payloads)) || payload_count=$required_payloads
maximum_payloads=$((hole_count / 2))
((payload_count <= maximum_payloads)) || payload_count=$maximum_payloads
((payload_count >= 1)) || die 'not enough allocation holes for payload files'

human_size() {
    local bytes=$1
    if ((bytes >= mib)); then
        printf '%d MiB' "$(((bytes + mib - 1) / mib))"
    else
        printf '%d KiB' "$(((bytes + 1023) / 1024))"
    fi
}

cat >&2 <<EOF
Creating fragmentation fixture on $mount_path ($filesystem, $mount_source)
Using up to ${fill_percent}% of its currently free space ($(human_size "$target_bytes"));
existing files will not be modified or removed.

EOF

mkdir -- "$fixture_path"
printf '%s\n' 'Created by trash-mounted-fragmentation.sh; safe to remove as one directory.' \
    >"$fixture_path/README.txt"

failed=false
report_failure() {
    local status=$?
    if ((status != 0)) && [[ $failed == false ]]; then
        failed=true
        printf '\nFAILED: a partial fixture remains at %s\n' "$fixture_path" >&2
        printf 'Remove that directory to reclaim its space.\n' >&2
    fi
    exit "$status"
}
trap report_failure ERR

allocate_file() {
    local path=$1
    local bytes=$2
    if command -v fallocate >/dev/null && fallocate --length "$bytes" -- "$path" 2>/dev/null; then
        return
    fi
    dd if=/dev/zero of="$path" bs=1M iflag=count_bytes count="$bytes" \
        conv=fsync status=none
}

append_initialized() {
    local path=$1
    local bytes=$2
    dd if=/dev/zero of="$path" bs=1M iflag=count_bytes count="$bytes" \
        oflag=append conv=notrunc,fsync status=none
}

printf '[1/5] Reserving the temporary allocation anchor (%s)\n' \
    "$(human_size "$anchor_bytes")" >&2
allocate_file "$fixture_path/allocation-anchor.bin" "$anchor_bytes"

printf '[2/5] Creating %d interleaved allocation slots (%s each)\n' \
    "$slot_count" "$(human_size "$slot_bytes")" >&2
for ((index = 0; index < slot_count; index++)); do
    printf -v slot_path '%s/slot-%06d.bin' "$fixture_path" "$index"
    allocate_file "$slot_path" "$slot_bytes"
    if (((index + 1) % 128 == 0 || index + 1 == slot_count)); then
        printf '      %d / %d slots\r' "$((index + 1))" "$slot_count" >&2
    fi
done
printf '\n' >&2
sync --file-system "$fixture_path"

printf '[3/5] Removing every other slot to make %d physical holes\n' "$hole_count" >&2
for ((index = 1; index < slot_count; index += 2)); do
    printf -v slot_path '%s/slot-%06d.bin' "$fixture_path" "$index"
    rm -- "$slot_path"
done
sync --file-system "$fixture_path"

printf '[4/5] Cycling initialized payload writes through the holes\n' >&2
for ((hole = 0; hole < hole_count; hole++)); do
    payload=$((hole % payload_count))
    printf -v payload_path '%s/fragmented-payload-%04d.bin' "$fixture_path" "$payload"
    append_initialized "$payload_path" "$slot_bytes"
    if (((hole + 1) % 32 == 0 || hole + 1 == hole_count)); then
        printf '      %d / %d payload chunks\r' "$((hole + 1))" "$hole_count" >&2
    fi
done
printf '\n' >&2
sync --file-system "$fixture_path"

printf '[5/5] Removing the anchor to leave contiguous defrag workspace\n' >&2
rm -- "$fixture_path/allocation-anchor.bin"
sync --file-system "$fixture_path"
trap - ERR

final_available=$(df --block-size=1 --output=avail -- "$mount_path" | tail -n 1)
final_available=${final_available//[[:space:]]/}
printf '\nDONE: deliberately fragmented fixture created at %s\n' "$fixture_path" >&2
printf 'Payload data: %s across %d files; free space now: %s\n' \
    "$(human_size "$payload_total_bytes")" "$payload_count" \
    "$(human_size "$final_available")" >&2
printf 'Analyze it with: just analyze %q\n' "$mount_path" >&2
printf 'Reclaim it with: rm -rf -- %q\n' "$fixture_path" >&2
