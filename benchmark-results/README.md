# Benchmark artifacts

`latest.json` is the machine-readable result for the latest committed run. The adjacent stdout/stderr files are the final comparison-run logs, and `stress-run.log` contains the complete fixture, comparison, endurance, and repair/rollback transcript.

The current fixture is a sparse 10 GiB ext4 image created on `/tmp` tmpfs in a live-USB environment. Measurements are 10 interleaved warm-cache repetitions without a global cache drop. They characterize this run only; they do not represent physical NVMe/HDD throughput or bit-exact correctness parity.
