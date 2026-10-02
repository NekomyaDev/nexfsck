#!/usr/bin/env python3
"""Reproducible ext4 inode-, extent-, directory-, and xattr-heavy profiles."""

import hashlib
import json
import math
import os
import pathlib
import re
import shutil
import statistics
import subprocess
import tempfile
import time


ROOT = pathlib.Path(__file__).resolve().parents[1]
NEX = pathlib.Path(os.environ.get("NEXFSCK_BIN", ROOT / "target/release/nexfsck"))
OUT = pathlib.Path(os.environ.get(
    "NEXFSCK_WORKLOAD_RESULTS", "/tmp/nexfsck-workload-profiles.json"
))
RUNS = int(os.environ.get("NEXFSCK_PROFILE_RUNS", "10"))


def sha256_file(path):
    digest = hashlib.sha256()
    with pathlib.Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_json(stdout):
    decoder = json.JSONDecoder()
    for match in re.finditer(r"\{", stdout):
        try:
            value, _ = decoder.raw_decode(stdout[match.start():])
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and "errors_detected" in value:
            return value
    raise RuntimeError("nexfsck did not emit structured JSON")


def timed(command):
    started = time.perf_counter()
    result = subprocess.run(
        ["/usr/bin/time", "-f", "PROFILE_RSS_KIB=%M", *map(str, command)],
        capture_output=True,
        text=True,
    )
    elapsed = time.perf_counter() - started
    match = re.search(r"PROFILE_RSS_KIB=(\d+)", result.stderr)
    return result, elapsed, int(match.group(1)) if match else None


def percentile95(values):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(0.95 * len(ordered)) - 1)]


def measure(image, profile_name, fixture_metrics):
    samples = {"e2fsck_seconds": [], "nexfsck_seconds": []}
    rss_kib = []
    telemetry = None
    commands = {
        "e2fsck": ["e2fsck", "-f", "-n", str(image)],
        "nexfsck": [str(NEX), "--json", "-n", str(image)],
    }
    for _ in range(RUNS):
        e2, elapsed, _ = timed(commands["e2fsck"])
        if e2.returncode != 0:
            raise RuntimeError(f"e2fsck rejected clean {profile_name} fixture: {e2.stderr}")
        samples["e2fsck_seconds"].append(elapsed)
        nex, elapsed, rss = timed(commands["nexfsck"])
        if nex.returncode != 0:
            raise RuntimeError(f"nexfsck rejected clean {profile_name} fixture: {nex.stdout}{nex.stderr}")
        telemetry = parse_json(nex.stdout)
        samples["nexfsck_seconds"].append(elapsed)
        if rss is not None:
            rss_kib.append(rss)

    nex_median = statistics.median(samples["nexfsck_seconds"])
    active = telemetry.get("active_inodes", 0)
    entries = telemetry.get("directory_entries", 0)
    extents = telemetry.get("extent_intervals", 0)
    groups = telemetry.get("total_block_groups", 0)
    return {
        "profile": profile_name,
        "runs": RUNS,
        "cache_policy": "interleaved warm-cache; no global cache drop",
        "commands": commands,
        "samples": samples,
        "e2fsck_median_seconds": statistics.median(samples["e2fsck_seconds"]),
        "e2fsck_p95_seconds": percentile95(samples["e2fsck_seconds"]),
        "e2fsck_population_stddev_seconds": statistics.pstdev(samples["e2fsck_seconds"]),
        "nexfsck_median_seconds": nex_median,
        "nexfsck_p95_seconds": percentile95(samples["nexfsck_seconds"]),
        "nexfsck_population_stddev_seconds": statistics.pstdev(samples["nexfsck_seconds"]),
        "nexfsck_max_rss_kib": max(rss_kib) if rss_kib else None,
        "active_inodes": active,
        "inode_checksum_validations": telemetry.get("inode_checksum_validations"),
        "directory_entries": entries,
        "directory_blocks": telemetry.get("directory_blocks_checked"),
        "htree_nodes": fixture_metrics.get("htree_nodes"),
        "directory_checksum_validations": telemetry.get("directory_checksum_validations"),
        "directory_checksum_failures": telemetry.get("directory_checksum_failures"),
        "extent_intervals": extents,
        "external_extent_nodes": fixture_metrics.get("external_extent_nodes"),
        "max_extent_tree_depth": fixture_metrics.get("max_extent_tree_depth"),
        "metadata_ownership_checks": extents,
        "xattr_external_blocks": fixture_metrics.get("xattr_external_blocks"),
        "xattr_shared_blocks": fixture_metrics.get("xattr_shared_blocks"),
        "xattr_entries_checked": telemetry.get("xattr_entries_checked"),
        "xattr_entry_hash_validations": telemetry.get("xattr_entries_checked"),
        "xattr_block_hash_validations": telemetry.get("xattr_blocks_checked"),
        "xattr_hash_failures": telemetry.get("xattr_hash_failures"),
        "xattr_refcount_failures": telemetry.get("xattr_refcount_failures"),
        "block_groups": groups,
        "nexfsck_ns_per_active_inode": nex_median * 1e9 / max(active, 1),
        "nexfsck_ns_per_directory_entry": nex_median * 1e9 / max(entries, 1),
        "nexfsck_ns_per_extent": nex_median * 1e9 / max(extents, 1),
        "nexfsck_ns_per_block_group": nex_median * 1e9 / max(groups, 1),
        "validation_counters": telemetry,
        "fixture_sha256": sha256_file(image),
        "fixture_bytes": image.stat().st_size,
        **fixture_metrics,
    }


