//! `nexfsck-core`
//!
//! Core types, zerocopy Plain-Old-Data (POD) on-disk layouts, and parsing
//! logic for the Linux ext4 filesystem.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use thiserror::Error;

/// Standard ext2/3/4 magic number (0xEF53).
pub const EXT4_SUPER_MAGIC: u16 = 0xEF53;

/// Offset in bytes from partition start to primary superblock.
pub const EXT4_SUPERBLOCK_OFFSET: u64 = 1024;

/// Standard superblock size (1024 bytes).
pub const EXT4_SUPERBLOCK_SIZE: usize = 1024;

/// Extent magic number (0xF30A).
pub const EXT4_EXTENT_MAGIC: u16 = 0xF30A;

#[derive(Error, Debug)]
pub enum CoreError {
    #[error("Invalid magic number: expected 0x{expected:04X}, found 0x{found:04X}")]
    InvalidMagic { expected: u16, found: u16 },

    #[error("Buffer too small: expected {expected} bytes, found {found} bytes")]
    BufferTooSmall { expected: usize, found: usize },

    #[error("Corrupted structure: {0}")]
    Corruption(String),

    #[error("Unsupported filesystem feature: {0}")]
    UnsupportedFeature(String),
}

/// ext4 superblock on-disk structure (first 1024 bytes).
/// Modeled strictly with plain integer primitives to guarantee zerocopy safety.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Ext4Superblock {
    pub s_inodes_count: u32,
    pub s_blocks_count_lo: u32,
    pub s_r_blocks_count_lo: u32,
    pub s_free_blocks_count_lo: u32,
    pub s_free_inodes_count: u32,
    pub s_first_data_block: u32,
    pub s_log_block_size: u32,
    pub s_log_cluster_size: u32,
    pub s_blocks_per_group: u32,
    pub s_clusters_per_group: u32,
    pub s_inodes_per_group: u32,
    pub s_mtime: u32,
    pub s_wtime: u32,
    pub s_mnt_count: u16,
    pub s_max_mnt_count: u16,
    pub s_magic: u16,
    pub s_state: u16,
    pub s_errors: u16,
    pub s_minor_rev_level: u16,
    pub s_lastcheck: u32,
    pub s_checkinterval: u32,
    pub s_creator_os: u32,
    pub s_rev_level: u32,
    pub s_def_resuid: u16,
    pub s_def_resgid: u16,
    pub s_first_ino: u32,
    pub s_inode_size: u16,
    pub s_block_group_nr: u16,
    pub s_feature_compat: u32,
    pub s_feature_incompat: u32,
    pub s_feature_ro_compat: u32,
    pub s_uuid: [u8; 16],
    pub s_volume_name: [u8; 16],
    pub s_last_mounted: [u8; 64],
    pub s_algorithm_usage_bitmap: u32,
    pub s_prealloc_blocks: u8,
    pub s_prealloc_dir_blocks: u8,
    pub s_reserved_gdt_blocks: u16,
    pub s_journal_uuid: [u8; 16],
    pub s_journal_inum: u32,
    pub s_journal_dev: u32,
    pub s_last_orphan: u32,
    pub s_hash_seed: [u32; 4],
    pub s_def_hash_version: u8,
    pub s_jnl_backup_type: u8,
    pub s_desc_size: u16,
    pub s_default_mount_opts: u32,
    pub s_first_meta_bg: u32,
    pub s_mkfs_time: u32,
    pub s_jnl_blocks: [u32; 17],
    pub s_blocks_count_hi: u32,
    pub s_r_blocks_count_hi: u32,
    pub s_free_blocks_count_hi: u32,
    pub s_min_extra_isize: u16,
    pub s_want_extra_isize: u16,
    pub s_flags: u32,
    pub s_raid_stride: u16,
    pub s_mmp_interval: u16,
    pub s_mmp_block: u64,
    pub s_raid_stripe_width: u32,
    pub s_log_groups_per_flex: u8,
    pub s_checksum_type: u8,
    pub s_encryption_level: u8,
    pub s_reserved_pad: u8,
    pub s_kbytes_written: u64,
    pub s_snapshot_inum: u32,
    pub s_snapshot_id: u32,
    pub s_snapshot_r_blocks_count: u64,
    pub s_snapshot_list: u32,
    pub s_error_count: u32,
    pub s_first_error_time: u32,
    pub s_first_error_ino: u32,
    pub s_first_error_block: u64,
    pub s_first_error_func: [u8; 32],
    pub s_first_error_line: u32,
    pub s_last_error_time: u32,
    pub s_last_error_ino: u32,
    pub s_last_error_line: u32,
    pub s_last_error_block: u64,
    pub s_last_error_func: [u8; 32],
    pub s_mount_opts: [u8; 64],
    pub s_usr_quota_inum: u32,
    pub s_grp_quota_inum: u32,
    pub s_overhead_clusters: u32,
    pub s_backup_bgs: [u32; 2],
    pub s_encrypt_algos: [u8; 4],
    pub s_encrypt_pw_salt: [u8; 16],
    pub s_lpf_ino: u32,
    pub s_prj_quota_inum: u32,
    pub s_checksum_seed: u32,
    pub s_wtime_hi: u8,
    pub s_mtime_hi: u8,
    pub s_mkfs_time_hi: u8,
    pub s_lastcheck_hi: u8,
    pub s_first_error_time_hi: u8,
    pub s_last_error_time_hi: u8,
    pub s_pad: [u8; 2],
    pub s_encoding: u16,
    pub s_encoding_flags: u16,
    pub s_orphan_file_inum: u32,
    pub s_reserved: [u32; 94],
    pub s_checksum: u32,
}

