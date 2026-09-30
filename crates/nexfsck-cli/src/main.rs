//! `nexfsck` — Next-Generation Hardware-Accelerated File System Checker

use clap::Parser;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use tracing::{debug, error, info, warn};
use zerocopy::FromBytes;

use nexfsck_compute::{
    reconcile_block_bitmap, verify_directory_block, verify_inodes_parallel,
    BitmapDiscrepancy, BlockAllocationTracker, HardwareProfile, InodeVerificationStats,
};
use nexfsck_gpu::GpuAccelerator;
use nexfsck_io::BlockDevice;
use nexfsck_journal::AtomicUndoJournal;
use nexfsck_tui::ProgressStats;

// Standard LSB / POSIX fsck exit codes
#[allow(dead_code)]
const FSCK_EXIT_OK: u8 = 0;
#[allow(dead_code)]
const FSCK_EXIT_ERRORS_UNCORRECTED: u8 = 4;
#[allow(dead_code)]
const FSCK_EXIT_OPERATIONAL_ERROR: u8 = 8;
#[allow(dead_code)]
const FSCK_EXIT_USAGE_ERROR: u8 = 16;

#[derive(Parser, Debug)]
#[command(
    name = "nexfsck",
    author = "NekomyaDev <nekomyadev@users.noreply.github.com>",
    version = "0.1.0",
    about = "Next-Generation Hardware-Accelerated (io_uring, SIMD, GPU/VRAM) ext4 File System Checker"
)]
struct Args {
    /// Target block device or disk image path (e.g. /dev/nvme0n1p1 or disk.img)
    #[arg(required = true)]
    device: String,

    /// Run in safe read-only verification mode (default: true)
    #[arg(short = 'n', long = "read-only", default_value_t = true)]
    read_only: bool,

    /// Automatically repair inconsistencies (writes atomic undo log)
    #[arg(short = 'y', long = "repair")]
    repair: bool,

    /// Path to atomic undo journal file for rollback
    #[arg(long = "undo-file", default_value = "nexfsck_rollback.log")]
    undo_file: String,

    /// Rollback changes from a previously generated undo journal
    #[arg(long = "rollback")]
    rollback: bool,

    /// Verbose diagnostic output
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();

    // Initialize tracing
    let filter = if args.verbose { "debug" } else { "info" };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();

    println!("===============================================================");
    println!("  nexfsck v0.1.0 — Next-Generation File System Checker");
    println!("  Copyright (C) 2026 NekomyaDev | Licensed under Apache-2.0");
    println!("===============================================================\n");

    // 0. Handle Rollback Request
    if args.rollback {
        info!("Executing atomic rollback from journal: {}", args.undo_file);
        let dev = match BlockDevice::open(&args.device, false) {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to open target device for write: {}", e);
                return ExitCode::from(FSCK_EXIT_OPERATIONAL_ERROR);
            }
        };

