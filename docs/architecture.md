# Architecture and privilege boundary

The filesystem backends do not depend on Qt or on a transport. Three build
modes select how the GUI or CLI reaches the same service API:

- The default Cargo build uses `DevelopmentClient`. It asks systemd, through
  PolicyKit, to launch the current client executable as a transient root service
  in a hidden helper mode. The two processes use a private peer-to-peer D-Bus
  connection, so no development files need to be installed.
- `--no-default-features` selects `InProcessClient`, an explicit unprivileged
  fallback for systems without systemd or a graphical PolicyKit agent.
- The `system-helper` GUI feature uses the system D-Bus client. CMake enables
  this feature and builds the separate privileged helper for installation.

Both clients deliver the same `ServiceEvent` domain messages, so pause, resume,
cancellation, live maps, reports, and execution follow one service path. The
CLI adds a deterministic textual view of those events.

The development peer connection also owns the transient helper's lifetime.
Closing or crashing the GUI closes the socket; the helper waits for that close,
then exits, and systemd's `--collect` removes the stopped transient unit.

## Privileged read path

The GUI obtains the volume list from an unprivileged helper method so that the
mount IDs come from the helper's hardened mount namespace. When the user
presses Analyze, it calls `net.gootz.defragger.Helper1.StartAnalysis` on the
system bus with the interactive-authentication flag. The root-owned helper asks
PolicyKit to check the calling connection for
`net.gootz.defragger.read-all-files`; the desktop's PolicyKit agent owns all
password UI. Jobs and completed analyses are bound to that unique D-Bus caller,
and every later operation verifies the owner. A client can own only one active
job, abandoned jobs are cancelled, and unused completed analyses expire.

The helper receives only opaque volume, analysis, and plan IDs. It discovers
and validates the selected device itself and streams serialized domain events.
Neither client passes a path, file descriptor, physical offset, or command to
execute across the privilege boundary.

- Mount discovery merges `/proc/self/mountinfo`, sysfs block devices, and udev
  filesystem metadata, so supported unmounted volumes remain selectable.
- ext4's physical allocation map comes from `FS_IOC_GETFSMAP`.
- Per-file physical extents come from unsynchronized `FS_IOC_FIEMAP`.
- Mounted FAT12/16/32 and exFAT share a file-mapping reader. It tries FIEMAP first and
  falls back to FIBMAP, which Linux restricts to `CAP_SYS_RAWIO`. Linux also
  does not provide their filesystem-wide allocation map, so unobserved space
  remains unknown and those reports are marked partial.
- `statx(2)` mount IDs keep traversal inside the selected mount.
- No filesystem utility or shell command is spawned. Development mode invokes
  only `systemd-run` to establish the privilege boundary. Entries that remain
  inaccessible or cannot be mapped are counted and make the report explicitly
  partial. Unmounted classic FAT uses a raw parser instead, producing a complete
  allocation map when its FAT copies and chains validate.

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

As files are inspected or moved, the backend publishes coherent replacement
maps. During a move it also publishes exact source/read and donor/write ranges,
which the GUI draws as differently colored contours.

## Privileged write paths

The clients never run as root. `StartDefrag` uses the separate
`net.gootz.defragger.modify-filesystem` PolicyKit action. The helper reopens and
revalidates every candidate immediately before operating on it.

The ext4 writer uses unlinked donor files and `EXT4_IOC_MOVE_EXT` directly. An
offline ext4 volume is privately mounted in the job's mount namespace. Offline
analysis uses a clean, read-only `noload` mount; if journal recovery is needed,
the helper requests modification authorization and privately mounts read-write
to replay the journal before analysis. Ext4's on-disk error flag blocks every
writable mount, and an `EBADMSG` allocation-map result is treated as filesystem
corruption rather than a recoverable visualization failure. The complete donor
layout is published before a file move. Chunk data is range-synced and released
from the page cache, while the donor retains exchanged source extents until it
is closed once at the file boundary. FIEMAP/GETFSMAP and the UI map are then
refreshed once instead of after every chunk. A partially improved file may be
retried once with a donor allocated from the filesystem root. Cancellation is
observed before a move or after a range-sync boundary. There is no shell
command, `e4defrag`, or root GUI fallback.

The FAT16/FAT32 writer only accepts an unmounted, clean, mirrored classic FAT
snapshot. It reparses and compares the boot sector, allocation tables, file
chains, and directory slots immediately before opening the write path. FAT12
and exFAT are never written.

Defrag mode moves only fragmented policy candidates that already have a wholly
free contiguous destination. Compact mode packs safely movable regular files
toward low cluster addresses; directories, bad clusters, oversized files, and
other hard pins divide the packed regions. A contiguous file moved only to make
packing possible is identified as a supporting move in the plan.

Each cluster copy is read back before metadata points at it. The writer then
installs the destination FAT chain, flushes it, pivots the short directory
entry, flushes again, and finally frees the old chain. Mirrored FAT copies are
updated with FAT1 last, and FAT32 FSInfo is refreshed at completion. FAT has no
journal, so a crash can still leave unreferenced allocated clusters for fsck,
but the ordering does not expose an unverified destination as file data.

`FilesystemBackend`, `FilesystemAnalysis`, and `PreparedPlan` keep filesystem
details out of the clients.
