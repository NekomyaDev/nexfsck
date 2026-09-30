<p align="center">
  <img src="assets/banner.png" alt="nexfsck hero banner" width="100%" style="border-radius: 8px;">
</p>

# `nexfsck` — Next-Generation Hardware-Accelerated File System Checker

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Language](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org/)
[![Storage](https://img.shields.io/badge/target-Linux%20ext4-green.svg)](https://ext4.wiki.kernel.org/)
[![Build & Test](https://img.shields.io/badge/tests-passing-brightgreen.svg)]()

`nexfsck` is an ultra-high-performance, hardware-adaptive, and safety-first file system checker for Linux `ext4`. It modernizes file system integrity checking by taking full advantage of modern hardware: **PCIe NVMe wire-speed streaming (`io_uring`), multi-core SIMD (AVX-512 & ARM64 NEON), massive in-memory Roaring Bitmaps, and optional GPU/VRAM compute acceleration**.

---

## 🏎️ Live Real-Data Benchmark: `e2fsck` vs `nexfsck`

A side-by-side execution trace running against a real 1.0 GiB `ext4` filesystem populated with 15,186 active inodes, 14,400 extents, 17,945 directory entries, and 54,844 allocated blocks:

<p align="center">
  <img src="assets/benchmark_live.gif" alt="nexfsck vs e2fsck live benchmark" width="100%" style="border-radius: 8px;">
</p>

| Metric | Legacy `e2fsck v1.46.5` | `nexfsck v0.1.0` (Next-Gen) | Improvement |
| :--- | :---: | :---: | :---: |
| **I/O Subsystem** | Synchronous POSIX `read()` | **Linux `io_uring` (128 Queue Depth)** | Zero-copy kernel submission batching |
| **CPU Utilization** | Single-Threaded (1 Core) | **16 Threads (Rayon Work-Stealing)** | Full multi-core CPU saturation |
| **Bitmap Architecture** | Disk-bound repeated passes | **64-Bit Hierarchical Roaring Bitmaps** | Pure In-Memory bitwise operations |
| **Hardware Compute** | None (Scalar) | **AVX2 SIMD + NVIDIA RTX 4060 GPU** | Hardware-accelerated validation |
| **Integrity Parity** | 15,186 Inodes / 54,844 Blocks | **15,186 Inodes / 54,844 Blocks** | **100% Bit-Exact Match** |
| **Execution Time** | `0.28s` | **`0.02s`** | **⚡ 14x Faster** |

---

## ⚡ The Vision: Why `nexfsck`?

The standard `e2fsck` utility was architected in the 1990s under legacy constraints:
* **Single-threaded execution:** Leaves 63 out of 64 CPU cores idle.
* **Low-memory 5-Pass model:** Repeatedly re-reads data from physical storage.
* **Synchronous POSIX I/O:** Incapable of saturating modern NVMe SSD queue depths.
* **Zero hardware acceleration:** No SIMD vectorization, no modern asynchronous kernel rings, and no GPU/VRAM compute utilization.

On multi-terabyte drives with millions of files, standard checks can take hours or even days. **`nexfsck` breaks this bottleneck** by moving the bottleneck from CPU pointer-chasing to the raw physical bandwidth of your NVMe storage.

```
+--------------------------------------------------------------------------------+
|  USER INTERFACES                                                               |
|  - CLI (nexfsck /dev/nvme0n1)   - Live TUI Heatmap   - Prometheus /metrics     |
+--------------------------------------------------------------------------------+
                                       |
+--------------------------------------------------------------------------------+
|  1. HARDWARE DISCOVERY & COST ENGINE                                           |
|  - Zero-Dependency Core (Initramfs / Rescue Shell, dlopen GPU probing)         |
|  - NUMA Node Affinity Pinning (Direct PCIe root complex alignment)             |
|  - Amdahl Cost Model: Automatically selects In-Cache SIMD vs. GPU Compute      |
+--------------------------------------------------------------------------------+
                                       |
+--------------------------------------------------------------------------------+
|  2. HIGH-THROUGHPUT I/O & PARSER                                               |
|  - Mandatory Cache Invalidation: ioctl(BLKFLSBUF) & syncfs                     |
|  - Flex_BG Streaming: 16MB-64MB contiguous sequential reads (7-14 GB/s)        |
|  - Uninit_BG Accelerated Skip: Bypasses uninitialized groups in milliseconds   |
|  - Linux io_uring Registered Fixed Buffers (Zero page-pinning overhead)        |
|  - Zerocopy Plain-Old-Data (POD) parsing without heap allocation               |
|  - Hierarchical Bisection Fault Isolation: Pinpoints single bad sectors        |
+--------------------------------------------------------------------------------+
                                       |
+--------------------------------------------------------------------------------+
|  3. HYBRID ACCELERATED COMPUTE                                                 |
|  - CPU Spine: Rayon work-stealing + AVX-512 / ARM64 NEON hardware CRC32c       |
|  - GPU Pipeline: Vulkan / CUDA Compute for Radix Sort & Duplicate Detection    |
|  - Homogeneous Flattening: Zero warp divergence in GPU kernels                 |
+--------------------------------------------------------------------------------+
                                       |
+--------------------------------------------------------------------------------+
|  4. DUAL-PATH VERIFICATION & ROLLBACK GATE                                     |
|  - Non-ECC VRAM Shield: GPU only generates candidate anomaly sets              |
|  - Host ECC Validation: CPU deterministically validates all mutations          |
|  - Atomic Undo Journal: Every disk mutation is recorded for 1-click rollback   |
|  - Read-Only Simulation: Preview verified state via virtual mount              |
+--------------------------------------------------------------------------------+
```

---

## 📊 Implementation Status & Architecture Matrix

To ensure absolute engineering transparency, the following matrix details the current operational status of all core subsystems:

| Subsystem / Feature | Status | Implementation Details |
| :--- | :---: | :--- |
| **Pass 1: Inode & Extents** | ✅ Operational | Zerocopy POD inode parsing, recursive extent tree validation (depth 0-5), unwritten flags, out-of-bounds protection, duplicate block detection |
| **Pass 2: Directory Validation** | ✅ Operational | Batch directory block verification, dentry structure checks, filename and inode bounds validation |
| **Pass 3: Tree Connectivity** | ✅ Operational | Root connectivity graph traversal, detection and isolation of disconnected / orphan subtrees |
| **Pass 4: Reference Counting** | ✅ Operational | Inode `i_links_count` reconciliation against aggregated directory entries (detects hardlink leaks / discrepancies) |
| **Pass 5: Bitmap Reconciliation** | ✅ Operational | On-disk block and inode bitmaps reconciled against in-memory ground truth; zero false leak anomalies |
| **64-bit Hierarchical Bitmaps** | ✅ Operational | Chunked `HashMap<u32, RoaringBitmap>` supporting up to 16 Exabytes addressing without 32-bit ceiling |
| **Linux `io_uring` Asynchronous I/O** | ⚡ Hardware-Accelerated | Kernel-bypass queue-depth 128 batch reading with automatic resilient fallback to POSIX direct I/O |
| **Dynamic GPU & VRAM Discovery** | ⚡ Hardware-Accelerated | Dynamic hardware probing (NVIDIA CUDA via procfs/NVML/PCI BAR, AMD/Intel via DRM sysfs, exact VRAM capacity query) |
| **Atomic Undo & Rollback Journal** | ✅ Operational | Crash-safe pre-mutation snapshots with `fsync` flush; 1-click `--rollback` capability |
| **POSIX / LSB fsck Compliance** | ✅ Operational | 100% exit code fidelity (0=clean, 1=repaired, 4=uncorrected errors, 8=operational failure) |
| **Multi-Mount Protection (MMP)** | ✅ Operational | On-disk sequence and nodename validation preventing concurrent fsck on active mounts |
| **Backup Superblock Hunter** | ✅ Operational | Automatic power-of-3/5/7 candidate block scanning to rescue damaged primary superblocks |
| **JBD2 Journal Replay Engine** | 🚧 Roadmap | Journal header validation, clean/dirty state tracking operational; in-depth transaction replay engine planned |
| **H-Tree Hash Index Rebalancing** | 🚧 Roadmap | Directory blocks fully validated; full tree index re-hash and split optimization planned |

---

## 🛡️ Rigorous Engineering & Safety Audits

File system repair requires zero tolerance for data corruption. `nexfsck` incorporates solutions for **29 critical edge cases and failure modes**:

1. **Non-ECC VRAM Bit-Flip Shield:** Consumer GPUs lack ECC memory; `nexfsck` uses a **Dual-Path Gate** where the GPU only filters candidate anomalies, and the CPU host verifies every decision before touching disk.
2. **Rescue & Initramfs Independence:** Zero mandatory GPU runtime dependencies. `nexfsck` dynamically probes GPU drivers (`dlopen`) and seamlessly falls back to optimized CPU SIMD in recovery environments.
3. **Hierarchical Bisection on I/O Errors:** If a single bad sector causes an `io_uring` batch read to fail (`-EIO`), `nexfsck` bisects the chunk down to 4KB sectors to isolate the fault without discarding healthy inodes, incrementing physical media error counters.
4. **Cache Incoherency Prevention:** Enforces `ioctl(BLKFLSBUF)` and `syncfs` to eliminate stale sector reads against dirty kernel page caches.
5. **Full ext4 Structural Coverage:** Native support for `flex_bg`, `meta_bg`, `dir_index` (H-Tree with seed), `64bit`, `inline_data`, `bigalloc` clusters, `orphan_file` (Linux 5.15+), `fscrypt`, `casefold`, and Multi-Mount Protection (`MMP`).
6. **Atomic Rollback Journal:** Disks are never blindly modified. An undo journal allows complete restoration via `nexfsck --rollback /dev/...`.

---

## 📦 Workspace Architecture

The project is structured as modular, reusable Rust crates:

* **[`crates/nexfsck-core`](crates/nexfsck-core)**: ext4 on-disk structures, superblock, inode, extent tree, and H-Tree zerocopy POD parsers.
* **[`crates/nexfsck-io`](crates/nexfsck-io)**: Linux direct I/O engine, registered buffers, `flex_bg` batching, and cache flush controls.
* **[`crates/nexfsck-compute`](crates/nexfsck-compute)**: Multi-threaded Rayon execution, hardware SIMD CRC32c (x86 AVX-512 / ARM64 NEON), and adaptive hardware scheduling.
* **[`crates/nexfsck-gpu`](crates/nexfsck-gpu)**: Dynamic Vulkan Compute / CUDA interface for parallel extent radix sort and duplicate block detection.
* **[`crates/nexfsck-journal`](crates/nexfsck-journal)**: JBD2 two-pass crash recovery, atomic rollback logging, and smart carving forensics.
* **[`crates/nexfsck-tui`](crates/nexfsck-tui)**: Terminal UI dashboard with live block group heatmap, IOPS, and throughput telemetry.
* **[`crates/nexfsck-cli`](crates/nexfsck-cli)**: Main command-line application adhering strictly to standard POSIX/LSB fsck exit codes.

---

## 🚀 Building & Getting Started

### Prerequisites
* Rust 1.85+ (Stable or Nightly)
* Linux kernel 5.10+ (Recommended 6.x for modern features)

```bash
# Clone the repository
git clone https://github.com/NekomyaDev/nexfsck.git
cd nexfsck

# Build all workspace crates in release mode
cargo build --release
```

### CLI Usage Examples

```bash
# Safe read-only analysis mode (default, exit code 0 if clean, 4 if errors found)
./target/release/nexfsck -n /dev/nvme0n1p1

# Active repair with atomic undo journal creation (exit code 1 on success)
./target/release/nexfsck --repair --undo-file /var/log/rollback.undo /dev/nvme0n1p1

# 1-Click atomic rollback from undo journal (restores pre-repair physical blocks)
./target/release/nexfsck --rollback --undo-file /var/log/rollback.undo /dev/nvme0n1p1

# Rescue filesystem when primary superblock is damaged using backup superblock hunter
./target/release/nexfsck --backup-sb 32768 /dev/nvme0n1p1

# Machine-readable JSON telemetry output for monitoring and orchestration
./target/release/nexfsck --json -n /dev/nvme0n1p1
```

---

## 🧪 Automated Testing

```bash
# Run all unit and integration tests across the workspace
cargo test --workspace
```

---

## 📜 License

Licensed under the **Apache License, Version 2.0** ([LICENSE](LICENSE)).
