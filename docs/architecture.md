# Architecture and privilege boundary

The filesystem backends do not depend on Qt or on a transport. Three build
modes select how the GUI reaches the same service API:

- The default Cargo build uses `DevelopmentClient`. It asks systemd, through
  PolicyKit, to launch the current GUI executable as a transient root service
  in a hidden helper mode. The two processes use a private peer-to-peer D-Bus
  connection, so no development files need to be installed.
- `--no-default-features` selects `InProcessClient`, an explicit unprivileged
  fallback for systems without systemd or a graphical PolicyKit agent.
- The `system-helper` GUI feature uses the system D-Bus client. CMake enables
  this feature and builds the separate privileged helper for installation.

Both modes deliver the same `ServiceEvent` domain messages to the controller,
so pause, resume, cancellation, live maps, reports, and plan previews follow one
UI code path.

The development peer connection also owns the transient helper's lifetime.
Closing or crashing the GUI closes the socket; the helper waits for that close,
then exits, and systemd's `--collect` removes the stopped transient unit.

## Privileged read path

The GUI obtains the volume list from an unprivileged helper method so that the
mount IDs come from the helper's hardened mount namespace. When the user
presses Analyze, it calls `io.github.defragger.Helper1.StartAnalysis` on the
system bus with the interactive-authentication flag. The root-owned helper asks
PolicyKit to check the calling connection for
`io.github.defragger.read-all-files`; the desktop's PolicyKit agent owns all
password UI. Jobs and completed analyses are bound to that unique D-Bus caller,
and every later operation verifies the owner. A client can own only one active
job, abandoned jobs are cancelled, and unused completed analyses expire.

The helper receives only a mount ID. It discovers and validates the selected
mount itself, streams serialized domain events to the GUI, and supports pause,
resume, cancellation, and read-only plan construction. The GUI never passes a
path, file descriptor, physical offset, or command to execute across the
privilege boundary.

- Mount discovery reads `/proc/self/mountinfo` and uses `statvfs(3)`.
- ext4's physical allocation map comes from `FS_IOC_GETFSMAP`.
- Per-file physical extents come from unsynchronized `FS_IOC_FIEMAP`.
- FAT12/16/32 and exFAT share a file-mapping reader. It tries FIEMAP first and
  falls back to FIBMAP, which Linux restricts to `CAP_SYS_RAWIO`. Linux also
  does not provide their filesystem-wide allocation map, so unobserved space
  remains unknown and those reports are marked partial.
- `statx(2)` mount IDs keep traversal inside the selected mount.
- No filesystem utility or shell command is spawned. Development mode invokes
  only `systemd-run` to establish the privilege boundary. Entries that remain
  inaccessible or cannot be mapped are counted and make the report explicitly
  partial.

The backend maintains 4,096 physical-range bins. The GUI reevaluates its tile
count from the available pixel area and combines adjacent bins to fill the map
with fixed 9-pixel tiles. Each displayed tile keeps exact
basis-point composition for empty space, contiguous data, fragmented data,
unscanned allocation, and typed metadata. The display chooses one priority
color per tile (fragmented data first), lightens it according to the fraction
of the tile occupied by that category, and exposes the complete composition on
hover. Metadata categories are filesystem-neutral: headers, journal/log,
allocation tables, file metadata, group descriptors, block and file bitmaps,
reserved, and other. A backend fills whichever categories its kernel API can
identify.

As files are inspected, the backend publishes changed bins and the GUI
reclassifies those blocks as contiguous or fragmented scanned data. A future
writer will publish the same events before and after extent moves.

## Future privileged write path

The Plasma GUI must never run as root. Any future move-extents API must use a
separate PolicyKit action. The helper will reopen and revalidate every path and
extent; it must not trust file descriptors, physical offsets, or eligibility
decisions supplied by the GUI.

The ext4 writer will use `EXT4_IOC_MOVE_EXT` and donor files directly. It will
require a mounted read-write filesystem and authorization. There is deliberately
no `pkexec`, shell command, `e4defrag`, or root GUI fallback.

`FilesystemBackend`, `FilesystemAnalysis`, and `PreparedPlan` keep filesystem
details out of the controller. FAT and exFAT share their read path, while their
future offline writers can still use different on-disk implementations.
