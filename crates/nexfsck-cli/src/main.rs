//! `nexfsck` — experimental ext4 file system checker

use clap::Parser;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use nexfsck_compute::{
    collect_directory_blocks, collect_inode_extents, reconcile_block_bitmap,
    reconcile_inode_bitmap, verify_directory_block_detailed, verify_htree_directory,
    verify_inodes_parallel, BitmapDiscrepancy, BlockAllocationTracker, HardwareProfile,
    InodeBitmapDiscrepancy, InodeVerificationStats,
};
use nexfsck_gpu::{BlockInterval, GpuAccelerator};
use nexfsck_io::BlockDevice;
use nexfsck_journal::AtomicUndoJournal;
use nexfsck_tui::{LiveMetrics, MetricsServer, ProgressStats};

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
    about = "Experimental parallel ext4 file system checker"
)]
struct Args {
    /// Target block device or disk image path (e.g. /dev/nvme0n1p1 or disk.img)
    #[arg(required = true)]
    device: String,

    /// Run in safe read-only verification mode (default: true)
    #[arg(short = 'n', long = "read-only", default_value_t = true)]
    read_only: bool,

    /// Automatically repair supported inconsistencies (writes a pre-image undo log)
    #[arg(short = 'y', long = "repair")]
    repair: bool,

    /// Use a specific backup superblock block number to rescue the filesystem
    #[arg(short = 'b', long = "backup-sb")]
    backup_sb: Option<u64>,

    /// Path to the pre-image undo journal used for rollback
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

