//! `nexfsck-core`
//!
//! Core types, zerocopy Plain-Old-Data (POD) on-disk layouts, and parsing
//! logic for the Linux ext4 filesystem.

use thiserror::Error;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Standard ext2/3/4 magic number (0xEF53).
pub const EXT4_SUPER_MAGIC: u16 = 0xEF53;

/// Offset in bytes from partition start to primary superblock.
pub const EXT4_SUPERBLOCK_OFFSET: u64 = 1024;

/// Standard superblock size (1024 bytes).
pub const EXT4_SUPERBLOCK_SIZE: usize = 1024;

/// Extent magic number (0xF30A).
pub const EXT4_EXTENT_MAGIC: u16 = 0xF30A;

// Special ext4 Inode numbers
pub const EXT4_BAD_INO: u32 = 1;
pub const EXT4_ROOT_INO: u32 = 2;
pub const EXT4_USR_QUOTA_INO: u32 = 3;
pub const EXT4_GRP_QUOTA_INO: u32 = 4;
pub const EXT4_BOOT_LOADER_INO: u32 = 5;
pub const EXT4_UNDEL_DIR_INO: u32 = 6;
pub const EXT4_RESIZE_INO: u32 = 7;
pub const EXT4_JOURNAL_INO: u32 = 8;
pub const EXT4_EXCLUDE_INO: u32 = 9;
pub const EXT4_REPLICA_INO: u32 = 10;
pub const EXT4_LOST_FOUND_INO: u32 = 11;

// Inode flags
pub const EXT4_SECRM_FL: u32 = 0x00000001;
pub const EXT4_UNRM_FL: u32 = 0x00000002;
pub const EXT4_COMPR_FL: u32 = 0x00000004;
pub const EXT4_SYNC_FL: u32 = 0x00000008;
pub const EXT4_IMMUTABLE_FL: u32 = 0x00000010;
pub const EXT4_APPEND_FL: u32 = 0x00000020;
pub const EXT4_NODUMP_FL: u32 = 0x00000040;
pub const EXT4_NOATIME_FL: u32 = 0x00000080;
pub const EXT4_INDEX_FL: u32 = 0x00001000;
pub const EXT4_EXTENTS_FL: u32 = 0x00080000;
pub const EXT4_INLINE_DATA_FL: u32 = 0x10000000;
pub const EXT4_ENCRYPT_FL: u32 = 0x00000800;
pub const EXT4_CASEFOLD_FL: u32 = 0x40000000;

// Block group descriptor flags
pub const EXT4_BG_INODE_UNINIT: u16 = 0x0001;
pub const EXT4_BG_BLOCK_UNINIT: u16 = 0x0002;
pub const EXT4_BG_INODE_ZEROED: u16 = 0x0004;

// Filesystem feature flags
pub const EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub const EXT4_FEATURE_RO_COMPAT_LARGE_FILE: u32 = 0x0002;
pub const EXT4_FEATURE_RO_COMPAT_BTREE_DIR: u32 = 0x0004;
pub const EXT4_FEATURE_RO_COMPAT_HUGE_FILE: u32 = 0x0008;
pub const EXT4_FEATURE_RO_COMPAT_GDT_CSUM: u32 = 0x0010;
pub const EXT4_FEATURE_RO_COMPAT_DIR_NLINK: u32 = 0x0020;
pub const EXT4_FEATURE_RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
pub const EXT4_FEATURE_RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
pub const EXT4_FEATURE_RO_COMPAT_QUOTA: u32 = 0x0100;
pub const EXT4_FEATURE_RO_COMPAT_BIGALLOC: u32 = 0x0200;
pub const EXT4_FEATURE_RO_COMPAT_PROJECT: u32 = 0x2000;
pub const EXT4_FEATURE_RO_COMPAT_VERITY: u32 = 0x8000;

