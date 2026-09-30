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
| H-Tree root/leaf validation and child bounds | `validates_minimal_htree_root_and_leaf`, `rejects_out_of_bounds_htree_child` |
| CUDA/CPU collision equivalence; CUDA required with env flag | `collision_detection_is_deterministic` |
| JBD2 committed, incomplete, and revoked transactions | `plans_only_committed_jbd2_data`, `ignores_uncommitted_tail`, `revoke_removes_committed_write` |
| Heatmap/rates and Prometheus format | `heatmap_and_rates_are_reported`, `prometheus_contains_all_counters` |
| 64-bit chunk boundaries and high-block collision tracking | `tracks_ranges_across_the_32_bit_chunk_boundary`, `detects_collision_above_four_billion_blocks` |

Important uncovered cases include process-kill fault injection during repair/rollback, JBD2 checksum-v3 replay application, registered-buffer kernel integration on multiple kernel versions, very large sparse block-map memory profiling, media-error hardware injection, and multi-device/feature combinations.
