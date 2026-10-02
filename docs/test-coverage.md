# Test coverage inventory

This inventory reflects the automated tests currently in the repository. It is deliberately a behavior map, not a claim that all ext4 edge cases are covered.

| Behavior | Test |
| --- | --- |
| Clean ext4 image verification | `test_clean_ext4_image_verification` |
| Invalid primary superblock magic | `test_corrupted_magic_detection` |
| Undo pre-image restoration | `test_atomic_rollback_and_restore` |
| JSON summary output | `test_json_telemetry_output` |
| False-free block detection and repair | `test_active_repair_false_free_block` |
| False-free inode detection and repair | `test_active_repair_false_free_inode` |
| Corrupt directory entry detection | `test_corrupt_directory_entry_detection` |
| Superblock POD size | `test_superblock_size` |
| Group descriptor POD size | `test_group_desc_size` |
| Extent header POD size | `test_extent_header_size` |
| 48-bit extent address composition | `test_extent_address_calculation` |
| Unwritten extent flag handling | `test_extent_unwritten` |
| CRC32c known vector and hardware/scalar equivalence | `crc32c_known_vector`, `dispatched_crc_matches_scalar_for_lengths_and_seeds` |
| Inode CRC32c layout/seed semantics and real-image corruption oracle | `inode_checksum_matches_independent_reference_across_layouts_and_seeds`, `inode_checksum_feature_and_unused_inode_semantics_match_e2fsprogs`, `test_real_ext4_inode_checksum_clean_layouts_and_seed_modes`, `test_real_ext4_inode_checksum_corruption_matches_e2fsprogs` |
| Superblock, group descriptor, block/inode bitmap checksum corruption oracles | `test_metadata_checksum_corruption_oracle` |
| Directory leaf and indexed HTree checksum corruption oracles | `test_directory_checksum_corruption_oracle`, `test_htree_checksum_corruption_oracle` |
| External extent-node checksum clean/corruption oracle | `test_external_extent_block_checksum_corruption_oracle` |
| External xattr clean/header/bounds/value/checksum/payload corruption oracle | `test_external_xattr_block_integrity_oracle` |
| No checksum-driven repair when metadata integrity is uncertain | `test_external_extent_block_checksum_corruption_oracle` |
| Unsupported feature fail-closed behavior | `test_unsupported_feature_fails_closed` |
| Real MMP feature is refused before verification without asserting owner safety | `test_mmp_filesystem_is_rejected_before_verification` |
| Expected backup superblocks are checked and checksum corruption blocks clean/repair | `test_backup_superblocks_are_checked_and_checksum_corruption_blocks_clean` |
| Orphan-file semantics and unknown incompat/RO-compat bits fail closed with valid superblock checksum | `test_unknown_incompat_and_ro_compat_bits_fail_closed` |
| H-Tree root/leaf validation and child bounds | `validates_minimal_htree_root_and_leaf`, `rejects_out_of_bounds_htree_child` |
| CUDA/CPU collision equivalence; CUDA required with env flag | `collision_detection_is_deterministic` |
| JBD2 synthetic block-stream planning and v3 metadata checksums | `plans_only_committed_jbd2_data`, `ignores_uncommitted_tail`, `revoke_removes_committed_write`, `validates_checksum_v3_and_64bit_target`, `verifies_v3_descriptor_commit_and_revoke_checksums` (not wired into on-disk filesystem inspection) |
| JBD2 unknown incompat and unvalidated compat-checksum policy | `journal_feature_policy_rejects_unknown_and_unvalidated_checksum_modes` |
| Interrupted JBD2 replay and durable pre-image rollback | `interrupted_replay_can_be_rolled_back_from_synced_preimages` |
| Heatmap/rates and Prometheus format | `heatmap_and_rates_are_reported`, `prometheus_contains_all_counters` |
| 64-bit chunk boundaries and high-block collision tracking | `tracks_ranges_across_the_32_bit_chunk_boundary`, `detects_collision_above_four_billion_blocks` |

The CI matrix covers Ubuntu 22.04/24.04 and Rust 1.85/stable. A reproducible bitmap RSS profiler is available as the `bitmap_profile` example.

Important uncovered cases include real power-loss injection (the automated replay test uses a deterministic interruption), CLI application of real-world JBD2 logs, registered-buffer integration across a broader kernel fleet, media-error hardware injection, and multi-device/feature combinations.