pub const EXT4_FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002;
pub const EXT4_FEATURE_INCOMPAT_RECOVER: u32 = 0x0004;
pub const EXT4_FEATURE_INCOMPAT_JOURNAL_DEV: u32 = 0x0008;
pub const EXT4_FEATURE_INCOMPAT_META_BG: u32 = 0x0010;
pub const EXT4_FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;
pub const EXT4_FEATURE_INCOMPAT_64BIT: u32 = 0x0080;
pub const EXT4_FEATURE_INCOMPAT_MMP: u32 = 0x0100;
pub const EXT4_FEATURE_INCOMPAT_FLEX_BG: u32 = 0x0200;
pub const EXT4_FEATURE_INCOMPAT_CSUM_SEED: u32 = 0x2000;
pub const EXT4_FEATURE_INCOMPAT_EA_INODE: u32 = 0x0400;
pub const EXT4_FEATURE_INCOMPAT_LARGEDIR: u32 = 0x4000;
pub const EXT4_FEATURE_INCOMPAT_INLINE_DATA: u32 = 0x8000;
pub const EXT4_FEATURE_INCOMPAT_ENCRYPT: u32 = 0x10000;
pub const EXT4_FEATURE_INCOMPAT_CASEFOLD: u32 = 0x20000;

pub const EXT4_FEATURE_COMPAT_ORPHAN_FILE: u32 = 0x1000;
pub const EXT4_FEATURE_COMPAT_SPARSE_SUPER2: u32 = 0x0200;
pub const EXT4_FEATURE_COMPAT_RESIZE_INODE: u32 = 0x0010;
pub const EXT4_BG_BLOCK_BITMAP_CSUM_HI_END: usize = 58;
pub const EXT4_BG_INODE_BITMAP_CSUM_HI_END: usize = 60;

// Directory file types
pub const EXT4_FT_UNKNOWN: u8 = 0;
pub const EXT4_FT_REG_FILE: u8 = 1;
pub const EXT4_FT_DIR: u8 = 2;
pub const EXT4_FT_CHRDEV: u8 = 3;
pub const EXT4_FT_BLKDEV: u8 = 4;
pub const EXT4_FT_FIFO: u8 = 5;
pub const EXT4_FT_SOCK: u8 = 6;
pub const EXT4_FT_SYMLINK: u8 = 7;
pub const EXT4_FT_DIR_CSUM: u8 = 0xDE;

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

    #[error("Out of bounds: {0}")]
    OutOfBounds(String),
}

/// ext4 superblock on-disk structure (1024 bytes).
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

    pub fn block_size(&self) -> u64 {
        1024u64 << u32::from_le(self.s_log_block_size)
    }

    pub fn inode_size(&self) -> u16 {
        let size = u16::from_le(self.s_inode_size);
        if size == 0 {
            128
        } else {
            size
        }
    }

    pub fn desc_size(&self) -> usize {
        let desc = u16::from_le(self.s_desc_size) as usize;
        if desc == 0 || !self.has_incompat_feature(EXT4_FEATURE_INCOMPAT_64BIT) {
            32
        } else {
            desc
        }
    }

    pub fn has_incompat_feature(&self, feature: u32) -> bool {
        (u32::from_le(self.s_feature_incompat) & feature) != 0
    }

    pub fn has_compat_feature(&self, feature: u32) -> bool {
        (u32::from_le(self.s_feature_compat) & feature) != 0
    }

    pub fn has_ro_compat_feature(&self, feature: u32) -> bool {
        (u32::from_le(self.s_feature_ro_compat) & feature) != 0
    }

    pub fn total_blocks(&self) -> u64 {
        let lo = u32::from_le(self.s_blocks_count_lo) as u64;
        let hi = u32::from_le(self.s_blocks_count_hi) as u64;
        (hi << 32) | lo
    }

    pub fn free_blocks(&self) -> u64 {
        let lo = u32::from_le(self.s_free_blocks_count_lo) as u64;
        let hi = u32::from_le(self.s_free_blocks_count_hi) as u64;
        (hi << 32) | lo
    }

    pub fn total_inodes(&self) -> u32 {
        u32::from_le(self.s_inodes_count)
    }

    pub fn first_inode(&self) -> u32 {
        let first = u32::from_le(self.s_first_ino);
        if first == 0 {
            11
        } else {
            first
        }
    }

    pub fn inodes_per_group(&self) -> u32 {
        u32::from_le(self.s_inodes_per_group)
    }

    pub fn blocks_per_group(&self) -> u32 {
        u32::from_le(self.s_blocks_per_group)
    }

    pub fn block_groups_count(&self) -> u64 {
        let bpg = self.blocks_per_group() as u64;
        if bpg == 0 {
            return 0;
        }
        let total = self.total_blocks();
        total.div_ceil(bpg)
    }

    /// Determines if a specific block group contains a superblock and group descriptor backup.
    pub fn group_has_superblock(&self, bg: u64) -> bool {
        if bg == 0 {
            return true;
        }
        if self.has_compat_feature(EXT4_FEATURE_COMPAT_SPARSE_SUPER2) {
            let b1 = u32::from_le(self.s_backup_bgs[0]) as u64;
            let b2 = u32::from_le(self.s_backup_bgs[1]) as u64;
            return (b1 != 0 && bg == b1) || (b2 != 0 && bg == b2);
        }
        if self.has_ro_compat_feature(EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER) {
            if bg == 1 {
                return true;
            }
            for &base in &[3, 5, 7] {
                let mut val = base;
                while val <= bg {
                    if val == bg {
                        return true;
                    }
                    val *= base;
                }
            }
            return false;
        }
        true
    }
}