def make_image(path, size, inode_count=None):
    subprocess.run(["truncate", "-s", size, str(path)], check=True)
    command = ["mkfs.ext4", "-q", "-F", "-b", "4096", "-O", "metadata_csum,dir_index"]
    if inode_count:
        command += ["-N", str(inode_count)]
    subprocess.run(command + [str(path)], check=True)


def mount_image(image, mount):
    mount.mkdir(parents=True, exist_ok=True)
    subprocess.run(["sudo", "-n", "mount", "-o", "loop", str(image), str(mount)], check=True)
    subprocess.run(["sudo", "-n", "chmod", "0777", str(mount)], check=True)


def unmount_image(mount):
    subprocess.run(["sync"], check=True)
    subprocess.run(["sudo", "-n", "umount", str(mount)], check=True)


def debugfs_text(image, command):
    result = subprocess.run(["debugfs", "-R", command, str(image)], capture_output=True, text=True)
    return result.stdout + result.stderr


def external_extent_metrics(image, paths):
    nodes = set()
    max_depth = 0
    for path in paths:
        text = debugfs_text(image, f"stat {path}")
        for depth, block in re.findall(r"\(ETB(\d+)\):([0-9]+)", text):
            nodes.add(int(block))
            max_depth = max(max_depth, int(depth) + 1)
    return {"external_extent_nodes": len(nodes), "max_extent_tree_depth": max_depth}


