//! `nexfsck` — experimental ext4 file system checker

use clap::{Parser, ValueEnum};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::FileTypeExt;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use zerocopy::IntoBytes;

use nexfsck_compute::{
    collect_directory_blocks, collect_inode_extent_tree_blocks, collect_inode_extents,
    reconcile_block_bitmap, reconcile_inode_bitmap, verify_directory_block_compact,
    verify_htree_directory, verify_inodes_parallel_with_metadata, BitmapDiscrepancy,
    BlockAllocationTracker, Ext4MetadataChecksum, HardwareProfile, InodeBitmapDiscrepancy,
    InodeChecksumResult, InodeChecksumVerifier, InodeVerificationStats, XattrBlockValidation,
};
use nexfsck_gpu::{BlockInterval, GpuAccelerator};
use nexfsck_io::{BlockDevice, IoBackend};
use nexfsck_journal::AtomicUndoJournal;
use nexfsck_tui::{LiveMetrics, MetricsServer, ProgressStats};

// Standard LSB / POSIX fsck exit codes
const FSCK_EXIT_OK: u8 = 0;
const FSCK_EXIT_ERRORS_CORRECTED: u8 = 1;
const FSCK_EXIT_ERRORS_UNCORRECTED: u8 = 4;
const FSCK_EXIT_OPERATIONAL_ERROR: u8 = 8;
#[allow(dead_code)]
const FSCK_EXIT_USAGE_ERROR: u8 = 16;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ComputeChoice {
    Auto,
    Cpu,
    Cuda,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum IoChoice {
    Auto,
    Uring,
    Sync,
}

struct Profile {
    enabled: bool,
    process_start: Instant,
    last: Instant,
    stages: Vec<(&'static str, Duration)>,
}

impl Profile {
    fn new(enabled: bool, process_start: Instant) -> Self {
        Self {
            enabled,
            process_start,
            last: process_start,
            stages: Vec::new(),
        }
    }

    fn mark(&mut self, name: &'static str) {
        if self.enabled {
            let now = Instant::now();
            self.stages.push((name, now.duration_since(self.last)));
            self.last = now;
        }
    }

    fn record(&mut self, name: &'static str, duration: Duration) {
        if self.enabled {
            self.stages.push((name, duration));
        }
    }

    fn report(&self, backend: &str, io: &str) {
        if !self.enabled {
            return;
        }
        eprintln!("NEXFSCK_PROFILE_BEGIN backend={backend} io={io}");
        for (name, duration) in &self.stages {
            eprintln!(
                "NEXFSCK_PROFILE stage={name} milliseconds={:.3}",
                duration.as_secs_f64() * 1000.0
            );
        }
        eprintln!(
            "NEXFSCK_PROFILE internal_process_ms={:.3}",
            self.process_start.elapsed().as_secs_f64() * 1000.0
        );
        eprintln!("NEXFSCK_PROFILE_END");
    }
}

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

    /// Print a low-overhead phase timing breakdown to stderr
    #[arg(long)]
    profile: bool,

    /// Select extent-collision compute backend
    #[arg(long = "compute-backend", value_enum, default_value_t = ComputeChoice::Auto)]
    compute_backend: ComputeChoice,

    /// Select metadata I/O backend
    #[arg(long = "io-backend", value_enum, default_value_t = IoChoice::Auto)]
    io_backend: IoChoice,
}

fn unsupported_ext4_feature(sb: &nexfsck_core::Ext4Superblock) -> Option<&'static str> {
    use nexfsck_core::*;
    if sb.desc_size() > 64 {
        return Some("group descriptors larger than 64 bytes");
    }
    // Compat bits may be ignored by a generic ext4 reader, but orphan_file
    // changes inode-recovery semantics that this checker does not validate.
    if sb.has_compat_feature(EXT4_FEATURE_COMPAT_ORPHAN_FILE) {
        return Some("orphan_file metadata");
    }
    let incompat = u32::from_le(sb.s_feature_incompat);
    let incompat_known = EXT4_FEATURE_INCOMPAT_FILETYPE
        | EXT4_FEATURE_INCOMPAT_RECOVER
        | EXT4_FEATURE_INCOMPAT_JOURNAL_DEV
        | EXT4_FEATURE_INCOMPAT_META_BG
        | EXT4_FEATURE_INCOMPAT_EXTENTS
        | EXT4_FEATURE_INCOMPAT_64BIT
        | EXT4_FEATURE_INCOMPAT_MMP
        | EXT4_FEATURE_INCOMPAT_FLEX_BG
        | EXT4_FEATURE_INCOMPAT_CSUM_SEED
        | EXT4_FEATURE_INCOMPAT_EA_INODE
        | EXT4_FEATURE_INCOMPAT_LARGEDIR
        | EXT4_FEATURE_INCOMPAT_INLINE_DATA
        | EXT4_FEATURE_INCOMPAT_ENCRYPT
        | EXT4_FEATURE_INCOMPAT_CASEFOLD;
    if incompat & !incompat_known != 0 {
        return Some("unknown incompat feature bits");
    }
    for (bit, name) in [
        (EXT4_FEATURE_INCOMPAT_JOURNAL_DEV, "external_journal_device"),
        (
            EXT4_FEATURE_INCOMPAT_MMP,
            "MMP checksum validation is incomplete",
        ),
        (EXT4_FEATURE_INCOMPAT_EA_INODE, "ea_inode"),
        (EXT4_FEATURE_INCOMPAT_LARGEDIR, "large_dir"),
        (EXT4_FEATURE_INCOMPAT_INLINE_DATA, "inline_data"),
        (EXT4_FEATURE_INCOMPAT_ENCRYPT, "encryption"),
        (EXT4_FEATURE_INCOMPAT_CASEFOLD, "casefold"),
    ] {
        if incompat & bit != 0 {
            return Some(name);
        }
    }
    let ro = u32::from_le(sb.s_feature_ro_compat);
    let ro_known = EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER
        | EXT4_FEATURE_RO_COMPAT_LARGE_FILE
        | EXT4_FEATURE_RO_COMPAT_BTREE_DIR
        | EXT4_FEATURE_RO_COMPAT_HUGE_FILE
        | EXT4_FEATURE_RO_COMPAT_GDT_CSUM
        | EXT4_FEATURE_RO_COMPAT_DIR_NLINK
        | EXT4_FEATURE_RO_COMPAT_EXTRA_ISIZE
        | EXT4_FEATURE_RO_COMPAT_METADATA_CSUM;
    if ro & !ro_known != 0 {
        return Some("unknown or unverified ro_compat feature bits");
    }
    for (bit, name) in [
        (EXT4_FEATURE_RO_COMPAT_QUOTA, "quota metadata"),
        (EXT4_FEATURE_RO_COMPAT_BIGALLOC, "bigalloc"),
        (EXT4_FEATURE_RO_COMPAT_PROJECT, "project quota"),
        (EXT4_FEATURE_RO_COMPAT_VERITY, "verity"),
    ] {
        if ro & bit != 0 {
            return Some(name);
        }
    }
    if u32::from_le(sb.s_feature_compat) & EXT4_FEATURE_COMPAT_ORPHAN_FILE != 0 {
        return Some("orphan_file");
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairBlockReason {
    MetadataChecksumFailure,
    BackupGeometryConflict,
    ExtentSemanticFailure,
    XattrIntegrityFailure,
    XattrHashFailure,
    XattrRefcountFailure,
    XattrSemanticFailure,
    ExtentMetadataOverlap,
    DirectoryReferenceFailure,
    JournalIntegrityUnknown,
    JournalReplayRequired,
    MediaError,
}

impl RepairBlockReason {
    fn code(self) -> &'static str {
        match self {
            Self::MetadataChecksumFailure => "metadata_checksum_failure",
            Self::BackupGeometryConflict => "backup_geometry_conflict",
            Self::ExtentSemanticFailure => "extent_semantic_failure",
            Self::XattrIntegrityFailure => "xattr_integrity_failure",
            Self::XattrHashFailure => "xattr_hash_failure",
            Self::XattrRefcountFailure => "xattr_refcount_failure",
            Self::XattrSemanticFailure => "xattr_semantic_failure",
            Self::ExtentMetadataOverlap => "extent_metadata_overlap",
            Self::DirectoryReferenceFailure => "directory_reference_failure",
            Self::JournalIntegrityUnknown => "journal_integrity_unknown",
            Self::JournalReplayRequired => "journal_replay_required",
            Self::MediaError => "media_error",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::MetadataChecksumFailure => "metadata checksum failure",
            Self::BackupGeometryConflict => "backup superblock integrity/geometry conflict",
            Self::ExtentSemanticFailure => "inode or extent structure is invalid",
            Self::XattrIntegrityFailure => "external xattr structure or read is invalid",
            Self::XattrHashFailure => "external xattr semantic hash is invalid",
            Self::XattrRefcountFailure => "external xattr shared reference count disagrees",
            Self::XattrSemanticFailure => "external xattr entries are structurally invalid",
            Self::ExtentMetadataOverlap => {
                "file data extent overlaps protected filesystem metadata"
            }
            Self::DirectoryReferenceFailure => "directory, symlink, or reference validation failed",
            Self::JournalIntegrityUnknown => "journal integrity is uncertain",
            Self::JournalReplayRequired => "journal replay is required and unsupported",
            Self::MediaError => "storage read reported media errors",
        }
    }
}

#[derive(Debug, Default)]
struct RepairTrustState {
    blocked_reasons: Vec<RepairBlockReason>,
}

#[derive(Debug, Default)]
struct BackupSuperblockSummary {
    checked: u64,
    valid_consistent: u64,
    invalid_checksum: u64,
    invalid_magic_or_unreadable: u64,
    inconsistent: u64,
}

fn verify_expected_backup_superblocks(
    dev: &BlockDevice,
    primary: &nexfsck_core::Ext4Superblock,
    checksum: Ext4MetadataChecksum,
) -> BackupSuperblockSummary {
    let mut summary = BackupSuperblockSummary::default();
    let block_size = primary.block_size();
    let first_data = u64::from(u32::from_le(primary.s_first_data_block));
    let blocks_per_group = u64::from(primary.blocks_per_group());
    if block_size == 0 || blocks_per_group == 0 {
        summary.invalid_magic_or_unreadable += 1;
        return summary;
    }

    for group in 1..primary.block_groups_count() {
        if !primary.group_has_superblock(group) {
            continue;
        }
        let Some(block) = group
            .checked_mul(blocks_per_group)
            .and_then(|offset| first_data.checked_add(offset))
        else {
            summary.invalid_magic_or_unreadable += 1;
            continue;
        };
        summary.checked += 1;
        let candidate = match dev.read_superblock_at_block(block, block_size) {
            Ok(candidate) => candidate,
            Err(error) => {
                summary.invalid_magic_or_unreadable += 1;
                warn!(
                    "Backup superblock group {group} at block {block} unreadable/invalid: {error}"
                );
                continue;
            }
        };
        if !checksum.verify_superblock(candidate.as_bytes()) {
            summary.invalid_checksum += 1;
            warn!("Backup superblock group {group} at block {block} has an invalid checksum");
            continue;
        }
        let same_geometry = candidate.total_blocks() == primary.total_blocks()
            && candidate.total_inodes() == primary.total_inodes()
            && candidate.block_size() == primary.block_size()
            && candidate.blocks_per_group() == primary.blocks_per_group()
            && candidate.inodes_per_group() == primary.inodes_per_group()
            && candidate.inode_size() == primary.inode_size()
            && candidate.desc_size() == primary.desc_size()
            && candidate.s_uuid == primary.s_uuid
            && candidate.s_feature_compat == primary.s_feature_compat
            && candidate.s_feature_incompat == primary.s_feature_incompat
            && candidate.s_feature_ro_compat == primary.s_feature_ro_compat
            && candidate.s_checksum_type == primary.s_checksum_type
            && candidate.s_checksum_seed == primary.s_checksum_seed
            && u16::from_le(candidate.s_block_group_nr) as u64 == group;
        if same_geometry {
            summary.valid_consistent += 1;
        } else {
            summary.inconsistent += 1;
            warn!("Backup superblock group {group} at block {block} has a valid checksum but disagrees with primary geometry/features");
        }
    }
    info!(
        "Backup superblocks: checked={}, valid_consistent={}, invalid_checksum={}, invalid_magic_or_unreadable={}, inconsistent={}",
        summary.checked,
        summary.valid_consistent,
        summary.invalid_checksum,
        summary.invalid_magic_or_unreadable,
        summary.inconsistent
    );
    summary
}

impl RepairTrustState {
    fn block(&mut self, reason: RepairBlockReason) {
        if !self.blocked_reasons.contains(&reason) {
            self.blocked_reasons.push(reason);
        }
    }

    fn is_eligible(&self) -> bool {
        self.blocked_reasons.is_empty()
    }
}

fn main() -> ExitCode {
    let process_start = Instant::now();
    let args = Args::parse();
    let mut profile = Profile::new(args.profile, process_start);
    profile.mark("cli_config_parsing");

    // Initialize tracing
    let filter = if args.verbose { "debug" } else { "info" };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
    profile.mark("logging_initialization");

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
    profile.mark("hardware_detection");

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
            "deferred until collision workload is known", 0.0, false
        );
    }

    // 2. Open Block Device & Flush Cache
    let is_read_only = !args.repair;
    let io_backend = match args.io_backend {
        IoChoice::Auto => match std::fs::metadata(&args.device) {
            Ok(metadata) if metadata.file_type().is_block_device() => IoBackend::IoUring,
            _ => IoBackend::Sync,
        },
        IoChoice::Uring => IoBackend::IoUring,
        IoChoice::Sync => IoBackend::Sync,
    };
    let dev = match BlockDevice::open_with_backend(&args.device, is_read_only, io_backend) {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to open device '{}': {}", args.device, e);
            return ExitCode::from(FSCK_EXIT_OPERATIONAL_ERROR);
        }
    };
    profile.mark("filesystem_open_io_uring_registered_buffers");
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
        let mut selected = None;
        let mut invalid_checksum = false;
        let mut last_error = None;
        for block_size in [4096, 2048, 1024] {
            match dev.read_superblock_at_block(blk, block_size) {
                Ok(candidate) => {
                    let verifier = Ext4MetadataChecksum::new(&candidate);
                    if verifier.verify_superblock(candidate.as_bytes()) {
                        selected = Some(candidate);
                        break;
                    }
                    invalid_checksum = true;
                }
                Err(error) => last_error = Some(error),
            }
        }
        if let Some(candidate) = selected {
            info!(
                "Using checksum-validated backup superblock at block {}",
                blk
            );
            candidate
        } else {
            error!(
                "Backup superblock at block {} could not be validated: {}",
                blk,
                last_error.map_or_else(
                    || "invalid superblock checksum".to_string(),
                    |e| e.to_string()
                )
            );
            if invalid_checksum && args.json {
                println!("{{\"superblock_checksum_failures\":1,\"errors_detected\":1}}");
            }
            return ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED);
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
                        "Primary superblock is invalid. Backup superblock candidates at block(s): {:?}.",
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
    let metadata_checksum = Ext4MetadataChecksum::new(&sb);
    let checksum_started = Instant::now();
    let superblock_checksum_valid = metadata_checksum.verify_superblock(sb.as_bytes());
    profile.record("superblock_checksum_validation", checksum_started.elapsed());
    if !superblock_checksum_valid {
        error!("Selected superblock checksum is invalid");
        let candidates = dev.find_backup_superblocks();
        if !candidates.is_empty() {
            let blocks: Vec<u64> = candidates.iter().map(|(block, _)| *block).collect();
            warn!(
                "Backup superblock candidates (checksum must be checked with --backup-sb): {:?}",
                blocks
            );
        }
        if args.json {
            println!("{{\"superblock_checksum_failures\":1,\"errors_detected\":1}}");
        }
        return ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED);
    }
    if let Some(feature) = unsupported_ext4_feature(&sb) {
        error!(
            "Filesystem feature '{}' is not supported by this checker; refusing to report it clean",
            feature
        );
        return ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED);
    }
    let backup_superblocks = verify_expected_backup_superblocks(&dev, &sb, metadata_checksum);
    let backup_superblock_failures = backup_superblocks.invalid_checksum
        + backup_superblocks.invalid_magic_or_unreadable
        + backup_superblocks.inconsistent;
    profile.mark("superblock_initialization");

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
    let descriptor_size = sb.desc_size();
    let checksum_started = Instant::now();
    let bad_group_descriptors = group_descriptors
        .iter()
        .enumerate()
        .filter(|(index, desc)| {
            !metadata_checksum.verify_group_desc(*index as u32, desc, descriptor_size)
        })
        .count();
    profile.record(
        "group_descriptor_checksum_validation",
        checksum_started.elapsed(),
    );
    if bad_group_descriptors != 0 {
        error!(
            "{} group descriptor checksum(s) are invalid",
            bad_group_descriptors
        );
        if args.json {
            println!(
                "{{\"group_descriptor_checksum_failures\":{},\"errors_detected\":{}}}",
                bad_group_descriptors, bad_group_descriptors
            );
        }
        return ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED);
    }
    profile.mark("block_group_metadata_reads");

    // JBD2 Crash Recovery Journal Inspection
    let mut journal_is_dirty = false;
    let mut journal_integrity_failures = 0u64;
    let mut journal_transaction_blocks_checked = 0u64;
    let mut journal_committed_transactions = 0u64;
    let mut journal_integrity_state = "absent";
    match nexfsck_journal::inspect_journal(&dev, &sb, &group_descriptors, is_64bit) {
        Ok(Some(inspection)) => {
            journal_transaction_blocks_checked = inspection.transaction_blocks_checked;
            journal_committed_transactions = inspection.committed_transactions;
            journal_integrity_state = match inspection.state {
                nexfsck_journal::JournalTransactionState::Clean => "clean",
                nexfsck_journal::JournalTransactionState::CommittedTransactions => {
                    "committed_transactions"
                }
                nexfsck_journal::JournalTransactionState::IncompleteTail => "incomplete_tail",
            };
            let jbd = inspection.superblock;
            let is_dirty = !jbd.is_clean()
                || sb.has_incompat_feature(nexfsck_core::EXT4_FEATURE_INCOMPAT_RECOVER);
            journal_is_dirty = is_dirty;
            if !args.json {
                info!(
                    "JBD2 Journal Active | Seq: {} | Block Size: {}B | Clean: {}",
                    jbd.sequence(),
                    jbd.block_size(),
                    !is_dirty
                );
                info!(
                    "JBD2 transaction inspection: {:?}, {} blocks checked, {} committed transaction(s)",
                    inspection.state,
                    inspection.transaction_blocks_checked,
                    inspection.committed_transactions
                );
            }
            if is_dirty {
                warn!("Filesystem journal is DIRTY: uncommitted or pending transactions exist.");
            }
        }
        Ok(None) => {}
        Err(error) => {
            journal_is_dirty = true;
            journal_integrity_failures = 1;
            journal_integrity_state = "corrupt_or_unsupported";
            error!("JBD2 journal integrity could not be established: {}", error);
        }
    }
    profile.mark("journal_inspection");

    // 5. Initialize adaptive allocation tracking and telemetry
    let tracker = BlockAllocationTracker::new(total_blocks);
    let protected_metadata_tracker = BlockAllocationTracker::new(total_blocks);
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
    let reserved_gdt = if sb.has_compat_feature(nexfsck_core::EXT4_FEATURE_COMPAT_RESIZE_INODE) {
        u16::from_le(sb.s_reserved_gdt_blocks) as u64
    } else {
        0
    };
    let inode_table_blocks =
        (sb.inodes_per_group() as u64 * sb.inode_size() as u64).div_ceil(block_size);

    if first_data_block > 0 {
        tracker.mark_range(0, first_data_block as u32);
        protected_metadata_tracker.mark_range(0, first_data_block as u32);
    }

    for (bg_idx, desc) in group_descriptors.iter().enumerate() {
        let first_block = (bg_idx as u64) * (blocks_per_group as u64) + first_data_block;
        if sb.group_has_superblock(bg_idx as u64) && first_block < total_blocks {
            let meta_blocks =
                (1 + gdt_blocks + reserved_gdt).min(total_blocks - first_block) as u32;
            tracker.mark_range(first_block, meta_blocks);
            protected_metadata_tracker.mark_range(first_block, meta_blocks);
        }
        tracker.mark_range(desc.block_bitmap(is_64bit), 1);
        protected_metadata_tracker.mark_range(desc.block_bitmap(is_64bit), 1);
        tracker.mark_range(desc.inode_bitmap(is_64bit), 1);
        protected_metadata_tracker.mark_range(desc.inode_bitmap(is_64bit), 1);
        tracker.mark_range(desc.inode_table_block(is_64bit), inode_table_blocks as u32);
        protected_metadata_tracker
            .mark_range(desc.inode_table_block(is_64bit), inode_table_blocks as u32);
    }
    profile.mark("metadata_tracker_setup");

    if !args.json {
        info!("Pass 1: Checking Inode Tables and Extent Trees in parallel...");
    }

    let mut directory_blocks_to_check = Vec::new();
    let mut directory_layouts: std::collections::HashMap<u32, (Vec<u64>, bool)> =
        std::collections::HashMap::new();
    let mut inode_generations: std::collections::HashMap<u32, u32> =
        std::collections::HashMap::new();
    let mut all_scanned_inodes = Vec::new();
    let mut compute_intervals = Vec::new();
    let mut inode_table_read_time = Duration::ZERO;
    let mut inode_decode_time = Duration::ZERO;
    let mut inode_metadata_collection_time = Duration::ZERO;
    let mut inode_validation_tracking_time = Duration::ZERO;
    let mut inode_checksum_time = Duration::ZERO;
    let mut inactive_inode_count = 0u64;
    let inode_checksum_verifier = InodeChecksumVerifier::new(&sb);
    let mut invalid_inode_checksums = HashSet::new();
    // Count every inode reference, but read/validate each shared xattr block once.
    let mut xattr_references: HashMap<u64, (u32, Option<u32>)> = HashMap::new();
    let mut xattr_validation_time = Duration::ZERO;

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
        let operation_started = Instant::now();
        let table_result = dev.read_inode_table(&sb, desc, is_64bit);
        inode_table_read_time += operation_started.elapsed();
        match table_result {
            Ok(table_bytes) => {
                let inode_size = sb.inode_size() as usize;
                let first_inode = (bg_idx as u32) * sb.inodes_per_group() + 1;
                let operation_started = Instant::now();
                for (index, raw_inode) in table_bytes.chunks_exact(inode_size).enumerate() {
                    inode_stats
                        .inode_checksum_validations
                        .fetch_add(1, Ordering::Relaxed);
                    if matches!(
                        inode_checksum_verifier.verify(first_inode + index as u32, raw_inode),
                        InodeChecksumResult::Invalid { .. }
                    ) {
                        invalid_inode_checksums.insert(first_inode + index as u32);
                        inode_stats
                            .inode_checksum_failures
                            .fetch_add(1, Ordering::Relaxed);
                        group_had_error = true;
                    }
                }
                inode_checksum_time += operation_started.elapsed();
                let operation_started = Instant::now();
                let inodes = BlockDevice::parse_inodes_from_table(&table_bytes, inode_size);
                inode_decode_time += operation_started.elapsed();
                inactive_inode_count +=
                    inodes.iter().filter(|inode| !inode.is_used()).count() as u64;

                let operation_started = Instant::now();
                for (i, inode) in inodes.iter().enumerate() {
                    let ino_num = (bg_idx as u32) * sb.inodes_per_group() + (i as u32) + 1;
                    let generation = u32::from_le(inode.i_generation);
                    let xattr_block = inode.external_xattr_block();
                    if inode.is_used()
                        && !invalid_inode_checksums.contains(&ino_num)
                        && xattr_block != 0
                    {
                        let entry = xattr_references.entry(xattr_block).or_insert((0, None));
                        entry.0 = entry.0.saturating_add(1);
                        if xattr_block >= total_blocks {
                            inode_stats
                                .xattr_block_corruptions
                                .fetch_add(1, Ordering::Relaxed);
                            group_had_error = true;
                        } else if entry.1.is_none() {
                            tracker.mark_range(xattr_block, 1);
                            protected_metadata_tracker.mark_range(xattr_block, 1);
                            let started = Instant::now();
                            match dev.read_block(xattr_block, block_size) {
                                Ok(bytes) => match metadata_checksum
                                    .validate_xattr_block(xattr_block, &bytes)
                                {
                                    XattrBlockValidation::Valid {
                                        refcount,
                                        entry_count,
                                    } => {
                                        entry.1 = Some(refcount);
                                        inode_stats
                                            .xattr_blocks_checked
                                            .fetch_add(1, Ordering::Relaxed);
                                        inode_stats
                                            .xattr_entries_checked
                                            .fetch_add(entry_count as u64, Ordering::Relaxed);
                                    }
                                    XattrBlockValidation::InvalidStructure => {
                                        inode_stats
                                            .xattr_block_corruptions
                                            .fetch_add(1, Ordering::Relaxed);
                                        inode_stats
                                            .xattr_semantic_failures
                                            .fetch_add(1, Ordering::Relaxed);
                                        group_had_error = true;
                                    }
                                    XattrBlockValidation::InvalidChecksum => {
                                        inode_stats
                                            .xattr_checksum_failures
                                            .fetch_add(1, Ordering::Relaxed);
                                        group_had_error = true;
                                    }
                                    XattrBlockValidation::InvalidHash => {
                                        inode_stats
                                            .xattr_hash_failures
                                            .fetch_add(1, Ordering::Relaxed);
                                        group_had_error = true;
                                    }
                                },
                                Err(_) => {
                                    inode_stats
                                        .xattr_block_corruptions
                                        .fetch_add(1, Ordering::Relaxed);
                                    group_had_error = true;
                                }
                            }
                            xattr_validation_time += started.elapsed();
                        }
                    }
                    if inode.is_used() && inode.is_dir() {
                        inode_generations.insert(ino_num, generation);
                        let blocks = collect_directory_blocks(inode, &|blk| {
                            dev.read_block(blk, block_size).ok()
                        });
                        directory_layouts
                            .insert(ino_num, (blocks.clone(), inode.is_indexed_directory()));
                        for blk in blocks {
                            directory_blocks_to_check.push((ino_num, blk));
                        }
                    }
                    if inode.is_used()
                        && ino_num == u32::from_le(sb.s_journal_inum)
                        && inode.uses_extents()
                        && !inode.is_inline_data()
                    {
                        for (start_block, block_count) in collect_inode_extents(inode, &|blk| {
                            dev.read_block(blk, block_size).ok()
                        }) {
                            protected_metadata_tracker.mark_range(start_block, block_count);
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
                inode_metadata_collection_time += operation_started.elapsed();

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

    // All inode-referenced xattr and journal data blocks have now been found.
    // Pre-discover external extent-tree nodes too, so cross-inode data pointers
    // cannot evade metadata-overlap checks due to inode scan order.
    let metadata_schedule_started = Instant::now();
    let extent_metadata_tracker = BlockAllocationTracker::new_sparse(sb.total_blocks());
    for (_, inodes) in &all_scanned_inodes {
        for inode in inodes {
            for block in collect_inode_extent_tree_blocks(inode, &|block| {
                dev.read_block(block, block_size).ok()
            }) {
                extent_metadata_tracker.mark_range(block, 1);
            }
        }
    }
    for (bg_idx, inodes) in &all_scanned_inodes {
        let first_inode = (*bg_idx as u32) * sb.inodes_per_group() + 1;
        let operation_started = Instant::now();
        verify_inodes_parallel_with_metadata(
            inodes,
            first_inode,
            &tracker,
            Some(&protected_metadata_tracker),
            Some(&extent_metadata_tracker),
            u32::from_le(sb.s_journal_inum),
            &inode_stats,
            metadata_checksum,
            &|blk| dev.read_block(blk, block_size).ok(),
        );
        inode_validation_tracking_time += operation_started.elapsed();
    }
    inode_metadata_collection_time += metadata_schedule_started.elapsed();
    profile.mark("inode_table_scanning_extent_tree_parsing");
    profile.record("inode_table_reads", inode_table_read_time);
    profile.record("inode_decoding", inode_decode_time);
    profile.record("inode_checksum_total", inode_checksum_time);
    profile.record("xattr_block_reads_validation", xattr_validation_time);
    profile.record(
        "inode_metadata_extent_collection",
        inode_metadata_collection_time,
    );
    profile.record(
        "inode_validation_allocation_tracking",
        inode_validation_tracking_time,
    );
    if profile.enabled {
        eprintln!("NEXFSCK_PROFILE counter=inactive_inodes value={inactive_inode_count}");
    }

    // CUDA is deliberately lazy: loading the driver/context before the amount
    // of collision work is known was the dominant fixed startup cost.
    let gpu = match args.compute_backend {
        ComputeChoice::Cpu => GpuAccelerator::cpu_only(),
        ComputeChoice::Cuda => GpuAccelerator::probe(),
        // Calibration found no end-to-end CUDA crossover in the tested range.
        // Auto therefore remains on CPU; CUDA stays explicitly selectable so
        // future machines/workloads can be recalibrated without hiding work.
        ComputeChoice::Auto => GpuAccelerator::cpu_only(),
    };
    profile.mark("cuda_driver_context_module_initialization");
    let compute_backend = if gpu.is_available() {
        "CUDA PTX + CPU revalidation"
    } else {
        "CPU fallback"
    };
    let (gpu_collisions, collision_profile) =
        gpu.find_interval_collisions_profiled(&mut compute_intervals);
    profile.mark("extent_collision_total");
    profile.record("extent_collision_sort", collision_profile.sort);
    profile.record(
        "host_to_gpu_preparation_copy",
        collision_profile.host_preparation_and_copy,
    );
    profile.record("cuda_kernel_launch", collision_profile.kernel_launch);
    profile.record("gpu_synchronization", collision_profile.gpu_synchronization);
    profile.record("gpu_copy_back", collision_profile.copy_back);
    profile.record(
        "cpu_collision_revalidation",
        collision_profile.cpu_revalidation,
    );
    drop(gpu);
    profile.mark("cuda_cleanup_shutdown");
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
    let inode_slots = sb.total_inodes() as usize + 1;
    let mut actual_link_counts = vec![0u16; inode_slots];
    let mut reachable_from_dir = vec![false; inode_slots];
    reachable_from_dir[nexfsck_core::EXT4_ROOT_INO as usize] = true;

    let blocks_to_fetch: Vec<u64> = directory_blocks_to_check.iter().map(|(_, b)| *b).collect();
    let directory_read_started = Instant::now();
    let batch_reads = dev.read_blocks_batch(&blocks_to_fetch, block_size);
    let directory_read_time = directory_read_started.elapsed();
    let cache_build_started = Instant::now();
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
    if invalid_inode_checksums.is_empty() {
        for (observed, header_refcount) in xattr_references.values() {
            if let Some(header_refcount) = header_refcount {
                if observed != header_refcount {
                    inode_stats
                        .xattr_refcount_failures
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
    let directory_cache_build_time = cache_build_started.elapsed();

    let htree_started = Instant::now();
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
                .map(Vec::as_slice)
        });
        for message in &htree.errors {
            warn!("H-Tree inode {}: {}", inode, message);
        }
        inode_stats
            .corrupt_directories
            .fetch_add(htree.errors.len() as u64, Ordering::Relaxed);
        inode_stats
            .htree_structural_failures
            .fetch_add(htree.errors.len() as u64, Ordering::Relaxed);
    }
    let htree_time = htree_started.elapsed();

    let dirent_started = Instant::now();
    let directory_blocks_checked = directory_blocks_to_check.len() as u64;
    let mut directory_checksum_time = Duration::ZERO;
    for (_dir_ino, dir_block) in directory_blocks_to_check {
        if let Some(block_bytes) = block_cache.get(&dir_block) {
            let is_root = directory_layouts
                .get(&_dir_ino)
                .is_some_and(|(layout, indexed)| *indexed && layout.first() == Some(&dir_block));
            let generation = inode_generations.get(&_dir_ino).copied().unwrap_or(0);
            let checksum_started = Instant::now();
            inode_stats
                .directory_checksum_validations
                .fetch_add(1, Ordering::Relaxed);
            let checksum_valid = metadata_checksum.verify_directory_block(
                _dir_ino,
                generation,
                block_bytes,
                is_root,
            );
            directory_checksum_time += checksum_started.elapsed();
            if !checksum_valid {
                inode_stats
                    .directory_checksum_failures
                    .fetch_add(1, Ordering::Relaxed);
            }
            let res = verify_directory_block_compact(block_bytes, sb.total_inodes());
            total_dentry_count += res.entries.len();
            if res.corrupt_entries > 0 {
                inode_stats
                    .corrupt_directories
                    .fetch_add(res.corrupt_entries, Ordering::Relaxed);
                inode_stats
                    .directory_structural_failures
                    .fetch_add(res.corrupt_entries, Ordering::Relaxed);
            }
            for entry in res.entries {
                actual_link_counts[entry.inode as usize] =
                    actual_link_counts[entry.inode as usize].saturating_add(1);
                if !entry.is_dot_or_dotdot {
                    reachable_from_dir[entry.inode as usize] = true;
                }
            }
        }
    }
    let dirent_time = dirent_started.elapsed();
    profile.mark("directory_pass");
    profile.record("directory_block_reads", directory_read_time);
    profile.record("directory_block_cache_build", directory_cache_build_time);
    profile.record("directory_htree_validation", htree_time);
    profile.record("directory_checksum_validation", directory_checksum_time);
    profile.record("directory_dirent_parse_and_reference_arrays", dirent_time);
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
                && !reachable_from_dir[ino_num as usize]
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
    profile.mark("connectivity_pass");
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
                let actual_links = actual_link_counts[ino_num as usize];
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
    profile.mark("reference_count_pass");
    if !args.json {
        info!(
            "Pass 4: Inode reference counts validated (mismatches: {}).",
            link_mismatches_count
        );
    }

    // Pass 5: Block & Inode Allocation Bitmap Reconciliation
    if !args.json {
        info!("Pass 5: Reconciling on-disk bitmaps against reconstructed allocation state...");
    }
    let mut total_block_discrepancy = BitmapDiscrepancy::default();
    let mut total_inode_discrepancy = InodeBitmapDiscrepancy::default();
    let mut block_bitmap_read_time = Duration::ZERO;
    let mut inode_bitmap_read_time = Duration::ZERO;
    let mut block_bitmap_compare_time = Duration::ZERO;
    let mut inode_bitmap_compare_time = Duration::ZERO;
    let mut block_bitmap_checksum_time = Duration::ZERO;
    let mut inode_bitmap_checksum_time = Duration::ZERO;
    for (bg_idx, desc) in group_descriptors.iter().enumerate() {
        if desc.is_block_uninit() && desc.is_inode_uninit() {
            continue;
        }

        let first_block = (bg_idx as u64) * (blocks_per_group as u64) + first_data_block;
        if first_block >= total_blocks {
            break;
        }
        let blocks_in_this_group = (total_blocks - first_block).min(blocks_per_group as u64) as u32;

        if !desc.is_block_uninit() {
            let operation_started = Instant::now();
            let block_bitmap = dev.read_block_bitmap(desc, is_64bit, block_size);
            block_bitmap_read_time += operation_started.elapsed();
            if let Ok(disk_bm) = block_bitmap {
                let checksum_started = Instant::now();
                let checksum_valid = metadata_checksum.verify_block_bitmap(
                    bg_idx as u32,
                    &disk_bm,
                    desc,
                    descriptor_size,
                );
                block_bitmap_checksum_time += checksum_started.elapsed();
                if !checksum_valid {
                    inode_stats
                        .block_bitmap_checksum_failures
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    let operation_started = Instant::now();
                    let disc = reconcile_block_bitmap(
                        &disk_bm,
                        &tracker,
                        first_block,
                        blocks_in_this_group,
                    );
                    block_bitmap_compare_time += operation_started.elapsed();
                    total_block_discrepancy.false_free_blocks += disc.false_free_blocks;
                    total_block_discrepancy.leaked_blocks += disc.leaked_blocks;
                }
            }
        }

        if !desc.is_inode_uninit() {
            let operation_started = Instant::now();
            let inode_bitmap = dev.read_inode_bitmap(desc, is_64bit, block_size);
            inode_bitmap_read_time += operation_started.elapsed();
            if let Ok(disk_inomb) = inode_bitmap {
                let checksum_started = Instant::now();
                let checksum_valid = metadata_checksum.verify_inode_bitmap(
                    bg_idx as u32,
                    &disk_inomb
                        [..((sb.inodes_per_group() as usize).div_ceil(8)).min(disk_inomb.len())],
                    desc,
                    descriptor_size,
                );
                inode_bitmap_checksum_time += checksum_started.elapsed();
                if !checksum_valid {
                    inode_stats
                        .inode_bitmap_checksum_failures
                        .fetch_add(1, Ordering::Relaxed);
                } else if let Some((_, inodes)) =
                    all_scanned_inodes.iter().find(|(idx, _)| *idx == bg_idx)
                {
                    let operation_started = Instant::now();
                    let disc = reconcile_inode_bitmap(
                        &disk_inomb,
                        inodes,
                        sb.first_inode(),
                        bg_idx,
                        sb.inodes_per_group(),
                    );
                    inode_bitmap_compare_time += operation_started.elapsed();
                    total_inode_discrepancy.false_free_inodes += disc.false_free_inodes;
                    total_inode_discrepancy.leaked_inodes += disc.leaked_inodes;
                }
            }
        }
    }
    profile.mark("bitmap_reconciliation");
    profile.record("block_bitmap_reads", block_bitmap_read_time);
    profile.record("inode_bitmap_reads", inode_bitmap_read_time);
    profile.record(
        "block_bitmap_checksum_validation",
        block_bitmap_checksum_time,
    );
    profile.record(
        "inode_bitmap_checksum_validation",
        inode_bitmap_checksum_time,
    );
    profile.record("block_bitmap_word_comparison", block_bitmap_compare_time);
    profile.record("inode_bitmap_comparison", inode_bitmap_compare_time);

    let corruptions = inode_stats.extent_corruptions.load(Ordering::Relaxed);
    let duplicates = inode_stats.duplicate_blocks.load(Ordering::Relaxed);
    let oob = inode_stats.out_of_bounds_blocks.load(Ordering::Relaxed);
    let fast_symlinks = inode_stats.fast_symlinks_checked.load(Ordering::Relaxed);
    let corrupt_symlinks = inode_stats.corrupted_symlinks.load(Ordering::Relaxed);
    let orphan_dirs = inode_stats.orphan_directories.load(Ordering::Relaxed);
    let link_mismatches = inode_stats.link_count_mismatches.load(Ordering::Relaxed);
    let inode_checksum_validations = inode_stats
        .inode_checksum_validations
        .load(Ordering::Relaxed);
    let inode_checksum_failures = inode_stats.inode_checksum_failures.load(Ordering::Relaxed);
    let directory_checksum_validations = inode_stats
        .directory_checksum_validations
        .load(Ordering::Relaxed);
    let block_bitmap_checksum_failures = inode_stats
        .block_bitmap_checksum_failures
        .load(Ordering::Relaxed);
    let inode_bitmap_checksum_failures = inode_stats
        .inode_bitmap_checksum_failures
        .load(Ordering::Relaxed);
    let directory_checksum_failures = inode_stats
        .directory_checksum_failures
        .load(Ordering::Relaxed);
    let directory_structural_failures = inode_stats
        .directory_structural_failures
        .load(Ordering::Relaxed);
    let htree_structural_failures = inode_stats
        .htree_structural_failures
        .load(Ordering::Relaxed);
    let extent_block_checksum_failures = inode_stats
        .extent_block_checksum_failures
        .load(Ordering::Relaxed);
    let xattr_block_corruptions = inode_stats.xattr_block_corruptions.load(Ordering::Relaxed);
    let xattr_checksum_failures = inode_stats.xattr_checksum_failures.load(Ordering::Relaxed);
    let xattr_blocks_checked = inode_stats.xattr_blocks_checked.load(Ordering::Relaxed);
    let xattr_entries_checked = inode_stats.xattr_entries_checked.load(Ordering::Relaxed);
    let xattr_hash_failures = inode_stats.xattr_hash_failures.load(Ordering::Relaxed);
    let xattr_refcount_failures = inode_stats.xattr_refcount_failures.load(Ordering::Relaxed);
    let xattr_semantic_failures = inode_stats.xattr_semantic_failures.load(Ordering::Relaxed);
    let extent_metadata_overlaps = inode_stats
        .extent_metadata_overlap_failures
        .load(Ordering::Relaxed);
    let checksum_failures = inode_checksum_failures
        + block_bitmap_checksum_failures
        + inode_bitmap_checksum_failures
        + directory_checksum_failures
        + extent_block_checksum_failures
        + xattr_checksum_failures;
    let media_errors = dev.media_errors();

    // Repair trust is deliberately stricter than read-only reporting. A
    // bitmap discrepancy is repairable only when the metadata used to
    // reconstruct it is independently trustworthy.
    let mut repair_trust = RepairTrustState::default();
    if checksum_failures != 0 {
        repair_trust.block(RepairBlockReason::MetadataChecksumFailure);
    }
    if backup_superblock_failures != 0 {
        repair_trust.block(RepairBlockReason::BackupGeometryConflict);
    }
    if corruptions + duplicates + oob != 0 {
        repair_trust.block(RepairBlockReason::ExtentSemanticFailure);
    }
    if extent_metadata_overlaps != 0 {
        repair_trust.block(RepairBlockReason::ExtentMetadataOverlap);
    }
    if xattr_block_corruptions != 0 {
        repair_trust.block(RepairBlockReason::XattrIntegrityFailure);
    }
    if xattr_hash_failures != 0 {
        repair_trust.block(RepairBlockReason::XattrHashFailure);
    }
    if xattr_refcount_failures != 0 {
        repair_trust.block(RepairBlockReason::XattrRefcountFailure);
    }
    if xattr_semantic_failures != 0 {
        repair_trust.block(RepairBlockReason::XattrSemanticFailure);
    }
    if corrupt_symlinks + corrupt_dirs + orphan_dirs + link_mismatches != 0 {
        repair_trust.block(RepairBlockReason::DirectoryReferenceFailure);
    }
    if journal_is_dirty {
        repair_trust.block(RepairBlockReason::JournalReplayRequired);
    }
    if journal_integrity_failures != 0 {
        repair_trust.block(RepairBlockReason::JournalIntegrityUnknown);
    }
    if media_errors != 0 {
        repair_trust.block(RepairBlockReason::MediaError);
    }

    stats.errors_found += corruptions
        + duplicates
        + oob
        + corrupt_symlinks
        + corrupt_dirs
        + orphan_dirs
        + link_mismatches
        + inode_checksum_failures
        + block_bitmap_checksum_failures
        + inode_bitmap_checksum_failures
        + directory_checksum_failures
        + extent_block_checksum_failures
        + xattr_block_corruptions
        + xattr_checksum_failures
        + xattr_hash_failures
        + xattr_refcount_failures
        + extent_metadata_overlaps
        + backup_superblock_failures
        + journal_integrity_failures
        + u64::from(journal_is_dirty)
        + total_block_discrepancy.false_free_blocks
        + total_inode_discrepancy.false_free_inodes
        + media_errors;

    // 6. Active Repair Mode Execution
    let mut errors_were_corrected = false;
    let has_repairable_errors = total_block_discrepancy.false_free_blocks > 0
        || total_inode_discrepancy.false_free_inodes > 0;
    if args.repair && has_repairable_errors && !repair_trust.is_eligible() {
        error!(
            "Repair blocked by trust policy: {}",
            repair_trust
                .blocked_reasons
                .iter()
                .map(|reason| reason.description())
                .collect::<Vec<_>>()
                .join("; ")
        );
    } else if args.repair && has_repairable_errors {
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
    profile.mark("repair_or_read_only_finalization");

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
        println!(
            "  \"directory_blocks_checked\": {},",
            directory_blocks_checked
        );
        println!(
            "  \"directory_structural_failures\": {},",
            directory_structural_failures
        );
        println!(
            "  \"htree_structural_failures\": {},",
            htree_structural_failures
        );
        println!("  \"extent_intervals\": {},", compute_intervals.len());
        println!("  \"corrupt_directories\": {},", corrupt_dirs);
        println!("  \"orphan_directories\": {},", orphan_dirs);
        println!("  \"link_count_mismatches\": {},", link_mismatches);
        println!(
            "  \"inode_checksum_validations\": {},",
            inode_checksum_validations
        );
        println!(
            "  \"inode_checksum_failures\": {},",
            inode_checksum_failures
        );
        println!(
            "  \"directory_checksum_validations\": {},",
            directory_checksum_validations
        );
        println!("  \"superblock_checksum_failures\": 0,");
        println!(
            "  \"backup_superblocks_checked\": {},",
            backup_superblocks.checked
        );
        println!(
            "  \"backup_superblocks_valid_consistent\": {},",
            backup_superblocks.valid_consistent
        );
        println!(
            "  \"backup_superblocks_invalid_checksum\": {},",
            backup_superblocks.invalid_checksum
        );
        println!(
            "  \"backup_superblocks_invalid_or_unreadable\": {},",
            backup_superblocks.invalid_magic_or_unreadable
        );
        println!(
            "  \"backup_superblocks_inconsistent\": {},",
            backup_superblocks.inconsistent
        );
        println!("  \"group_descriptor_checksum_failures\": 0,");
        println!(
            "  \"block_bitmap_checksum_failures\": {},",
            block_bitmap_checksum_failures
        );
        println!(
            "  \"inode_bitmap_checksum_failures\": {},",
            inode_bitmap_checksum_failures
        );
        println!(
            "  \"directory_checksum_failures\": {},",
            directory_checksum_failures
        );
        println!(
            "  \"extent_block_checksum_failures\": {},",
            extent_block_checksum_failures
        );
        println!(
            "  \"xattr_block_corruptions\": {},",
            xattr_block_corruptions
        );
        println!(
            "  \"xattr_checksum_failures\": {},",
            xattr_checksum_failures
        );
        println!("  \"xattr_blocks_checked\": {},", xattr_blocks_checked);
        println!("  \"xattr_entries_checked\": {},", xattr_entries_checked);
        println!("  \"xattr_hash_failures\": {},", xattr_hash_failures);
        println!(
            "  \"xattr_refcount_failures\": {},",
            xattr_refcount_failures
        );
        println!(
            "  \"xattr_semantic_failures\": {},",
            xattr_semantic_failures
        );
        println!(
            "  \"extent_metadata_overlap_failures\": {},",
            extent_metadata_overlaps
        );
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
        println!(
            "  \"journal_transaction_blocks_checked\": {},",
            journal_transaction_blocks_checked
        );
        println!(
            "  \"journal_committed_transactions\": {},",
            journal_committed_transactions
        );
        println!(
            "  \"journal_integrity_state\": \"{}\",",
            journal_integrity_state
        );
        println!(
            "  \"journal_integrity_failures\": {},",
            journal_integrity_failures
        );
        println!("  \"errors_detected\": {},", stats.errors_found);
        println!("  \"errors_corrected\": {},", errors_were_corrected);
        println!("  \"repair_eligible\": {},", repair_trust.is_eligible());
        println!(
            "  \"repair_blocked_reasons\": [{}],",
            repair_trust
                .blocked_reasons
                .iter()
                .map(|reason| format!("\"{}\"", reason.code()))
                .collect::<Vec<_>>()
                .join(", ")
        );
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
            "Allocated Blocks   : {} (Tracked in {})",
            tracker.allocated_count(),
            tracker.representation_name()
        );
        println!("Directory Entries  : {}", total_dentry_count);
        println!("Corrupt Directories: {}", corrupt_dirs);
        println!("Orphan Directories : {}", orphan_dirs);
        println!("Link Discrepancies : {}", link_mismatches);
        println!("Inode Csum Failures: {}", inode_checksum_failures);
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
        if !repair_trust.is_eligible() {
            println!(
                "Repair Blocked     : {}",
                repair_trust
                    .blocked_reasons
                    .iter()
                    .map(|reason| reason.description())
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        println!("--------------------------------------------------");

        if errors_were_corrected {
            info!("Filesystem errors were successfully corrected. Rollback journal created.");
        } else if stats.errors_found == 0 {
            info!("Filesystem consistency check completed cleanly with ZERO errors.");
        } else {
            warn!(
                "Filesystem consistency check completed with {} inconsistency error(s).",
                stats.errors_found
            );
        }
    }

    profile.mark("summary_output_logging");
    let io_name = if dev.is_io_uring_active() {
        "io_uring"
    } else {
        "sync"
    };
    profile.report(compute_backend, io_name);

    if errors_were_corrected {
        ExitCode::from(FSCK_EXIT_ERRORS_CORRECTED)
    } else if stats.errors_found == 0 {
        ExitCode::from(FSCK_EXIT_OK)
    } else {
        ExitCode::from(FSCK_EXIT_ERRORS_UNCORRECTED)
    }
}
