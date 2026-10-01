#!/usr/bin/env python3
"""Reproducible inode-heavy and extent-heavy tmpfs benchmark fixtures."""

import json
import os
import shutil
import statistics
import subprocess
import time

ROOT = "/home/pop-os/nexfsck"
NEX = f"{ROOT}/target/release/nexfsck"
OUT = f"{ROOT}/benchmark-results/workload-profiles.json"
MOUNT = "/tmp/nexfsck_profile_mnt"
RUNS = 10

def timed(command):
    started = time.perf_counter()
    result = subprocess.run(command, capture_output=True, text=True)
    return result, time.perf_counter() - started

def measure(image, profile_name):
    samples = {"e2fsck": [], "nexfsck": []}
    telemetry = None
    for _ in range(RUNS):
        e2, elapsed = timed(["e2fsck", "-f", "-n", image])
        assert e2.returncode == 0
        samples["e2fsck"].append(elapsed)
        nex, elapsed = timed([NEX, "--json", "-n", image])
        assert nex.returncode == 0
        samples["nexfsck"].append(elapsed)
        telemetry = json.loads(nex.stdout)
    active = telemetry["active_inodes"]
    entries = telemetry["directory_entries"]
    extents = telemetry["extent_intervals"]
    groups = telemetry["total_block_groups"]
    nex_median = statistics.median(samples["nexfsck"])
    return {
        "profile": profile_name,
        "runs": RUNS,
        "cache_policy": "interleaved warm-cache; no global cache drop",
        "samples": samples,
        "e2fsck_median_seconds": statistics.median(samples["e2fsck"]),
        "nexfsck_median_seconds": nex_median,
        "active_inodes": active,
        "directory_entries": entries,
        "extent_intervals": extents,
        "block_groups": groups,
        "nexfsck_ns_per_active_inode": nex_median * 1e9 / max(active, 1),
        "nexfsck_ns_per_directory_entry": nex_median * 1e9 / max(entries, 1),
        "nexfsck_ns_per_extent": nex_median * 1e9 / max(extents, 1),
        "nexfsck_ns_per_block_group": nex_median * 1e9 / max(groups, 1),
    }

def make_image(path, size, inode_count=None):
    subprocess.check_call(["truncate", "-s", size, path])
    command = ["mkfs.ext4", "-q", "-F", "-b", "4096"]
    if inode_count:
        command += ["-N", str(inode_count)]
    subprocess.check_call(command + [path])
    os.makedirs(MOUNT, exist_ok=True)
    subprocess.check_call(["sudo", "mount", "-o", "loop", path, MOUNT])
    subprocess.check_call(["sudo", "chmod", "777", MOUNT])

def unmount():
    subprocess.check_call(["sync"])
    subprocess.check_call(["sudo", "umount", MOUNT])

def main():
    benchmarked_source_commit = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
    ).strip()
    results = []
    image = "/tmp/nexfsck_inode_heavy.img"
    try:
        make_image(image, "4G", 300_000)
        for directory in range(300):
            base = f"{MOUNT}/d{directory:03d}"
            os.mkdir(base)
            for item in range(700):
                open(f"{base}/f{item:04d}", "wb").close()
        unmount()
        results.append(measure(image, "inode-heavy: 210k empty files"))
    finally:
        subprocess.run(["sudo", "umount", MOUNT], stderr=subprocess.DEVNULL)
        if os.path.exists(image): os.remove(image)

    image = "/tmp/nexfsck_extent_heavy.img"
    try:
        make_image(image, "4G", 32_000)
        paths = [f"{MOUNT}/fragmented-{index:04d}" for index in range(2_000)]
        handles = [open(path, "wb", buffering=0) for path in paths]
        payload = b"x" * 4096
        for _ in range(32):
            for handle in handles:
                handle.write(payload)
            for handle in handles:
                os.fsync(handle.fileno())
        for handle in handles: handle.close()
        unmount()
        results.append(measure(image, "extent-heavy: 2k files x 32 interleaved extents"))
    finally:
        subprocess.run(["sudo", "umount", MOUNT], stderr=subprocess.DEVNULL)
        if os.path.exists(image): os.remove(image)
        shutil.rmtree(MOUNT, ignore_errors=True)

    with open(OUT, "w") as output:
        json.dump(
            {
                "schema_version": 2,
                "benchmarked_source_commit": benchmarked_source_commit,
                "artifact_publication_commit": None,
                "artifact_publication_commit_resolution": (
                    "git log -1 --format=%H -- benchmark-results/workload-profiles.json"
                ),
                "profiles": results,
            },
            output,
            indent=2,
        )
        output.write("\n")
    print(json.dumps(results, indent=2))

if __name__ == "__main__":
    main()
