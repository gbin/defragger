# Architecture and privilege boundary

The GUI talks only in `ServiceRequest` and `ServiceEvent` domain messages. The v0
`InProcessClient` dispatches those messages to the same-process service, but the
filesystem backend does not depend on Qt or on that transport.

## v0 read path

- Mount discovery reads `/proc/self/mountinfo` and uses `statvfs(3)`.
- ext4's physical allocation map comes from `FS_IOC_GETFSMAP`.
- Per-file physical extents come from unsynchronized `FS_IOC_FIEMAP`.
- FAT12/16/32 and exFAT share a file-mapping reader. It tries FIEMAP first and
  falls back to FIBMAP, which Linux restricts to `CAP_SYS_RAWIO`. Linux also
  does not provide their filesystem-wide allocation map, so unobserved space
  remains unknown and those reports are marked partial.
- `statx(2)` mount IDs keep traversal inside the selected mount.
- No executable is spawned. Inaccessible files are counted and make the report
  explicitly partial.

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

The Plasma GUI must never run as root. A separately installed, narrowly scoped
system service will expose the same protocol over D-Bus and use Polkit for
authorization. Read-all-files and move-extents actions should have separate
Polkit actions. The helper will reopen and revalidate every path and extent; it
must not trust file descriptors, physical offsets, or eligibility decisions
supplied by the GUI.

The ext4 writer will use `EXT4_IOC_MOVE_EXT` and donor files directly. It will
require a mounted read-write filesystem and authorization. There is deliberately
no `pkexec`, shell command, `e4defrag`, or root GUI fallback.

`FilesystemBackend`, `FilesystemAnalysis`, and `PreparedPlan` keep filesystem
details out of the controller. FAT and exFAT share their read path, while their
future offline writers can still use different on-disk implementations.
