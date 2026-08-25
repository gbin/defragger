# Defragger

[![License: MIT or Apache 2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![Platform: Linux](https://img.shields.io/badge/platform-Linux-lightgrey.svg)](https://kernel.org/)
![Status: Alpha](https://img.shields.io/badge/status-alpha-red.svg)

**A real graphical defragmenter for Linux. Occasionally useful but weirdly satisfying.**

![Defragger reorganizing a generated test volume](docs/defragger.gif)

After moving to Linux, 20+ years ago ... I discovered that the Windows utility I missed most was
the graphical disk defragmenter.

Not because modern Linux filesystems need a weekly defrag ritual. The old block
map was one of those accidental interfaces that made the machine legible: you
could actually watch the filesystem reorganize itself.

Then I needed a reproducible storage layout while testing a high-throughput
logger. Fragmentation and extent allocation were adding measurable variance,
even on NVMe, so the nostalgia project became a real filesystem tool.

Defragger shows how files, free space, and filesystem metadata are allocated,
finds fragmented files, and can reorganize supported filesystems without
shelling out to `e4defrag` or `filefrag`.


> [!WARNING]
> This is alpha filesystem software. Back up important data. The ext4 path has
> received the most testing. FAT write support is newer and should first be
> used with disposable or fully backed-up volumes.

## Try it safely

```sh
just demo
```

This decompresses the repository's deliberately fragmented ext4 fixture,
attaches the copy as a temporary loop device, and opens Defragger. Select the
loop device printed in the terminal. Closing Defragger detaches and deletes the
temporary image; the demo does not write to your real volumes.

The demo needs `just`, `zstd`, loop-device support, and `sudo` for attaching the
temporary image. The application itself remains unprivileged.

## How it works

Defragger is not a wrapper around existing command-line tools.

For ext4, it obtains filesystem allocation maps through `FS_IOC_GETFSMAP`, maps
individual files through `FS_IOC_FIEMAP`, and moves extents using the kernel's
`EXT4_IOC_MOVE_EXT` interface.

The graphical application and CLI never run as root. Privileged operations are
performed by a narrow PolicyKit-authorized helper over D-Bus. The helper accepts
validated, opaque operation IDs rather than arbitrary paths or commands.

Classic FAT writing is offline only. Defragger validates both FAT copies and
file chains, reads copied data back before redirecting directory metadata, and
updates mirrored allocation tables conservatively.

See the [architecture notes](docs/architecture.md) for the complete privilege
boundary, validation rules, and update ordering.

## What it does

- Analyzes fragmentation and displays an adaptive filesystem block map.
- Defragments ext4 files and offline FAT16/FAT32 volumes.
- Compacts offline FAT16/FAT32 volumes by moving data toward the start of the
  filesystem.
- Uses PolicyKit for operations that need elevated access while keeping the GUI
  and CLI unprivileged.
- Supports safe cancellation and reports progress and final metrics.

## Supported environment

Defragger runs on Linux with Qt 6 and KDE Kirigami. KDE Plasma is the primary
desktop target, but the project is not specific to Arch Linux. Privileged mode
requires systemd, PolicyKit, and an active graphical PolicyKit agent.

| Filesystem | Analysis | Defragmentation | Compaction |
| --- | --- | --- | --- |
| ext4 | Online | Online | — |
| FAT16/FAT32 | Online | Offline | Offline |
| FAT12 | Online | — | — |
| exFAT | Online | — | — |

The build requires Rust 1.85 or newer, CMake 3.24 or newer, Qt 6 Base and
Declarative, Kirigami, a Qt Quick Controls desktop style, PolicyKit, and a C++
toolchain. Package names vary by distribution.

## Run from source

The default development build uses a transient systemd helper and prompts for
authorization through PolicyKit; it does not need to be installed first.

```sh
cargo run --release
# or
just run
```

Run without the privileged helper when systemd or PolicyKit is unavailable:

```sh
cargo run --release --no-default-features
```

The CLI accepts device paths, mount points, device symlinks, and loop-image
paths:

```sh
cargo run --release -p defragger-cli -- list
cargo run --release -p defragger-cli -- analyze /dev/nvme0n1p2
cargo run --release -p defragger-cli -- defrag /dev/nvme0n1p2 --yes
cargo run --release -p defragger-cli -- compact /dev/sdb1 --yes
```

## System-wide installation

```sh
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr
cmake --build build
sudo cmake --install build
sudo systemctl daemon-reload
```

This installs the application, CLI, root-owned D-Bus helper, systemd service,
and PolicyKit actions. See the [architecture notes](docs/architecture.md) for
the privilege boundaries and implementation details.

## License

Copyright 2026 Guillaume Binet.

Defragger is dual-licensed under the [MIT License](LICENSE-MIT) or the
[Apache License 2.0](LICENSE-APACHE), at your option.

Third-party components and assets remain under their respective licenses. In
particular, the application icon is from KDE's Breeze Icons project; see its
[attribution and licensing details](packaging/ICON_ATTRIBUTION.md).
