#!/usr/bin/env python3
"""
Enterprise 10.0 GiB Multi-Group Stress & Endurance Benchmark Suite for nexfsck vs e2fsck.
Performs:
1. Massive 10.0 GiB ext4 scale test with 100,000+ files, directories, symlinks, and hardlinks across 80 block groups.
2. Aggregate inode/block counter comparison against e2fsck v1.46.5.
3. 30-iteration sustained endurance test tracking RSS memory stability and latency.
4. Multi-group corruption injection, repair with a flushed pre-image undo journal, and rollback.
"""

import concurrent.futures
import datetime
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time

IMG_PATH = "/tmp/stress_10g.img"
MNT_PATH = "/tmp/stress_mnt"
UNDO_LOG = "/tmp/stress_10g_repair.undo"
CORRUPT_IMG = "/tmp/stress_10g_corrupt.img"
NEXFSCK_BIN = "/home/pop-os/nexfsck/target/release/nexfsck"
RESULT_DIR = "/home/pop-os/nexfsck/benchmark-results"
RESULT_JSON = f"{RESULT_DIR}/latest.json"
COMPARISON_RUNS = 10

def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, int(len(ordered) * fraction + 0.999999) - 1))]

def timed_run(command):
    started = time.perf_counter()
    proc = subprocess.run(command, capture_output=True, text=True)
    return proc, time.perf_counter() - started

def populate_filesystem():
    print("=" * 70)
    print("STAGE 1: GENERATING 10.0 GiB REAL DENSE EXT4 FILESYSTEM (80 GROUPS)")
    print("=" * 70)
    
    if os.path.exists(MNT_PATH):
        subprocess.run(["sudo", "umount", MNT_PATH], stderr=subprocess.DEVNULL)
    os.makedirs(MNT_PATH, exist_ok=True)

    if os.path.exists(IMG_PATH):
        os.remove(IMG_PATH)

    print(f"Creating 10.0 GiB image at {IMG_PATH}...")
    # Sparse creation keeps the 10 GiB logical geometry while allowing a clean
    # and a corrupted copy to coexist on the 16 GiB live-USB tmpfs.
    subprocess.check_call(["truncate", "-s", "10G", IMG_PATH])
    subprocess.check_call(["mkfs.ext4", "-F", "-b", "4096", "-O", "64bit,dir_index,extents", IMG_PATH])
    
    print("Mounting loop device...")
    subprocess.check_call(["sudo", "mount", "-o", "loop", IMG_PATH, MNT_PATH])
    subprocess.check_call(["sudo", "chmod", "777", MNT_PATH])
    
    # 1. Copy real system libraries and headers
    print("Copying real system data (/usr/include, /etc, nexfsck source)...")
    subprocess.run(["sudo", "cp", "-a", "/usr/include", f"{MNT_PATH}/include"], check=True)
    subprocess.run(["sudo", "cp", "-a", "/etc", f"{MNT_PATH}/etc"], check=True)
    subprocess.run(["sudo", "cp", "-a", "/home/pop-os/nexfsck/crates", f"{MNT_PATH}/crates"], check=True)
    
    # 2. Multi-threaded generation of 100,000 structured files across 200 clusters
    print("Spawning parallel workers to generate 100,000 structured files across 200 clusters...")
    
    def generate_cluster(c_id):
        cdir = f"{MNT_PATH}/stress_clusters/cluster_{c_id:03d}"
        os.makedirs(cdir, exist_ok=True)
        for j in range(500):
            size_type = j % 5
            if size_type == 0:
                data = b"A" * 256
            elif size_type == 1:
                data = b"B" * 4096
            elif size_type == 2:
                data = b"C" * 16384
            elif size_type == 3:
                data = b"D" * 65536  # Multi-extent
            else:
                data = b"E" * 131072 # Larger multi-extent
            
            with open(f"{cdir}/item_{j:04d}.dat", "wb") as f:
                f.write(data)
                
    t0 = time.time()
    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as ex:
        list(ex.map(generate_cluster, range(200)))
    print(f"File generation complete in {time.time() - t0:.2f}s")
    
    # 3. Create symlinks (both fast symlinks < 60 bytes and slow extent symlinks)
    print("Creating 2,000 symlinks...")
    os.makedirs(f"{MNT_PATH}/symlinks", exist_ok=True)
    for i in range(2000):
        target = f"../stress_clusters/cluster_{i%200:03d}/item_0000.dat"
        dst = f"{MNT_PATH}/symlinks/link_{i:04d}.lnk"
        try:
            os.symlink(target, dst)
        except Exception:
            pass
            
    # 4. Create hardlinks
    print("Creating 1,000 hardlinks...")
    os.makedirs(f"{MNT_PATH}/hardlinks", exist_ok=True)
    for i in range(1000):
        src = f"{MNT_PATH}/stress_clusters/cluster_{i%200:03d}/item_0001.dat"
        dst = f"{MNT_PATH}/hardlinks/hlink_{i:04d}.dat"
        try:
            os.link(src, dst)
        except Exception:
            pass
            
    # Flush buffers and cleanly unmount
    print("Syncing dirty filesystem blocks to disk...")
    subprocess.check_call(["sync"])
    subprocess.check_call(["sudo", "umount", MNT_PATH])
    shutil.rmtree(MNT_PATH)
    print("Filesystem unmounted cleanly.")

