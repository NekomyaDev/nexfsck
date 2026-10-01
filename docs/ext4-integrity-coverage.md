# ext4 metadata integrity coverage

This matrix describes what the checker verifies in its current implementation.
“Parsed” means the structure is read and interpreted; it does not imply that
its checksum is verified. A checksum mismatch is an uncorrected error. Nexfsck
does not rewrite a checksum unless it has independently repaired and validated
the underlying metadata.

## `metadata_csum` structures

| Structure | Before this phase | Current implementation | Notes |
| --- | --- | --- | --- |
| Primary superblock | Parsed, no checksum | Validated | CRC32c over bytes 0..1020 with initial `~0`; only CRC32c checksum type accepted |
| Backup superblocks | Magic parsed during discovery | Expected copies systematically checked | Supported sparse-super/sparse-super2/non-sparse placements are read; checksum and geometry/feature/UUID consistency are checked. Verification only; no replacement |
| Group descriptor | Parsed, no checksum | Validated | Descriptor-size bytes, group number and zeroed checksum field; 16-bit stored value |
| Block bitmap | Compared, no checksum | Validated | CRC32c over bitmap buffer with filesystem checksum seed; truncated when descriptor lacks high field |
| Inode bitmap | Compared, no checksum | Validated | CRC32c over `ceil(inodes_per_group/8)` bytes |
| Inode | Validated | Validated | Raw inode size, inode number, generation and checksum fields follow e2fsprogs behavior |
| Directory leaf block | Dirents parsed, no checksum | Validated | CRC32c uses filesystem seed, inode number, generation and bytes before the fake tail |
| HTree root/node | Structure partially validated | Checksum validated; structural validation partial | CRC32c uses inode context, occupied entry range and dx tail; real indexed-image corruption test |
| External extent-tree block | Extents parsed, no checksum | Checksum + semantic validation, still partial | CRC32c uses inode number/generation seed and `eh_max` tail. Depth transitions, capacities, key/range ordering, zero lengths, bounds, cycles and reused child blocks are checked. Explicit metadata-block-vs-data semantics remain incomplete |
| Extended-attribute block | Not parsed | Checksum and bounds validation; partial semantics | External value ranges/header/refcount parsed; kernel CRC32c uses filesystem seed and block number. Entry/value hashes and cross-inode shared-block refcount accounting are not verified |
| MMP block | Feature detected before scan | Explicitly rejected before verification | Nexfsck does not sample sequence stability across the configured MMP interval or perform the kernel/e2fsprogs ownership protocol. A single checksum-valid read cannot prove exclusive ownership; read-only and repair paths both refuse MMP filesystems |
| JBD2 | Journal inode/superblock state parsed | Superblock structure/checksum validated; transaction coverage partial | JBD2 v3 superblock CRC32c, type, block geometry and log bounds are checked. Legacy v1 and v2 checksums, descriptor/commit/revoke transaction checksums and dirty-log replay are not implemented; dirty/recovery-required state blocks repair and is never reported clean |

The JBD2 rules above are cross-checked against the [Linux ext4 journal format
documentation](https://github.com/torvalds/linux/blob/master/Documentation/filesystems/ext4/journal.rst)
and [JBD2 checksum implementation](https://github.com/torvalds/linux/blob/master/fs/jbd2/journal.c).
MMP is intentionally rejected until its [Linux sequence/ownership protocol](https://github.com/torvalds/linux/blob/master/fs/ext4/mmp.c)
can be implemented and exercised safely; checksum validity alone is not an ownership proof.

For `gdt_csum` without `metadata_csum`, group descriptors use the legacy CRC16
rule over UUID, group number and descriptor bytes excluding the checksum field.

## Feature compatibility

| Feature | Status | Current scope / limitation |
| --- | --- | --- |
| extents | Partially supported | Root/external nodes validate depth, capacity, ordering/range constraints, bounds, zero lengths, and cycles/reused children; metadata-block exclusion and full e2fsck semantic parity remain incomplete |
| Backup superblocks | Verification supported for current layouts | Expected backup-bearing groups are systematically located, checksummed, and compared with primary geometry/features; no replacement or primary recovery is performed |
| External xattr blocks | Partially supported | External xattr block structure/checksum and bounds verified; semantic hashes and cross-inode refcount accounting remain unchecked |
| 64bit | Read-only supported | High block-address fields and descriptor sizes are parsed |
| metadata_csum | Partially supported | Superblock, group descriptor, allocation bitmap, inode, directory/HTree, external extent and external xattr block checksums validated; xattr semantics and MMP remain incomplete |
| metadata_csum_seed | Read-only supported | Explicit checksum seed used for implemented checksum classes |
| gdt_csum | Partially supported | Legacy group descriptor CRC16 validated |
| sparse_super2 | Read-only supported | Backup-superblock placement follows the two explicit backup group numbers |
| dir_index | Partially supported | HTree checksum is validated; full structural/rebalancing parity is not claimed |
| flex_bg | Read-only supported | Metadata may reside outside its nominal group; descriptor locations are followed |
| bigalloc | Explicitly rejected | Cluster accounting semantics are not verified |
| inline_data | Explicitly rejected | Inline payload and xattr semantics are not verified |
| ea_inode | Explicitly rejected | External xattr inode semantics are not verified |
| orphan_file | Explicitly rejected | Orphan-file contents are not validated |
| quota | Explicitly rejected | Quota metadata consistency is not validated |
| project quota | Explicitly rejected | Project-quota metadata consistency is not validated |
| verity | Explicitly rejected | Merkle tree and descriptor verification are not performed |
| encryption | Explicitly rejected | Encryption-specific directory semantics are not verified |
| casefold | Explicitly rejected | Unicode normalization and casefold semantics are not checked |
| MMP | Explicitly rejected | Ownership cannot be established safely without sequence-stability sampling and the MMP protocol; even read-only verification refuses rather than infer safety from one block read |
| external journal | Explicitly rejected | External-device discovery/replay is not implemented |
| Unknown incompat / RO-compat bits | Explicitly rejected | Unknown incompatibility and unverified RO-compatibility bits fail before scanning |
| Unknown compat bits | Ignored per ext4 compat semantics | The recognized `orphan_file` compat feature is explicitly rejected because its inode-recovery semantics are not checked |

This matrix is intentionally conservative. Parsed or read-only supported does
not imply parity with e2fsck recovery behavior.

If any checksum failure is observed, repair mode refuses all bitmap mutations
for that run. A checksum mismatch does not establish whether the payload or only
the stored checksum is wrong. The extent-node corruption oracle also verifies
that no undo journal is created and the image bytes remain unchanged.

The CLI reports centralized repair eligibility with stable reason codes in JSON
and explanations in human output. Backup checksum/geometry failures, invalid
inode/extent structure, directory/reference errors, dirty journal state, and
storage media errors block bitmap repair. A checksum-failed filesystem may
still be scanned read-only to collect independent diagnostics.

The initial differential runner is `scripts/differential_ext4.py`. Its JSON
captures six independent mutations (primary/backup superblock checksum, group
descriptor checksum, block/inode bitmap payload, and inode checksum), exact byte
offsets, commands, exit codes, counters, and diagnostics. It deliberately
lists the remaining metadata classes as uncovered; this is not complete
differential parity. Current observed behavior includes e2fsck 1.46.5 exiting
zero for a corrupted backup checksum (not checked by its ordinary `-fn` scan)
and reporting but ignoring a descriptor checksum mismatch in read-only mode.
