# Defragger

Defragger is a Plasma-first Linux filesystem analyzer written in Rust with a
Qt Quick/Kirigami interface. The v0 release is deliberately read-only and
supports mounted ext4 filesystems.

It calls Linux filesystem ioctls directly. It does not execute `e4defrag`,
`filefrag`, or any other filesystem utility.

## Build

Runtime/build dependencies on Arch Linux:

```text
cmake extra-cmake-modules kirigami qqc2-desktop-style qt6-base qt6-declarative rust
```

For a quick development build:

```sh
cargo build --workspace
cargo test --workspace
```

For an installed KDE application:

```sh
cmake -B build -DCMAKE_BUILD_TYPE=Debug -DCMAKE_INSTALL_PREFIX="$HOME/.local"
cmake --build build
cmake --install build
```

Run a development build on Wayland with:

```sh
QT_QPA_PLATFORM=wayland cargo run --package defragger
```

The analyzer does not follow symbolic links or cross mount boundaries. In the
single-process v0 build it reports only files readable by the current user and
marks incomplete results as partial.

The adaptive block map updates while analysis proceeds. A 4,096-bin backend
map is combined into as many fixed 9-pixel tiles as the available area can
hold, and is reevaluated when the window changes size. Tiles use one priority
color: red fragmented data, gray unscanned allocation, white explicitly free,
green contiguous data, or a typed metadata color. Partial occupancy produces
a lighter shade, and hovering shows the exact composition and physical range.
See [the architecture notes](docs/architecture.md) for the planned
D-Bus/Polkit helper and write boundary.