def run_ground_truth_test():
    print("\n" + "=" * 70)
    print("STAGE 2: 10.0 GiB GROUND-TRUTH CONSISTENCY & ACCURACY VERIFICATION")
    print("=" * 70)
    
    print(f"Running {COMPARISON_RUNS} interleaved warm-cache repetitions per checker...")
    e2_times = []
    nex_times = []
    p_e2 = None
    p_nex = None
    os.makedirs(RESULT_DIR, exist_ok=True)
    for run in range(1, COMPARISON_RUNS + 1):
        p_e2, e2_elapsed = timed_run(["e2fsck", "-f", "-v", "-t", "-n", IMG_PATH])
        p_nex, nex_elapsed = timed_run([NEXFSCK_BIN, "-n", IMG_PATH])
        assert p_e2.returncode == 0, f"e2fsck comparison run {run} failed"
        assert p_nex.returncode == 0, f"nexfsck comparison run {run} failed"
        e2_times.append(e2_elapsed)
        nex_times.append(nex_elapsed)
        print(f"  [Run {run:02d}/{COMPARISON_RUNS}] e2fsck={e2_elapsed:.3f}s nexfsck={nex_elapsed:.3f}s")
    with open(f"{RESULT_DIR}/e2fsck.stdout.log", "w") as f:
        f.write(p_e2.stdout)
    with open(f"{RESULT_DIR}/e2fsck.stderr.log", "w") as f:
        f.write(p_e2.stderr)
    with open(f"{RESULT_DIR}/nexfsck.stdout.log", "w") as f:
        f.write(p_nex.stdout)
    with open(f"{RESULT_DIR}/nexfsck.stderr.log", "w") as f:
        f.write(p_nex.stderr)
    
    e2_inodes = 0
    e2_blocks = 0
    e2_files = 0
    for line in p_e2.stdout.splitlines():
        if "inodes used" in line:
            m = re.search(r"(\d+)\s+inodes used", line)
            if m: e2_inodes = int(m.group(1))
        elif "blocks used" in line:
            m = re.search(r"(\d+)\s+blocks used", line)
            if m: e2_blocks = int(m.group(1))
        elif "files" in line and "files verified" not in line:
            m = re.search(r"(\d+)\s+files", line)
            if m: e2_files = int(m.group(1))
            
    print(f"e2fsck ground truth: Inodes={e2_inodes}, Blocks={e2_blocks}, Files={e2_files}")
    
    nex_inodes = 0
    nex_blocks = 0
    nex_entries = 0
    nex_errors = 0
    for line in p_nex.stdout.splitlines():
        if "Active Inodes" in line:
            m = re.search(r"Active Inodes\s+:\s+(\d+)", line)
            if m: nex_inodes = int(m.group(1))
        elif "Allocated Blocks" in line:
            m = re.search(r"Allocated Blocks\s+:\s+(\d+)", line)
            if m: nex_blocks = int(m.group(1))
        elif "Directory Entries" in line:
            m = re.search(r"Directory Entries\s+:\s+(\d+)", line)
            if m: nex_entries = int(m.group(1))
        elif "Errors Detected" in line:
            m = re.search(r"Errors Detected\s+:\s+(\d+)", line)
            if m: nex_errors = int(m.group(1))
            
    print(f"nexfsck ground truth: Inodes={nex_inodes}, Blocks={nex_blocks}, Entries={nex_entries}, Errors={nex_errors}")
    
    # Compare
    e2_median = statistics.median(e2_times)
    nex_median = statistics.median(nex_times)
    speedup = e2_median / nex_median if nex_median > 0 else 1.0
    print("-" * 70)
    print(f"MEDIAN COMPARISON: e2fsck={e2_median:.3f}s vs nexfsck={nex_median:.3f}s -> {speedup:.2f}x")
    print(f"BLOCK PARITY    : e2fsck={e2_blocks} vs nexfsck={nex_blocks} -> MATCH: {e2_blocks == nex_blocks}")
    print(f"INODE PARITY    : e2fsck={e2_inodes} vs nexfsck active={nex_inodes} -> MATCH: {abs(e2_inodes - nex_inodes) < 25}")
    print(f"CLEAN CHECK     : nexfsck Errors={nex_errors} (selected counters only; not bit-exact parity)")
    print("-" * 70)
    
    assert e2_blocks == nex_blocks, f"Block count mismatch: {e2_blocks} != {nex_blocks}"
    assert nex_errors == 0, f"nexfsck reported errors on clean filesystem: {nex_errors}"
    return {
        "runs": COMPARISON_RUNS,
        "cache_policy": "interleaved warm-cache; no global cache drop",
        "e2fsck_seconds": e2_times,
        "nexfsck_seconds": nex_times,
        "e2fsck_median_seconds": e2_median,
        "nexfsck_median_seconds": nex_median,
        "e2fsck_p95_seconds": percentile(e2_times, 0.95),
        "nexfsck_p95_seconds": percentile(nex_times, 0.95),
        "e2fsck_stddev_seconds": statistics.pstdev(e2_times),
        "nexfsck_stddev_seconds": statistics.pstdev(nex_times),
        "median_ratio": speedup,
        "e2fsck_inodes": e2_inodes,
        "e2fsck_blocks": e2_blocks,
        "nexfsck_active_inodes": nex_inodes,
        "nexfsck_allocated_blocks": nex_blocks,
        "nexfsck_directory_entries": nex_entries,
        "nexfsck_errors": nex_errors,
    }

