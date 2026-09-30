//! `nexfsck-compute`
//!
//! Multi-core compute engine and hardware capability discovery,
//! multi-level extent tree analysis, directory validation,
//! and parallel bitmap state reduction.

use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use zerocopy::FromBytes;

use nexfsck_core::{
    Ext4DirEntry2Header, Ext4Extent, Ext4ExtentHeader, Ext4ExtentIdx, Ext4Inode, EXT4_FT_DIR_CSUM,
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

/// Computes ext4 CRC32c (Castagnoli) with runtime hardware dispatch.
pub fn ext4_crc32c(seed: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("sse4.2") {
        // SAFETY: guarded by runtime feature detection.
        return unsafe { crc32c_x86(seed, data) };
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("crc") {
        // SAFETY: guarded by runtime feature detection.
        return unsafe { crc32c_arm(seed, data) };
    }
    crc32c_scalar(seed, data)
}

fn crc32c_scalar(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    crc
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_x86(mut crc: u32, mut data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};
    while data.len() >= 8 {
        let word = u64::from_le_bytes(data[..8].try_into().unwrap());
        crc = _mm_crc32_u64(crc as u64, word) as u32;
        data = &data[8..];
    }
    for &byte in data {
        crc = _mm_crc32_u8(crc, byte);
    }
    crc
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn crc32c_arm(mut crc: u32, mut data: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};
    while data.len() >= 8 {
        let word = u64::from_le_bytes(data[..8].try_into().unwrap());
        crc = __crc32cd(crc, word);
        data = &data[8..];
    }
    for &byte in data {
        crc = __crc32cb(crc, byte);
    }
    crc
}

#[cfg(test)]
mod crc_tests {
    use super::*;

    #[test]
    fn crc32c_known_vector() {
        assert_eq!(ext4_crc32c(0xffff_ffff, b"123456789"), 0x1cf9_6d7c);
    }

    #[test]
    fn dispatched_crc_matches_scalar_for_lengths_and_seeds() {
        let data: Vec<u8> = (0..1024).map(|n| (n * 31) as u8).collect();
        for seed in [0, 1, 0xffff_ffff, 0x1234_5678] {
            for len in 0..=data.len() {
                assert_eq!(
                    ext4_crc32c(seed, &data[..len]),
                    crc32c_scalar(seed, &data[..len])
                );
            }
        }
    }
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

/// Thread-safe 64-bit block allocation tracker using chunked Roaring Bitmaps.
/// The address representation is 64-bit; operational exabyte-scale behavior is not guaranteed.
pub struct BlockAllocationTracker {
    total_blocks: u64,
    allocated_count: AtomicU64,
    pub chunks: RwLock<HashMap<u32, RoaringBitmap>>,
}

impl BlockAllocationTracker {
    pub fn new(total_blocks: u64) -> Self {
        Self {
            total_blocks,
            allocated_count: AtomicU64::new(0),
            chunks: RwLock::new(HashMap::new()),
        }
    }

    /// Marks a range of 64-bit blocks as allocated. Returns true if no collision detected.
    pub fn mark_range(&self, start_block: u64, count: u32) -> bool {
        let mut chunks = self.chunks.write().unwrap();
        let mut collision = false;
        let mut newly_added = 0u64;

        for b in start_block..(start_block + count as u64) {
            let chunk_idx = (b >> 32) as u32;
            let offset = (b & 0xFFFF_FFFF) as u32;

            let bm = chunks.entry(chunk_idx).or_default();
            if !bm.insert(offset) {
                collision = true;
            } else {
                newly_added += 1;
            }
        }
        self.allocated_count
            .fetch_add(newly_added, Ordering::Relaxed);
        !collision
    }

    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    pub fn allocated_count(&self) -> u64 {
        self.allocated_count.load(Ordering::Relaxed)
    }

    pub fn is_allocated(&self, block: u64) -> bool {
        let chunk_idx = (block >> 32) as u32;
        let offset = (block & 0xFFFF_FFFF) as u32;

        let chunks = self.chunks.read().unwrap();
        if let Some(bm) = chunks.get(&chunk_idx) {
            bm.contains(offset)
        } else {
            false
        }
    }
}

/// Validates a batch of inodes in parallel using Rayon work-stealing.
pub fn verify_inodes_parallel<F>(
    inodes: &[Ext4Inode],
    tracker: &BlockAllocationTracker,
    stats: &InodeVerificationStats,
    read_block_fn: &F,
) where
    F: Fn(u64) -> Option<Vec<u8>> + Sync,
{
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
                read_block_fn,
                0,
            );
        } else if !inode.is_inline_data() && !inode.is_fast_symlink() {
            // Traditional direct and indirect block pointers (Ext2/3/4 legacy format, e.g. resize inode 7)
            for offset in (0..inode.i_block.len()).step_by(4) {
                let blk = u32::from_le_bytes(inode.i_block[offset..offset + 4].try_into().unwrap())
                    as u64;
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
    let chunks = tracker.chunks.read().unwrap();

    for i in 0..blocks_in_group {
        let abs_block = first_block_in_group + i as u64;
        let byte_idx = (i / 8) as usize;
        let bit_idx = i % 8;

        let disk_is_allocated = if byte_idx < disk_bitmap.len() {
            (disk_bitmap[byte_idx] & (1 << bit_idx)) != 0
        } else {
            false
        };

        let chunk_idx = (abs_block >> 32) as u32;
        let offset = (abs_block & 0xFFFF_FFFF) as u32;
        let tracker_is_allocated = chunks
            .get(&chunk_idx)
            .map(|bm| bm.contains(offset))
            .unwrap_or(false);

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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HTreeValidationResult {
    pub indexed_blocks: u64,
    pub leaf_blocks: u64,
    pub errors: Vec<String>,
}

pub fn verify_htree_directory<F>(
    root: &[u8],
    logical_block_count: u32,
    read_logical: &F,
) -> HTreeValidationResult
where
    F: Fn(u32) -> Option<Vec<u8>>,
{
    let mut result = HTreeValidationResult::default();
    if root.len() < 40 {
        result
            .errors
            .push("H-Tree root is shorter than 40 bytes".into());
        return result;
    }
    let reserved = u32::from_le_bytes(root[24..28].try_into().unwrap());
    let hash_version = root[28];
    let info_length = root[29];
    let levels = root[30];
    if reserved != 0 {
        result
            .errors
            .push("H-Tree reserved field is non-zero".into());
    }
    if hash_version > 6 {
        result
            .errors
            .push(format!("unsupported H-Tree hash version {hash_version}"));
    }
    if info_length != 8 {
        result
            .errors
            .push(format!("invalid H-Tree info length {info_length}"));
    }
    if levels > 2 {
        result.errors.push(format!("invalid H-Tree depth {levels}"));
    }
    let mut visited = std::collections::HashSet::new();
    validate_dx_entries(
        root,
        32,
        levels,
        logical_block_count,
        read_logical,
        &mut visited,
        &mut result,
    );
    result
}

fn validate_dx_entries<F>(
    block: &[u8],
    count_offset: usize,
    levels: u8,
    logical_block_count: u32,
    read_logical: &F,
    visited: &mut std::collections::HashSet<u32>,
    result: &mut HTreeValidationResult,
) where
    F: Fn(u32) -> Option<Vec<u8>>,
{
    if count_offset + 8 > block.len() {
        result
            .errors
            .push("truncated H-Tree count/limit table".into());
        return;
    }
    let limit =
        u16::from_le_bytes(block[count_offset..count_offset + 2].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(
        block[count_offset + 2..count_offset + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let capacity = (block.len() - count_offset) / 8;
    if limit == 0 || limit > capacity {
        result
            .errors
            .push(format!("H-Tree limit {limit} exceeds capacity {capacity}"));
        return;
    }
    if count == 0 || count > limit {
        result
            .errors
            .push(format!("H-Tree count {count} is outside 1..={limit}"));
        return;
    }
    result.indexed_blocks += 1;
    let mut previous_hash = 0u32;
    for index in 0..count {
        let offset = count_offset + index * 8;
        let hash = if index == 0 {
            0
        } else {
            u32::from_le_bytes(block[offset..offset + 4].try_into().unwrap())
        };
        let child =
            u32::from_le_bytes(block[offset + 4..offset + 8].try_into().unwrap()) & 0x00ff_ffff;
        if index > 1 && hash < previous_hash {
            result
                .errors
                .push(format!("H-Tree hashes are not ordered at entry {index}"));
        }
        previous_hash = hash;
        if child == 0 || child >= logical_block_count {
            result.errors.push(format!(
                "H-Tree child logical block {child} is out of bounds"
            ));
            continue;
        }
        if !visited.insert(child) {
            result.errors.push(format!(
                "H-Tree child logical block {child} is referenced more than once"
            ));
            continue;
        }
        let Some(child_data) = read_logical(child) else {
            result.errors.push(format!(
                "H-Tree child logical block {child} could not be read"
            ));
            continue;
        };
        if levels == 0 {
            result.leaf_blocks += 1;
            if verify_directory_block_detailed(&child_data, u32::MAX).corrupt_entries > 0 {
                result.errors.push(format!(
                    "H-Tree leaf logical block {child} contains invalid entries"
                ));
            }
        } else {
            validate_dx_entries(
                &child_data,
                8,
                levels - 1,
                logical_block_count,
                read_logical,
                visited,
                result,
            );
        }
    }
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

        if rec_len < 8 || rec_len & 3 != 0 || offset + rec_len > block_len {
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

/// Recursively collects all physical block numbers allocated to a directory inode across all extent tree depths.
pub fn collect_directory_blocks<F>(inode: &Ext4Inode, read_block_fn: &F) -> Vec<u64>
where
    F: Fn(u64) -> Option<Vec<u8>>,
{
    let mut blocks = Vec::new();

    if inode.uses_extents() && !inode.is_inline_data() {
        collect_directory_blocks_from_extent(&inode.i_block, read_block_fn, 0, &mut blocks);
    } else if !inode.is_inline_data() && !inode.is_fast_symlink() {
        for offset in (0..inode.i_block.len()).step_by(4) {
            let blk =
                u32::from_le_bytes(inode.i_block[offset..offset + 4].try_into().unwrap()) as u64;
            if blk != 0 {
                blocks.push(blk);
            }
        }
    }

    blocks
}

/// Collects physical data extents from every level of an inode extent tree.
pub fn collect_inode_extents<F>(inode: &Ext4Inode, read_block_fn: &F) -> Vec<(u64, u32)>
where
    F: Fn(u64) -> Option<Vec<u8>>,
{
    let mut extents = Vec::new();
    if inode.uses_extents() && !inode.is_inline_data() {
        collect_extents_from_node(&inode.i_block, read_block_fn, 0, &mut extents);
    }
    extents
}

fn collect_extents_from_node<F>(
    data: &[u8],
    read_block_fn: &F,
    depth: u16,
    out: &mut Vec<(u64, u32)>,
) where
    F: Fn(u64) -> Option<Vec<u8>>,
{
    if depth > 5 {
        return;
    }
    let Ok((header, rest)) = Ext4ExtentHeader::ref_from_prefix(data) else {
        return;
    };
    if !header.is_valid_magic() || header.entries() > header.max() {
        return;
    }
    if header.depth() == 0 {
        let size = std::mem::size_of::<Ext4Extent>();
        for index in 0..header.entries() as usize {
            let offset = index * size;
            let Some(bytes) = rest.get(offset..) else {
                break;
            };
            if let Ok((extent, _)) = Ext4Extent::ref_from_prefix(bytes) {
                out.push((extent.physical_start(), extent.block_count()));
            }
        }
    } else {
        let size = std::mem::size_of::<Ext4ExtentIdx>();
        for index in 0..header.entries() as usize {
            let offset = index * size;
            let Some(bytes) = rest.get(offset..) else {
                break;
            };
            if let Ok((entry, _)) = Ext4ExtentIdx::ref_from_prefix(bytes) {
                if let Some(child) = read_block_fn(entry.child_block()) {
                    collect_extents_from_node(&child, read_block_fn, depth + 1, out);
                }
            }
        }
    }
}

fn collect_directory_blocks_from_extent<F>(
    data: &[u8],
    read_block_fn: &F,
    current_depth: u16,
    blocks: &mut Vec<u64>,
) where
    F: Fn(u64) -> Option<Vec<u8>>,
{
    if current_depth > 5 {
        return;
    }

    let (header, rest) = match Ext4ExtentHeader::ref_from_prefix(data) {
        Ok(res) => res,
        Err(_) => return,
    };

    if !header.is_valid_magic() {
        return;
    }

    let entries = header.entries() as usize;
    let max_entries = header.max() as usize;
    if entries > max_entries {
        return;
    }

    if header.depth() == 0 {
        let extent_size = std::mem::size_of::<Ext4Extent>();
        for i in 0..entries {
            let offset = i * extent_size;
            if offset + extent_size > rest.len() {
                break;
            }
            if let Ok((ext, _)) = Ext4Extent::ref_from_prefix(&rest[offset..]) {
                let start = ext.physical_start();
                let count = ext.block_count();
                for b in 0..count {
                    blocks.push(start + b as u64);
                }
            }
        }
    } else {
        let idx_size = std::mem::size_of::<Ext4ExtentIdx>();
        for i in 0..entries {
            let offset = i * idx_size;
            if offset + idx_size > rest.len() {
                break;
            }
            if let Ok((idx, _)) = Ext4ExtentIdx::ref_from_prefix(&rest[offset..]) {
                let child = idx.child_block();
                if let Some(child_data) = read_block_fn(child) {
                    collect_directory_blocks_from_extent(
                        &child_data,
                        read_block_fn,
                        current_depth + 1,
                        blocks,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod htree_tests {
    use super::*;

    #[test]
    fn validates_minimal_htree_root_and_leaf() {
        let mut root = vec![0u8; 4096];
        root[29] = 8;
        root[32..34].copy_from_slice(&508u16.to_le_bytes());
        root[34..36].copy_from_slice(&1u16.to_le_bytes());
        root[36..40].copy_from_slice(&1u32.to_le_bytes());
        let mut leaf = vec![0u8; 4096];
        leaf[4..6].copy_from_slice(&4096u16.to_le_bytes());
        let result =
            verify_htree_directory(&root, 2, &|logical| (logical == 1).then(|| leaf.clone()));
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.leaf_blocks, 1);
    }

    #[test]
    fn rejects_out_of_bounds_htree_child() {
        let mut root = vec![0u8; 4096];
        root[29] = 8;
        root[32..34].copy_from_slice(&508u16.to_le_bytes());
        root[34..36].copy_from_slice(&1u16.to_le_bytes());
        root[36..40].copy_from_slice(&9u32.to_le_bytes());
        let result = verify_htree_directory(&root, 2, &|_| None);
        assert!(result
            .errors
            .iter()
            .any(|error| error.contains("out of bounds")));
    }
}

#[cfg(test)]
mod bitmap_tests {
    use super::*;

    #[test]
    fn tracks_ranges_across_the_32_bit_chunk_boundary() {
        let tracker = BlockAllocationTracker::new(u64::MAX);
        assert!(tracker.mark_range(u32::MAX as u64 - 1, 4));
        assert!(tracker.is_allocated(u32::MAX as u64 - 1));
        assert!(tracker.is_allocated(u32::MAX as u64 + 2));
        assert_eq!(tracker.allocated_count(), 4);
        assert_eq!(tracker.chunks.read().unwrap().len(), 2);
    }

    #[test]
    fn detects_collision_above_four_billion_blocks() {
        let tracker = BlockAllocationTracker::new(u64::MAX);
        let high = (1u64 << 32) + 123;
        assert!(tracker.mark_range(high, 8));
        assert!(!tracker.mark_range(high + 7, 2));
    }
}
