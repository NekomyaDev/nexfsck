//! `nexfsck` — Next-Generation Hardware-Accelerated File System Checker

use clap::Parser;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use tracing::{debug, error, info, warn};
use zerocopy::FromBytes;

use nexfsck_compute::{
    reconcile_block_bitmap, reconcile_inode_bitmap, verify_directory_block,
    verify_inodes_parallel, BitmapDiscrepancy, BlockAllocationTracker, HardwareProfile,
    InodeBitmapDiscrepancy, InodeVerificationStats,
};
use nexfsck_gpu::GpuAccelerator;
use nexfsck_io::BlockDevice;
use nexfsck_journal::AtomicUndoJournal;
use nexfsck_tui::ProgressStats;

// Standard LSB / POSIX fsck exit codes
const FSCK_EXIT_OK: u8 = 0;
const FSCK_EXIT_ERRORS_CORRECTED: u8 = 1;
const FSCK_EXIT_ERRORS_UNCORRECTED: u8 = 4;
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

    /// Use a specific backup superblock block number to rescue the filesystem
    #[arg(short = 'b', long = "backup-sb")]
    backup_sb: Option<u64>,

    /// Path to atomic undo journal file for rollback
    #[arg(long = "undo-file", default_value = "nexfsck_rollback.log")]
    undo_file: String,

    /// Rollback changes from a previously generated undo journal
    #[arg(long = "rollback")]
    rollback: bool,

    /// Output machine-readable JSON telemetry
    #[arg(long = "json")]
    json: bool,

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

    if !args.json {
        println!("===============================================================");
        println!("  nexfsck v0.1.0 — Next-Generation File System Checker");
        println!("  Copyright (C) 2026 NekomyaDev | Licensed under Apache-2.0");
        println!("===============================================================\n");
    }

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

    if !args.json {
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
    }

    // 2. Open Block Device & Flush Cache
    let is_read_only = !args.repair;
    if !args.json {
        info!("Opening target storage: {} (read_only = {})", args.device, is_read_only);
    }
    let dev = match BlockDevice::open(&args.device, is_read_only) {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to open device '{}': {}", args.device, e);
            return ExitCode::from(FSCK_EXIT_OPERATIONAL_ERROR);
        }
    };

    // 3. Read Superblock (Primary or Backup Hunter)
    let sb = if let Some(blk) = args.backup_sb {
        info!("Using specified backup superblock at block {}", blk);
        match dev.read_superblock_at_block(blk, 4096) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to read backup superblock at block {}: {}", blk, e);
                return ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED);
            }
        }
    } else {
        match dev.read_superblock() {
            Ok(s) => s,
            Err(e) => {
                error!("Primary superblock validation failed: {}", e);
                let backups = dev.find_backup_superblocks();
                if !backups.is_empty() {
                    let blocks: Vec<u64> = backups.iter().map(|(b, _)| *b).collect();
                    warn!(
                        "Primary superblock corrupted! Found valid backup superblock(s) at block(s): {:?}.",
                        blocks
                    );
                    warn!("Run again with '--backup-sb {}' to rescue this filesystem.", blocks[0]);
                }
                return ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED);
            }
        }
    };

    // Multi-Mount Protection (MMP) Check
    if let Ok(Some(mmp)) = dev.read_mmp(&sb) {
        if !args.json {
            info!(
                "Multi-Mount Protection (MMP) Active | Seq: {} | Node: {}",
                mmp.sequence(),
                String::from_utf8_lossy(&mmp.mmp_nodename).trim_matches('\0')
            );
        }
    }

    let is_64bit = sb.has_incompat_feature(nexfsck_core::EXT4_FEATURE_INCOMPAT_64BIT);
    let total_blocks = sb.total_blocks();
    let bg_count = sb.block_groups_count();
    let block_size = sb.block_size();

    if !args.json {
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
    }

    // 4. Read Block Group Descriptors
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

    let first_data_block = u32::from_le(sb.s_first_data_block) as u64;
    let blocks_per_group = sb.blocks_per_group();
    let desc_size = sb.desc_size() as u64;
    let gdt_blocks = ((bg_count * desc_size) + block_size - 1) / block_size;
    let reserved_gdt = u16::from_le(sb.s_reserved_gdt_blocks) as u64;
    let inode_table_blocks = ((sb.inodes_per_group() as u64 * sb.inode_size() as u64) + block_size - 1) / block_size;

    if first_data_block > 0 {
        tracker.mark_range(0, first_data_block as u32);
    }

    for (bg_idx, desc) in group_descriptors.iter().enumerate() {
        let first_block = (bg_idx as u64) * (blocks_per_group as u64) + first_data_block;
        if sb.group_has_superblock(bg_idx as u64) && first_block < total_blocks {
            let meta_blocks = (1 + gdt_blocks + reserved_gdt).min(total_blocks - first_block) as u32;
            tracker.mark_range(first_block, meta_blocks);
        }
        tracker.mark_range(desc.block_bitmap(is_64bit), 1);
        tracker.mark_range(desc.inode_bitmap(is_64bit), 1);
        tracker.mark_range(desc.inode_table_block(is_64bit), inode_table_blocks as u32);
    }

    if !args.json {
        info!("Pass 1: Checking Inode Tables and Extent Trees in parallel...");
    }

    let mut directory_blocks_to_check = Vec::new();
    let mut all_scanned_inodes = Vec::new();

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

                for inode in &inodes {
                    if inode.is_used() && inode.is_dir() && inode.uses_extents() {
                        if let Some(hdr) = inode.extent_header() {
                            if hdr.depth() == 0 && hdr.entries() > 0 {
                                if let Ok((ext, _)) = nexfsck_core::Ext4Extent::ref_from_prefix(&inode.i_block[12..]) {
                                    directory_blocks_to_check.push(ext.physical_start());
                                }
                            }
                        }
                    }
                }

                verify_inodes_parallel(&inodes, &tracker, &inode_stats);
                stats.bytes_scanned += table_bytes.len() as u64;
                all_scanned_inodes.push((bg_idx, inodes));
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
    if !args.json {
        info!("Pass 2: Checking Directory Entries and H-Trees ({} directory block(s))...", directory_blocks_to_check.len());
    }
    let mut total_dentry_count = 0;
    for dir_block in directory_blocks_to_check {
        if let Ok(block_bytes) = dev.read_block(dir_block, block_size) {
            let entries = verify_directory_block(&block_bytes);
            total_dentry_count += entries.len();
        }
    }
    if !args.json {
        info!("Pass 2: Validated {} directory entries.", total_dentry_count);
    }

    // Pass 5: Block & Inode Allocation Bitmap Reconciliation
    if !args.json {
        info!("Pass 5: Reconciling On-Disk Bitmaps against In-Memory Roaring Bitmaps...");
    }
    let mut total_block_discrepancy = BitmapDiscrepancy::default();
    let mut total_inode_discrepancy = InodeBitmapDiscrepancy::default();
    for (bg_idx, desc) in group_descriptors.iter().enumerate() {
        if desc.is_block_uninit() {
            continue;
        }

        let first_block = (bg_idx as u64) * (blocks_per_group as u64) + first_data_block;
        if first_block >= total_blocks {
            break;
        }
        let blocks_in_this_group = (total_blocks - first_block).min(blocks_per_group as u64) as u32;

        if let Ok(disk_bm) = dev.read_block_bitmap(desc, is_64bit, block_size) {
            let disc = reconcile_block_bitmap(&disk_bm, &tracker, first_block, blocks_in_this_group);
            total_block_discrepancy.false_free_blocks += disc.false_free_blocks;
            total_block_discrepancy.leaked_blocks += disc.leaked_blocks;
        }

        if let Ok(disk_inomb) = dev.read_inode_bitmap(desc, is_64bit, block_size) {
            if let Some((_, inodes)) = all_scanned_inodes.iter().find(|(idx, _)| *idx == bg_idx) {
                let disc = reconcile_inode_bitmap(&disk_inomb, inodes);
                total_inode_discrepancy.false_free_inodes += disc.false_free_inodes;
                total_inode_discrepancy.leaked_inodes += disc.leaked_inodes;
            }
        }
    }

    let corruptions = inode_stats.extent_corruptions.load(Ordering::Relaxed);
    let duplicates = inode_stats.duplicate_blocks.load(Ordering::Relaxed);
    let oob = inode_stats.out_of_bounds_blocks.load(Ordering::Relaxed);
    let fast_symlinks = inode_stats.fast_symlinks_checked.load(Ordering::Relaxed);
    let corrupt_symlinks = inode_stats.corrupted_symlinks.load(Ordering::Relaxed);

    stats.errors_found += corruptions + duplicates + oob + corrupt_symlinks + total_block_discrepancy.false_free_blocks + total_inode_discrepancy.false_free_inodes;

    // 6. Active Repair Mode Execution
    let mut errors_were_corrected = false;
    if args.repair && total_block_discrepancy.false_free_blocks > 0 {
        info!("Active Repair Mode: Correcting false-free block allocation bitmaps...");
        if let Ok(mut undo_journal) = AtomicUndoJournal::new(Some(&args.undo_file), block_size as u32) {
            for (bg_idx, desc) in group_descriptors.iter().enumerate() {
                if desc.is_block_uninit() {
                    continue;
                }
                let first_block = (bg_idx as u64) * (blocks_per_group as u64) + (u32::from_le(sb.s_first_data_block) as u64);
                if first_block >= total_blocks {
                    break;
                }
                let blocks_in_this_group = (total_blocks - first_block).min(blocks_per_group as u64) as u32;

                if let Ok(mut disk_bm) = dev.read_block_bitmap(desc, is_64bit, block_size) {
                    let mut modified = false;
                    for i in 0..blocks_in_this_group {
                        let abs_block = first_block + i as u64;
                        if tracker.is_allocated(abs_block) {
                            let byte_idx = (i / 8) as usize;
                            let bit_idx = i % 8;
                            if byte_idx < disk_bm.len() && (disk_bm[byte_idx] & (1 << bit_idx)) == 0 {
                                disk_bm[byte_idx] |= 1 << bit_idx;
                                modified = true;
                            }
                        }
                    }

                    if modified {
                        let orig_bm = dev.read_block_bitmap(desc, is_64bit, block_size).unwrap_or_default();
                        let _ = undo_journal.record_mutation(desc.block_bitmap(is_64bit), &orig_bm);
                        if dev.write_block(desc.block_bitmap(is_64bit), block_size, &disk_bm).is_ok() {
                            debug!("Repaired block bitmap for group {}", bg_idx);
                            errors_were_corrected = true;
                        }
                    }
                }
            }
            let _ = dev.flush_kernel_buffers();
            info!("Repairs committed to disk with atomic undo journal: {}", args.undo_file);
        }
    }

    // 7. Output Reporting (Text or JSON)
    if args.json {
        println!("{{");
        println!("  \"total_block_groups\": {},", bg_count);
        println!("  \"inodes_scanned\": {},", inode_stats.total_inodes_scanned.load(Ordering::Relaxed));
        println!("  \"active_inodes\": {},", inode_stats.used_inodes.load(Ordering::Relaxed));
        println!("  \"directory_inodes\": {},", inode_stats.directory_inodes.load(Ordering::Relaxed));
        println!("  \"regular_file_inodes\": {},", inode_stats.regular_file_inodes.load(Ordering::Relaxed));
        println!("  \"symlink_inodes\": {},", inode_stats.symlink_inodes.load(Ordering::Relaxed));
        println!("  \"fast_symlinks_checked\": {},", fast_symlinks);
        println!("  \"corrupted_symlinks\": {},", corrupt_symlinks);
        println!("  \"extent_trees_validated\": {},", inode_stats.extent_trees_checked.load(Ordering::Relaxed));
        println!("  \"allocated_blocks\": {},", tracker.allocated_count());
        println!("  \"directory_entries\": {},", total_dentry_count);
        println!("  \"extent_corruptions\": {},", corruptions);
        println!("  \"duplicate_blocks\": {},", duplicates);
        println!("  \"out_of_bounds_blocks\": {},", oob);
        println!("  \"false_free_blocks\": {},", total_block_discrepancy.false_free_blocks);
        println!("  \"leaked_blocks\": {},", total_block_discrepancy.leaked_blocks);
        println!("  \"false_free_inodes\": {},", total_inode_discrepancy.false_free_inodes);
        println!("  \"leaked_inodes\": {},", total_inode_discrepancy.leaked_inodes);
        println!("  \"errors_detected\": {},", stats.errors_found);
        println!("  \"errors_corrected\": {},", errors_were_corrected);
        println!("  \"elapsed_seconds\": {:.4}", stats.elapsed_secs());
        println!("}}");
    } else {
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
            "  - Symlinks       : {} (Fast Symlinks: {})",
            inode_stats.symlink_inodes.load(Ordering::Relaxed),
            fast_symlinks
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
        println!("Corrupt Symlinks   : {}", corrupt_symlinks);
        println!("False-Free Blocks  : {}", total_block_discrepancy.false_free_blocks);
        println!("Leaked Blocks      : {}", total_block_discrepancy.leaked_blocks);
        println!("False-Free Inodes  : {}", total_inode_discrepancy.false_free_inodes);
        println!("Leaked Inodes      : {}", total_inode_discrepancy.leaked_inodes);
        println!("--------------------------------------------------");

        if errors_were_corrected {
            info!("Filesystem errors were successfully corrected. Rollback journal created.");
            return ExitCode::from(FSCK_EXIT_ERRORS_CORRECTED);
        }

        if stats.errors_found == 0 {
            info!("Filesystem consistency check completed cleanly with ZERO errors.");
            return ExitCode::from(FSCK_EXIT_OK);
        } else {
            warn!(
                "Filesystem consistency check completed with {} inconsistency error(s).",
                stats.errors_found
            );
            return ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED);
        }
    }

    if errors_were_corrected {
        ExitCode::from(FSCK_EXIT_ERRORS_CORRECTED)
    } else if stats.errors_found == 0 {
        ExitCode::from(FSCK_EXIT_OK)
    } else {
        ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED)
    }
}
