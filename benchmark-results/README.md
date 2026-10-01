# Benchmark artifacts

`latest.json` is the machine-readable result for the latest run, including every comparison/backend sample, profile stages, endurance samples, and repair/rollback exit codes. The adjacent stdout/stderr files are the final comparison-run logs. `profile.stderr.log` is the raw developer profile.

`workload-profiles.json` contains the inode-heavy and extent-heavy raw samples and normalized rates. `thread-scaling.json` records the reverted forced-parallel experiment, while `bitmap-representations.json` records the dense-word versus Roaring comparison.

The current fixture is a sparse 10 GiB ext4 image created on `/tmp` tmpfs in a live-USB environment. Measurements are 10 interleaved warm-cache repetitions without a global cache drop. They characterize this run only; they do not represent physical NVMe/HDD throughput or bit-exact correctness parity.
