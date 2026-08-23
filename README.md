# Defragger

Defragger is a Plasma-first Linux filesystem analyzer and ext4 defragmenter
written in Rust with Qt Quick/Kirigami and command-line clients. It analyzes
mounted or offline ext4 and analyzes mounted FAT12/16/32 and exFAT.

It calls Linux filesystem ioctls directly. It does not execute `e4defrag`,
`filefrag`, or any other filesystem utility.

## Build

Runtime/build dependencies on Arch Linux:

```text
cmake extra-cmake-modules kirigami polkit polkit-kde-agent
qqc2-desktop-style qt6-base qt6-declarative rust
```

## Standalone development

The default Cargo build starts the current GUI executable a second time as a
transient root systemd service. systemd's PolicyKit action produces the normal
desktop authentication dialog; no helper binary, D-Bus policy, service file,
PolicyKit action, CMake build, installation, or daemon reload is needed:

```sh
cargo run -r
# or, with just installed:
just run
```

The command-line client uses the same transient helper and authentication:

```sh
cargo run -r -p defragger-cli -- list
cargo run -r -p defragger-cli -- analyze /dev/nvme0n1p2
cargo run -r -p defragger-cli -- defrag /dev/nvme0n1p2 --yes --require-fully-defragmented
```

Shortcuts: `just list`, `just analyze DEVICE`, and `just defrag DEVICE`. Device
symlinks, mount points, and loop backing-image paths are also accepted.

It streams stable textual progress, physical read/write ranges, and final
metrics. Ctrl-C requests cancellation and waits for a safe extent-move
boundary. Root-only fixture tests can bypass the helper with `--direct`.

The transient process runs the same helper implementation as the installed
service and talks to the GUI over a private D-Bus peer connection. If the GUI
exits or crashes, that connection closes, the helper exits, and systemd
collects the transient unit.

This mode requires systemd and an active graphical PolicyKit agent. An explicit
unprivileged fallback remains available for environments without them:

```sh
cargo run -r --no-default-features
# or:
just run-unprivileged
```

The fallback runs the service in the GUI process with the permissions of your
shell user. Protected files are skipped, and FAT/exFAT FIBMAP may be unavailable
without `CAP_SYS_RAWIO`.

## Installed privileged mode

The production split-service build is explicit. CMake enables the
`system-helper` Cargo feature for the GUI and builds the separate helper:

```sh
cmake -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr
cmake --build build
sudo cmake --install build
sudo systemctl daemon-reload
```

The equivalent shortcuts are:

```sh
just system-build
just system-install
```

Once the helper is installed, the helper-backed GUI can also be run directly
from Cargo while developing:

```sh
cargo run --release --package defragger --no-default-features --features system-helper
```

The system-wide install includes a root-owned D-Bus helper and separate
PolicyKit actions for analysis and modification. The desktop PolicyKit agent
owns the authentication UI; the Qt/Kirigami GUI and CLI remain unprivileged. A
per-user installation cannot install or activate this helper.

Force Wayland in either mode with:

```sh
QT_QPA_PLATFORM=wayland cargo run --release --package defragger
```

The analyzer does not follow symbolic links or cross mount boundaries. The
installed helper can read protected files after PolicyKit authorization. FAT
and exFAT file fragmentation uses FIEMAP where available and Linux's
capability-gated FIBMAP fallback otherwise. The helper uses a bounded
capability set for protected-file inspection, private mounts, and ext4 extent
moves. Because Linux does not expose a
filesystem-wide allocation map for them, their map also leaves free space and
filesystem metadata explicitly unknown.

The adaptive block map updates while analysis proceeds. A 4,096-bin backend
map is combined into as many fixed 9-pixel tiles as the available area can
hold, and is reevaluated when the window changes size. Tiles use one priority
color: red fragmented data, gray unscanned allocation, white explicitly free,
green contiguous data, or a typed metadata color. Partial occupancy produces
a lighter shade, and hovering shows the exact composition and physical range.
See [the architecture notes](docs/architecture.md) for the D-Bus/PolicyKit and
write boundaries.
