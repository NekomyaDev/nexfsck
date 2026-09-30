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

## 🛡️ Rigorous Engineering & Safety Audits

File system repair requires zero tolerance for data corruption. `nexfsck` incorporates solutions for **29 critical edge cases and failure modes**:

1. **Non-ECC VRAM Bit-Flip Shield:** Consumer GPUs lack ECC memory; `nexfsck` uses a **Dual-Path Gate** where the GPU only filters candidate anomalies, and the CPU host verifies every decision before touching disk.
2. **Rescue & Initramfs Independence:** Zero mandatory GPU runtime dependencies. `nexfsck` dynamically probes GPU drivers (`dlopen`) and seamlessly falls back to optimized CPU SIMD in recovery environments.
3. **Hierarchical Bisection on I/O Errors:** If a single bad sector causes a 64MB `io_uring` batch read to fail (`-EIO`), `nexfsck` bisects the chunk down to 4KB sectors to isolate the fault without discarding healthy inodes.
4. **Cache Incoherency Prevention:** Enforces `ioctl(BLKFLSBUF)` and `syncfs` to eliminate stale sector reads against dirty kernel page caches.
5. **Full ext4 Feature Parity:** Native support for `flex_bg`, `meta_bg`, `dir_index` (H-Tree with seed), `64bit`, `inline_data`, `bigalloc` clusters, `orphan_file` (Linux 5.15+), `fscrypt`, `casefold`, and Multi-Mount Protection (`MMP`).
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

# Run verification in read-only analysis mode
./target/release/nexfsck --read-only /dev/nvme0n1p1
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
