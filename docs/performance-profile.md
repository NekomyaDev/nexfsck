# 10 GiB latency investigation (2026-10-01)

This report covers the Ryzen 7 5700X / RTX 4060 live-USB environment described in
`benchmark-results/latest.json`. The image was a sparse 10 GiB ext4 file on tmpfs
with 80 groups and about 100,000 generated files. All headline values include
process startup and shutdown and use 10 interleaved warm-cache runs.

## Missing wall time

Before this change, a run reported about 0.15 seconds internally but took about
0.37 seconds externally. A control run against a nonexistent image still took
about 0.26 seconds, proving that the gap preceded filesystem scanning. `strace`
and in-process timers attributed it to eager CUDA setup and teardown:

- CUDA driver/context/PTX initialization: about 135–198 ms (first-use dependent)
- CUDA context/module teardown: about 60–70 ms
- `nvidia-smi` subprocess used only for the display VRAM value: about 10–20 ms
- the old internal timer started after hardware discovery and ended before Rust
  dropped the CUDA context, so none of those fixed costs appeared in it

`--profile` now starts immediately on entry, prints phase times to stderr, and
explicitly times CUDA destruction. The external benchmark remains authoritative.

## Backend A/B results

| Backend | Median | p95 | Population stddev |
| --- | ---: | ---: | ---: |
| io_uring + CUDA | 0.322455 s | 0.464815 s | 0.043170 s |
| io_uring + CPU | 0.123586 s | 0.132016 s | 0.003603 s |
| sync + CUDA | 0.314344 s | 0.332762 s | 0.005845 s |
| sync + CPU | 0.117062 s | 0.132360 s | 0.004868 s |
| adaptive | 0.117667 s | 0.156456 s | 0.011583 s |

For this regular tmpfs image, io_uring setup cost about 3.6 ms and did not repay
itself when batching 2,412 directory blocks. Auto mode therefore uses sync I/O
for regular image files and retains io_uring for block devices. This is not a
claim that sync I/O wins on physical block devices; that requires physical-media
cold/warm measurements.

## CUDA crossover calibration

`collision_crossover` compares identical reverse-ordered interval arrays and
verifies identical results. CUDA initialization was 197.773 ms in this run.

| Intervals | CPU total | CUDA dispatch (context warm) | CUDA first-run total |
| ---: | ---: | ---: | ---: |
| 100,000 | 0.110 ms | 1.250 ms | 199.023 ms |
| 500,000 | 0.557 ms | 6.049 ms | 203.822 ms |
| 1,000,000 | 1.374 ms | 10.686 ms | 208.459 ms |
| 2,000,000 | 5.631 ms | 19.520 ms | 217.293 ms |
| 5,000,000 | 15.842 ms | 52.603 ms | 250.376 ms |
| 10,000,000 | 28.621 ms | 127.377 ms | 325.150 ms |

No CUDA crossover exists in the measured range, even with an already initialized
context. Adaptive mode therefore selects CPU rather than inventing an unsupported
threshold. CUDA remains available with `--compute-backend cuda` for calibration.

## Adaptive profile after optimization

One representative adaptive run reported 112.14 ms internally and approximately
113 ms externally. The largest phases were:

1. inode-table scan and extent validation: 47.70 ms
2. bitmap reconciliation: 28.32 ms
3. directory pass: 26.56 ms
4. journal inspection: 2.29 ms
5. extent collision sort/check: 2.08 ms
6. reference-count pass: 2.48 ms

Small group-sized inode batches now run serially. On this workload, forcing Rayon
to 1/2/4/8/16 threads measured medians of 0.113/0.126/0.133/0.147/0.150 seconds.
The shared allocation tracker serialized extent updates, so work-stealing added
overhead. Rayon remains enabled for batches above 16,384 inodes.

## Correctness and limitations

### Inode checksum correctness cost

Commit `dbc61335a6331e433e4de76ed473c0d62e96692e` added full raw-inode
CRC32c verification to the real scan path. It covers the inode number,
generation, complete configured inode size, low/high checksum fields, UUID seed,
and explicit `metadata_csum_seed` semantics. Tests use real e2fsprogs-created
128-byte and 256-byte inode images and require both nexfsck and e2fsck to reject
metadata, generation, and stored-checksum corruption.

On the final clean-revision 10 GiB run, checksum validation consumed 6.639 ms.
The complete inode/extent phase was 30.937 ms: table reads 6.444 ms, inode
decoding 10.297 ms, metadata/extent collection 5.024 ms, and validation/allocation
tracking 2.213 ms. Directory validation was 10.730 ms, of which block reads were
6.830 ms. Bitmap reconciliation remained below 1 ms. Full external medians were
51.713 ms for nexfsck and 125.783 ms for e2fsck; nexfsck p95 was 53.557 ms and
population standard deviation was 0.847 ms. Peak RSS was 36.61 MiB.

The checksum seed is computed once per filesystem and hardware CRC dispatch is
performed once per inode checksum chain. A fused checksum/decode timer was not
retained because it would obscure the independently measured correctness cost;
the existing two linear table passes remain a documented optimization target.

Fusion was experimentally compared on a clean 128 MiB, 4 KiB-block, indexed
ext4 image with 900 generated files. Twenty interleaved warm-cache repetitions
per mode measured separate checksum/decode at 21.553 ms median (23.726 ms p95,
1.014 ms sample standard deviation), versus fused at 21.676 ms (22.851 ms p95,
0.821 ms standard deviation). The 0.123 ms median regression is within run
variance and the fused instrumentation had less reliable attribution, so the
separate-pass implementation was retained. This is a negative experiment, not
evidence that fusion can never help larger fixtures.

The preceding benchmark revision measured e2fsck at 0.127931 s and adaptive
nexfsck at 0.118554 s median (1.08x on that fixture only), with 36.86–37.49 MiB
RSS. On checksum-coverage implementation commit
`893c92771d0b5340c250be8b3ac20ab8f63458ba`, the 10 GiB rerun measured e2fsck
at 125.043 ms and nexfsck at 54.407 ms median (p95 130.033 / 56.398 ms,
population standard deviation 2.287 / 0.762 ms). The profile attributed 5.787
ms to inode CRC32c and 1.061 ms to directory checksums; additional superblock,
group descriptor and bitmap checksum validation measured below timer resolution
or below 0.02 ms in aggregate. Allocated blocks matched at 1,182,241; e2fsck's
total inode count exceeded nexfsck's active count by seven reserved inodes.
Nexfsck reported zero errors, completed all 30 endurance passes, and passed
corruption detection, repair, post-repair verification, rollback, and restored
corruption detection. RSS ranged from 36.37 to 36.62 MiB. The raw run is
published in `benchmark-results/latest.json` with the exact source revision.

This does not establish a physical-storage crossover or general superiority over
e2fsck. Larger inode-heavy and extent-heavy disk fixtures remain future benchmark
work; the live environment's 16 GiB tmpfs prevented keeping larger repair copies.