/// ext4 Block Group Descriptor on-disk layout (64-byte unified structure).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Ext4GroupDesc {
    pub bg_block_bitmap_lo: u32,
    pub bg_inode_bitmap_lo: u32,
    pub bg_inode_table_lo: u32,
    pub bg_free_blocks_count_lo: u16,
    pub bg_free_inodes_count_lo: u16,
    pub bg_used_dirs_count_lo: u16,
    pub bg_flags: u16,
    pub bg_exclude_bitmap_lo: u32,
    pub bg_block_bitmap_csum_lo: u16,
    pub bg_inode_bitmap_csum_lo: u16,
    pub bg_itable_unused_lo: u16,
    pub bg_checksum: u16,
    // 64-bit fields
    pub bg_block_bitmap_hi: u32,
    pub bg_inode_bitmap_hi: u32,
    pub bg_inode_table_hi: u32,
    pub bg_free_blocks_count_hi: u16,
    pub bg_free_inodes_count_hi: u16,
    pub bg_used_dirs_count_hi: u16,
    pub bg_itable_unused_hi: u16,
    pub bg_exclude_bitmap_hi: u32,
    pub bg_block_bitmap_csum_hi: u16,
    pub bg_inode_bitmap_csum_hi: u16,
    pub bg_reserved: u32,
}

impl Ext4GroupDesc {
    pub fn inode_table_block(&self, is_64bit: bool) -> u64 {
        let lo = u32::from_le(self.bg_inode_table_lo) as u64;
        if is_64bit {
            let hi = u32::from_le(self.bg_inode_table_hi) as u64;
            (hi << 32) | lo
        } else {
            lo
        }
    }

    pub fn block_bitmap(&self, is_64bit: bool) -> u64 {
        let lo = u32::from_le(self.bg_block_bitmap_lo) as u64;
        if is_64bit {
            let hi = u32::from_le(self.bg_block_bitmap_hi) as u64;
            (hi << 32) | lo
        } else {
            lo
        }
    }

    pub fn inode_bitmap(&self, is_64bit: bool) -> u64 {
        let lo = u32::from_le(self.bg_inode_bitmap_lo) as u64;
        if is_64bit {
            let hi = u32::from_le(self.bg_inode_bitmap_hi) as u64;
            (hi << 32) | lo
        } else {
            lo
        }
    }

    pub fn flags(&self) -> u16 {
        u16::from_le(self.bg_flags)
    }

    pub fn is_inode_uninit(&self) -> bool {
        (self.flags() & EXT4_BG_INODE_UNINIT) != 0
    }

    pub fn is_block_uninit(&self) -> bool {
        (self.flags() & EXT4_BG_BLOCK_UNINIT) != 0
    }

    pub fn free_blocks_count(&self, is_64bit: bool) -> u32 {
        let lo = u16::from_le(self.bg_free_blocks_count_lo) as u32;
        if is_64bit {
            let hi = u16::from_le(self.bg_free_blocks_count_hi) as u32;
            (hi << 16) | lo
        } else {
            lo
        }
    }

