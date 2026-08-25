# ext4 defragmentation fixture

`ext4-fragmented.img.zst` is a 32 MiB ext4 filesystem containing four
128 KiB files. Each file has 32 initialized one-block extents interleaved with
the other three files. The rest of the filesystem has enough contiguous free
space for each file to reach one extent.

Regenerate it with `./generate-ext4-fragmented.sh`. The script requires
`mkfs.ext4`, `debugfs`, `e2fsck`, and `zstd`; these are test-only tools and are
never invoked by Defragger itself.

Run the capability-gated end-to-end smoke test from the repository root with:

```sh
crates/defrag-service/tests/fixtures/run-ext4-fixture.sh
# or
just integration-test
```

Run it as your normal user. Cargo and fixture preparation remain unprivileged;
the script invokes `sudo` only for loop-device setup/teardown, the compiled test
process that needs `CAP_SYS_ADMIN`, and `e2fsck`.

## FAT16/FAT32 matrix

`run-fat-fixtures.sh` formats fresh FAT16 and FAT32 images and mounts them with
Linux vfat to create deliberately fragmented data, a subdirectory, and long
VFAT names. It unmounts before running defrag and compact, checks the result
with `fsck.fat -n`, then mounts read-only and verifies every payload through the
kernel filesystem driver.

Run `just integration-test-fat` as your normal user. The runtime implementation
does not invoke `fsck.fat`; it is an independent integration-test oracle.

## Mounted-volume fragmentation stress fixture

To deliberately fragment a disposable mounted ext4, FAT, or exFAT filesystem,
run this from the repository root:

```sh
just trash-fragmentation /mnt/disposable
```

The path must be an exact mount point, cannot be `/`, and must be writable by
the invoking user. For a root-owned test mount, explicitly run the recipe as
`sudo just trash-fragmentation /mnt/disposable`. The recipe consumes 96% of the
space that was free when it started, creates alternating allocation slots,
deletes half, and cycles writes to multiple payload files through the resulting
holes. Finally it removes a large allocation anchor so the defragmenter has
contiguous workspace.

The filesystem becomes nearly full during the run and sees substantial writes
when `fallocate` is unavailable. Existing files are not changed or removed, but
other applications using the filesystem may run out of space temporarily. The
recipe refuses to replace a previous fixture directory.
After testing, reclaim the space by removing the single
`.defragger-fragmentation-fixture` directory from that mount.

For unusual fixture sizes, `DEFRAGGER_FRAGMENT_FILL_PERCENT` can be set from 80
through 98 and `DEFRAGGER_FRAGMENT_SLOT_COUNT` can be set to an even value from
128 through 16384.