impl Ext4Superblock {
    /// Validates the superblock magic number.
    pub fn verify_magic(&self) -> Result<(), CoreError> {
        let magic = u16::from_le(self.s_magic);
        if magic != EXT4_SUPER_MAGIC {
            return Err(CoreError::InvalidMagic {
                expected: EXT4_SUPER_MAGIC,
                found: magic,
            });
        }
        Ok(())
    }

    /// Computes the block size in bytes (1024 << s_log_block_size).
    pub fn block_size(&self) -> u64 {
        1024u64 << u32::from_le(self.s_log_block_size)
    }

    /// Computes total block count by combining 32-bit low and high fields.
    pub fn total_blocks(&self) -> u64 {
        let lo = u32::from_le(self.s_blocks_count_lo) as u64;
        let hi = u32::from_le(self.s_blocks_count_hi) as u64;
        (hi << 32) | lo
    }

    /// Computes total free blocks by combining 32-bit low and high fields.
    pub fn free_blocks(&self) -> u64 {
        let lo = u32::from_le(self.s_free_blocks_count_lo) as u64;
        let hi = u32::from_le(self.s_free_blocks_count_hi) as u64;
        (hi << 32) | lo
    }

    /// Computes the total number of block groups.
    pub fn block_groups_count(&self) -> u64 {
        let blocks_per_group = u32::from_le(self.s_blocks_per_group) as u64;
        if blocks_per_group == 0 {
            return 0;
        }
        let total = self.total_blocks();
        (total + blocks_per_group - 1) / blocks_per_group
    }
}

/// Extent header on-disk structure.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Ext4ExtentHeader {
    pub eh_magic: u16,
    pub eh_entries: u16,
    pub eh_max: u16,
    pub eh_depth: u16,
    pub eh_generation: u32,
}

impl Ext4ExtentHeader {
    pub fn is_valid_magic(&self) -> bool {
        u16::from_le(self.eh_magic) == EXT4_EXTENT_MAGIC
    }
}

/// Extent entry on-disk structure (leaf node).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Ext4Extent {
    pub ee_block: u32,
    pub ee_len: u16,
    pub ee_start_hi: u16,
    pub ee_start_lo: u32,
}

impl Ext4Extent {
    /// Computes physical block starting address.
    pub fn physical_start(&self) -> u64 {
        let hi = u16::from_le(self.ee_start_hi) as u64;
        let lo = u32::from_le(self.ee_start_lo) as u64;
        (hi << 32) | lo
    }

    /// Checks if this extent is preallocated and unwritten (bit 15 set).
    pub fn is_unwritten(&self) -> bool {
        (u16::from_le(self.ee_len) & 0x8000) != 0
    }

    /// Returns actual block count, masking out the unwritten bit.
    pub fn block_count(&self) -> u32 {
        let len = u16::from_le(self.ee_len);
        (len & 0x7FFF) as u32
    }
}