    /// Expose live Prometheus metrics at /metrics (example: 127.0.0.1:9898)
    #[arg(long = "metrics-listen")]
    metrics_listen: Option<String>,
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
        println!("  nexfsck v0.1.0 — Experimental ext4 File System Checker");
        println!("  Copyright (C) 2026 NekomyaDev | Licensed under Apache-2.0");
        println!("===============================================================\n");
    }

    // 0. Handle Rollback Request
    if args.rollback {
        info!(
            "Restoring recorded block pre-images from journal: {}",
            args.undo_file
        );
        let dev = match BlockDevice::open(&args.device, false) {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to open target device for write: {}", e);
                return ExitCode::from(FSCK_EXIT_OPERATIONAL_ERROR);
            }
        };

        match AtomicUndoJournal::rollback_from_file(&args.undo_file, &dev) {
            Ok(restored) => {
                info!(
                    "Rollback successful! Restored {} physical block(s).",
                    restored
                );
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
            "CPU capabilities (detection only; no dedicated SIMD kernels): AVX-512: {}, AVX2: {}, ARM CRC: {}",
            hw.has_avx512, hw.has_avx2, hw.has_arm_crc32
        );
        info!(
            "GPU device: {} [reported memory: {:.1} GB, CUDA compute active: {}]",
            gpu.device_name(),
            gpu.vram_bytes() as f64 / (1024.0 * 1024.0 * 1024.0),
            gpu.is_available()
        );
    }

    // 2. Open Block Device & Flush Cache
    let is_read_only = !args.repair;
    let dev = match BlockDevice::open(&args.device, is_read_only) {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to open device '{}': {}", args.device, e);
            return ExitCode::from(FSCK_EXIT_OPERATIONAL_ERROR);
        }
    };
    if !args.json {
        info!(
            "Opening target storage: {} (read_only = {})",
            args.device, is_read_only
        );
        info!(
            "I/O Engine        : Linux io_uring (Queue Depth 128) [Active: {}]",
            dev.is_io_uring_active()
        );
        info!("Registered buffers: {}", dev.has_registered_io_buffers());
    }

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
                    warn!(
                        "Run again with '--backup-sb {}' to rescue this filesystem.",
                        blocks[0]
                    );
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

    // JBD2 Crash Recovery Journal Inspection
    let mut journal_is_dirty = false;
    if let Ok(Some(jbd)) = nexfsck_journal::inspect_journal(&dev, &sb, &group_descriptors, is_64bit)
    {
        let is_dirty =
            !jbd.is_clean() || sb.has_incompat_feature(nexfsck_core::EXT4_FEATURE_INCOMPAT_RECOVER);
        journal_is_dirty = is_dirty;
        if !args.json {
            info!(
                "JBD2 Journal Active | Seq: {} | Block Size: {}B | Clean: {}",
                jbd.sequence(),
                jbd.block_size(),
                !is_dirty
            );
        }
        if is_dirty {
            warn!("Filesystem journal is DIRTY: uncommitted or pending transactions exist.");
        }
    }

    // 5. Initialize In-Memory Roaring Bitmaps & Telemetry
    let tracker = BlockAllocationTracker::new(total_blocks);
    let inode_stats = InodeVerificationStats::new();
    let mut stats = ProgressStats::new(bg_count);
    let live_metrics = Arc::new(LiveMetrics::new(bg_count));
    let _metrics_server = match args.metrics_listen.as_deref() {
        Some(address) => match MetricsServer::start(address, Arc::clone(&live_metrics)) {
            Ok(server) => Some(server),
            Err(error) => {
                error!("Failed to start metrics endpoint at {}: {}", address, error);
                return ExitCode::from(FSCK_EXIT_OPERATIONAL_ERROR);
            }
        },
        None => None,
    };

    let first_data_block = u32::from_le(sb.s_first_data_block) as u64;
    let blocks_per_group = sb.blocks_per_group();
    let desc_size = sb.desc_size() as u64;
    let gdt_blocks = (bg_count * desc_size).div_ceil(block_size);
    let reserved_gdt = u16::from_le(sb.s_reserved_gdt_blocks) as u64;
    let inode_table_blocks =
        (sb.inodes_per_group() as u64 * sb.inode_size() as u64).div_ceil(block_size);

    if first_data_block > 0 {
        tracker.mark_range(0, first_data_block as u32);
    }

    for (bg_idx, desc) in group_descriptors.iter().enumerate() {
        let first_block = (bg_idx as u64) * (blocks_per_group as u64) + first_data_block;
        if sb.group_has_superblock(bg_idx as u64) && first_block < total_blocks {
            let meta_blocks =
                (1 + gdt_blocks + reserved_gdt).min(total_blocks - first_block) as u32;
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
    let mut directory_layouts: std::collections::HashMap<u32, (Vec<u64>, bool)> =
        std::collections::HashMap::new();
    let mut all_scanned_inodes = Vec::new();
    let mut compute_intervals = Vec::new();

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
            stats.record_group(bg_idx, 0, 0, false);
            live_metrics.update(&stats);
            continue;
        }

        let mut group_had_error = false;
        let mut group_bytes = 0;
        match dev.read_inode_table(&sb, desc, is_64bit) {
            Ok(table_bytes) => {
                let inodes =
                    BlockDevice::parse_inodes_from_table(&table_bytes, sb.inode_size() as usize);

                for (i, inode) in inodes.iter().enumerate() {
                    let ino_num = (bg_idx as u32) * sb.inodes_per_group() + (i as u32) + 1;
                    if inode.is_used() && inode.is_dir() {
                        let blocks = collect_directory_blocks(inode, &|blk| {
                            dev.read_block(blk, block_size).ok()
                        });
                        directory_layouts
                            .insert(ino_num, (blocks.clone(), inode.is_indexed_directory()));
                        for blk in blocks {
                            directory_blocks_to_check.push((ino_num, blk));
                        }
                    }
                    if inode.is_used() && inode.uses_extents() && !inode.is_inline_data() {
                        compute_intervals.extend(
                            collect_inode_extents(inode, &|blk| {
                                dev.read_block(blk, block_size).ok()
                            })
                            .into_iter()
                            .map(|(start_block, block_count)| BlockInterval {
                                start_block,
                                block_count,
                            }),
                        );
                    }
                }

                verify_inodes_parallel(&inodes, &tracker, &inode_stats, &|blk| {
                    dev.read_block(blk, block_size).ok()
                });
                group_bytes = table_bytes.len() as u64;
                all_scanned_inodes.push((bg_idx, inodes));
            }
            Err(e) => {
                warn!(
                    "Block group {} inode table read error: {}. Isolating group.",
                    bg_idx, e
                );
                stats.errors_found += 1;
                group_had_error = true;
            }
        }

        stats.record_group(bg_idx, group_bytes, 1, group_had_error);
        live_metrics.update(&stats);
    }

    let compute_backend = if gpu.is_available() {
        "CUDA PTX + CPU revalidation"
    } else {
        "CPU fallback"
    };
    let gpu_collisions = gpu.find_interval_collisions(&mut compute_intervals);
    if !args.json {
        info!(
            "Extent collision compute: {} | intervals={} | candidates={}",
            compute_backend,
            compute_intervals.len(),
            gpu_collisions.len()
        );
    }

    // Pass 2: Directory Entries Validation
    if !args.json {
        info!(
            "Pass 2: Checking directory entries and indexed H-Trees ({} block(s))...",
            directory_blocks_to_check.len()
        );
    }
    let mut total_dentry_count = 0;
    let mut actual_link_counts: std::collections::HashMap<u32, u16> =
        std::collections::HashMap::new();
    let mut reachable_from_dir: std::collections::HashSet<u32> = std::collections::HashSet::new();

    reachable_from_dir.insert(nexfsck_core::EXT4_ROOT_INO);

    let blocks_to_fetch: Vec<u64> = directory_blocks_to_check.iter().map(|(_, b)| *b).collect();
    let batch_reads = dev.read_blocks_batch(&blocks_to_fetch, block_size);
    let mut block_cache: std::collections::HashMap<u64, Vec<u8>> =
        std::collections::HashMap::with_capacity(batch_reads.len());
    for (blk, res) in batch_reads {
        match res {
            Ok(bytes) => {
                block_cache.insert(blk, bytes);
            }
            Err(e) => {
                warn!("Failed to read directory block {}: {}", blk, e);
                inode_stats
                    .corrupt_directories
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    for (inode, (layout, indexed)) in &directory_layouts {
        if !indexed || layout.is_empty() {
            continue;
        }
        let Some(root) = block_cache.get(&layout[0]) else {
            continue;
        };
        let htree = verify_htree_directory(root, layout.len() as u32, &|logical| {
            layout
                .get(logical as usize)
                .and_then(|physical| block_cache.get(physical))
                .cloned()
        });
        for message in &htree.errors {
            warn!("H-Tree inode {}: {}", inode, message);
        }
        inode_stats
            .corrupt_directories
            .fetch_add(htree.errors.len() as u64, Ordering::Relaxed);
    }

    for (_dir_ino, dir_block) in directory_blocks_to_check {
        if let Some(block_bytes) = block_cache.get(&dir_block) {
            let res = verify_directory_block_detailed(block_bytes, sb.total_inodes());
            total_dentry_count += res.entries.len();
            if res.corrupt_entries > 0 {
                inode_stats
                    .corrupt_directories
                    .fetch_add(res.corrupt_entries, Ordering::Relaxed);
            }
            for (child_ino, name) in res.entries {
                *actual_link_counts.entry(child_ino).or_insert(0) += 1;
                if name != "." && name != ".." {
                    reachable_from_dir.insert(child_ino);
                }
            }
        }
    }
    let corrupt_dirs = inode_stats.corrupt_directories.load(Ordering::Relaxed);
    if !args.json {
        info!(
            "Pass 2: Validated {} directory entries (corrupted entries: {}).",
            total_dentry_count, corrupt_dirs
        );
    }

    // Pass 3: Directory Connectivity & Orphan Directory Reclamation
    if !args.json {
        info!("Pass 3: Checking Directory Connectivity and Orphan Subtrees...");
    }
    let mut orphan_directories_count = 0u64;
    for (bg_idx, inodes) in &all_scanned_inodes {
        for (i, inode) in inodes.iter().enumerate() {
            let ino_num = (*bg_idx as u32) * sb.inodes_per_group() + (i as u32) + 1;
            if inode.is_used()
                && inode.is_dir()
                && ino_num >= sb.first_inode()
                && !reachable_from_dir.contains(&ino_num)
            {
                warn!(
                    "Orphan directory detected: Inode {} is disconnected from root directory tree",
                    ino_num
                );
                orphan_directories_count += 1;
            }
        }
    }
    inode_stats
        .orphan_directories
        .store(orphan_directories_count, Ordering::Relaxed);
    if !args.json {
        info!(
            "Pass 3: Directory connectivity verified (orphan directories: {}).",
            orphan_directories_count
        );
    }

    // Pass 4: Inode Reference & Link Counts Verification
    if !args.json {
        info!("Pass 4: Checking Inode Reference Counts (Hard links)...");
    }
    let mut link_mismatches_count = 0u64;
    for (bg_idx, inodes) in &all_scanned_inodes {
        for (i, inode) in inodes.iter().enumerate() {
            let ino_num = (*bg_idx as u32) * sb.inodes_per_group() + (i as u32) + 1;
            if inode.is_used() && ino_num >= sb.first_inode() {
                let recorded_links = inode.links_count();
                let actual_links = actual_link_counts.get(&ino_num).copied().unwrap_or(0);
                if actual_links > 0 && recorded_links != actual_links {
                    debug!(
                        "Link count discrepancy on inode {}: recorded={}, actual={}",
                        ino_num, recorded_links, actual_links
                    );
                    link_mismatches_count += 1;
                }
            }
        }
    }
    inode_stats
        .link_count_mismatches
        .store(link_mismatches_count, Ordering::Relaxed);
    if !args.json {
        info!(
            "Pass 4: Inode reference counts validated (mismatches: {}).",
            link_mismatches_count
        );
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
            let disc =
                reconcile_block_bitmap(&disk_bm, &tracker, first_block, blocks_in_this_group);
            total_block_discrepancy.false_free_blocks += disc.false_free_blocks;
            total_block_discrepancy.leaked_blocks += disc.leaked_blocks;
        }

        if let Ok(disk_inomb) = dev.read_inode_bitmap(desc, is_64bit, block_size) {
            if let Some((_, inodes)) = all_scanned_inodes.iter().find(|(idx, _)| *idx == bg_idx) {
                let disc = reconcile_inode_bitmap(
                    &disk_inomb,
                    inodes,
                    sb.first_inode(),
                    bg_idx,
                    sb.inodes_per_group(),
                );
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
    let orphan_dirs = inode_stats.orphan_directories.load(Ordering::Relaxed);
    let link_mismatches = inode_stats.link_count_mismatches.load(Ordering::Relaxed);

    stats.errors_found += corruptions
        + duplicates
        + oob
        + corrupt_symlinks
        + corrupt_dirs
        + orphan_dirs
        + link_mismatches
        + total_block_discrepancy.false_free_blocks
        + total_inode_discrepancy.false_free_inodes
        + dev.media_errors();

    // 6. Active Repair Mode Execution
    let mut errors_were_corrected = false;
    let has_repairable_errors = total_block_discrepancy.false_free_blocks > 0
        || total_inode_discrepancy.false_free_inodes > 0;
    if args.repair && has_repairable_errors {
        info!("Active Repair Mode: Correcting false-free bitmaps with a flushed pre-image undo journal...");
        if let Ok(mut undo_journal) =
            AtomicUndoJournal::new(Some(&args.undo_file), block_size as u32)
        {
            // Repair Block Bitmaps
            if total_block_discrepancy.false_free_blocks > 0 {
                for (bg_idx, desc) in group_descriptors.iter().enumerate() {
                    if desc.is_block_uninit() {
                        continue;
                    }
                    let first_block =
                        (bg_idx as u64) * (blocks_per_group as u64) + first_data_block;
                    if first_block >= total_blocks {
                        break;
                    }
                    let blocks_in_this_group =
                        (total_blocks - first_block).min(blocks_per_group as u64) as u32;

                    if let Ok(mut disk_bm) = dev.read_block_bitmap(desc, is_64bit, block_size) {
                        let mut modified = false;
                        for i in 0..blocks_in_this_group {
                            let abs_block = first_block + i as u64;
                            if tracker.is_allocated(abs_block) {
                                let byte_idx = (i / 8) as usize;
                                let bit_idx = i % 8;
                                if byte_idx < disk_bm.len()
                                    && (disk_bm[byte_idx] & (1 << bit_idx)) == 0
                                {
                                    disk_bm[byte_idx] |= 1 << bit_idx;
                                    modified = true;
                                }
                            }
                        }

                        if modified {
                            match dev.read_block_bitmap(desc, is_64bit, block_size) {
                                Ok(orig_bm) if !orig_bm.is_empty() => {
                                    if let Err(e) = undo_journal
                                        .record_mutation(desc.block_bitmap(is_64bit), &orig_bm)
                                    {
                                        error!("Failed to record undo journal for group {}: {}. Aborting block mutation for safety.", bg_idx, e);
                                        continue;
                                    }
                                    if dev
                                        .write_block(
                                            desc.block_bitmap(is_64bit),
                                            block_size,
                                            &disk_bm,
                                        )
                                        .is_ok()
                                    {
                                        debug!("Repaired block bitmap for group {}", bg_idx);
                                        errors_were_corrected = true;
                                    }
                                }
                                _ => {
                                    error!("Failed to read pristine block bitmap for group {}. Aborting repair for safety.", bg_idx);
                                    continue;
                                }
                            }
                        }
                    }
                }
            }

            // Repair Inode Bitmaps
            if total_inode_discrepancy.false_free_inodes > 0 {
                for (bg_idx, desc) in group_descriptors.iter().enumerate() {
                    if let Some((_, inodes)) =
                        all_scanned_inodes.iter().find(|(idx, _)| *idx == bg_idx)
                    {
                        if let Ok(mut disk_inomb) =
                            dev.read_inode_bitmap(desc, is_64bit, block_size)
                        {
                            let mut modified = false;
                            for (i, inode) in inodes.iter().enumerate() {
                                if inode.is_used() {
                                    let byte_idx = i / 8;
                                    let bit_idx = i % 8;
                                    if byte_idx < disk_inomb.len()
                                        && (disk_inomb[byte_idx] & (1 << bit_idx)) == 0
                                    {
                                        disk_inomb[byte_idx] |= 1 << bit_idx;
                                        modified = true;
                                    }
                                }
                            }

                            if modified {
                                match dev.read_inode_bitmap(desc, is_64bit, block_size) {
                                    Ok(orig_inomb) if !orig_inomb.is_empty() => {
                                        if let Err(e) = undo_journal.record_mutation(
                                            desc.inode_bitmap(is_64bit),
                                            &orig_inomb,
                                        ) {
                                            error!("Failed to record undo journal for group {}: {}. Aborting inode mutation for safety.", bg_idx, e);
                                            continue;
                                        }
                                        if dev
                                            .write_block(
                                                desc.inode_bitmap(is_64bit),
                                                block_size,
                                                &disk_inomb,
                                            )
                                            .is_ok()
                                        {
                                            debug!("Repaired inode bitmap for group {}", bg_idx);
                                            errors_were_corrected = true;
                                        }
                                    }
                                    _ => {
                                        error!("Failed to read pristine inode bitmap for group {}. Aborting repair for safety.", bg_idx);
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let _ = dev.flush_kernel_buffers();
            info!(
                "Repairs written; block pre-images stored in undo journal: {}",
                args.undo_file
            );
        }
    }

    // 7. Output Reporting (Text or JSON)
    if args.json {
        println!("{{");
        println!("  \"total_block_groups\": {},", bg_count);
        println!(
            "  \"inodes_scanned\": {},",
            inode_stats.total_inodes_scanned.load(Ordering::Relaxed)
        );
        println!(
            "  \"active_inodes\": {},",
            inode_stats.used_inodes.load(Ordering::Relaxed)
        );
        println!(
            "  \"directory_inodes\": {},",
            inode_stats.directory_inodes.load(Ordering::Relaxed)
        );
        println!(
            "  \"regular_file_inodes\": {},",
            inode_stats.regular_file_inodes.load(Ordering::Relaxed)
        );
        println!(
            "  \"symlink_inodes\": {},",
            inode_stats.symlink_inodes.load(Ordering::Relaxed)
        );
        println!("  \"fast_symlinks_checked\": {},", fast_symlinks);
        println!("  \"corrupted_symlinks\": {},", corrupt_symlinks);
        println!(
            "  \"extent_trees_validated\": {},",
            inode_stats.extent_trees_checked.load(Ordering::Relaxed)
        );
        println!("  \"allocated_blocks\": {},", tracker.allocated_count());
        println!("  \"directory_entries\": {},", total_dentry_count);
        println!("  \"corrupt_directories\": {},", corrupt_dirs);
        println!("  \"orphan_directories\": {},", orphan_dirs);
        println!("  \"link_count_mismatches\": {},", link_mismatches);
        println!("  \"extent_corruptions\": {},", corruptions);
        println!("  \"duplicate_blocks\": {},", duplicates);
        println!("  \"out_of_bounds_blocks\": {},", oob);
        println!(
            "  \"false_free_blocks\": {},",
            total_block_discrepancy.false_free_blocks
        );
        println!(
            "  \"leaked_blocks\": {},",
            total_block_discrepancy.leaked_blocks
        );
        println!(
            "  \"false_free_inodes\": {},",
            total_inode_discrepancy.false_free_inodes
        );
        println!(
            "  \"leaked_inodes\": {},",
            total_inode_discrepancy.leaked_inodes
        );
        println!("  \"journal_dirty\": {},", journal_is_dirty);
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
        println!("Corrupt Directories: {}", corrupt_dirs);
        println!("Orphan Directories : {}", orphan_dirs);
        println!("Link Discrepancies : {}", link_mismatches);
        println!("Extent Corruptions : {}", corruptions);
        println!("Duplicate Blocks   : {}", duplicates);
        println!("Out-of-Bounds Blks : {}", oob);
        println!("Corrupt Symlinks   : {}", corrupt_symlinks);
        println!(
            "False-Free Blocks  : {}",
            total_block_discrepancy.false_free_blocks
        );
        println!(
            "Leaked Blocks      : {}",
            total_block_discrepancy.leaked_blocks
        );
        println!(
            "False-Free Inodes  : {}",
            total_inode_discrepancy.false_free_inodes
        );
        println!(
            "Leaked Inodes      : {}",
            total_inode_discrepancy.leaked_inodes
        );
        println!("Journal Dirty      : {}", journal_is_dirty);
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
