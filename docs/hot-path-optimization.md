# Hot-path optimization report (2026-10-01)

This continues the fixed-cost investigation in `performance-profile.md`. All
headline comparisons include complete process wall time. The primary fixture is
the same sparse 10 GiB ext4 image on tmpfs with 80 groups and approximately
100,000 generated files.

## Before profile

The representative pre-change profile was:

| Stage | Time |
| --- | ---: |
| inode table scan and extent parsing | 45.56 ms |
| bitmap reconciliation | 31.91 ms |
| directory pass | 25.88 ms |
| extent collision sort/check | 2.03 ms |
| journal inspection | 2.37 ms |

## Allocation and synchronization findings

- `BlockAllocationTracker` acquired one global `RwLock<HashMap<..., RoaringBitmap>>`
  write lock for every extent range. Each block was then inserted individually.
- Bitmap reconciliation performed about 2.6 million Roaring membership lookups.
- Directory parsing allocated a `String` for every entry and accumulated results
  in `HashMap<u32, u16>` and `HashSet<u32>` despite the bounded inode namespace.
- HTree leaf validation called the allocating directory parser a second time.
- Inode batches are group-sized (8,192 here). Forced Rayon execution serialized
  tracker writes and cost more than the ~2 ms validation work available to split.

## Implemented changes

1. Filesystems whose allocation bitset fits within 64 MiB use a contiguous
   `Vec<u64>`. Larger sparse address spaces retain chunked Roaring storage and
   full 64-bit addressing.
2. Dense range insertion sets word masks under one lock and uses `count_ones` for
   exact new/collision accounting.
3. Dense bitmap reconciliation compares reconstructed and on-disk data with
   word-at-a-time AND/NOT operations. The sparse Roaring fallback is unchanged.
4. Randomized equivalence tests compare word reconciliation with the original
   scalar per-block semantics, including unaligned group starts and tail masks.
5. The checker directory parser validates borrowed name bytes and stores only
   inode number plus dot/dotdot state. The public name-returning compatibility
   parser remains available.
6. Link counts and reachability now use inode-indexed contiguous arrays.
7. HTree validation borrows cached blocks and uses the compact leaf validator.
8. `--profile` now reports inode reads/decoding/collection/tracking, directory
   reads/cache/HTree/dirents, and bitmap reads/block/inode comparisons separately.

## Detailed after profile

One representative run measured 40.01 ms internally:

| Stage/substage | Time |
| --- | ---: |
| inode/extent total | 22.03 ms |
| ├ inode-table reads | 6.31 ms |
| ├ inode decoding | 8.88 ms |
| ├ directory/extent metadata collection | 4.67 ms |
| └ inode validation + allocation tracking | 1.96 ms |
| directory total | 10.27 ms |
| ├ directory block reads | 6.39 ms |
| ├ HTree validation | 1.62 ms |
| ├ dirent parsing + reference arrays | 2.08 ms |
| └ cache construction | 0.09 ms |
| bitmap total | 0.83 ms |
| ├ block bitmap word comparison | 0.12 ms |
| ├ inode bitmap comparison | 0.59 ms |
| └ bitmap reads | 0.11 ms |

## Bitmap representation benchmark

For 2,621,440 blocks across 80 groups:

| Representation | Total | Per group |
| --- | ---: | ---: |
| dense 64-bit words | 0.165 ms | 2,068 ns |
| chunked Roaring membership | 48.397 ms | 604,963 ns |

AVX2 was not retained. The scalar word loop is only about 0.12 ms in the real
profile, so SIMD cannot provide a meaningful end-to-end gain here. Adding a
second implementation and runtime dispatch was not justified by measurement.

## Thread scaling and negative experiments

Forced Rayon on every inode group produced these full-process medians:

| Threads | Median |
| ---: | ---: |
| 1 | 44.66 ms |
| 2 | 52.60 ms |
| 4 | 53.27 ms |
| 8 | 58.81 ms |
| 16 | 62.97 ms |

The forced-parallel change was reverted. The tracker lock is no longer the
dominant serial cost, but group validation contains too little work to amortize
Rayon scheduling and lock ownership transfer. Large batches still retain the
existing parallel path. A borrowed-only HTree change showed no gain until the
allocating leaf parser was also replaced; only the combined winning change was
kept.

## Final 10 GiB result

Ten interleaved warm-cache runs measured:

- e2fsck median: 0.130 s
- nexfsck median: 0.053 s in the direct comparison
- adaptive backend-matrix median: 0.048 s
- sync CPU median: 0.0476 s
- 30/30 endurance passes; observed RSS 37.62–37.87 MiB
- allocated-block count matched at 1,182,223; nexfsck errors: zero

The previous adaptive median was 0.119 s with roughly 37 MiB RSS. The dense
bitset increases RSS by less than 1 MiB on this fixture and reduces wall time by
roughly 60%. These results remain tmpfs/warm-cache specific.

## Additional workloads

These are historical results from benchmark source
`dbc61335a6331e433e4de76ed473c0d62e96692e`, before later inode/external extent
checksum and xattr-integrity changes. They have not been rerun against the
current integrity-enabled source and are not current results. The historical
`workload_profiles.py` runner measured:

| Fixture | e2fsck | nexfsck | Normalized Nexfsck rate |
| --- | ---: | ---: | ---: |
| inode-heavy, 210,304 active inodes | 161.96 ms | 52.81 ms | 251 ns/active inode |
| extent-heavy, 34,004 intervals | 14.69 ms | 7.52 ms | 221 ns/extent |

Raw samples and directory/group normalizations are in
`benchmark-results/workload-profiles.json`.

## Physical storage

The current integrity build has also been measured on a sparse regular image
stored on a mounted ext4 HDD; the image was loop-mounted only for population and
was removed after the run. This is not a raw-device test and used no cold-cache
protocol. The exact result is in
`benchmark-results/physical_hdd/latest.json`. No raw-device repair or
destructive test was performed, and no general physical-storage or io_uring
crossover claim is made.
