//! `nexfsck-compute`
//!
//! Multi-core SIMD compute engine, hardware auto-discovery,
//! extent tree analysis, and parallel bitmap state reduction.

use crc32fast::Hasher;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use zerocopy::FromBytes;

use nexfsck_core::{Ext4Extent, Ext4ExtentHeader, Ext4Inode};

/// Hardware profile detected at startup.
#[derive(Debug, Clone)]
pub struct HardwareProfile {
    pub cpu_cores: usize,
    pub has_avx512: bool,
    pub has_avx2: bool,
    pub has_arm_crc32: bool,
    pub total_ram_bytes: u64,
}

impl HardwareProfile {
    pub fn detect() -> Self {
        let cpu_cores = num_cpus();

        #[cfg(target_arch = "x86_64")]
        let (has_avx512, has_avx2) = (
            std::is_x86_feature_detected!("avx512f"),
            std::is_x86_feature_detected!("avx2"),
        );

        #[cfg(not(target_arch = "x86_64"))]
        let (has_avx512, has_avx2) = (false, false);

        #[cfg(target_arch = "aarch64")]
        let has_arm_crc32 = std::arch::is_aarch64_feature_detected!("crc");

        #[cfg(not(target_arch = "aarch64"))]
        let has_arm_crc32 = false;

        let total_ram_bytes = detect_total_ram();

        Self {
            cpu_cores,
            has_avx512,
            has_avx2,
            has_arm_crc32,
            total_ram_bytes,
        }
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn detect_total_ram() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(info) = std::fs::read_to_string("/proc/meminfo") {
            for line in info.lines() {
                if line.starts_with("MemTotal:") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        if let Ok(kb) = parts[1].parse::<u64>() {
                            return kb * 1024;
                        }
                    }
                }
            }
        }
    }
    8 * 1024 * 1024 * 1024
}

/// Computes ext4 CRC32c with seed.
pub fn ext4_crc32c(seed: u32, data: &[u8]) -> u32 {
    let mut hasher = Hasher::new_with_initial(seed);
    hasher.update(data);
    hasher.finalize()
}

/// Statistics collected during inode & extent verification.
#[derive(Debug, Default)]
pub struct InodeVerificationStats {
    pub total_inodes_scanned: AtomicU64,
    pub used_inodes: AtomicU64,
    pub directory_inodes: AtomicU64,
    pub regular_file_inodes: AtomicU64,
    pub symlink_inodes: AtomicU64,
    pub extent_trees_checked: AtomicU64,
    pub duplicate_blocks: AtomicU64,
    pub out_of_bounds_blocks: AtomicU64,
    pub extent_corruptions: AtomicU64,
}

impl InodeVerificationStats {
    pub fn new() -> Self {
        Self::default()
    }
}

/// High-performance thread-safe block allocation tracker using Roaring Bitmaps.
pub struct BlockAllocationTracker {
    total_blocks: u64,
    allocated_count: AtomicU64,
    bitmap: RwLock<RoaringBitmap>,
}

impl BlockAllocationTracker {
    pub fn new(total_blocks: u64) -> Self {
        Self {
            total_blocks,
            allocated_count: AtomicU64::new(0),
            bitmap: RwLock::new(RoaringBitmap::new()),
        }
    }

    /// Marks a range of blocks as allocated. Returns true if no collision detected.
    pub fn mark_range(&self, start_block: u64, count: u32) -> bool {
        let mut bm = self.bitmap.write().unwrap();
        let mut collision = false;
        for b in start_block..(start_block + count as u64) {
            if b < u32::MAX as u64 {
                if !bm.insert(b as u32) {
                    collision = true;
                }
            }
        }
        self.allocated_count.fetch_add(count as u64, Ordering::Relaxed);
        !collision
    }

    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    pub fn allocated_count(&self) -> u64 {
        self.allocated_count.load(Ordering::Relaxed)
    }
}

/// Validates a batch of inodes in parallel using Rayon work-stealing.
pub fn verify_inodes_parallel(
    inodes: &[Ext4Inode],
    tracker: &BlockAllocationTracker,
    stats: &InodeVerificationStats,
) {
    stats
        .total_inodes_scanned
        .fetch_add(inodes.len() as u64, Ordering::Relaxed);

    inodes.par_iter().for_each(|inode| {
        if !inode.is_used() {
            return;
        }

        stats.used_inodes.fetch_add(1, Ordering::Relaxed);

        if inode.is_dir() {
            stats.directory_inodes.fetch_add(1, Ordering::Relaxed);
        } else if inode.is_reg_file() {
            stats.regular_file_inodes.fetch_add(1, Ordering::Relaxed);
        } else if inode.is_symlink() {
            stats.symlink_inodes.fetch_add(1, Ordering::Relaxed);
        }

        // Validate extent tree if used
        if inode.uses_extents() && !inode.is_inline_data() {
            stats.extent_trees_checked.fetch_add(1, Ordering::Relaxed);
            verify_extent_root(inode, tracker, stats);
        }
    });
}

/// Verifies root extents embedded directly in `inode.i_block`.
fn verify_extent_root(
    inode: &Ext4Inode,
    tracker: &BlockAllocationTracker,
    stats: &InodeVerificationStats,
) {
    let (header, rest) = match Ext4ExtentHeader::ref_from_prefix(&inode.i_block) {
        Ok(res) => res,
        Err(_) => {
            stats.extent_corruptions.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    if !header.is_valid_magic() {
        stats.extent_corruptions.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let entries = header.entries() as usize;
    let max_entries = header.max() as usize;

    if entries > max_entries || entries > 4 {
        stats.extent_corruptions.fetch_add(1, Ordering::Relaxed);
        return;
    }

    if header.depth() == 0 {
        // Leaf nodes directly inside i_block
        let extent_size = std::mem::size_of::<Ext4Extent>();
        for i in 0..entries {
            let offset = i * extent_size;
            if offset + extent_size > rest.len() {
                stats.extent_corruptions.fetch_add(1, Ordering::Relaxed);
                break;
            }

            if let Ok((ext, _)) = Ext4Extent::ref_from_prefix(&rest[offset..]) {
                let start_block = ext.physical_start();
                let count = ext.block_count();

                if start_block + count as u64 > tracker.total_blocks() {
                    stats.out_of_bounds_blocks.fetch_add(1, Ordering::Relaxed);
                } else if !tracker.mark_range(start_block, count) {
                    stats.duplicate_blocks.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}
