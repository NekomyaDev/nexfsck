#!/usr/bin/env python3
"""Run a small deterministic checksum-corruption differential suite.

The clean 512 MiB image is generated once and never modified. Each mutation is
applied to a fresh copy. This is an initial suite, not a claim of complete
e2fsck parity; unsupported corruption classes are listed in the JSON report.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MISSING_CASES = [
    "inode_structure", "extent_root", "extent_internal_node", "extent_leaf",
    "directory_dirent", "directory_checksum", "htree", "external_xattr",
    "mmp", "jbd2_descriptor", "jbd2_commit", "jbd2_revoke",
]


def run(command):
    return subprocess.run(command, capture_output=True, text=True, check=False)


def flip(path, offset):
    with path.open("r+b") as image:
        image.seek(offset)
        original = image.read(1)
        if not original:
            raise ValueError(f"offset {offset} is beyond {path}")
        image.seek(offset)
        image.write(bytes([original[0] ^ 1]))
        image.flush()
        os.fsync(image.fileno())


def parse_dumpe2fs(image):
    header = run(["dumpe2fs", "-h", str(image)])
    if header.returncode not in (0, 1):
        raise RuntimeError(header.stderr)
    values = {}
    for line in header.stdout.splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            values[key.strip()] = value.strip()
    groups = run(["dumpe2fs", "-g", str(image)])
    if groups.returncode not in (0, 1):
        raise RuntimeError(groups.stderr)
    group0 = next(line for line in groups.stdout.splitlines() if line.startswith("0:"))
    fields = group0.split(":")
    block_size = int(values["Block size"])
    desc_table_block = 2 if block_size == 1024 else 1
    desc_size = int(values.get("Group descriptor size", "32"))
    return values, block_size, desc_table_block * block_size, desc_size, int(fields[4]), int(fields[5])


def inode_checksum_offset(image):
    result = run(["debugfs", "-R", "imap <2>", str(image)])
    text = result.stdout + result.stderr
    match = re.search(r"located at block\s+(\d+),\s*offset\s+0x([0-9a-fA-F]+)", text)
    if not match:
        raise RuntimeError(f"cannot locate root inode: {text}")
    block, within = (int(match.group(1)), int(match.group(2), 16))
    _, block_size, _, _, _, _ = parse_dumpe2fs(image)
    return block * block_size + within + 124


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--nexfsck", default=str(ROOT / "target/release/nexfsck"))
    parser.add_argument("--output", default=str(ROOT / "benchmark-results/differential/latest.json"))
    parser.add_argument("--force", action="store_true", help="replace the requested JSON output")
    args = parser.parse_args()
    output_path = Path(args.output)
    if output_path.exists() and not args.force:
        raise FileExistsError(f"refusing to overwrite {output_path}; pass --force explicitly")

    binary = Path(args.nexfsck).resolve()
    if not binary.is_file():
        raise FileNotFoundError(f"build nexfsck first; binary not found: {binary}")
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    source_commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    status = subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True)
    source_diff = subprocess.check_output(["git", "diff", "--binary", "HEAD"], cwd=ROOT)
    with tempfile.TemporaryDirectory(prefix="nexfsck-differential-") as temp:
        work = Path(temp)
        clean = work / "clean.ext4"
        subprocess.check_call(["truncate", "-s", "512M", str(clean)])
        subprocess.check_call(["mkfs.ext4", "-q", "-F", "-O", "metadata_csum", str(clean)])
        features, block_size, gdt_offset, desc_size, block_bitmap, inode_bitmap = parse_dumpe2fs(clean)
        backup_group = 1
        backup_offset = backup_group * 32768 * block_size + 1020
        cases = [
            ("primary_superblock_checksum", 1024 + 1020),
            ("backup_superblock_checksum", backup_offset),
            ("group_descriptor_checksum", gdt_offset + 30),
            ("block_bitmap_payload", block_bitmap * block_size),
            ("inode_bitmap_payload", inode_bitmap * block_size),
            ("inode_checksum", inode_checksum_offset(clean)),
        ]
        records = []
        for case_id, byte_offset in cases:
            image = work / f"{case_id}.ext4"
            shutil.copyfile(clean, image)
            flip(image, byte_offset)
            nex_command = [str(binary), "--json", "-n", str(image)]
            e2_command = ["e2fsck", "-f", "-n", str(image)]
            nex = run(nex_command)
            e2 = run(e2_command)
            try:
                counters = json.loads(nex.stdout)
            except json.JSONDecodeError:
                counters = None
            nex_detected = nex.returncode != 0
            e2_text = e2.stderr + e2.stdout
            e2_diagnostic_detected = bool(re.search(
                r"checksum.*invalid|invalid.*checksum|checksum.*should be|corrupt|invalid",
                e2_text,
                re.IGNORECASE,
            ))
            e2_detected = e2.returncode != 0 or e2_diagnostic_detected
            records.append({
                "case_id": case_id,
                "feature_set": features.get("Filesystem features", "unknown"),
                "mutation": {"byte_offset": byte_offset, "operation": "xor byte with 0x01"},
                "nexfsck": {
                    "command": nex_command,
                    "exit_code": nex.returncode,
                    "structured_counters": counters,
                    "diagnostic": (nex.stderr + nex.stdout)[-4000:],
                },
                "e2fsck": {
                    "command": e2_command,
                    "exit_code": e2.returncode,
                    "diagnostic_classification": (
                        "reported_corruption" if e2_diagnostic_detected else "no_corruption_diagnostic"
                    ),
                    "diagnostic": e2_text[-4000:],
                },
                "agreement": nex_detected == e2_detected,
                "exit_code_agreement": nex.returncode == e2.returncode,
                "disagreement_class": None if nex_detected == e2_detected else (
                    "likely_e2fsck_scope_difference" if case_id == "backup_superblock_checksum"
                    else "needs_manual_triage"
                ),
                "expected": "nexfsck detects corruption; e2fsck result is recorded as oracle data",
            })

        report = {
            "schema_version": 1,
            "benchmarked_source_commit": source_commit,
            "benchmarked_worktree_clean": not bool(status.strip()),
            "benchmarked_worktree_diff_sha256": hashlib.sha256(source_diff).hexdigest() if source_diff else None,
            "benchmarked_binary_sha256": binary_hash,
            "fixture": {"size": "512 MiB", "block_size": block_size, "features": features.get("Filesystem features", "unknown")},
            "clean_fixture_preserved": True,
            "cases": records,
            "not_yet_covered": MISSING_CASES,
            "summary": {
                "total": len(records),
                "agreement": sum(case["agreement"] for case in records),
                "disagreement": sum(not case["agreement"] for case in records),
            },
        }
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report["summary"], indent=2))


if __name__ == "__main__":
    main()
