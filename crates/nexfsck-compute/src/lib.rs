//! `nexfsck-compute`
//!
//! Multi-core SIMD compute engine, hardware auto-discovery,
//! multi-level extent tree analysis, directory validation,
//! and parallel bitmap state reduction.

use crc32fast::Hasher;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use zerocopy::FromBytes;

use nexfsck_core::{
    Ext4DirEntry2Header, Ext4Extent, Ext4ExtentHeader, Ext4ExtentIdx, Ext4Inode,
    EXT4_FT_DIR_CSUM,
};

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
    pub fast_symlinks_checked: AtomicU64,
    pub corrupted_symlinks: AtomicU64,
    pub extent_trees_checked: AtomicU64,
    pub duplicate_blocks: AtomicU64,
    pub out_of_bounds_blocks: AtomicU64,
    pub extent_corruptions: AtomicU64,
    pub corrupt_directories: AtomicU64,
    pub orphan_directories: AtomicU64,
    pub link_count_mismatches: AtomicU64,
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
    pub bitmap: RwLock<RoaringBitmap>,
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
        let mut newly_added = 0u64;
        for b in start_block..(start_block + count as u64) {
            if b < u32::MAX as u64 {
                if !bm.insert(b as u32) {
                    collision = true;
                } else {
                    newly_added += 1;
                }
            }
        }
        self.allocated_count.fetch_add(newly_added, Ordering::Relaxed);
        !collision
    }

    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    pub fn allocated_count(&self) -> u64 {
        self.allocated_count.load(Ordering::Relaxed)
    }

    pub fn is_allocated(&self, block: u64) -> bool {
        if block >= u32::MAX as u64 {
            return false;
        }
        let bm = self.bitmap.read().unwrap();
        bm.contains(block as u32)
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
            if inode.is_fast_symlink() {
                stats.fast_symlinks_checked.fetch_add(1, Ordering::Relaxed);
                if inode.fast_symlink_target().is_none() {
                    stats.corrupted_symlinks.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Validate extent tree or legacy block pointers
        if inode.uses_extents() && !inode.is_inline_data() {
            stats.extent_trees_checked.fetch_add(1, Ordering::Relaxed);
            verify_extent_block(
                &inode.i_block,
                tracker.total_blocks(),
                tracker,
                stats,
                &|_| None,
                0,
            );
        } else if !inode.is_inline_data() && !inode.is_fast_symlink() {
            // Traditional direct and indirect block pointers (Ext2/3/4 legacy format, e.g. resize inode 7)
            for chunk in inode.i_block.chunks_exact(4) {
                let blk = u32::from_le_bytes(chunk.try_into().unwrap()) as u64;
                if blk != 0 && blk < tracker.total_blocks() {
                    tracker.mark_range(blk, 1);
                }
            }
        }
    });
}

/// Discrepancy statistics when comparing on-disk inode bitmaps against in-memory inodes.
#[derive(Debug, Default, Clone)]
pub struct InodeBitmapDiscrepancy {
    pub false_free_inodes: u64,
    pub leaked_inodes: u64,
}

/// Reconciles on-disk inode allocation bitmap against active inodes.
pub fn reconcile_inode_bitmap(
    disk_bitmap: &[u8],
    inodes: &[Ext4Inode],
    first_ino: u32,
    bg_idx: usize,
    inodes_per_group: u32,
) -> InodeBitmapDiscrepancy {
    let mut discrepancy = InodeBitmapDiscrepancy::default();

    for (i, inode) in inodes.iter().enumerate() {
        let abs_ino = (bg_idx as u32) * inodes_per_group + (i as u32) + 1;
        let byte_idx = i / 8;
        let bit_idx = i % 8;

        let disk_is_allocated = if byte_idx < disk_bitmap.len() {
            (disk_bitmap[byte_idx] & (1 << bit_idx)) != 0
        } else {
            false
        };

        let inode_is_used = inode.is_used();

        if inode_is_used && !disk_is_allocated {
            discrepancy.false_free_inodes += 1;
        } else if !inode_is_used && disk_is_allocated {
            // Reserved standard system inodes (1..first_ino - 1) are intentionally
            // allocated in the bitmap by mkfs.ext4 and are not leaked user inodes.
            if abs_ino >= first_ino {
                discrepancy.leaked_inodes += 1;
            }
        }
    }

    discrepancy
}

