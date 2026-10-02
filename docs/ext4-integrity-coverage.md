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
| External extent-tree block | Extents parsed, no checksum | Checksum + semantic validation, still partial | CRC32c uses inode number/generation seed and `eh_max` tail. Depth transitions, capacities, key/range ordering, zero lengths, bounds, cycles and reused child blocks are checked. File data is compared against tracked superblock/GDT/reserved-GDT, bitmap, inode-table, journal-inode, external extent-node and external xattr blocks. Metadata discovery is descriptor-driven for bitmap/inode-table placement (including flex_bg), but backup/GDT variants and all metadata roles are not yet independently proven complete |
| Extended-attribute block | Not parsed | Checksum, bounds, ordering, entry/block hashes, and observed shared-reference count validated | External values must be block-local; ea_inode references remain rejected. Zero `h_hash` is accepted as ext4's “never share” sentinel; nonzero block hashes and all entry hashes use Linux/e2fsprogs algorithms. Header refcount is compared with checksum-valid inode references. Real-image xattr differential coverage is present; e2fsck may not diagnose every nonzero semantic hash mutation in read-only mode |
| MMP block | Feature detected before scan | Explicitly rejected before verification | Nexfsck does not sample sequence stability across the configured MMP interval or perform the kernel/e2fsprogs ownership protocol. A single checksum-valid read cannot prove exclusive ownership; read-only and repair paths both refuse MMP filesystems |
| JBD2 | Journal inode/superblock state parsed | V3 superblock and bounded active transaction scan are verification-only | The internal journal inode is extent-mapped with bounds checks. CRC32c v3 descriptor/commit blocks, tags/data checksums, revoke bounds/checksum helpers, sequence continuity and incomplete tails are checked. V1/v2, async commit, external journal, and replay are rejected. Real descriptor+commit path coverage exists; a real revoke transaction remains unavailable |

### Metadata ownership map (current extent-overlap checks)

| Metadata class | Discovery source | Current confidence / limitation |
| --- | --- | --- |
| Primary superblock and primary GDT | Primary geometry, descriptor size, group count | Contiguous primary range is protected; meta_bg layouts are not claimed supported |
| Backup superblocks and backup GDT copies | `group_has_superblock`, sparse-super/sparse-super2 policy, descriptor geometry | Expected copies are independently read/checksummed; physical ranges are conservatively protected. Layouts whose placement cannot be derived from the supported geometry fail closed via feature policy |
| Reserved GDT blocks | `resize_inode` compat feature and `s_reserved_gdt_blocks` | Protected only when resize_inode is enabled; no resize operation is attempted |
| Block/inode bitmaps | Physical block numbers in group descriptors | Descriptor locations are used rather than nominal group placement, including flex_bg |
| Inode tables | Physical inode-table block in each descriptor plus inode geometry | Descriptor locations are used; range is bounded to filesystem size |
| Journal inode data | Journal inode extent mapping | The bounded mapped journal range is protected; dirty/replay-required journals are scanned read-only and always block clean/repair eligibility |
| External extent-tree nodes | Pre-discovery traversal of inode extent trees | Kept in a separate sparse ownership tracker to detect file-data aliases without treating a tree node as its own conflicting metadata |
| External xattr blocks | Checksum-valid inode references and xattr block reads | Cross-inode references counted; corrupt/unknown xattr references block repair |
| Other metadata (quota, orphan, ea_inode, MMP, verity, etc.) | Feature policy | Explicitly rejected where their ownership/semantics are not implemented; not silently considered protected or clean |

Current real-image extent-ownership mutations redirect a checksum-valid file
extent to the block bitmap, inode bitmap, inode table, primary GDT, backup
superblock, and its external xattr block. Each must increment
`extent_metadata_overlap_failures`, block repair, and preserve the image on a
repair attempt. Superblock block zero itself is not used as a data target
because ext4 reserves physical block zero and the checker independently treats
it as an invalid data address; external extent-node overlap is not yet in this
real-image mutation set.

