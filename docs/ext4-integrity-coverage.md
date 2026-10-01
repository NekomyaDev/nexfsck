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
| Backup superblocks | Magic parsed during discovery | Partially validated | Explicit backup selection verifies its checksum; automatic backup discovery does not validate every backup copy |
| Group descriptor | Parsed, no checksum | Validated | Descriptor-size bytes, group number and zeroed checksum field; 16-bit stored value |
| Block bitmap | Compared, no checksum | Validated | CRC32c over bitmap buffer with filesystem checksum seed; truncated when descriptor lacks high field |
| Inode bitmap | Compared, no checksum | Validated | CRC32c over `ceil(inodes_per_group/8)` bytes |
| Inode | Validated | Validated | Raw inode size, inode number, generation and checksum fields follow e2fsprogs behavior |
| Directory leaf block | Dirents parsed, no checksum | Validated | CRC32c uses filesystem seed, inode number, generation and bytes before the fake tail |
| HTree root/node | Structure partially validated | Checksum validated; structural validation partial | CRC32c uses inode context, occupied entry range and dx tail; real indexed-image corruption test |
| External extent-tree block | Extents parsed, no checksum | Validated | CRC32c uses inode number/generation seed and the `eh_max`-defined extent tail; clean and corrupted real-image oracle test |
| Extended-attribute block | Not parsed | Checksum and bounds validation; partial semantics | External value ranges/header/refcount parsed; kernel CRC32c uses filesystem seed and block number. Header/entry hash consistency and cross-inode refcount accounting are not yet verified |
| MMP block | Feature detected before scan | Explicitly rejected | No MMP sequence/checksum or active-owner validation; MMP filesystems are not reported clean |
| JBD2 | Journal inode/superblock state parsed | Partially validated; malformed/unknown formats fail closed | The inspection path validates basic location/header/features and clean/dirty state. Replay-plan helpers test v3 data-tag validation, but JBD2 superblock/descriptor/commit/revoke checksum coverage is not complete or claimed |

For `gdt_csum` without `metadata_csum`, group descriptors use the legacy CRC16
rule over UUID, group number and descriptor bytes excluding the checksum field.

## Feature compatibility

| Feature | Status | Current scope / limitation |
| --- | --- | --- |
| extents | Partially supported | Inline roots/external nodes parsed and external-node checksums verified; complete extent-tree semantic parity is not claimed |
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
| MMP | Explicitly rejected | Checksum and active-owner safety checks are incomplete |
| external journal | Explicitly rejected | External-device discovery/replay is not implemented |

This matrix is intentionally conservative. Parsed or read-only supported does
not imply parity with e2fsck recovery behavior.

If any checksum failure is observed, repair mode refuses all bitmap mutations
for that run. A checksum mismatch does not establish whether the payload or only
the stored checksum is wrong. The extent-node corruption oracle also verifies
that no undo journal is created and the image bytes remain unchanged.

The CLI now reports a centralized repair eligibility decision and its blocking
reasons in JSON/human output. Invalid inode/extent structure, directory or
reference validation errors, dirty journal state, and storage media errors also
block bitmap repair. A checksum-failed filesystem may still be scanned read-only
to collect independent diagnostics.