    pub fn free_inodes_count(&self, is_64bit: bool) -> u32 {
        let lo = u16::from_le(self.bg_free_inodes_count_lo) as u32;
        if is_64bit {
            let hi = u16::from_le(self.bg_free_inodes_count_hi) as u32;
            (hi << 16) | lo
        } else {
            lo
        }
    }
}

/// ext4 Inode on-disk structure (first 156 bytes).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Ext4Inode {
    pub i_mode: u16,
    pub i_uid: u16,
    pub i_size_lo: u32,
    pub i_atime: u32,
    pub i_ctime: u32,
    pub i_mtime: u32,
    pub i_dtime: u32,
    pub i_gid: u16,
    pub i_links_count: u16,
    pub i_blocks_lo: u32,
    pub i_flags: u32,
    pub osd1: u32,
    pub i_block: [u8; 60],
    pub i_generation: u32,
    pub i_file_acl_lo: u32,
    pub i_size_high: u32,
    pub i_obso_faddr: u32,
    pub osd2: [u8; 12],
    pub i_extra_isize: u16,
    pub i_checksum_hi: u16,
    pub i_ctime_extra: u32,
    pub i_mtime_extra: u32,
    pub i_atime_extra: u32,
    pub i_crtime: u32,
    pub i_crtime_extra: u32,
    pub i_version_hi: u32,
    pub i_projid: u32,
}

impl Ext4Inode {
    /// Physical block holding this inode's external extended attributes, if any.
    pub fn external_xattr_block(&self) -> u64 {
        let low = u32::from_le(self.i_file_acl_lo) as u64;
        let high = u16::from_le_bytes([self.osd2[2], self.osd2[3]]) as u64;
        (high << 32) | low
    }

    pub fn mode(&self) -> u16 {
        u16::from_le(self.i_mode)
    }

    pub fn is_used(&self) -> bool {
        self.mode() != 0 && self.links_count() > 0
    }

    pub fn is_dir(&self) -> bool {
        (self.mode() & 0xF000) == 0x4000
    }

    pub fn is_reg_file(&self) -> bool {
        (self.mode() & 0xF000) == 0x8000
    }

    pub fn is_symlink(&self) -> bool {
        (self.mode() & 0xF000) == 0xA000
    }

    pub fn links_count(&self) -> u16 {
        u16::from_le(self.i_links_count)
    }

    pub fn flags(&self) -> u32 {
        u32::from_le(self.i_flags)
    }

    pub fn uses_extents(&self) -> bool {
        (self.flags() & EXT4_EXTENTS_FL) != 0
    }

    pub fn is_inline_data(&self) -> bool {
        (self.flags() & EXT4_INLINE_DATA_FL) != 0
    }

    pub fn is_indexed_directory(&self) -> bool {
        self.is_dir() && (self.flags() & EXT4_INDEX_FL) != 0
    }

    pub fn file_size(&self) -> u64 {
        let lo = u32::from_le(self.i_size_lo) as u64;
        let hi = u32::from_le(self.i_size_high) as u64;
        (hi << 32) | lo
    }

    pub fn is_fast_symlink(&self) -> bool {
        self.is_symlink() && self.file_size() <= 60 && !self.uses_extents()
    }

    pub fn fast_symlink_target(&self) -> Option<String> {
        if !self.is_fast_symlink() {
            return None;
        }
        let len = self.file_size() as usize;
        if len > 60 {
            return None;
        }
        Some(String::from_utf8_lossy(&self.i_block[..len]).to_string())
    }

    pub fn extent_header(&self) -> Option<Ext4ExtentHeader> {
        if !self.uses_extents() || self.is_inline_data() {
            return None;
        }
        let (hdr, _) = Ext4ExtentHeader::ref_from_prefix(&self.i_block).ok()?;
        if hdr.is_valid_magic() {
            Some(*hdr)
        } else {
            None
        }
    }
}

pub const EXT4_MMP_MAGIC: u32 = 0x004D4D50;

/// Multi-Mount Protection on-disk structure.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Ext4Mmp {
    pub mmp_magic: u32,
    pub mmp_seq: u32,
    pub mmp_time: u64,
    pub mmp_nodename: [u8; 64],
    pub mmp_bdevname: [u8; 32],
    pub mmp_check_interval: u16,
    pub mmp_pad: u16,
    pub mmp_checksum: u32,
}