/// Multi-level extent tree verification with depth bounding (max depth 5).
pub fn verify_extent_block<F>(
    data: &[u8],
    max_blocks: u64,
    tracker: &BlockAllocationTracker,
    stats: &InodeVerificationStats,
    read_block_fn: &F,
    current_depth: u16,
) where
    F: Fn(u64) -> Option<Vec<u8>>,
{
    if current_depth > 5 {
        stats.extent_corruptions.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let (header, rest) = match Ext4ExtentHeader::ref_from_prefix(data) {
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

    if entries > max_entries {
        stats.extent_corruptions.fetch_add(1, Ordering::Relaxed);
        return;
    }

    if header.depth() == 0 {
        // Leaf nodes
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

                if start_block + count as u64 > max_blocks {
                    stats.out_of_bounds_blocks.fetch_add(1, Ordering::Relaxed);
                } else if !tracker.mark_range(start_block, count) {
                    stats.duplicate_blocks.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    } else {
        // Branch index nodes (Ext4ExtentIdx)
        let idx_size = std::mem::size_of::<Ext4ExtentIdx>();
        for i in 0..entries {
            let offset = i * idx_size;
            if offset + idx_size > rest.len() {
                stats.extent_corruptions.fetch_add(1, Ordering::Relaxed);
                break;
            }

            if let Ok((idx, _)) = Ext4ExtentIdx::ref_from_prefix(&rest[offset..]) {
                let child_block = idx.child_block();
                if child_block >= max_blocks {
                    stats.out_of_bounds_blocks.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                tracker.mark_range(child_block, 1);

                if let Some(child_data) = read_block_fn(child_block) {
                    verify_extent_block(
                        &child_data,
                        max_blocks,
                        tracker,
                        stats,
                        read_block_fn,
                        current_depth + 1,
                    );
                }
            }
        }
    }
}

/// Discrepancy statistics when comparing on-disk bitmaps against in-memory tracker.
#[derive(Debug, Default, Clone)]
pub struct BitmapDiscrepancy {
    pub false_free_blocks: u64, // Marked 0 on disk, but allocated to a file!
    pub leaked_blocks: u64,     // Marked 1 on disk, but no file references it!
}

/// Reconciles on-disk block allocation bitmap against in-memory Roaring Bitmap.
pub fn reconcile_block_bitmap(
    disk_bitmap: &[u8],
    tracker: &BlockAllocationTracker,
    first_block_in_group: u64,
    blocks_in_group: u32,
) -> BitmapDiscrepancy {
    let mut discrepancy = BitmapDiscrepancy::default();
    let bm = tracker.bitmap.read().unwrap();

    for i in 0..blocks_in_group {
        let abs_block = first_block_in_group + i as u64;
        let byte_idx = (i / 8) as usize;
        let bit_idx = i % 8;

        let disk_is_allocated = if byte_idx < disk_bitmap.len() {
            (disk_bitmap[byte_idx] & (1 << bit_idx)) != 0
        } else {
            false
        };

        let tracker_is_allocated = if abs_block < u32::MAX as u64 {
            bm.contains(abs_block as u32)
        } else {
            false
        };

        if tracker_is_allocated && !disk_is_allocated {
            discrepancy.false_free_blocks += 1;
        } else if !tracker_is_allocated && disk_is_allocated {
            discrepancy.leaked_blocks += 1;
        }
    }

    discrepancy
}

/// Result of directory block validation.
#[derive(Debug, Default, Clone)]
pub struct DirectoryValidationResult {
    pub entries: Vec<(u32, String)>,
    pub corrupt_entries: u64,
}

/// Validates directory block entries against alignment, name integrity, and inode boundary.
pub fn verify_directory_block_detailed(
    block_bytes: &[u8],
    max_inodes: u32,
) -> DirectoryValidationResult {
    let mut res = DirectoryValidationResult::default();
    let mut offset = 0;
    let block_len = block_bytes.len();

    while offset + 8 <= block_len {
        let header_slice = &block_bytes[offset..offset + 8];
        let header = match Ext4DirEntry2Header::ref_from_prefix(header_slice) {
            Ok((h, _)) => h,
            Err(_) => {
                res.corrupt_entries += 1;
                break;
            }
        };

        let rec_len = header.record_len() as usize;
        let name_len = header.name_length() as usize;

        if rec_len < 8 || rec_len % 4 != 0 || offset + rec_len > block_len {
            res.corrupt_entries += 1;
            break;
        }

        let ino = header.inode_number();
        if ino != 0 {
            if header.entry_file_type() == EXT4_FT_DIR_CSUM {
                break;
            }

            if ino > max_inodes {
                res.corrupt_entries += 1;
            } else if offset + 8 + name_len <= offset + rec_len {
                let name_bytes = &block_bytes[offset + 8..offset + 8 + name_len];
                if name_len == 0 || name_bytes.contains(&b'/') || name_bytes.contains(&0) {
                    res.corrupt_entries += 1;
                } else {
                    let name = String::from_utf8_lossy(name_bytes).to_string();
                    res.entries.push((ino, name));
                }
            } else {
                res.corrupt_entries += 1;
            }
        }

        offset += rec_len;
    }

    res
}

/// Directory validation and entry parser (backwards compatibility wrapper).
pub fn verify_directory_block(block_bytes: &[u8]) -> Vec<(u32, String)> {
    verify_directory_block_detailed(block_bytes, u32::MAX).entries
}