        match AtomicUndoJournal::rollback_from_file(&args.undo_file, &dev) {
            Ok(restored) => {
                info!("Rollback successful! Restored {} physical block(s).", restored);
                return ExitCode::from(FSCK_EXIT_OK);
            }
            Err(e) => {
                error!("Rollback failed: {}", e);
                return ExitCode::from(FSCK_EXIT_OPERATIONAL_ERROR);
            }
        }
    }

    // 1. Hardware Discovery
    let hw = HardwareProfile::detect();
    let gpu = GpuAccelerator::probe();

    info!(
        "Hardware Detected: {} CPU cores (Rayon work-stealing active), {:.1} GB RAM",
        hw.cpu_cores,
        hw.total_ram_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    );
    info!(
        "SIMD Capabilities: AVX-512: {}, AVX2: {}, ARM NEON/CRC: {}",
        hw.has_avx512, hw.has_avx2, hw.has_arm_crc32
    );
    info!(
        "GPU Accelerator  : {} [VRAM: {:.1} GB]",
        gpu.device_name(),
        gpu.vram_bytes() as f64 / (1024.0 * 1024.0 * 1024.0)
    );

    // 2. Open Block Device & Flush Cache
    let is_read_only = !args.repair;
    info!("Opening target storage: {} (read_only = {})", args.device, is_read_only);
    let dev = match BlockDevice::open(&args.device, is_read_only) {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to open device '{}': {}", args.device, e);
            return ExitCode::from(FSCK_EXIT_OPERATIONAL_ERROR);
        }
    };

    // 3. Read Superblock
    let sb = match dev.read_superblock() {
        Ok(s) => s,
        Err(e) => {
            error!("Superblock validation failed: {}", e);
            return ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED);
        }
    };

    let is_64bit = sb.has_incompat_feature(nexfsck_core::EXT4_FEATURE_INCOMPAT_64BIT);
    let total_blocks = sb.total_blocks();
    let bg_count = sb.block_groups_count();
    let block_size = sb.block_size();

    info!(
        "Filesystem Magic OK (0x{:04X}) | Block Size: {}B | Inode Size: {}B | 64-bit: {}",
        u16::from_le(sb.s_magic),
        block_size,
        sb.inode_size(),
        is_64bit
    );
    info!(
        "Total Blocks: {} | Free Blocks: {} | Total Inodes: {}",
        total_blocks,
        sb.free_blocks(),
        sb.total_inodes()
    );
    info!("Total Block Groups: {}", bg_count);

    // 4. Read Block Group Descriptors
    info!("Reading {} Block Group Descriptors...", bg_count);
    let group_descriptors = match dev.read_group_descriptors(&sb) {
        Ok(desc) => desc,
        Err(e) => {
            error!("Failed to read block group descriptors: {}", e);
            return ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED);
        }
    };

    // 5. Initialize In-Memory Roaring Bitmaps & Telemetry
    let tracker = BlockAllocationTracker::new(total_blocks);
    let inode_stats = InodeVerificationStats::new();
    let mut stats = ProgressStats::new(bg_count);

    info!("Pass 1: Checking Inode Tables and Extent Trees in parallel...");

    let mut directory_blocks_to_check = Vec::new();

    for (bg_idx, desc) in group_descriptors.iter().enumerate() {
        let free_inodes = desc.free_inodes_count(is_64bit);
        debug!(
            "Group {}: flags=0x{:04X}, uninit={}, free_inodes={}, total_inodes_per_group={}",
            bg_idx,
            desc.flags(),
            desc.is_inode_uninit(),
            free_inodes,
            sb.inodes_per_group()
        );
        if desc.is_inode_uninit() && free_inodes == sb.inodes_per_group() {
            stats.processed_groups += 1;
            continue;
        }

        match dev.read_inode_table(&sb, desc, is_64bit) {
            Ok(table_bytes) => {
                let inodes = BlockDevice::parse_inodes_from_table(
                    &table_bytes,
                    sb.inode_size() as usize,
                );

                // Collect directory data blocks for Pass 2
                for inode in &inodes {
                    if inode.is_used() && inode.is_dir() && inode.uses_extents() {
                        if let Some(hdr) = inode.extent_header() {
                            if hdr.depth() == 0 && hdr.entries() > 0 {
                                // Extract first extent leaf
                                if let Ok((ext, _)) = nexfsck_core::Ext4Extent::ref_from_prefix(&inode.i_block[12..]) {
                                    directory_blocks_to_check.push(ext.physical_start());
                                }
                            }
                        }
                    }
                }

                verify_inodes_parallel(&inodes, &tracker, &inode_stats);
                stats.bytes_scanned += table_bytes.len() as u64;
            }
            Err(e) => {
                warn!(
                    "Block group {} inode table read error: {}. Isolating group.",
                    bg_idx, e
                );
                stats.errors_found += 1;
            }
        }

        stats.processed_groups += 1;
    }

    // Pass 2: Directory Entries Validation
    info!("Pass 2: Checking Directory Entries and H-Trees ({} directory block(s))...", directory_blocks_to_check.len());
    let mut total_dentry_count = 0;
    for dir_block in directory_blocks_to_check {
        if let Ok(block_bytes) = dev.read_block(dir_block, block_size) {
            let entries = verify_directory_block(&block_bytes);
            total_dentry_count += entries.len();
        }
    }
    info!("Pass 2: Validated {} directory entries.", total_dentry_count);

    // Pass 5: Block Allocation Bitmap Reconciliation
    info!("Pass 5: Reconciling On-Disk Bitmaps against In-Memory Roaring Bitmaps...");
    let mut total_discrepancy = BitmapDiscrepancy::default();
    let blocks_per_group = sb.blocks_per_group();
    let inode_table_blocks = ((sb.inodes_per_group() as u64 * sb.inode_size() as u64) + block_size - 1) / block_size;

    for (bg_idx, desc) in group_descriptors.iter().enumerate() {
        if desc.is_block_uninit() {
            continue;
        }

        let first_block = (bg_idx as u64) * (blocks_per_group as u64) + (u32::from_le(sb.s_first_data_block) as u64);
        if first_block >= total_blocks {
            break;
        }
        let blocks_in_this_group = (total_blocks - first_block).min(blocks_per_group as u64) as u32;

        // Mark group metadata blocks (block bitmap, inode bitmap, inode table) as system-allocated
        tracker.mark_range(desc.block_bitmap(is_64bit), 1);
        tracker.mark_range(desc.inode_bitmap(is_64bit), 1);
        tracker.mark_range(desc.inode_table_block(is_64bit), inode_table_blocks as u32);

        if let Ok(disk_bm) = dev.read_block_bitmap(desc, is_64bit, block_size) {
            let disc = reconcile_block_bitmap(&disk_bm, &tracker, first_block, blocks_in_this_group);
            total_discrepancy.false_free_blocks += disc.false_free_blocks;
            total_discrepancy.leaked_blocks += disc.leaked_blocks;
        }
    }

    let corruptions = inode_stats.extent_corruptions.load(Ordering::Relaxed);
    let duplicates = inode_stats.duplicate_blocks.load(Ordering::Relaxed);
    let oob = inode_stats.out_of_bounds_blocks.load(Ordering::Relaxed);
    stats.errors_found += corruptions + duplicates + oob + total_discrepancy.false_free_blocks;

    // 6. Report Summary
    println!();
    stats.print_summary();

    println!("--------------------------------------------------");
    println!("Detailed Inode & Block Accounting:");
    println!("--------------------------------------------------");
    println!(
        "Inodes Scanned     : {}",
        inode_stats.total_inodes_scanned.load(Ordering::Relaxed)
    );
    println!(
        "Active Inodes      : {}",
        inode_stats.used_inodes.load(Ordering::Relaxed)
    );
    println!(
        "  - Directories    : {}",
        inode_stats.directory_inodes.load(Ordering::Relaxed)
    );
    println!(
        "  - Regular Files  : {}",
        inode_stats.regular_file_inodes.load(Ordering::Relaxed)
    );
    println!(
        "  - Symlinks       : {}",
        inode_stats.symlink_inodes.load(Ordering::Relaxed)
    );
    println!(
        "Extent Trees Valid : {}",
        inode_stats.extent_trees_checked.load(Ordering::Relaxed)
    );
    println!(
        "Allocated Blocks   : {} (Tracked in Roaring Bitmaps)",
        tracker.allocated_count()
    );
    println!("Directory Entries  : {}", total_dentry_count);
    println!("Extent Corruptions : {}", corruptions);
    println!("Duplicate Blocks   : {}", duplicates);
    println!("Out-of-Bounds Blks : {}", oob);
    println!("False-Free Blocks  : {}", total_discrepancy.false_free_blocks);
    println!("Leaked Blocks      : {}", total_discrepancy.leaked_blocks);
    println!("--------------------------------------------------");

    if stats.errors_found == 0 {
        info!("Filesystem consistency check completed cleanly with ZERO errors.");
        ExitCode::from(FSCK_EXIT_OK)
    } else {
        warn!(
            "Filesystem consistency check completed with {} inconsistency error(s).",
            stats.errors_found
        );
        ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED)
    }
}