### JBD2 support matrix

| Mode | Status |
| --- | --- |
| Journal superblock structure/geometry | Supported |
| Journal with no checksum feature | Supported structurally; no transaction checksum is available |
| Checksum v1 | Rejected |
| Checksum v2 | Rejected |
| Checksum v3 superblock | Supported |
| Checksum v3 descriptor/commit transaction blocks | VerificationOnly; exercised through the real internal journal inode on a generated mounted ext4 image; no replay |
| Checksum v3 revoke transaction blocks | VerificationOnly; parser/helper covered synthetically, no real revoke transaction fixture yet |
| Descriptor validation | VerificationOnly; bounded active-ring traversal checks header/sequence, tag bounds/flags, target range, data checksums, and transaction commit; multiple descriptors supported |
| Descriptor checksum | VerificationOnly; CRC32c seeded with journal UUID, exercised on real ext4 transaction |
| Commit checksum | VerificationOnly; CRC32c validation exercised on real ext4 transaction; sequence must match |
| Revoke checksum | VerificationOnly; v3 CRC32c, used-byte bounds, entry alignment, and target bounds; real transaction fixture outstanding |
| 64-bit journal tags | VerificationOnly; high word parsed on real ext4 v3 descriptor, synthetic out-of-range coverage |
| Async commit | Rejected |
| Dirty journal | VerificationOnly; active ring scanned within validated geometry and a 64 MiB memory bound; dirty state always blocks clean/repair eligibility |
| Replay | Rejected; no replay attempted |
| External journal | Rejected |

The differential runner creates a disposable ext4 image mounted with
`commit=600,data=journal`, writes and fsyncs a file, then snapshots before
unmount. The captured active transaction had sequence 2, one descriptor, five
data blocks, and a commit; Nexfsck reported one committed transaction and kept
repair ineligible because replay is not implemented. The runner compares each
mutation against the dirty active baseline so the baseline's own non-clean
status is not mistaken for mutation detection.

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
| extents | Partially supported | Root/external nodes validate depth, capacity, ordering/range constraints, bounds, zero lengths, cycles/reused children, and overlap with metadata roles currently classified by Nexfsck; complete ownership and e2fsck parity remain incomplete |
| Backup superblocks | Verification supported for current layouts | Expected backup-bearing groups are systematically located, checksummed, and compared with primary geometry/features; no replacement or primary recovery is performed |
| External xattr blocks | Read-only verified for supported local-value format | Entry ordering/bounds, local value overlap, Linux/e2fsprogs entry hashes, nonzero block hash, checksum, and observed cross-inode reference count are checked. ea_inode remains rejected |
| 64bit | Read-only supported | High block-address fields and descriptor sizes are parsed |
| metadata_csum | Partially supported | Superblock, group descriptor, allocation bitmap, inode, directory/HTree, external extent and external xattr block checksums validated; journal transaction checksums and MMP remain incomplete |
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
inode/extent structure, extent-to-protected-metadata overlap, xattr semantic or
refcount failures, directory/reference errors, dirty journal state, and storage
media errors block bitmap repair. A checksum-failed filesystem may
still be scanned read-only to collect independent diagnostics.

The differential runner is `scripts/differential_ext4.py`. It currently creates
six base corruption cases, two real extent-to-metadata overlap cases, and
eleven real external-xattr cases, including a kernel-created shared xattr block when loop-mount tooling is available. The
JSON captures parsed Nexfsck counters, repair trust reasons, source/binary
identity, both commands and exit statuses, and e2fsck diagnostics. A checked-in
expected-results manifest and CI gate require Nexfsck to retain required
detections and counters. Remaining extent-metadata-overlap, broad directory,
and JBD2 transaction cases are not yet in the machine-run differential suite;
this is not complete differential parity. Observed e2fsck 1.46.5 differences
include ordinary `-fn` exiting zero for a corrupted backup checksum and for a
nonzero xattr semantic block hash that Nexfsck rejects.
