# Benchmark artifacts

`latest.json` is the machine-readable result for the latest run, including every comparison/backend sample, profile stages, endurance samples, and repair/rollback exit codes. The adjacent stdout/stderr files are the final comparison-run logs. `profile.stderr.log` is the raw developer profile.

Schema 2 records `provenance.benchmarked_source_commit` and whether that source
tree was clean when the run began. If it was dirty, the artifact also records a
SHA-256 of the binary Git diff. A Git commit cannot embed its own final hash, so
`artifact_publication_commit` remains `null`; resolve the commit that published
the artifact with:

```sh
git log -1 --format=%H -- benchmark-results/latest.json
```

Benchmark claims refer to `benchmarked_source_commit`, not the later artifact
publication commit.

`workload-profiles.json` contains the inode-heavy and extent-heavy raw samples and normalized rates. `thread-scaling.json` records the reverted forced-parallel experiment, while `bitmap-representations.json` records the dense-word versus Roaring comparison.

The current fixture is a sparse 10 GiB ext4 image created on `/tmp` tmpfs in a live-USB environment. Measurements are 10 interleaved warm-cache repetitions without a global cache drop. They characterize this run only; they do not represent physical NVMe/HDD throughput or bit-exact correctness parity.
