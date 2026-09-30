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
