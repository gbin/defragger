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