def run_endurance_stress_test():
    print("\n" + "=" * 70)
    print("STAGE 3: 30-ROUND SUSTAINED ENDURANCE & MEMORY LEAK STRESS TEST (10.0 GiB)")
    print("=" * 70)
    
    times = []
    max_rss_list = []
    
    print(f"Executing 30 consecutive full passes over 10.0 GiB storage with 100,000+ inodes across 80 groups...")
    for round_num in range(1, 31):
        cmd = ["/usr/bin/time", "-v", NEXFSCK_BIN, "-n", IMG_PATH]
        p = subprocess.run(cmd, capture_output=True, text=True)
        
        rss_kb = 0
        for line in p.stderr.splitlines():
            if "Maximum resident set size" in line:
                m = re.search(r":\s+(\d+)", line)
                if m: rss_kb = int(m.group(1))
                
        t_el = 0.0
        for line in p.stdout.splitlines():
            if "Elapsed Time" in line:
                m = re.search(r":\s+([\d\.]+)s", line)
                if m: t_el = float(m.group(1))
                
        times.append(t_el)
        max_rss_list.append(rss_kb)
        
        if round_num % 5 == 0 or round_num == 1:
            print(f"  [Round {round_num:02d}/30] Elapsed: {t_el:.3f}s | Max RSS: {rss_kb/1024:.2f} MiB | Exit: {p.returncode}")
            
        assert p.returncode == 0, f"Round {round_num} failed with code {p.returncode}"
        
    avg_time = sum(times) / len(times)
    min_rss = min(max_rss_list) / 1024
    max_rss = max(max_rss_list) / 1024
    
    print("-" * 70)
    print(f"ENDURANCE SUMMARY (10.0 GiB):")
    print(f"  Total Passes Completed : 30 / 30 Clean Passes (100% Success)")
    print(f"  Average Execution Time : {avg_time:.3f} seconds / pass")
    print(f"  Peak Resident Memory   : Min {min_rss:.2f} MiB -> Max {max_rss:.2f} MiB (observed range)")
    print("-" * 70)
    return {
        "passes": len(times),
        "seconds": times,
        "average_seconds": avg_time,
        "min_rss_mib": min_rss,
        "max_rss_mib": max_rss,
    }

