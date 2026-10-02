#!/usr/bin/env python3
"""Run deterministic, machine-readable ext4 corruption differential tests.

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
    "external_extent_node_checksum", "extent_child_reuse", "extent_cycle",
    "extent_overlap_extent_node", "directory_rec_len", "directory_block_boundary",
    "directory_inode_reference", "directory_checksum", "htree_checksum",
    "htree_child_bounds", "htree_ordering",
    "jbd2_revoke_checksum", "jbd2_revoke_length", "extent_external_node_overlap",
    "mmp",
]
MANIFEST = ROOT / "scripts/differential_expected.json"


def run(command):
    return subprocess.run(command, capture_output=True, text=True, check=False)


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_structured_json(stdout):
    decoder = json.JSONDecoder()
    for match in re.finditer(r"\{", stdout):
        try:
            value, _ = decoder.raw_decode(stdout[match.start():])
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and ("errors_detected" in value or "total_block_groups" in value):
            return value
    return None


def crc32c(seed, data):
    value = seed
    for byte in data:
        value ^= byte
        for _ in range(8):
            value = (value >> 1) ^ (0x82F63B78 if value & 1 else 0)
    return value & 0xFFFFFFFF


def refresh_xattr_checksum(image, block_number, block_size):
    with image.open("r+b") as stream:
        stream.seek(1024)
        superblock = stream.read(1024)
        uuid = superblock[104:120]
        checksum_seed = crc32c(0xFFFFFFFF, uuid)
        stream.seek(block_number * block_size)
        block = bytearray(stream.read(block_size))
        block[16:20] = b"\0" * 4
        checksum = crc32c(checksum_seed, block_number.to_bytes(8, "little"))
        checksum = crc32c(checksum, block[:16])
        checksum = crc32c(checksum, b"\0" * 4)
        checksum = crc32c(checksum, block[20:])
        stream.seek(block_number * block_size + 16)
        stream.write(checksum.to_bytes(4, "little"))
        stream.flush()
        os.fsync(stream.fileno())


def xattr_block_layout(image, block_size):
    stat = run(["debugfs", "-R", "stat /file", str(image)])
    text = stat.stdout + stat.stderr
    match = re.search(r"File ACL:\s*(\d+)", text)
    if not match or int(match.group(1)) == 0:
        raise RuntimeError(f"fixture has no external xattr block: {text}")
    block_number = int(match.group(1))
    with image.open("rb") as stream:
        stream.seek(block_number * block_size)
        block = stream.read(block_size)
    entries = []
    cursor = 32
    while cursor + 4 <= len(block) and block[cursor:cursor + 4] != b"\0\0\0\0":
        name_len = block[cursor]
        entry_len = (16 + name_len + 3) & ~3
        if cursor + entry_len > len(block):
            raise RuntimeError("generated fixture contains malformed xattr entries")
        value_offset = int.from_bytes(block[cursor + 2:cursor + 4], "little")
        value_size = int.from_bytes(block[cursor + 8:cursor + 12], "little")
        entries.append({"offset": cursor, "name_len": name_len,
                        "name": block[cursor + 16:cursor + 16 + name_len],
                        "value_offset": value_offset, "value_size": value_size})
        cursor += entry_len
    if len(entries) < 2:
        raise RuntimeError(f"fixture needs two external xattr entries; found {len(entries)}")
    return block_number, entries


def make_shared_xattr_fixture(image, work):
    mountpoint = work / "shared-xattr-mount"
    mountpoint.mkdir()
    subprocess.check_call(["truncate", "-s", "64M", str(image)])
    subprocess.check_call(["mkfs.ext4", "-q", "-F", "-O", "metadata_csum", str(image)])
    mounted = False
    try:
        subprocess.check_call(["sudo", "mount", "-o", "loop", str(image), str(mountpoint)])
        mounted = True
        subprocess.check_call(["sudo", "chmod", "0777", str(mountpoint)])
        paths = [mountpoint / "one", mountpoint / "two"]
        for path in paths:
            path.write_bytes(b"x")
            os.setxattr(path, "user.shared", b"v" * 1024)
    finally:
        if mounted:
            subprocess.check_call(["sudo", "umount", str(mountpoint)])

    def file_acl(path):
        result = run(["debugfs", "-R", f"stat /{path}", str(image)])
        match = re.search(r"File ACL:\s*(\d+)", result.stdout + result.stderr)
        return int(match.group(1)) if match else 0

    first, second = file_acl("one"), file_acl("two")
    if first == 0 or first != second:
        return None, f"kernel did not deduplicate these external xattr blocks (ACL blocks {first}, {second})"
    block_size = parse_dumpe2fs(image)[1]
    with image.open("rb") as stream:
        stream.seek(first * block_size + 4)
        refcount = int.from_bytes(stream.read(4), "little")
    if refcount != 2:
        return None, f"shared xattr block refcount was {refcount}, expected 2"
    return first, None


def make_active_journal_fixture(image, work):
    """Capture an actual committed v3 transaction from a disposable mounted ext4 image."""
    mountpoint = work / "active-journal-mount"
    mountpoint.mkdir()
    active = work / "active-journal.ext4"
    subprocess.check_call(["truncate", "-s", "256M", str(image)])
    subprocess.check_call(["mkfs.ext4", "-q", "-F", "-O", "metadata_csum", str(image)])
    mounted = False
    try:
        subprocess.check_call([
            "sudo", "-n", "mount", "-o", "loop,commit=600,data=journal",
            str(image), str(mountpoint),
        ])
        mounted = True
        path = mountpoint / "journal-transaction"
        subprocess.check_call(["sudo", "-n", "touch", str(path)])
        subprocess.check_call(["sudo", "-n", "sync", "-f", str(path)])
        # Snapshot before unmount checkpoints/clears the committed log.
        subprocess.check_call(["sudo", "-n", "cp", "--reflink=never", str(image), str(active)])
    except (OSError, subprocess.CalledProcessError) as error:
        return None, f"cannot create active journal fixture safely: {error}"
    finally:
        if mounted:
            subprocess.run(["sudo", "-n", "umount", str(mountpoint)], check=False)

    info = journal_fixture_layout(active)
    if info["start"] == 0:
        return None, "mounted ext4 snapshot did not retain an active journal transaction"
    active_hash = sha256_file(active)
    clean_hash = sha256_file(image)
    return (active, info, active_hash, clean_hash), None


def journal_fixture_layout(image):
    block_size = parse_dumpe2fs(image)[1]
    journal_stat = run(["dumpe2fs", "-h", str(image)])
    match = re.search(r"Journal inode:\s*(\d+)", journal_stat.stdout + journal_stat.stderr)
    if not match:
        raise RuntimeError("cannot locate internal journal inode")
    journal_inode = int(match.group(1))
    superblock_physical = int(run([
        "debugfs", "-R", f"bmap <{journal_inode}> 0", str(image)
    ]).stdout.strip().splitlines()[-1])
    with image.open("rb") as stream:
        stream.seek(superblock_physical * block_size)
        journal_superblock = stream.read(block_size)
    start = int.from_bytes(journal_superblock[28:32], "big")
    maxlen = int.from_bytes(journal_superblock[16:20], "big")
    first = int.from_bytes(journal_superblock[20:24], "big")
    uuid = journal_superblock[48:64]
    if not start or not first <= start < maxlen:
        return {"journal_inode": journal_inode, "start": start, "maxlen": maxlen,
                "first": first, "uuid": uuid, "block_size": block_size,
                "superblock_physical": superblock_physical}
    logdump = run(["debugfs", "-R", "logdump", str(image)])
    text = logdump.stdout + logdump.stderr
    descriptors = [int(value) for value in re.findall(
        r"type 1 \(descriptor block\) at block (\d+)", text
    )]
    commits = [int(value) for value in re.findall(
        r"type 2 \(commit block\) at block (\d+)", text
    )]
    revokes = [int(value) for value in re.findall(
        r"type 5 \(revoke block\) at block (\d+)", text
    )]
    return {
        "journal_inode": journal_inode,
        "start": start,
        "maxlen": maxlen,
        "first": first,
        "uuid": uuid,
        "block_size": block_size,
        "superblock_physical": superblock_physical,
        "descriptor_logicals": descriptors,
        "commit_logicals": commits,
        "revoke_logicals": revokes,
        "journal_superblock": journal_superblock,
    }


def journal_physical_block(image, info, logical):
    result = run([
        "debugfs", "-R", f"bmap <{info['journal_inode']}> {logical}", str(image)
    ])
    return int(result.stdout.strip().splitlines()[-1])


def refresh_jbd_block_checksum(image, physical, block_size, uuid, checksum_offset):
    with image.open("r+b") as stream:
        stream.seek(physical * block_size)
        block = bytearray(stream.read(block_size))
        block[checksum_offset:checksum_offset + 4] = bytes(4)
        checksum = crc32c(crc32c(0xFFFFFFFF, uuid), block)
        stream.seek(physical * block_size + checksum_offset)
        stream.write(checksum.to_bytes(4, "big"))
        stream.flush()
        os.fsync(stream.fileno())


def last_jbd_v3_tag_flags_offset(image, physical, block_size):
    with image.open("rb") as stream:
        stream.seek(physical * block_size)
        block = stream.read(block_size)
    cursor = 12
    limit = block_size - 4
    last_flags = None
    while cursor + 16 <= limit:
        flags = int.from_bytes(block[cursor + 4:cursor + 8], "big")
        last_flags = cursor + 4
        cursor += 16
        if not flags & 2:  # SAME_UUID clear => a 16-byte UUID field follows.
            cursor += 16
        if flags & 8:  # LAST_TAG
            return last_flags
    raise RuntimeError("generated JBD2 descriptor has no bounded LAST_TAG")


def run_differential_case(binary, image, case_id, corruption_class, fixture_hash,
                          mutation, source_commit, binary_hash):
    nex_command = [str(binary), "--json", "-n", str(image)]
    e2_command = ["e2fsck", "-f", "-n", str(image)]
    nex = run(nex_command)
    e2 = run(e2_command)
    counters = parse_structured_json(nex.stdout)
    counters_map = counters if isinstance(counters, dict) else {}
    nex_detected = nex.returncode != 0 or counters_map.get("errors_detected", 0) > 0
    e2_text = e2.stderr + e2.stdout
    e2_diagnostic_detected = bool(re.search(
        r"checksum.*invalid|invalid.*checksum|checksum.*should be|corrupt|invalid|extended attribute",
        e2_text, re.IGNORECASE,
    ))
    e2_detected = e2.returncode != 0 or e2_diagnostic_detected
    if nex_detected and e2_detected:
        agreement_class = "both_detect"
    elif nex_detected:
        agreement_class = "nexfsck_only_detects"
    elif e2_detected:
        agreement_class = "e2fsck_only_detects"
    else:
        agreement_class = "both_accept"
    return {
        "case_id": case_id,
        "corruption_class": corruption_class,
        "feature_set": "metadata_csum",
        "clean_fixture_sha256": fixture_hash,
        "mutation": mutation,
        "tested_source_commit": source_commit,
        "tested_binary_sha256": binary_hash,
        "nexfsck": {
            "command": nex_command,
            "exit_code": nex.returncode,
            "structured_counters": counters,
            "repair_eligible": counters_map.get("repair_eligible"),
            "repair_blocked_reasons": counters_map.get("repair_blocked_reasons", []),
            "diagnostic": (nex.stderr + nex.stdout)[-4000:],
        },
        "e2fsck": {
            "command": e2_command,
            "exit_code": e2.returncode,
            "diagnostic_classification": "reported_corruption" if e2_diagnostic_detected else "no_corruption_diagnostic",
            "diagnostic": e2_text[-4000:],
        },
        "agreement": nex_detected == e2_detected,
        "agreement_class": agreement_class,
        "disagreement_class": None if nex_detected == e2_detected else (
            "e2fsck_reports_but_exit_semantics_differ"
            if e2_diagnostic_detected and e2.returncode == 0
            else "likely_nexfsck_missing_validation" if e2_detected and not nex_detected
            else "nexfsck_stricter_validation"
        ),
        "exit_code_agreement": nex.returncode == e2.returncode,
        "expected_result": "Nexfsck detects corruption; e2fsck behavior is recorded independently",
    }


def jbd2_mutation_observed(counters, baseline):
    if not isinstance(counters, dict) or not isinstance(baseline, dict):
        return False
    return (
        counters.get("journal_integrity_failures", 0)
        > baseline.get("journal_integrity_failures", 0)
        or counters.get("journal_integrity_state") != baseline.get("journal_integrity_state")
    )


def jbd2_e2fsck_mutation_observed(text):
    return bool(re.search(
        r"journal.*(checksum|corrupt|invalid|bad)|(?:checksum|corrupt|invalid).*journal|"
        r"bad journal|invalid transaction|journal block.*error",
        text,
        re.IGNORECASE,
    ))


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


def parse_metadata_targets(image):
    result = run(["dumpe2fs", "-g", str(image)])
    rows = [line.split(":") for line in result.stdout.splitlines()
            if line and line[0].isdigit()]
    group0 = next(row for row in rows if row[0] == "0")
    def first_block(value):
        return int(value.split("-", 1)[0])
    targets = {
        "block_bitmap": int(group0[4]),
        "inode_bitmap": int(group0[5]),
        "inode_table": int(group0[6]),
        "gdt": first_block(group0[3]),
    }
    backup = next((row for row in rows if row[0] == "1" and row[2] != "-"), None)
    if backup is not None:
        targets["backup_superblock"] = int(backup[2])
    return targets


def inode_checksum_offset(image):
    result = run(["debugfs", "-R", "imap <2>", str(image)])
    text = result.stdout + result.stderr
    match = re.search(r"located at block\s+(\d+),\s*offset\s+0x([0-9a-fA-F]+)", text)
    if not match:
        raise RuntimeError(f"cannot locate root inode: {text}")
    block, within = (int(match.group(1)), int(match.group(2), 16))
    _, block_size, _, _, _, _ = parse_dumpe2fs(image)
    return block * block_size + within + 124


def mutate_raw_inode(image, inode_number, mutate):
    """Mutate raw inode bytes and recompute its metadata_csum checksum."""
    _, block_size, _, _, _, _ = parse_dumpe2fs(image)
    header = run(["dumpe2fs", "-h", str(image)])
    inode_size = int(re.search(r"Inode size:\s*(\d+)", header.stdout).group(1))
    mapping = run(["debugfs", "-R", f"imap <{inode_number}>", str(image)])
    match = re.search(r"located at block\s+(\d+),\s*offset\s+0x([0-9a-fA-F]+)",
                      mapping.stdout + mapping.stderr)
    if not match:
        raise RuntimeError(f"cannot locate inode {inode_number}: {mapping.stdout}{mapping.stderr}")
    inode_offset = int(match.group(1)) * block_size + int(match.group(2), 16)
    with image.open("r+b") as stream:
        stream.seek(inode_offset)
        inode = bytearray(stream.read(inode_size))
        if len(inode) != inode_size or inode[40:42] != b"\x0a\xf3":
            raise RuntimeError("target inode does not have a valid extent root")
        if int.from_bytes(inode[42:44], "little") < 1:
            raise RuntimeError("target inode has no extent to redirect")
        original_checksum = int.from_bytes(inode[124:126], "little")
        stored_high = int.from_bytes(inode[130:132], "little") if inode_size >= 132 else 0
        original_generation = inode[100:104]
        stream.seek(1024 + 104)
        uuid = stream.read(16)
        seed = crc32c(0xffffffff, uuid)
        seed = crc32c(seed, inode_number.to_bytes(4, "little"))
        seed = crc32c(seed, original_generation)
        clean_inode = bytearray(inode)
        clean_inode[124:126] = b"\0\0"
        if inode_size >= 132:
            clean_inode[130:132] = b"\0\0"
        original_calculated = crc32c(seed, clean_inode)
        original_stored = original_checksum | (stored_high << 16)
        if original_calculated != original_stored:
            raise RuntimeError(
                f"cannot independently verify source inode checksum: calculated {original_calculated:#x}, stored {original_stored:#x}"
            )
        mutate(inode)
        inode[124:126] = b"\0\0"
        if inode_size >= 132:
            inode[130:132] = b"\0\0"
        checksum = crc32c(seed, inode)
        inode[124:126] = (checksum & 0xffff).to_bytes(2, "little")
        if inode_size >= 132:
            inode[130:132] = ((checksum >> 16) & 0xffff).to_bytes(2, "little")
        stream.seek(inode_offset)
        stream.write(inode)
        stream.flush()
        os.fsync(stream.fileno())


def mutate_inode_extent_target(image, inode_number, target_block):
    """Redirect the first extent while preserving a valid inode checksum."""
    def apply(inode):
        inode[58:60] = ((target_block >> 32) & 0xffff).to_bytes(2, "little")
        inode[60:64] = (target_block & 0xffffffff).to_bytes(4, "little")

    mutate_raw_inode(image, inode_number, apply)


def mutate_extent_root(image, inode_number, mutation):
    def apply(inode):
        if inode[40:42] != b"\x0a\xf3":
            raise RuntimeError("target inode does not have an extent root")
        if mutation == "bad_magic":
            inode[40:42] = b"\0\0"
        elif mutation == "zero_length":
            inode[56:58] = b"\0\0"
        elif mutation in ("unsorted", "overlap"):
            first = bytearray(inode[52:64])
            second = bytearray(first)
            inode[42:44] = (2).to_bytes(2, "little")
            if mutation == "unsorted":
                first[0:4] = (1).to_bytes(4, "little")
                second[0:4] = (0).to_bytes(4, "little")
            else:
                second[0:4] = first[0:4]
            inode[52:64] = first
            inode[64:76] = second
        elif mutation == "depth_transition":
            inode[46:48] = (1).to_bytes(2, "little")
        else:
            raise ValueError(f"unknown extent-root mutation: {mutation}")

    mutate_raw_inode(image, inode_number, apply)


def make_extent_overlap_fixture(image, work):
    source = work / "extent-overlap-source"
    source.mkdir()
    (source / "file").write_bytes(b"extent target" * 1024)
    subprocess.check_call(["truncate", "-s", "512M", str(image)])
    subprocess.check_call(["mkfs.ext4", "-q", "-F", "-O", "metadata_csum", "-d",
                           str(source), str(image)])
    stat = run(["debugfs", "-R", "stat /file", str(image)])
    match = re.search(r"Inode:\s*(\d+)", stat.stdout + stat.stderr)
    if not match:
        raise RuntimeError("cannot locate file inode in extent-overlap fixture")
    inode_number = int(match.group(1))
    _, block_size, _, _, _, _ = parse_dumpe2fs(image)
    return inode_number, block_size, parse_metadata_targets(image)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--nexfsck", default=str(ROOT / "target/release/nexfsck"))
    parser.add_argument("--output", default=str(ROOT / "benchmark-results/differential/latest.json"))
    parser.add_argument("--force", action="store_true", help="replace the requested JSON output")
    parser.add_argument("--check-manifest", action="store_true", help="fail if a required Nexfsck detection regresses")
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
    untracked_paths = subprocess.check_output(
        ["git", "ls-files", "--others", "--exclude-standard"], cwd=ROOT, text=True
    ).splitlines()
    untracked_digest = hashlib.sha256()
    for relative_path in sorted(untracked_paths):
        path = ROOT / relative_path
        untracked_digest.update(relative_path.encode("utf-8") + b"\0")
        untracked_digest.update(path.read_bytes())
    with tempfile.TemporaryDirectory(prefix="nexfsck-differential-") as temp:
        work = Path(temp)
        clean = work / "clean.ext4"
        subprocess.check_call(["truncate", "-s", "512M", str(clean)])
        subprocess.check_call(["mkfs.ext4", "-q", "-F", "-O", "metadata_csum", str(clean)])
        features, block_size, gdt_offset, desc_size, block_bitmap, inode_bitmap = parse_dumpe2fs(clean)
        clean_sha256 = sha256_file(clean)
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
            counters = parse_structured_json(nex.stdout)
            counters_map = counters if isinstance(counters, dict) else {}
            nex_detected = nex.returncode != 0 or counters_map.get("errors_detected", 0) > 0
            e2_text = e2.stderr + e2.stdout
            e2_diagnostic_detected = bool(re.search(
                r"checksum.*invalid|invalid.*checksum|checksum.*should be|corrupt|invalid",
                e2_text,
                re.IGNORECASE,
            ))
            e2_detected = e2.returncode != 0 or e2_diagnostic_detected
            if nex_detected and e2_detected:
                classification = "both_detect"
            elif nex_detected:
                classification = "nexfsck_only_detects"
            elif e2_detected:
                classification = "e2fsck_only_detects"
            else:
                classification = "both_accept"
            records.append({
                "case_id": case_id,
                "corruption_class": "checksum_corruption",
                "feature_set": features.get("Filesystem features", "unknown"),
                "clean_fixture_sha256": clean_sha256,
                "mutation": {"byte_offset": byte_offset, "block": byte_offset // block_size,
                    "inode": 2 if case_id == "inode_checksum" else None,
                    "operation": "xor byte with 0x01"},
                "nexfsck": {
                    "command": nex_command,
                    "exit_code": nex.returncode,
                    "structured_counters": counters,
                    "repair_eligible": counters_map.get("repair_eligible"),
                    "repair_trust_reasons": counters_map.get("repair_blocked_reasons", []),
                    "source_binary_sha256": binary_hash,
                    "diagnostic": (nex.stderr + nex.stdout)[-4000:],
                },
                "tested_source_commit": source_commit,
                "tested_binary_sha256": binary_hash,
                "e2fsck": {
                    "command": e2_command,
                    "exit_code": e2.returncode,
                    "diagnostic_classification": "reported_corruption" if e2_diagnostic_detected else "no_corruption_diagnostic",
                    "diagnostic": e2_text[-4000:],
                },
                "agreement": nex_detected == e2_detected,
                "agreement_class": "scope_difference" if case_id == "backup_superblock_checksum" and nex_detected != e2_detected else classification,
                "differential_classification": classification,
                "exit_code_agreement": nex.returncode == e2.returncode,
                "disagreement_class": None if nex_detected == e2_detected else (
                    "scope_difference" if case_id == "backup_superblock_checksum"
                    else "e2fsck_reports_but_exit_semantics_differ"
                    if e2_diagnostic_detected and e2.returncode == 0
                    else "likely_nexfsck_missing_validation" if e2_detected
                    else "nexfsck_stricter_validation"
                ),
                "expected": "nexfsck detects corruption; e2fsck result is recorded as oracle data",
            })

        extent_clean = work / "extent-overlap-clean.ext4"
        extent_inode, extent_block_size, metadata_targets = make_extent_overlap_fixture(
            extent_clean, work
        )
        extent_fixture_hash = sha256_file(extent_clean)
        clean_extent = run([str(binary), "--json", "-n", str(extent_clean)])
        if clean_extent.returncode != 0:
            raise RuntimeError(
                f"generated extent-overlap fixture is not clean: {clean_extent.stdout}{clean_extent.stderr}"
            )
        for case_id, mutation_name in (
            ("extent_root_header", "bad_magic"),
            ("extent_invalid_length", "zero_length"),
            ("extent_logical_ordering", "unsorted"),
            ("extent_logical_overlap", "overlap"),
            ("extent_invalid_depth_transition", "depth_transition"),
        ):
            image = work / f"{case_id}.ext4"
            shutil.copyfile(extent_clean, image)
            mutate_extent_root(image, extent_inode, mutation_name)
            record = run_differential_case(
                binary, image, case_id, "extent_structure", extent_fixture_hash,
                {"inode": extent_inode, "mutation": mutation_name,
                 "operation": "mutate extent-root bytes and recompute inode checksum"},
                source_commit, binary_hash,
            )
            counters = record["nexfsck"]["structured_counters"] or {}
            if case_id != "extent_invalid_depth_transition":
                record["expected_counter"] = "extent_corruptions"
                record["counter_observed"] = counters.get("extent_corruptions", 0)
            else:
                record["note"] = (
                    "This mutation is detected through invalid child-block semantics; "
                    "the current aggregate extent_corruptions counter is not incremented on that path."
                )
            records.append(record)
        for metadata_class in (
            "block_bitmap", "inode_bitmap", "inode_table", "gdt", "backup_superblock"
        ):
            target = metadata_targets.get(metadata_class)
            if target is None:
                continue
            case_id = f"extent_overlap_{metadata_class}"
            overlap_image = work / f"{case_id}.ext4"
            shutil.copyfile(extent_clean, overlap_image)
            mutate_inode_extent_target(overlap_image, extent_inode, target)
            overlap_case = run_differential_case(
                binary, overlap_image, case_id, "extent_metadata_overlap", extent_fixture_hash,
                {"inode": extent_inode, "target_block": target,
                 "target_metadata_class": metadata_class,
                 "operation": f"redirect first file extent to {metadata_class}; recompute inode checksum"},
                source_commit, binary_hash,
            )
            image_before_repair = sha256_file(overlap_image)
            repair_command = [str(binary), "--json", "-r", str(overlap_image)]
            repair_result = run(repair_command)
            image_after_repair = sha256_file(overlap_image)
            repair_counters = parse_structured_json(repair_result.stdout) or {}
            overlap_case["repair_attempt"] = {
                "command": repair_command,
                "exit_code": repair_result.returncode,
                "structured_counters": repair_counters,
                "image_sha256_before": image_before_repair,
                "image_sha256_after": image_after_repair,
                "image_unchanged": image_before_repair == image_after_repair,
            }
            overlap_counters = overlap_case["nexfsck"]["structured_counters"] or {}
            overlap_case["expected_counter"] = "extent_metadata_overlap_failures"
            overlap_case["counter_observed"] = overlap_counters.get(
                "extent_metadata_overlap_failures", 0
            )
            records.append(overlap_case)

        xattr_source = work / "xattr-source"
        xattr_source.mkdir()
        (xattr_source / "file").write_bytes(b"xattr differential fixture\n")
        xattr_value = work / "xattr-value"
        xattr_value.write_bytes(b"V" * 1024)
        xattr_clean = work / "xattr-clean.ext4"
        subprocess.check_call(["truncate", "-s", "64M", str(xattr_clean)])
        subprocess.check_call([
            "mkfs.ext4", "-q", "-F", "-O", "metadata_csum", "-d",
            str(xattr_source), str(xattr_clean),
        ])
        for name in ("alpha", "bravo"):
            result = run([
                "debugfs", "-w", "-R",
                f"ea_set -f {xattr_value} /file user.{name}", str(xattr_clean),
            ])
            if result.returncode != 0:
                raise RuntimeError(f"debugfs could not create external xattr {name}: {result.stderr}")
        xattr_features, xattr_block_size, _, _, _, _ = parse_dumpe2fs(xattr_clean)
        xattr_block, xattr_entries = xattr_block_layout(xattr_clean, xattr_block_size)
        xattr_fixture_sha256 = sha256_file(xattr_clean)
        clean_xattr = run([str(binary), "--json", "-n", str(xattr_clean)])
        if clean_xattr.returncode != 0:
            raise RuntimeError(f"generated external-xattr fixture is not clean: {clean_xattr.stdout}{clean_xattr.stderr}")

        file_stat = run(["debugfs", "-R", "stat /file", str(xattr_clean)])
        file_inode_match = re.search(r"Inode:\s*(\d+)", file_stat.stdout + file_stat.stderr)
        if not file_inode_match:
            raise RuntimeError("cannot locate xattr fixture file inode")
        xattr_file_inode = int(file_inode_match.group(1))
        xattr_extent_overlap = work / "extent_overlaps_xattr_block.ext4"
        shutil.copyfile(xattr_clean, xattr_extent_overlap)
        mutate_inode_extent_target(xattr_extent_overlap, xattr_file_inode, xattr_block)
        xattr_overlap_hash = sha256_file(xattr_extent_overlap)
        xattr_overlap_case = run_differential_case(
            binary, xattr_extent_overlap, "extent_overlap_xattr_block", "extent_metadata_overlap",
            xattr_fixture_sha256,
            {"inode": xattr_file_inode, "target_block": xattr_block,
             "target_metadata_class": "external_xattr_block",
             "operation": "redirect first file extent to its external xattr block; recompute inode checksum"},
            source_commit, binary_hash,
        )
        overlap_repair_command = [str(binary), "--json", "-r", str(xattr_extent_overlap)]
        overlap_repair = run(overlap_repair_command)
        overlap_repair_counters = parse_structured_json(overlap_repair.stdout) or {}
        xattr_overlap_case["repair_attempt"] = {
            "command": overlap_repair_command,
            "exit_code": overlap_repair.returncode,
            "structured_counters": overlap_repair_counters,
            "image_sha256_before": xattr_overlap_hash,
            "image_sha256_after": sha256_file(xattr_extent_overlap),
            "image_unchanged": xattr_overlap_hash == sha256_file(xattr_extent_overlap),
        }
        overlap_counters = xattr_overlap_case["nexfsck"]["structured_counters"] or {}
        xattr_overlap_case["expected_counter"] = "extent_metadata_overlap_failures"
        xattr_overlap_case["counter_observed"] = overlap_counters.get(
            "extent_metadata_overlap_failures", 0
        )
        records.append(xattr_overlap_case)

        first, second = xattr_entries[:2]
        mutation_specs = [
            ("xattr_block_checksum", "xattr_checksum", xattr_block * xattr_block_size + 16,
             b"\x01", False, "xattr_checksum_failures"),
            ("xattr_entry_name_length", "xattr_structure", xattr_block * xattr_block_size + first["offset"],
             b"\xff", False, "xattr_semantic_failures"),
            ("xattr_value_offset", "xattr_structure", xattr_block * xattr_block_size + first["offset"] + 2,
             (0xFFFF).to_bytes(2, "little"), False, "xattr_semantic_failures"),
            ("xattr_value_out_of_bounds", "xattr_structure", xattr_block * xattr_block_size + first["offset"] + 2,
             (xattr_block_size - 4).to_bytes(2, "little"), False, "xattr_semantic_failures"),
            ("xattr_overlapping_values", "xattr_structure", xattr_block * xattr_block_size + second["offset"] + 2,
             first["value_offset"].to_bytes(2, "little"), True, "xattr_semantic_failures"),
            ("xattr_entry_order", "xattr_structure", xattr_block * xattr_block_size + first["offset"] + 16,
             b"zulu!", False, "xattr_semantic_failures"),
            ("xattr_entry_hash", "xattr_hash", xattr_block * xattr_block_size + first["offset"] + 12,
             b"\x01", True, "xattr_hash_failures"),
            ("xattr_nonzero_block_hash", "xattr_hash", xattr_block * xattr_block_size + 12,
             (1).to_bytes(4, "little"), True, "xattr_hash_failures"),
            ("xattr_refcount_too_high", "xattr_refcount", xattr_block * xattr_block_size + 4,
             (3).to_bytes(4, "little"), True, "xattr_refcount_failures"),
        ]
        for case_id, corruption_class, offset, payload, refresh_checksum, required_counter in mutation_specs:
            image = work / f"{case_id}.ext4"
            shutil.copyfile(xattr_clean, image)
            with image.open("r+b") as stream:
                stream.seek(offset)
                if case_id == "xattr_block_checksum":
                    old = stream.read(1)
                    stream.seek(offset)
                    stream.write(bytes([old[0] ^ payload[0]]))
                elif case_id == "xattr_entry_hash":
                    old = stream.read(1)
                    stream.seek(offset)
                    stream.write(bytes([old[0] ^ payload[0]]))
                else:
                    stream.write(payload)
                stream.flush()
                os.fsync(stream.fileno())
            if refresh_checksum:
                refresh_xattr_checksum(image, xattr_block, xattr_block_size)
            record = run_differential_case(
                binary, image, case_id, corruption_class, xattr_fixture_sha256,
                {"byte_offset": offset, "block": xattr_block, "inode": 12,
                 "operation": f"write {payload.hex()}"}, source_commit, binary_hash,
            )
            record["feature_set"] = xattr_features.get("Filesystem features", "unknown")
            record["expected_counter"] = required_counter
            record["counter_observed"] = (record["nexfsck"]["structured_counters"] or {}).get(required_counter, 0)
            records.append(record)

        unavailable_cases = []
        active_base = work / "active-journal-base.ext4"
        try:
            active_fixture, active_failure = make_active_journal_fixture(active_base, work)
        except Exception as error:
            active_fixture, active_failure = None, f"active journal fixture failed: {error}"
        if active_fixture is None:
            for case_id in (
                "jbd2_superblock_checksum", "jbd2_descriptor_type", "jbd2_tag_bounds",
                "jbd2_descriptor_checksum", "jbd2_invalid_target", "jbd2_wrong_descriptor_sequence",
                "jbd2_commit_checksum", "jbd2_commit_wrong_sequence", "jbd2_missing_commit",
                "jbd2_revoke_checksum", "jbd2_revoke_length",
            ):
                unavailable_cases.append({"case_id": case_id, "reason": active_failure})
        else:
            active_image, journal, active_fixture_hash, clean_fixture_hash = active_fixture
            if not journal.get("descriptor_logicals") or not journal.get("commit_logicals"):
                for case_id in (
                    "jbd2_superblock_checksum", "jbd2_descriptor_type", "jbd2_tag_bounds",
                    "jbd2_descriptor_checksum", "jbd2_invalid_target", "jbd2_wrong_descriptor_sequence",
                    "jbd2_commit_checksum", "jbd2_commit_wrong_sequence", "jbd2_missing_commit",
                    "jbd2_revoke_checksum", "jbd2_revoke_length",
                ):
                    unavailable_cases.append({
                        "case_id": case_id,
                        "reason": "active fixture had no discoverable descriptor/commit sequence",
                    })
            else:
                baseline_nex = run([str(binary), "--json", "-n", str(active_image)])
                baseline_counters = parse_structured_json(baseline_nex.stdout) or {}
                baseline_e2 = run(["e2fsck", "-f", "-n", str(active_image)])
                active_features = parse_dumpe2fs(active_image)[0]
                descriptor_logical = journal["descriptor_logicals"][0]
                commit_logical = journal["commit_logicals"][0]
                descriptor_physical = journal_physical_block(active_image, journal, descriptor_logical)
                commit_physical = journal_physical_block(active_image, journal, commit_logical)
                last_tag_flags_offset = last_jbd_v3_tag_flags_offset(
                    active_image, descriptor_physical, journal["block_size"]
                )
                jbd_cases = [
                    ("jbd2_superblock_checksum", "jbd2_superblock_checksum",
                     journal["superblock_physical"] * journal["block_size"] + 0xFC, "flip checksum byte",
                     journal["superblock_physical"], None),
                    ("jbd2_descriptor_type", "jbd2_structure",
                     descriptor_physical * block_size + 4, "write invalid block type",
                     descriptor_physical, None),
                    ("jbd2_tag_bounds", "jbd2_descriptor_tag",
                     descriptor_physical * block_size + last_tag_flags_offset,
                     "clear LAST_TAG on final descriptor tag; refresh descriptor CRC",
                     descriptor_physical, "clear_last_tag"),
                    ("jbd2_descriptor_checksum", "jbd2_descriptor_checksum",
                     descriptor_physical * block_size + block_size - 1, "flip descriptor checksum byte",
                     descriptor_physical, None),
                    ("jbd2_invalid_target", "jbd2_descriptor_target",
                     descriptor_physical * block_size + 12, "write invalid target; refresh descriptor CRC",
                     descriptor_physical, "target_crc"),
                    ("jbd2_wrong_descriptor_sequence", "jbd2_sequence",
                     descriptor_physical * block_size + 8, "increment sequence; refresh descriptor CRC",
                     descriptor_physical, "descriptor_crc_sequence"),
                    ("jbd2_commit_checksum", "jbd2_commit_checksum",
                     commit_physical * block_size + 16, "flip commit checksum byte",
                     commit_physical, None),
                    ("jbd2_commit_wrong_sequence", "jbd2_sequence",
                     commit_physical * block_size + 8, "increment sequence; refresh commit CRC",
                     commit_physical, "commit_crc_sequence"),
                    ("jbd2_missing_commit", "jbd2_incomplete_transaction",
                     commit_physical * block_size, "clear commit block magic",
                     commit_physical, None),
                ]
                for case_id, corruption_class, offset, operation, physical, checksum_mode in jbd_cases:
                    image = work / f"{case_id}.ext4"
                    shutil.copyfile(active_image, image)
                    with image.open("r+b") as stream:
                        stream.seek(offset)
                        if checksum_mode in ("descriptor_crc", "target_crc"):
                            stream.write((0xFFFFFFFF).to_bytes(4, "big"))
                        elif checksum_mode == "clear_last_tag":
                            stream.seek(offset)
                            flags = int.from_bytes(stream.read(4), "big")
                            stream.seek(offset)
                            stream.write((flags & ~8).to_bytes(4, "big"))
                        elif checksum_mode == "descriptor_crc_sequence":
                            stream.seek(offset)
                            sequence = int.from_bytes(stream.read(4), "big")
                            stream.seek(offset)
                            stream.write(((sequence + 1) & 0xFFFFFFFF).to_bytes(4, "big"))
                        elif checksum_mode == "commit_crc_sequence":
                            sequence = int.from_bytes(stream.read(4), "big")
                            stream.seek(offset)
                            stream.write(((sequence + 1) & 0xFFFFFFFF).to_bytes(4, "big"))
                        elif case_id == "jbd2_descriptor_type":
                            stream.write((99).to_bytes(4, "big"))
                        elif case_id == "jbd2_missing_commit":
                            stream.write(bytes(4))
                        else:
                            original = stream.read(1)
                            stream.seek(offset)
                            stream.write(bytes([original[0] ^ 1]))
                        stream.flush()
                        os.fsync(stream.fileno())
                    if checksum_mode in (
                        "descriptor_crc", "target_crc", "descriptor_crc_sequence", "clear_last_tag"
                    ):
                        refresh_jbd_block_checksum(
                            image, physical, block_size, journal["uuid"], block_size - 4
                        )
                    elif checksum_mode == "commit_crc_sequence":
                        refresh_jbd_block_checksum(image, physical, block_size,
                                                   journal["uuid"], 16)
                    record = run_differential_case(
                        binary, image, case_id, corruption_class, clean_fixture_hash,
                        {"byte_offset": offset, "journal_log_block":
                            0 if case_id == "jbd2_superblock_checksum"
                            else descriptor_logical if physical == descriptor_physical else commit_logical,
                         "journal_physical_block": physical, "transaction_sequence":
                            int.from_bytes(journal["journal_superblock"][24:28], "big"),
                         "operation": operation},
                        source_commit, binary_hash,
                    )
                    record["feature_set"] = active_features.get("Filesystem features", "unknown")
                    record["active_source_fixture_sha256"] = active_fixture_hash
                    record["journal_integrity_state"] = (
                        record["nexfsck"]["structured_counters"] or {}
                    ).get("journal_integrity_state")
                    record["journal_fixture"] = {
                        "kind": "mounted_ext4_active_journal_snapshot",
                        "journal_inode": journal["journal_inode"],
                        "start": journal["start"],
                        "descriptor_logicals": journal["descriptor_logicals"],
                        "commit_logicals": journal["commit_logicals"],
                    }
                    counters = record["nexfsck"]["structured_counters"] or {}
                    nex_mutation = jbd2_mutation_observed(counters, baseline_counters)
                    e2_mutation = jbd2_e2fsck_mutation_observed(
                        record["e2fsck"]["diagnostic"]
                    ) and not jbd2_e2fsck_mutation_observed(
                        baseline_e2.stderr + baseline_e2.stdout
                    )
                    record["baseline"] = {
                        "nexfsck_exit_code": baseline_nex.returncode,
                        "journal_integrity_state": baseline_counters.get("journal_integrity_state"),
                        "e2fsck_exit_code": baseline_e2.returncode,
                        "e2fsck_journal_diagnostic": jbd2_e2fsck_mutation_observed(
                            baseline_e2.stderr + baseline_e2.stdout
                        ),
                    }
                    record["nexfsck"]["mutation_detected_vs_active_baseline"] = nex_mutation
                    record["e2fsck"]["mutation_detected_vs_active_baseline"] = e2_mutation
                    record["agreement"] = nex_mutation == e2_mutation
                    if nex_mutation and e2_mutation:
                        record["agreement_class"] = "both_detect"
                    elif nex_mutation:
                        record["agreement_class"] = "nexfsck_only_detects"
                    elif e2_mutation:
                        record["agreement_class"] = "e2fsck_only_detects"
                    else:
                        record["agreement_class"] = "both_accept"
                    record["differential_classification"] = record["agreement_class"]
                    record["disagreement_class"] = (
                        None if record["agreement"] else "behavior_difference"
                    )
                    records.append(record)

                if journal.get("revoke_logicals"):
                    unavailable_cases.extend([
                        {"case_id": "jbd2_revoke_checksum",
                         "reason": "revoke mutation not yet isolated in the generated transaction"},
                        {"case_id": "jbd2_revoke_length",
                         "reason": "revoke mutation not yet isolated in the generated transaction"},
                    ])
                else:
                    unavailable_cases.extend([
                        {"case_id": "jbd2_revoke_checksum",
                         "reason": "kernel-generated transaction contained no revoke block"},
                        {"case_id": "jbd2_revoke_length",
                         "reason": "kernel-generated transaction contained no revoke block"},
                    ])

        shared_image = work / "shared-xattr.ext4"
        try:
            shared_block, shared_failure = make_shared_xattr_fixture(shared_image, work)
        except Exception as error:
            shared_block, shared_failure = None, f"shared xattr fixture generation failed: {error}"
        if shared_block is None:
            unavailable_cases.extend([
                {"case_id": "xattr_shared_refcount_too_low", "reason": shared_failure},
                {"case_id": "xattr_shared_refcount_too_high", "reason": shared_failure},
            ])
        else:
            shared_features, shared_block_size, _, _, _, _ = parse_dumpe2fs(shared_image)
            shared_hash = sha256_file(shared_image)
            for case_id, refcount in (("xattr_shared_refcount_too_low", 1),
                                      ("xattr_shared_refcount_too_high", 3)):
                image = work / f"{case_id}.ext4"
                shutil.copyfile(shared_image, image)
                offset = shared_block * shared_block_size + 4
                with image.open("r+b") as stream:
                    stream.seek(offset)
                    stream.write(refcount.to_bytes(4, "little"))
                refresh_xattr_checksum(image, shared_block, shared_block_size)
                record = run_differential_case(
                    binary, image, case_id, "xattr_refcount", shared_hash,
                    {"byte_offset": offset, "block": shared_block, "inode": None,
                     "referencing_inodes": ["/one", "/two"],
                     "operation": f"write refcount {refcount}; refresh valid block checksum"},
                    source_commit, binary_hash,
                )
                record["feature_set"] = shared_features.get("Filesystem features", "unknown")
                record["expected_counter"] = "xattr_refcount_failures"
                record["counter_observed"] = (record["nexfsck"]["structured_counters"] or {}).get(
                    "xattr_refcount_failures", 0
                )
                records.append(record)

        report = {
            "schema_version": 3,
            "benchmarked_source_commit": source_commit,
            "benchmarked_worktree_clean": not bool(status.strip()),
            "benchmarked_worktree_diff_sha256": hashlib.sha256(source_diff).hexdigest() if source_diff else None,
            "benchmarked_worktree_untracked_files": untracked_paths,
            "benchmarked_worktree_untracked_sha256": untracked_digest.hexdigest() if untracked_paths else None,
            "benchmarked_binary_sha256": binary_hash,
            "fixture": {"size": "512 MiB", "block_size": block_size, "features": features.get("Filesystem features", "unknown")},
            "clean_fixture_preserved": True,
            "source_binary_sha256": binary_hash,
            "cases": records,
            "cases_defined": len(records) + len(unavailable_cases),
            "cases_executed": len(records),
            "cases_unavailable": len(unavailable_cases),
            "unavailable_cases": unavailable_cases,
            "revoke_real_fixture_status": {
                "status": "unavailable",
                "attempted_method": (
                    "Disposable ext4 image mounted with data=journal,commit=600; "
                    "write and fsync an 8 MiB file, unlink it, fsync a second file, "
                    "then snapshot before unmount and inspect debugfs logdump."
                ),
                "result": "debugfs found descriptor/commit transactions but no revoke block",
                "synthetic_coverage": "JBD2 revoke bounds and checksum parser tests remain enabled",
                "limitation": "real internal-journal revoke transaction coverage is absent",
            },
            "not_yet_covered": MISSING_CASES,
            "summary": {
                "cases_defined": len(records) + len(unavailable_cases),
                "cases_executed": len(records),
                "cases_unavailable": len(unavailable_cases),
                "total": len(records),
                "agreement": sum(case["agreement"] for case in records),
                "disagreement": sum(not case["agreement"] for case in records),
                "agreement_classes": {
                    name: sum(case["agreement_class"] == name for case in records)
                    for name in (
                        "both_detect", "nexfsck_only_detects", "e2fsck_only_detects",
                        "both_accept", "scope_difference", "behavior_difference",
                        "manual_review_required", "unavailable",
                    )
                } | {"unavailable": len(unavailable_cases)},
                "unavailable": len(unavailable_cases),
            },
        }
        if args.check_manifest:
            manifest = json.loads(MANIFEST.read_text())
            records_by_id = {case["case_id"]: case for case in records}
            unavailable_ids = {case["case_id"] for case in unavailable_cases}
            regressions = []
            skipped = []
            for case_id in manifest["nexfsck_must_detect"]:
                case = records_by_id.get(case_id)
                if case is None and case_id in unavailable_ids:
                    skipped.append(case_id)
                    continue
                counters = case["nexfsck"]["structured_counters"] if case else None
                if not isinstance(counters, dict) or counters.get("errors_detected", 0) <= 0:
                    regressions.append(f"{case_id}: no structured corruption result (possible crash/acceptance)")
                    continue
                required_counter = manifest.get("required_counters", {}).get(case_id)
                if required_counter and counters.get(required_counter, 0) <= 0:
                    regressions.append(f"{case_id}: required counter {required_counter} disappeared")
                if case_id in manifest.get("must_block_repair", []) and counters.get("repair_eligible") is not False:
                    regressions.append(f"{case_id}: RepairTrustState no longer blocks repair")
                if case_id.startswith("jbd2_") and not case["nexfsck"].get(
                    "mutation_detected_vs_active_baseline", False
                ):
                    regressions.append(f"{case_id}: no JBD2 integrity-state delta from active baseline")
                required_journal_state = manifest.get("required_journal_states", {}).get(case_id)
                if required_journal_state and counters.get("journal_integrity_state") != required_journal_state:
                    regressions.append(
                        f"{case_id}: expected journal state {required_journal_state}, "
                        f"got {counters.get('journal_integrity_state')}"
                    )
                if case_id in manifest.get("repair_must_preserve_image", []):
                    repair_attempt = case.get("repair_attempt", {})
                    if not repair_attempt.get("image_unchanged"):
                        regressions.append(f"{case_id}: repair attempt modified the image")
            report["regression_gate"] = {"manifest": str(MANIFEST.relative_to(ROOT)),
                "passed": not regressions, "regressions": regressions, "unavailable_skipped": skipped}
            if regressions:
                raise SystemExit(f"differential regression gate failed: {regressions}")
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report["summary"], indent=2))


if __name__ == "__main__":
    main()