impl Ext4Mmp {
    pub fn is_valid_magic(&self) -> bool {
        u32::from_le(self.mmp_magic) == EXT4_MMP_MAGIC
    }

    pub fn sequence(&self) -> u32 {
        u32::from_le(self.mmp_seq)
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

    pub fn entries(&self) -> u16 {
        u16::from_le(self.eh_entries)
    }

    pub fn max(&self) -> u16 {
        u16::from_le(self.eh_max)
    }

    pub fn depth(&self) -> u16 {
        u16::from_le(self.eh_depth)
    }
}

/// Extent index node (internal branch).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Ext4ExtentIdx {
    pub ei_block: u32,
    pub ei_leaf_lo: u32,
    pub ei_leaf_hi: u16,
    pub ei_unused: u16,
}

impl Ext4ExtentIdx {
    pub fn logical_block(&self) -> u32 {
        u32::from_le(self.ei_block)
    }

    pub fn child_block(&self) -> u64 {
        let hi = u16::from_le(self.ei_leaf_hi) as u64;
        let lo = u32::from_le(self.ei_leaf_lo) as u64;
        (hi << 32) | lo
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
    pub fn logical_block(&self) -> u32 {
        u32::from_le(self.ee_block)
    }

    pub fn physical_start(&self) -> u64 {
        let hi = u16::from_le(self.ee_start_hi) as u64;
        let lo = u32::from_le(self.ee_start_lo) as u64;
        (hi << 32) | lo
    }

    pub fn is_unwritten(&self) -> bool {
        // ext4 encodes initialized lengths through 0x8000 inclusive. Values
        // strictly greater than EXT_INIT_MAX_LEN carry the unwritten flag.
        u16::from_le(self.ee_len) > 0x8000
    }

    pub fn block_count(&self) -> u32 {
        let len = u16::from_le(self.ee_len);
        if len > 0x8000 {
            (len - 0x8000) as u32
        } else {
            len as u32
        }
    }
}

/// Directory entry (ext4_dir_entry_2) header (8 bytes).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Ext4DirEntry2Header {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
}

impl Ext4DirEntry2Header {
    pub fn inode_number(&self) -> u32 {
        u32::from_le(self.inode)
    }

    pub fn record_len(&self) -> u16 {
        u16::from_le(self.rec_len)
    }

    pub fn name_length(&self) -> u8 {
        self.name_len
    }

    pub fn entry_file_type(&self) -> u8 {
        self.file_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_superblock_size() {
        assert_eq!(std::mem::size_of::<Ext4Superblock>(), 1024);
    }

    #[test]
    fn test_group_desc_size() {
        assert_eq!(std::mem::size_of::<Ext4GroupDesc>(), 64);
    }

    #[test]
    fn test_extent_header_size() {
        assert_eq!(std::mem::size_of::<Ext4ExtentHeader>(), 12);
        assert_eq!(std::mem::size_of::<Ext4ExtentIdx>(), 12);
        assert_eq!(std::mem::size_of::<Ext4Extent>(), 12);
    }

    #[test]
    fn test_extent_address_calculation() {
        let ext = Ext4Extent {
            ee_block: 0,
            ee_len: 10u16.to_le(),
            ee_start_hi: 1u16.to_le(),
            ee_start_lo: 50u32.to_le(),
        };
        assert_eq!(ext.physical_start(), (1u64 << 32) | 50);
        assert_eq!(ext.block_count(), 10);
        assert!(!ext.is_unwritten());
    }

    #[test]
    fn test_extent_unwritten() {
        let ext = Ext4Extent {
            ee_block: 0,
            ee_len: (0x8000 | 25u16).to_le(),
            ee_start_hi: 0,
            ee_start_lo: 100u32.to_le(),
        };
        assert!(ext.is_unwritten());
        assert_eq!(ext.block_count(), 25);
    }

    #[test]
    fn extent_length_boundary_0x8000_is_initialized() {
        let ext = Ext4Extent {
            ee_block: 0,
            ee_len: 0x8000u16.to_le(),
            ee_start_hi: 0,
            ee_start_lo: 1u32.to_le(),
        };
        assert!(!ext.is_unwritten());
        assert_eq!(ext.block_count(), 0x8000);
    }
}