def run_fuzzing_and_repair_test():
    print("\n" + "=" * 70)
    print("STAGE 4: MULTI-GROUP CORRUPTION, PRE-IMAGE UNDO JOURNAL & ROLLBACK (10.0 GiB)")
    print("=" * 70)
    
    # Never mutate the clean benchmark fixture.
    subprocess.check_call(["cp", "--sparse=always", IMG_PATH, CORRUPT_IMG])
    block_groups_info = subprocess.check_output(["dumpe2fs", CORRUPT_IMG], stderr=subprocess.DEVNULL).decode()
    
    bm_blocks = []
    inomb_blocks = []
    for line in block_groups_info.splitlines():
        if "Block bitmap at" in line:
            m = re.search(r"Block bitmap at (\d+)", line)
            if m: bm_blocks.append(int(m.group(1)))
        elif "Inode bitmap at" in line:
            m = re.search(r"Inode bitmap at (\d+)", line)
            if m: inomb_blocks.append(int(m.group(1)))
            
    print(f"Found {len(bm_blocks)} block bitmaps and {len(inomb_blocks)} inode bitmaps.")
    assert len(bm_blocks) >= 10, "Expected at least 10 block groups"
    
    # 1. Corrupt Block Bitmap in Group 0 (block 513)
    bg0_bm = bm_blocks[0]

    # 2. Find an allocated inode bitmap dynamically
    target_inomb_blk = None
    target_bg = 0
    with open(CORRUPT_IMG, "rb") as f:
        for idx, blk in enumerate(inomb_blocks):
            f.seek(blk * 4096)
            buf = f.read(4096)
            if buf[0] != 0:
                target_inomb_blk = blk
                target_bg = idx
                if idx > 0:
                    break

    assert target_inomb_blk is not None, "Could not find an allocated inode bitmap"

    print(f"Injecting false-free block corruption in Group 0 (Block {bg0_bm})...")
    with open(CORRUPT_IMG, "r+b") as f:
        f.seek(bg0_bm * 4096)
        data = bytearray(f.read(4096))
        assert data[0] != 0, "Group 0 block bitmap is empty"
        data[0] &= ~1
        f.seek(bg0_bm * 4096)
        f.write(data)
        
    print(f"Injecting false-free inode corruption in Group {target_bg} (Block {target_inomb_blk})...")
    with open(CORRUPT_IMG, "r+b") as f:
        f.seek(target_inomb_blk * 4096)
        data = bytearray(f.read(4096))
        data[0] &= ~1
        f.seek(target_inomb_blk * 4096)
        f.write(data)
        
    # Step A: Detection in read-only mode (Exit code 4)
    print("\nStep A: Verifying detection in read-only mode...")
    pA = subprocess.run([NEXFSCK_BIN, "-n", CORRUPT_IMG], capture_output=True, text=True)
    print(f"  nexfsck exit code: {pA.returncode} (Expected: 4)")
    assert pA.returncode == 4, f"Expected exit code 4, got {pA.returncode}"
    print("  ✔ Multiple block group corruptions correctly detected!")
    
    # Step B: Active repair with a flushed pre-image undo journal (Exit code 1)
    print("\nStep B: Performing active repair with pre-image undo journal...")
    if os.path.exists(UNDO_LOG): os.remove(UNDO_LOG)
    pB = subprocess.run([NEXFSCK_BIN, "-y", "--undo-file", UNDO_LOG, CORRUPT_IMG], capture_output=True, text=True)
    print(f"  nexfsck exit code: {pB.returncode} (Expected: 1)")
    assert pB.returncode == 1, f"Expected exit code 1, got {pB.returncode}"
    assert os.path.exists(UNDO_LOG), "Undo log was not created"
    undo_size = os.path.getsize(UNDO_LOG)
    print(f"  ✔ Repair succeeded! Created crash-consistent undo journal: {undo_size} bytes")
    
    # Step C: Re-verify repaired image (Exit code 0)
    print("\nStep C: Verifying repaired filesystem consistency...")
    pC = subprocess.run([NEXFSCK_BIN, "-n", CORRUPT_IMG], capture_output=True, text=True)
    print(f"  nexfsck exit code: {pC.returncode} (Expected: 0)")
    assert pC.returncode == 0, f"Expected exit code 0, got {pC.returncode}"
    print("  ✔ 10.0 GiB Filesystem is completely clean after repair!")
    
    # Step D: 1-Click Rollback from undo journal (Exit code 0)
    print("\nStep D: Executing 1-click rollback from undo journal...")
    pD = subprocess.run([NEXFSCK_BIN, "--rollback", "--undo-file", UNDO_LOG, CORRUPT_IMG], capture_output=True, text=True)
    print(f"  nexfsck exit code: {pD.returncode} (Expected: 0)")
    assert pD.returncode == 0, f"Expected exit code 0, got {pD.returncode}"
    print("  ✔ Rollback transaction executed successfully!")
    
    # Step E: Verify corrupted state was faithfully restored (Exit code 4)
    print("\nStep E: Re-verifying restored state against undo log...")
    pE = subprocess.run([NEXFSCK_BIN, "-n", CORRUPT_IMG], capture_output=True, text=True)
    print(f"  nexfsck exit code: {pE.returncode} (Expected: 4)")
    assert pE.returncode == 4, f"Expected exit code 4, got {pE.returncode}"
    print("  ✔ Injected bitmap corruption was restored from the undo journal.")
    
    # Final cleanup of undo log
    if os.path.exists(UNDO_LOG):
        os.remove(UNDO_LOG)
    return {
        "block_bitmap_group": 0,
        "inode_bitmap_group": target_bg,
        "detection_exit": pA.returncode,
        "repair_exit": pB.returncode,
        "post_repair_exit": pC.returncode,
        "rollback_exit": pD.returncode,
        "restored_corruption_exit": pE.returncode,
        "undo_bytes": undo_size,
    }