def make_profiles(work):
    profiles = []
    mount = pathlib.Path(tempfile.mkdtemp(prefix="nexfsck-profile-mnt-", dir="/tmp"))

    image = work / "inode-heavy.ext4"
    try:
        make_image(image, "4G", 300_000)
        mount_image(image, mount)
        for directory in range(300):
            base = mount / f"d{directory:03d}"
            base.mkdir()
            for item in range(700):
                (base / f"f{item:04d}").touch()
        unmount_image(mount)
        profiles.append(measure(image, "inode-heavy: 210k empty files", {}))
    finally:
        subprocess.run(["sudo", "-n", "umount", str(mount)], check=False, capture_output=True)
        image.unlink(missing_ok=True)

    image = work / "extent-heavy.ext4"
    paths = [f"/fragmented-{index:04d}" for index in range(256)]
    try:
        make_image(image, "2G", 40_000)
        mount_image(image, mount)
        handles = [(mount / path.lstrip("/")).open("wb", buffering=0) for path in paths]
        payload = b"x" * 4096
        for _ in range(64):
            for handle in handles:
                handle.write(payload)
            for handle in handles:
                os.fsync(handle.fileno())
        for handle in handles:
            handle.close()
        unmount_image(mount)
        ext_metrics = external_extent_metrics(image, paths)
        result = measure(image, "extent-heavy: 256 files x 64 interleaved writes", ext_metrics)
        if result["extent_intervals"] < 256:
            raise RuntimeError(f"invalid extent-heavy fixture: only {result['extent_intervals']} extents")
        profiles.append(result)
    finally:
        subprocess.run(["sudo", "-n", "umount", str(mount)], check=False, capture_output=True)
        image.unlink(missing_ok=True)

    image = work / "directory-heavy.ext4"
    try:
        make_image(image, "1G", 60_000)
        mount_image(image, mount)
        directory = mount / "large-indexed-dir"
        directory.mkdir()
        for item in range(30_000):
            (directory / f"entry-{item:05d}").touch()
        unmount_image(mount)
        htree = debugfs_text(image, "htree_dump /large-indexed-dir")
        if "is not indexed" in htree.lower() or "not a directory" in htree.lower():
            raise RuntimeError(f"directory-heavy fixture is not indexed: {htree[-1000:]}")
        htree_nodes = len(re.findall(
            r"(?im)^\s*(?:Root node|Node\s+\d+\s*:|Node dump|Node entries)", htree
        ))
        if htree_nodes == 0:
            raise RuntimeError(f"debugfs did not expose indexed HTree nodes: {htree[-1000:]}")
        profiles.append(measure(image, "directory-heavy: 30k entries in indexed directory", {
            "htree_nodes": htree_nodes,
        }))
    finally:
        subprocess.run(["sudo", "-n", "umount", str(mount)], check=False, capture_output=True)
        image.unlink(missing_ok=True)

    image = work / "xattr-heavy.ext4"
    try:
        make_image(image, "512M", 30_000)
        mount_image(image, mount)
        directory = mount / "xattrs"
        directory.mkdir()
        value = b"NEXFSCK_EXTERNAL_XATTR_" + b"V" * 1024
        paths = []
        for item in range(400):
            path = directory / f"file-{item:04d}"
            path.touch()
            os.setxattr(path, "user.nexfsck.profile", value)
            paths.append(path)
        unmount_image(mount)
        block_refs = {}
        for item in range(len(paths)):
            stat = debugfs_text(image, f"stat /xattrs/file-{item:04d}")
            match = re.search(r"File ACL:\s*(\d+)", stat)
            if match and int(match.group(1)) != 0:
                block = int(match.group(1))
                block_refs[block] = block_refs.get(block, 0) + 1
        if not block_refs:
            raise RuntimeError("invalid xattr-heavy profile: generated fixture has no external xattr blocks")
        profiles.append(measure(image, "xattr-heavy: 400 files with external xattr blocks", {
            "xattr_external_blocks": len(block_refs),
            "xattr_shared_blocks": sum(1 for refs in block_refs.values() if refs > 1),
            "xattr_max_shared_refcount": max(block_refs.values()),
            "xattr_fixture_file_references": len(paths),
        }))
    finally:
        subprocess.run(["sudo", "-n", "umount", str(mount)], check=False, capture_output=True)
        image.unlink(missing_ok=True)
        shutil.rmtree(mount, ignore_errors=True)
    return profiles


def main():
    if RUNS < 10:
        raise RuntimeError("final workload profiles require at least 10 repetitions")
    if not NEX.is_file():
        raise FileNotFoundError(f"release binary not found: {NEX}")
    source_commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    binary_hash = sha256_file(NEX)
    storage = os.environ.get("NEXFSCK_FIXTURE_STORAGE", "/tmp tmpfs")
    with tempfile.TemporaryDirectory(prefix="nexfsck-workloads-", dir="/tmp") as temporary:
        profiles = make_profiles(pathlib.Path(temporary))
    report = {
        "schema_version": 3,
        "benchmarked_source_commit": source_commit,
        "benchmarked_worktree_clean": not bool(subprocess.check_output(
            ["git", "status", "--porcelain", "--untracked-files=no", "--", ".", ":(exclude)benchmark-results/**"],
            cwd=ROOT, text=True,
        ).strip()),
        "benchmarked_binary_sha256": binary_hash,
        "fixture_storage": storage,
        "tool_versions": {
            "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
            "e2fsck": subprocess.run(["e2fsck", "-V"], capture_output=True, text=True).stderr.splitlines()[0],
            "mkfs.ext4": subprocess.run(["mkfs.ext4", "-V"], capture_output=True, text=True).stderr.splitlines()[0],
        },
        "profiles": profiles,
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
