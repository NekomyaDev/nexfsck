# Benchmark methodology

No benchmark number in this repository should be treated as a published performance or correctness result unless its raw artifacts are committed.

## Required record

Every comparison must record:

- commit hash and release profile;
- CPU model, logical/physical core count, RAM, storage model, and GPU model;
- kernel, Rust, `e2fsprogs`, and `nexfsck` versions;
- exact filesystem creation and population commands, feature flags, seed, and resulting image checksum;
- exact checker commands and flags;
- cache state and whether cache dropping or device flushing was performed;
- Rayon thread count and the I/O backend reported at runtime;
- warm-up policy, at least 10 measured repetitions, individual samples, median, p95, standard deviation, and peak RSS.

Use at least three fixture sizes. The size labels must be accompanied by exact image sizes and object counts; “large” must not imply production-scale testing. Sparse files must be identified as sparse because their logical size does not represent physical I/O volume.

## Fairness and scope

The tools must inspect equivalent filesystem features and use comparable read-only/repair modes. `e2fsck` is a mature checker with broader recovery coverage; differences in validation scope must be listed next to timing data. Avoid labels such as “legacy” or claims about architectural bottlenecks unless profiles from the measured runs support them.

An available GPU does not count as GPU execution. A result may name a GPU only after a compute backend reports dispatches and the raw run captures that fact.

## Correctness levels

Report these separately:

1. aggregate parity: selected inode/block counters match;
2. allocation-set parity: complete allocated inode and block sets match;
3. diagnostic parity: the same corruptions are identified;
4. repair parity: independently repaired copies have equivalent filesystem state and both pass a subsequent authoritative check;
5. byte identity: image hashes match, where deterministic layout makes this a meaningful expectation.

“Bit-exact” is reserved for level 5 and must include the compared hashes. Existing stress scripts currently check only selected counters and clean exit status, so they cannot establish bit-exact parity.

## Block-map memory profiling

Run both a locality-friendly and a sparse case and retain stdout plus `/usr/bin/time -v` output:

```bash
/usr/bin/time -v cargo run --release -p nexfsck-compute --example bitmap_profile -- 1000000 1
/usr/bin/time -v cargo run --release -p nexfsck-compute --example bitmap_profile -- 1000000 4294967296
```

The example reports actual RSS delta and chunk count. It measures address-map behavior on the executing machine; it does not establish exabyte-scale operational support.

## Measured 10 GiB comparisons (2026-10-01)

These are two storage-specific measurements of one fixture, not a scaling study.
Both use a sparse 10 GiB ext4 image, 4 KiB blocks, 80 block groups, 100,000
generated files, 2,000 requested symlinks, and 1,000 requested hardlinks.
The image was populated and checked through a loop mount. Checker comparisons
used ten interleaved runs with no global cache drop; all reported times are
external process wall time. First-run outliers remain in the samples and p95.

Both full raw JSON files retain every sample, backend timings, phase profile,
environment, 30-pass endurance data, and repair/rollback assertions:
[`tmpfs result`](../benchmark-results/latest.json) and
[`HDD regular-image result`](../benchmark-results/physical_hdd/latest.json).
They are authoritative for the exact medians, p95, standard deviations, and
backend matrix for these runs.
The release executable was rebuilt by the runner immediately before the runs;
each record names the exact source revision and binary SHA-256.
Resolve the later artifact-publication commit with the command stored in each
JSON artifact; a commit cannot contain its own hash.

The fixture reports 112,308 e2fsck “inodes used” versus 112,301 Nexfsck active
inodes (delta -7); those counters have not been shown to have identical
semantics, so this is not inode parity. Allocated block totals match, but that
is only aggregate parity—not allocated-set, diagnostic, or repair parity. The
main fixture has no external xattrs, and these measurements do not validate the
unimplemented inode-heavy, extent-heavy, directory-heavy, or xattr-heavy
workload profiles. The HDD result is a regular-file loop image, not a raw-device
test; no cold-cache comparison was performed. These timings do not establish
e2fsck-equivalent integrity coverage or production-scale performance.