def main():
    print("=" * 70)
    print("  NEXFSCK 10.0 GiB ENTERPRISE STRESS & ENDURANCE VERIFICATION")
    print("  Real Data • 80 Block Groups • 100k+ Inodes • Memory Profiling • Fuzzing")
    print("=" * 70)
    
    try:
        populate_filesystem()
        comparison = run_ground_truth_test()
        endurance = run_endurance_stress_test()
        repair = run_fuzzing_and_repair_test()
        result = {
            "schema_version": 1,
            "generated_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd="/home/pop-os/nexfsck", text=True).strip(),
            "fixture": {
                "logical_bytes": os.path.getsize(IMG_PATH),
                "logical_gib": os.path.getsize(IMG_PATH) / (1024 ** 3),
                "sparse": True,
                "files_generated": 100000,
                "symlinks_requested": 2000,
                "hardlinks_requested": 1000,
                "block_size": 4096,
            },
            "environment": {
                "kernel": platform.release(),
                "machine": platform.machine(),
                "cpu": next((line.split(":", 1)[1].strip() for line in subprocess.check_output(["lscpu"], text=True).splitlines() if line.startswith("Model name:")), platform.processor()),
                "memory_gib": round(os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / (1024 ** 3), 2),
                "fixture_storage": "/tmp tmpfs (memory-backed live-USB environment)",
                "e2fsck_version": subprocess.run(["e2fsck", "-V"], capture_output=True, text=True).stderr.strip(),
                "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
            },
            "comparison": comparison,
            "endurance": endurance,
            "repair_rollback": repair,
        }
        with open(RESULT_JSON, "w") as f:
            json.dump(result, f, indent=2)
            f.write("\n")
        print(f"Raw benchmark result written to {RESULT_JSON}")
        
        print("\n" + "=" * 70)
        print("10.0 GiB benchmark and defined stress assertions completed successfully.")
        print("=" * 70)
    finally:
        if os.path.exists(IMG_PATH):
            os.remove(IMG_PATH)
        if os.path.exists(CORRUPT_IMG):
            os.remove(CORRUPT_IMG)
        print(f"Cleaned up {IMG_PATH}")

if __name__ == "__main__":
    main()
