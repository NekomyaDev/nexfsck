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
use zerocopy::IntoBytes;

use nexfsck_core::{
    Ext4DirEntry2Header, Ext4Extent, Ext4ExtentHeader, Ext4ExtentIdx, Ext4GroupDesc, Ext4Inode,
    Ext4Superblock, EXT4_FEATURE_INCOMPAT_CSUM_SEED, EXT4_FEATURE_RO_COMPAT_METADATA_CSUM,
    EXT4_FT_DIR_CSUM,
};

const EXT4_SUPERBLOCK_CSUM_OFFSET: usize = 1020;
const EXT4_GROUP_DESC_CSUM_OFFSET: usize = 30;
const EXT4_XATTR_MAGIC: u32 = 0xea02_0000;
const EXT4_XATTR_HEADER_SIZE: usize = 32;
const EXT4_XATTR_ENTRY_HEADER_SIZE: usize = 16;
const EXT4_XATTR_REFCOUNT_MAX: u32 = 1024;
const EXT4_XATTR_VALUE_MAX: usize = 1 << 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XattrBlockValidation {
    Valid { refcount: u32 },
    InvalidStructure,
    InvalidChecksum,
}

/// Common filesystem checksum state. Each verifier below follows the ext4
/// structure-specific byte coverage rules; this type only shares the seed.
#[derive(Debug, Clone, Copy)]
pub struct Ext4MetadataChecksum {
    enabled: bool,
    crc32c_supported: bool,
    gdt_crc16: bool,
    seed: u32,
    uuid: [u8; 16],
}

impl Ext4MetadataChecksum {
    pub fn new(sb: &Ext4Superblock) -> Self {
        let enabled = sb.has_ro_compat_feature(EXT4_FEATURE_RO_COMPAT_METADATA_CSUM);
        let seed = if sb.has_incompat_feature(EXT4_FEATURE_INCOMPAT_CSUM_SEED) {
            u32::from_le(sb.s_checksum_seed)
        } else {
            ext4_crc32c(u32::MAX, &sb.s_uuid)
        };
        Self {
            enabled,
            crc32c_supported: u8::from_le(sb.s_checksum_type) == 1,
            gdt_crc16: sb.has_ro_compat_feature(nexfsck_core::EXT4_FEATURE_RO_COMPAT_GDT_CSUM),
            seed,
            uuid: sb.s_uuid,
        }
    }

    pub fn verify_superblock(&self, raw_superblock: &[u8]) -> bool {
        if !self.enabled {
            return true;
        }
        if !self.crc32c_supported {
            return false;
        }
        if raw_superblock.len() < nexfsck_core::EXT4_SUPERBLOCK_SIZE {
            return false;
        }
        let raw = &raw_superblock[..nexfsck_core::EXT4_SUPERBLOCK_SIZE];
        let expected =
            u32::from_le_bytes(raw[EXT4_SUPERBLOCK_CSUM_OFFSET..1024].try_into().unwrap());
        ext4_crc32c(u32::MAX, &raw[..EXT4_SUPERBLOCK_CSUM_OFFSET]) == expected
    }

    pub fn verify_group_descriptor(&self, group: u32, descriptor: &[u8]) -> bool {
        if !self.enabled && !self.gdt_crc16 {
            return true;
        }
        if self.enabled && !self.crc32c_supported {
            return false;
        }
        if descriptor.len() < 32 || EXT4_GROUP_DESC_CSUM_OFFSET + 2 > descriptor.len() {
            return false;
        }
        let provided = u16::from_le_bytes(descriptor[30..32].try_into().unwrap());
        let mut bytes = descriptor.to_vec();
        bytes[30..32].fill(0);
        if self.enabled {
            let mut crc = ext4_crc32c(self.seed, &group.to_le_bytes());
            crc = ext4_crc32c(crc, &bytes);
            provided == crc as u16
        } else {
            let mut crc = crc16_ext4(0xffff, &self.uuid);
            crc = crc16_ext4(crc, &group.to_le_bytes());
            crc = crc16_ext4(crc, &bytes[..EXT4_GROUP_DESC_CSUM_OFFSET]);
            crc = crc16_ext4(crc, &bytes[EXT4_GROUP_DESC_CSUM_OFFSET + 2..]);
            provided == crc
        }
    }

    pub fn verify_group_desc(
        &self,
        group: u32,
        descriptor: &Ext4GroupDesc,
        descriptor_size: usize,
    ) -> bool {
        self.verify_group_descriptor(group, &descriptor.as_bytes()[..descriptor_size.min(64)])
    }

    pub fn verify_block_bitmap(
        &self,
        group: u32,
        bitmap: &[u8],
        desc: &Ext4GroupDesc,
        descriptor_size: usize,
    ) -> bool {
        let has_high = descriptor_size >= nexfsck_core::EXT4_BG_BLOCK_BITMAP_CSUM_HI_END;
        let mut provided = u16::from_le(desc.bg_block_bitmap_csum_lo) as u32;
        if has_high {
            provided |= (u16::from_le(desc.bg_block_bitmap_csum_hi) as u32) << 16;
        }
        self.verify_bitmap(group, bitmap, provided, has_high)
    }

    pub fn verify_inode_bitmap(
        &self,
        group: u32,
        bitmap: &[u8],
        desc: &Ext4GroupDesc,
        descriptor_size: usize,
    ) -> bool {
        let has_high = descriptor_size >= nexfsck_core::EXT4_BG_INODE_BITMAP_CSUM_HI_END;
        let mut provided = u16::from_le(desc.bg_inode_bitmap_csum_lo) as u32;
        if has_high {
            provided |= (u16::from_le(desc.bg_inode_bitmap_csum_hi) as u32) << 16;
        }
        self.verify_bitmap(group, bitmap, provided, has_high)
    }

    pub fn verify_directory_block(
        &self,
        inode: u32,
        generation: u32,
        block: &[u8],
        htree_root: bool,
    ) -> bool {
        if !self.enabled {
            return true;
        }
        if !self.crc32c_supported {
            return false;
        }
        if block.len() < 12 {
            return false;
        }
        if htree_root
            || (u32::from_le_bytes(block[0..4].try_into().unwrap()) == 0
                && u16::from_le_bytes(block[4..6].try_into().unwrap()) as usize == block.len())
        {
            let count_offset = if htree_root { 32 } else { 8 };
            if count_offset + 4 > block.len() {
                return false;
            }
            let limit =
                u16::from_le_bytes(block[count_offset..count_offset + 2].try_into().unwrap())
                    as usize;
            let count = u16::from_le_bytes(
                block[count_offset + 2..count_offset + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let tail_offset = count_offset.saturating_add(limit.saturating_mul(8));
            let used_end = count_offset.saturating_add(count.saturating_mul(8));
            if count == 0
                || count > limit
                || tail_offset + 8 > block.len()
                || used_end > tail_offset
            {
                return false;
            }
            let provided =
                u32::from_le_bytes(block[tail_offset + 4..tail_offset + 8].try_into().unwrap());
            let mut crc = ext4_crc32c(self.seed, &inode.to_le_bytes());
            crc = ext4_crc32c(crc, &generation.to_le_bytes());
            crc = ext4_crc32c(crc, &block[..used_end]);
            crc = ext4_crc32c(crc, &block[tail_offset..tail_offset + 4]);
            crc = ext4_crc32c(crc, &[0; 4]);
            return provided == crc;
        }
        let tail = block.len() - 12;
        if u32::from_le_bytes(block[tail..tail + 4].try_into().unwrap()) != 0
            || u16::from_le_bytes(block[tail + 4..tail + 6].try_into().unwrap()) as usize != 12
            || block[tail + 6] != 0
            || block[tail + 7] != EXT4_FT_DIR_CSUM
        {
            return false;
        }
        let provided = u32::from_le_bytes(block[tail + 8..tail + 12].try_into().unwrap());
        let mut crc = ext4_crc32c(self.seed, &inode.to_le_bytes());
        crc = ext4_crc32c(crc, &generation.to_le_bytes());
        crc = ext4_crc32c(crc, &block[..tail]);
        provided == crc
    }

    /// Validates a non-inline extent-tree node using the inode checksum seed
    /// and the on-disk eh_max-defined tail position.
    pub fn verify_extent_tree_block(&self, inode: u32, generation: u32, block: &[u8]) -> bool {
        if !self.enabled {
            return true;
        }
        if !self.crc32c_supported || block.len() < 16 {
            return false;
        }
        let magic = u16::from_le_bytes(block[0..2].try_into().unwrap());
        if magic != nexfsck_core::EXT4_EXTENT_MAGIC {
            return false;
        }
        let max_entries = u16::from_le_bytes(block[4..6].try_into().unwrap()) as usize;
        let tail = 12usize.saturating_add(max_entries.saturating_mul(12));
        if tail.checked_add(4).is_none_or(|end| end > block.len()) {
            return false;
        }
        let provided = u32::from_le_bytes(block[tail..tail + 4].try_into().unwrap());
        let mut crc = ext4_crc32c(self.seed, &inode.to_le_bytes());
        crc = ext4_crc32c(crc, &generation.to_le_bytes());
        crc = ext4_crc32c(crc, &block[..tail]);
        provided == crc
    }

    /// Validate an external ext4 xattr block and its metadata checksum.
    /// Xattr inode values are rejected by feature policy before this parser is used.
    pub fn validate_xattr_block(&self, block_number: u64, block: &[u8]) -> XattrBlockValidation {
        if block.len() < EXT4_XATTR_HEADER_SIZE + 4
            || u32::from_le_bytes(block[0..4].try_into().unwrap()) != EXT4_XATTR_MAGIC
        {
            return XattrBlockValidation::InvalidStructure;
        }
        let refcount = u32::from_le_bytes(block[4..8].try_into().unwrap());
        let blocks = u32::from_le_bytes(block[8..12].try_into().unwrap());
        if !(1..=EXT4_XATTR_REFCOUNT_MAX).contains(&refcount)
            || blocks != 1
            || block[20..EXT4_XATTR_HEADER_SIZE]
                .iter()
                .any(|byte| *byte != 0)
        {
            return XattrBlockValidation::InvalidStructure;
        }

        let mut cursor = EXT4_XATTR_HEADER_SIZE;
        let entries_end = loop {
            let Some(prefix) = block.get(cursor..cursor.saturating_add(4)) else {
                return XattrBlockValidation::InvalidStructure;
            };
            if prefix.iter().all(|byte| *byte == 0) {
                break cursor + 4;
            }
            let Some(entry_header) =
                block.get(cursor..cursor.saturating_add(EXT4_XATTR_ENTRY_HEADER_SIZE))
            else {
                return XattrBlockValidation::InvalidStructure;
            };
            let name_len = entry_header[0] as usize;
            let name_index = entry_header[1];
            let value_offset = u16::from_le_bytes(entry_header[2..4].try_into().unwrap()) as usize;
            let value_inode = u32::from_le_bytes(entry_header[4..8].try_into().unwrap());
            let value_size = u32::from_le_bytes(entry_header[8..12].try_into().unwrap()) as usize;
            let entry_len = (EXT4_XATTR_ENTRY_HEADER_SIZE + name_len + 3) & !3;
            let Some(entry) = block.get(cursor..cursor.saturating_add(entry_len)) else {
                return XattrBlockValidation::InvalidStructure;
            };
            if !(1..=10).contains(&name_index)
                || entry[EXT4_XATTR_ENTRY_HEADER_SIZE..]
                    .iter()
                    .take(name_len)
                    .any(|byte| *byte == 0)
                || value_size > EXT4_XATTR_VALUE_MAX
                || value_inode != 0
            {
                return XattrBlockValidation::InvalidStructure;
            }
            if value_size != 0 {
                let padded_size = value_size.checked_add(3).map(|size| size & !3);
                let Some(padded_size) = padded_size else {
                    return XattrBlockValidation::InvalidStructure;
                };
                if value_offset & 3 != 0
                    || value_offset < cursor.saturating_add(entry_len).saturating_add(4)
                    || value_offset
                        .checked_add(padded_size)
                        .is_none_or(|end| end > block.len())
                {
                    return XattrBlockValidation::InvalidStructure;
                }
            }
            cursor += entry_len;
        };

        // Reject values that point into the entry-list terminator or any
        // name/header byte. Individual value extents are validated above.
        let mut cursor = EXT4_XATTR_HEADER_SIZE;
        while cursor + 4 <= entries_end - 4 {
            let name_len = block[cursor] as usize;
            let value_offset =
                u16::from_le_bytes(block[cursor + 2..cursor + 4].try_into().unwrap()) as usize;
            let value_inode = u32::from_le_bytes(block[cursor + 4..cursor + 8].try_into().unwrap());
            let value_size =
                u32::from_le_bytes(block[cursor + 8..cursor + 12].try_into().unwrap()) as usize;
            let entry_len = (EXT4_XATTR_ENTRY_HEADER_SIZE + name_len + 3) & !3;
            if value_size != 0 && value_inode == 0 && value_offset < entries_end {
                return XattrBlockValidation::InvalidStructure;
            }
            cursor += entry_len;
        }

        if self.enabled {
            if !self.crc32c_supported {
                return XattrBlockValidation::InvalidChecksum;
            }
            let provided = u32::from_le_bytes(block[16..20].try_into().unwrap());
            let mut crc = ext4_crc32c(self.seed, &block_number.to_le_bytes());
            crc = ext4_crc32c(crc, &block[..16]);
            crc = ext4_crc32c(crc, &[0; 4]);
            crc = ext4_crc32c(crc, &block[20..]);
            if provided != crc {
                return XattrBlockValidation::InvalidChecksum;
            }
        }
        XattrBlockValidation::Valid { refcount }
    }

    pub fn verify_bitmap(&self, _group: u32, bitmap: &[u8], provided: u32, has_high: bool) -> bool {
        if !self.enabled {
            return true;
        }
        if !self.crc32c_supported {
            return false;
        }
        let crc = ext4_crc32c(self.seed, bitmap);
        if has_high {
            provided == crc
        } else {
            provided == (crc & 0xffff)
        }
    }
}

fn crc16_ext4(mut crc: u16, bytes: &[u8]) -> u16 {
    for &byte in bytes {
        crc ^= byte as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xa001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

const INODE_GENERATION_OFFSET: usize = 100;
const INODE_CHECKSUM_LO_OFFSET: usize = 124;
const INODE_EXTRA_ISIZE_OFFSET: usize = 128;
const INODE_CHECKSUM_HI_OFFSET: usize = 130;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeChecksumResult {
    NotEnabled,
    Valid,
    Invalid { provided: u32, calculated: u32 },
}

/// Precomputed filesystem-level state for validating every inode in a table.
/// The checksum seed is invariant for the filesystem and must not be rebuilt
/// for each inode in a hot scan loop.
#[derive(Debug, Clone, Copy)]
pub struct InodeChecksumVerifier {
    enabled: bool,
    inode_size: usize,
    seed: u32,
}

impl InodeChecksumVerifier {
    pub fn new(superblock: &Ext4Superblock) -> Self {
        let enabled = superblock.has_ro_compat_feature(EXT4_FEATURE_RO_COMPAT_METADATA_CSUM);
        let seed = if superblock.has_incompat_feature(EXT4_FEATURE_INCOMPAT_CSUM_SEED) {
            u32::from_le(superblock.s_checksum_seed)
        } else {
            ext4_crc32c(u32::MAX, &superblock.s_uuid)
        };
        Self {
            enabled,
            inode_size: superblock.inode_size() as usize,
            seed,
        }
    }

    pub fn verify(&self, inode_number: u32, raw_inode: &[u8]) -> InodeChecksumResult {
        if !self.enabled {
            return InodeChecksumResult::NotEnabled;
        }
        if raw_inode.len() < self.inode_size || self.inode_size < 128 {
            return InodeChecksumResult::Invalid {
                provided: 0,
                calculated: 0,
            };
        }
        let raw_inode = &raw_inode[..self.inode_size];
        let low = u16::from_le_bytes([
            raw_inode[INODE_CHECKSUM_LO_OFFSET],
            raw_inode[INODE_CHECKSUM_LO_OFFSET + 1],
        ]) as u32;
        let extra_isize = if self.inode_size > 128 && raw_inode.len() >= 132 {
            u16::from_le_bytes([
                raw_inode[INODE_EXTRA_ISIZE_OFFSET],
                raw_inode[INODE_EXTRA_ISIZE_OFFSET + 1],
            ]) as usize
        } else {
            0
        };
        let has_high = self.inode_size > 128 && extra_isize >= 4 && raw_inode.len() >= 132;
        let provided = if has_high {
            low | ((u16::from_le_bytes([
                raw_inode[INODE_CHECKSUM_HI_OFFSET],
                raw_inode[INODE_CHECKSUM_HI_OFFSET + 1],
            ]) as u32)
                << 16)
        } else {
            low
        };

        let inode_number_bytes = inode_number.to_le_bytes();
        let zero = [0u8; 2];
        let mut calculated = if has_high {
            crc32c_chain(
                self.seed,
                &[
                    &inode_number_bytes,
                    &raw_inode[INODE_GENERATION_OFFSET..INODE_GENERATION_OFFSET + 4],
                    &raw_inode[..INODE_CHECKSUM_LO_OFFSET],
                    &zero,
                    &raw_inode[INODE_CHECKSUM_LO_OFFSET + 2..INODE_CHECKSUM_HI_OFFSET],
                    &zero,
                    &raw_inode[INODE_CHECKSUM_HI_OFFSET + 2..],
                ],
            )
        } else {
            crc32c_chain(
                self.seed,
                &[
                    &inode_number_bytes,
                    &raw_inode[INODE_GENERATION_OFFSET..INODE_GENERATION_OFFSET + 4],
                    &raw_inode[..INODE_CHECKSUM_LO_OFFSET],
                    &zero,
                    &raw_inode[INODE_CHECKSUM_LO_OFFSET + 2..],
                ],
            )
        };
        if !has_high {
            calculated &= 0xffff;
        }

        if provided == calculated || raw_inode[..128].iter().all(|byte| *byte == 0) {
            InodeChecksumResult::Valid
        } else {
            InodeChecksumResult::Invalid {
                provided,
                calculated,
            }
        }
    }
}

/// Implements the ext2fs_inode_csum_verify algorithm over the complete raw
/// on-disk inode, including the e2fsprogs all-zero unused-inode exception.
pub fn verify_inode_checksum(
    superblock: &Ext4Superblock,
    inode_number: u32,
    raw_inode: &[u8],
) -> InodeChecksumResult {
    InodeChecksumVerifier::new(superblock).verify(inode_number, raw_inode)
}

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

fn crc32c_chain(mut seed: u32, segments: &[&[u8]]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("sse4.2") {
        for segment in segments {
            // SAFETY: guarded by runtime feature detection once for the chain.
            seed = unsafe { crc32c_x86(seed, segment) };
        }
        return seed;
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("crc") {
        for segment in segments {
            // SAFETY: guarded by runtime feature detection once for the chain.
            seed = unsafe { crc32c_arm(seed, segment) };
        }
        return seed;
    }
    for segment in segments {
        seed = crc32c_scalar(seed, segment);
    }
    seed
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

    fn test_superblock(inode_size: u16, explicit_seed: Option<u32>) -> Ext4Superblock {
        let bytes = [0u8; 1024];
        let (superblock, _) = Ext4Superblock::ref_from_prefix(&bytes).unwrap();
        let mut superblock = *superblock;
        superblock.s_inode_size = inode_size.to_le();
        superblock.s_feature_ro_compat = EXT4_FEATURE_RO_COMPAT_METADATA_CSUM.to_le();
        superblock.s_uuid = [
            0x52, 0x66, 0x8f, 0x3c, 0x91, 0x47, 0x44, 0xbd, 0xb8, 0x23, 0xf6, 0x3f, 0xb5, 0xe6,
            0x2d, 0x69,
        ];
        if let Some(seed) = explicit_seed {
            superblock.s_feature_incompat = EXT4_FEATURE_INCOMPAT_CSUM_SEED.to_le();
            superblock.s_checksum_seed = seed.to_le();
        }
        superblock
    }

    fn reference_inode_checksum(
        superblock: &Ext4Superblock,
        inode_number: u32,
        raw_inode: &[u8],
    ) -> (u32, bool) {
        let mut copy = raw_inode.to_vec();
        copy[INODE_CHECKSUM_LO_OFFSET..INODE_CHECKSUM_LO_OFFSET + 2].fill(0);
        let extra_isize = if copy.len() > 128 {
            u16::from_le_bytes([
                copy[INODE_EXTRA_ISIZE_OFFSET],
                copy[INODE_EXTRA_ISIZE_OFFSET + 1],
            ]) as usize
        } else {
            0
        };
        let has_high = copy.len() > 128 && extra_isize >= 4;
        if has_high {
            copy[INODE_CHECKSUM_HI_OFFSET..INODE_CHECKSUM_HI_OFFSET + 2].fill(0);
        }
        let seed = if superblock.has_incompat_feature(EXT4_FEATURE_INCOMPAT_CSUM_SEED) {
            u32::from_le(superblock.s_checksum_seed)
        } else {
            ext4_crc32c(u32::MAX, &superblock.s_uuid)
        };
        let mut crc = ext4_crc32c(seed, &inode_number.to_le_bytes());
        crc = ext4_crc32c(
            crc,
            &raw_inode[INODE_GENERATION_OFFSET..INODE_GENERATION_OFFSET + 4],
        );
        crc = ext4_crc32c(crc, &copy);
        if !has_high {
            crc &= 0xffff;
        }
        (crc, has_high)
    }

    fn install_xattr_checksum(sb: &Ext4Superblock, block_number: u64, block: &mut [u8]) {
        let seed = if sb.has_incompat_feature(EXT4_FEATURE_INCOMPAT_CSUM_SEED) {
            u32::from_le(sb.s_checksum_seed)
        } else {
            ext4_crc32c(u32::MAX, &sb.s_uuid)
        };
        let mut crc = ext4_crc32c(seed, &block_number.to_le_bytes());
        crc = ext4_crc32c(crc, &block[..16]);
        crc = ext4_crc32c(crc, &[0; 4]);
        crc = ext4_crc32c(crc, &block[20..]);
        block[16..20].copy_from_slice(&crc.to_le_bytes());
    }

    fn valid_xattr_block(sb: &Ext4Superblock, block_number: u64) -> Vec<u8> {
        let mut block = vec![0u8; 4096];
        block[0..4].copy_from_slice(&EXT4_XATTR_MAGIC.to_le_bytes());
        block[4..8].copy_from_slice(&1u32.to_le_bytes());
        block[8..12].copy_from_slice(&1u32.to_le_bytes());
        block[32] = 1;
        block[33] = 1;
        block[34..36].copy_from_slice(&4088u16.to_le_bytes());
        block[40..44].copy_from_slice(&4u32.to_le_bytes());
        block[48] = b'a';
        block[4088..4092].copy_from_slice(b"data");
        install_xattr_checksum(sb, block_number, &mut block);
        block
    }

    fn install_inode_checksum(superblock: &Ext4Superblock, inode_number: u32, raw: &mut [u8]) {
        let (checksum, has_high) = reference_inode_checksum(superblock, inode_number, raw);
        raw[INODE_CHECKSUM_LO_OFFSET..INODE_CHECKSUM_LO_OFFSET + 2]
            .copy_from_slice(&(checksum as u16).to_le_bytes());
        if has_high {
            raw[INODE_CHECKSUM_HI_OFFSET..INODE_CHECKSUM_HI_OFFSET + 2]
                .copy_from_slice(&((checksum >> 16) as u16).to_le_bytes());
        }
    }

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

    #[test]
    fn inode_checksum_matches_independent_reference_across_layouts_and_seeds() {
        let mut state = 0x7e57_1a2b_3c4d_5e6fu64;
        for inode_size in [128u16, 256, 512] {
            for explicit_seed in [None, Some(0x1234_5678)] {
                let superblock = test_superblock(inode_size, explicit_seed);
                for inode_number in [1u32, 2, 11, 12, 65_537, u32::MAX] {
                    let mut raw = vec![0u8; inode_size as usize];
                    for byte in &mut raw {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        *byte = state as u8;
                    }
                    if inode_size > 128 {
                        raw[INODE_EXTRA_ISIZE_OFFSET..INODE_EXTRA_ISIZE_OFFSET + 2]
                            .copy_from_slice(&32u16.to_le_bytes());
                    }
                    install_inode_checksum(&superblock, inode_number, &mut raw);
                    assert_eq!(
                        verify_inode_checksum(&superblock, inode_number, &raw),
                        InodeChecksumResult::Valid
                    );

                    raw[40] ^= 0x80;
                    assert!(matches!(
                        verify_inode_checksum(&superblock, inode_number, &raw),
                        InodeChecksumResult::Invalid { .. }
                    ));
                }
            }
        }
    }

    #[test]
    fn inode_checksum_covers_number_generation_and_stored_fields() {
        let superblock = test_superblock(256, None);
        let mut raw = vec![0x5au8; 256];
        raw[INODE_EXTRA_ISIZE_OFFSET..INODE_EXTRA_ISIZE_OFFSET + 2]
            .copy_from_slice(&32u16.to_le_bytes());
        raw[INODE_GENERATION_OFFSET..INODE_GENERATION_OFFSET + 4]
            .copy_from_slice(&0x89ab_cdefu32.to_le_bytes());
        install_inode_checksum(&superblock, 42, &mut raw);
        assert_eq!(
            verify_inode_checksum(&superblock, 42, &raw),
            InodeChecksumResult::Valid
        );
        assert!(matches!(
            verify_inode_checksum(&superblock, 43, &raw),
            InodeChecksumResult::Invalid { .. }
        ));

        let mut changed_generation = raw.clone();
        changed_generation[INODE_GENERATION_OFFSET] ^= 1;
        assert!(matches!(
            verify_inode_checksum(&superblock, 42, &changed_generation),
            InodeChecksumResult::Invalid { .. }
        ));

        let mut changed_checksum = raw;
        changed_checksum[INODE_CHECKSUM_HI_OFFSET] ^= 1;
        assert!(matches!(
            verify_inode_checksum(&superblock, 42, &changed_checksum),
            InodeChecksumResult::Invalid { .. }
        ));
    }

    #[test]
    fn inode_checksum_feature_and_unused_inode_semantics_match_e2fsprogs() {
        let mut disabled = test_superblock(256, None);
        disabled.s_feature_ro_compat = 0;
        assert_eq!(
            verify_inode_checksum(&disabled, 12, &[0xa5; 256]),
            InodeChecksumResult::NotEnabled
        );

        let enabled = test_superblock(256, None);
        assert_eq!(
            verify_inode_checksum(&enabled, 12, &[0; 256]),
            InodeChecksumResult::Valid
        );
    }

    #[test]
    fn external_xattr_checksum_and_bounds_are_deterministic() {
        let mut sb = test_superblock(256, Some(0xfeed_beef));
        sb.s_checksum_type = 1;
        let verifier = Ext4MetadataChecksum::new(&sb);
        let block_number = 0x1_2345_6789;
        let clean = valid_xattr_block(&sb, block_number);
        assert_eq!(
            verifier.validate_xattr_block(block_number, &clean),
            XattrBlockValidation::Valid { refcount: 1 }
        );

        let mut bad_header = clean.clone();
        bad_header[0] ^= 1;
        assert_eq!(
            verifier.validate_xattr_block(block_number, &bad_header),
            XattrBlockValidation::InvalidStructure
        );
        let mut bad_bounds = clean.clone();
        bad_bounds[34..36].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(
            verifier.validate_xattr_block(block_number, &bad_bounds),
            XattrBlockValidation::InvalidStructure
        );
        let mut bad_checksum = clean.clone();
        bad_checksum[16] ^= 1;
        assert_eq!(
            verifier.validate_xattr_block(block_number, &bad_checksum),
            XattrBlockValidation::InvalidChecksum
        );
        assert_eq!(
            verifier.validate_xattr_block(block_number + 1, &clean),
            XattrBlockValidation::InvalidChecksum
        );

        let mut seed = 0x4d4d_5058_4154_5452u64;
        for length in 0..=512 {
            let mut arbitrary = vec![0u8; length];
            for byte in &mut arbitrary {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *byte = seed as u8;
            }
            let first = verifier.validate_xattr_block(block_number, &arbitrary);
            let second = verifier.validate_xattr_block(block_number, &arbitrary);
            assert_eq!(first, second, "nondeterministic result at len={length}");
        }
    }
}

/// Statistics collected during inode & extent verification.
#[derive(Debug, Default)]
pub struct InodeVerificationStats {
    pub superblock_checksum_failures: AtomicU64,
    pub group_descriptor_checksum_failures: AtomicU64,
    pub block_bitmap_checksum_failures: AtomicU64,
    pub inode_bitmap_checksum_failures: AtomicU64,
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
    pub inode_checksum_failures: AtomicU64,
    pub directory_checksum_failures: AtomicU64,
    pub extent_block_checksum_failures: AtomicU64,
    pub xattr_block_corruptions: AtomicU64,
    pub xattr_checksum_failures: AtomicU64,
}

#[derive(Clone, Copy)]
pub struct ExtentTreeChecksumContext {
    pub inode_number: u32,
    pub inode_generation: u32,
    pub metadata_checksum: Ext4MetadataChecksum,
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
    dense_words: Option<RwLock<Vec<u64>>>,
    pub chunks: RwLock<HashMap<u32, RoaringBitmap>>,
}

impl BlockAllocationTracker {
    pub fn new(total_blocks: u64) -> Self {
        // Cap the dense representation at 64 MiB. It is substantially faster
        // for ordinary filesystems while the chunked representation preserves
        // sparse 64-bit addressing for very large spaces.
        let dense_words = usize::try_from(total_blocks.div_ceil(64))
            .ok()
            .filter(|words| *words <= (64 * 1024 * 1024 / 8))
            .map(|words| RwLock::new(vec![0; words]));
        Self {
            total_blocks,
            allocated_count: AtomicU64::new(0),
            dense_words,
            chunks: RwLock::new(HashMap::new()),
        }
    }

    /// Marks a range of 64-bit blocks as allocated. Returns true if no collision detected.
    pub fn mark_range(&self, start_block: u64, count: u32) -> bool {
        if count == 0 {
            return true;
        }
        if let Some(dense) = &self.dense_words {
            let end = start_block
                .saturating_add(count as u64)
                .min(self.total_blocks);
            if start_block >= end {
                return false;
            }
            let mut words = dense.write().unwrap();
            let mut cursor = start_block;
            let mut newly_added = 0u64;
            let mut collision = false;
            while cursor < end {
                let word_index = (cursor / 64) as usize;
                let bit = (cursor % 64) as u32;
                let take = (end - cursor).min((64 - bit) as u64) as u32;
                let mask = if take == 64 {
                    u64::MAX
                } else {
                    ((1u64 << take) - 1) << bit
                };
                let occupied = words[word_index] & mask;
                collision |= occupied != 0;
                newly_added += (mask & !words[word_index]).count_ones() as u64;
                words[word_index] |= mask;
                cursor += take as u64;
            }
            self.allocated_count
                .fetch_add(newly_added, Ordering::Relaxed);
            return !collision;
        }
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
        if let Some(dense) = &self.dense_words {
            if block >= self.total_blocks {
                return false;
            }
            let words = dense.read().unwrap();
            return words[(block / 64) as usize] & (1u64 << (block % 64)) != 0;
        }
        let chunk_idx = (block >> 32) as u32;
        let offset = (block & 0xFFFF_FFFF) as u32;

        let chunks = self.chunks.read().unwrap();
        if let Some(bm) = chunks.get(&chunk_idx) {
            bm.contains(offset)
        } else {
            false
        }
    }

    pub fn is_dense(&self) -> bool {
        self.dense_words.is_some()
    }

    pub fn sparse_chunk_count(&self) -> usize {
        self.chunks.read().unwrap().len()
    }

    pub fn representation_name(&self) -> &'static str {
        if self.is_dense() {
            "dense bitset"
        } else {
            "chunked Roaring bitmaps"
        }
    }
}

/// Validates a batch of inodes in parallel using Rayon work-stealing.
pub fn verify_inodes_parallel<F>(
    inodes: &[Ext4Inode],
    first_inode_number: u32,
    tracker: &BlockAllocationTracker,
    stats: &InodeVerificationStats,
    metadata_checksum: Ext4MetadataChecksum,
    read_block_fn: &F,
) where
    F: Fn(u64) -> Option<Vec<u8>> + Sync,
{
    stats
        .total_inodes_scanned
        .fetch_add(inodes.len() as u64, Ordering::Relaxed);

    let verify = |(index, inode): (usize, &Ext4Inode)| {
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
            let inode_number = first_inode_number + index as u32;
            let inode_generation = u32::from_le(inode.i_generation);
            verify_extent_block(
                &inode.i_block,
                ExtentTreeChecksumContext {
                    inode_number,
                    inode_generation,
                    metadata_checksum,
                },
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
    };

    // Typical ext4 group-sized batches (8K inodes in the benchmark fixture)
    // are faster serially because extent accounting writes through one shared
    // tracker. Rayon remains useful for unusually large batches.
    if inodes.len() <= 16_384 {
        inodes.iter().enumerate().for_each(verify);
    } else {
        inodes.par_iter().enumerate().for_each(verify);
    }
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
    checksum_context: ExtentTreeChecksumContext,
    max_blocks: u64,
    tracker: &BlockAllocationTracker,
    stats: &InodeVerificationStats,
    read_block_fn: &F,
    current_depth: u16,
) where
    F: Fn(u64) -> Option<Vec<u8>>,
{
    let mut visited = std::collections::HashSet::new();
    let context = ExtentValidationContext {
        checksum_context,
        max_blocks,
        tracker,
        stats,
        read_block_fn: &read_block_fn,
    };
    let _ = verify_extent_node(data, &context, current_depth, None, &mut visited);
}

/// Returns the node's logical range as `(first, end_exclusive)`. The visited
/// set is per inode tree, so cycles and child-node aliasing are rejected even
/// when both references are otherwise in filesystem bounds.
struct ExtentValidationContext<'a, F>
where
    F: Fn(u64) -> Option<Vec<u8>>,
{
    checksum_context: ExtentTreeChecksumContext,
    max_blocks: u64,
    tracker: &'a BlockAllocationTracker,
    stats: &'a InodeVerificationStats,
    read_block_fn: &'a F,
}

fn verify_extent_node<F>(
    data: &[u8],
    context: &ExtentValidationContext<'_, F>,
    current_depth: u16,
    expected: Option<(u16, u32)>,
    visited: &mut std::collections::HashSet<u64>,
) -> Option<(u32, u64)>
where
    F: Fn(u64) -> Option<Vec<u8>>,
{
    let corrupt = || {
        context
            .stats
            .extent_corruptions
            .fetch_add(1, Ordering::Relaxed);
    };
    if current_depth > 5 {
        corrupt();
        return None;
    }

    if current_depth > 0
        && !context
            .checksum_context
            .metadata_checksum
            .verify_extent_tree_block(
                context.checksum_context.inode_number,
                context.checksum_context.inode_generation,
                data,
            )
    {
        context
            .stats
            .extent_block_checksum_failures
            .fetch_add(1, Ordering::Relaxed);
        return None;
    }

    let (header, rest) = match Ext4ExtentHeader::ref_from_prefix(data) {
        Ok(res) => res,
        Err(_) => {
            corrupt();
            return None;
        }
    };

    if !header.is_valid_magic() {
        corrupt();
        return None;
    }

    let entries = header.entries() as usize;
    let max_entries = header.max() as usize;
    let depth = header.depth();
    if depth > 5 || (current_depth > 0 && entries == 0) || (depth == 0 && max_entries == 0) {
        corrupt();
        return None;
    }
    let extent_size = std::mem::size_of::<Ext4Extent>();
    let entry_size = if depth == 0 {
        extent_size
    } else {
        std::mem::size_of::<Ext4ExtentIdx>()
    };
    let external_tail =
        usize::from(current_depth > 0 && metadata_checksum_enabled(context.checksum_context)) * 4;
    let physical_capacity = data
        .len()
        .saturating_sub(std::mem::size_of::<Ext4ExtentHeader>() + external_tail)
        / entry_size;

    if entries > max_entries || max_entries > physical_capacity {
        corrupt();
        return None;
    }

    if expected.is_some_and(|(parent_depth, _)| depth != parent_depth) {
        corrupt();
        return None;
    }

    if header.depth() == 0 {
        // Leaf nodes
        let mut first = None;
        let mut previous_end = None;
        for i in 0..entries {
            let offset = i * extent_size;
            if offset + extent_size > rest.len() {
                corrupt();
                return None;
            }

            let Ok((ext, _)) = Ext4Extent::ref_from_prefix(&rest[offset..]) else {
                corrupt();
                return None;
            };
            let logical = ext.logical_block();
            let count = ext.block_count();
            let logical_end = logical as u64 + count as u64;
            let start_block = ext.physical_start();
            let Some(physical_end) = start_block.checked_add(count as u64) else {
                context
                    .stats
                    .out_of_bounds_blocks
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if count == 0
                || previous_end.is_some_and(|end| (logical as u64) < end)
                || logical_end > (u32::MAX as u64 + 1)
            {
                corrupt();
                return None;
            }
            if start_block == 0 || physical_end > context.max_blocks {
                context
                    .stats
                    .out_of_bounds_blocks
                    .fetch_add(1, Ordering::Relaxed);
            } else if !context.tracker.mark_range(start_block, count) {
                context
                    .stats
                    .duplicate_blocks
                    .fetch_add(1, Ordering::Relaxed);
            }
            first.get_or_insert(logical);
            previous_end = Some(logical_end);
        }
        let range = first.zip(previous_end);
        if let (Some((_, key)), Some((first, _))) = (expected, range) {
            if first != key {
                corrupt();
                return None;
            }
        }
        range
    } else {
        // Branch index nodes (Ext4ExtentIdx)
        let idx_size = std::mem::size_of::<Ext4ExtentIdx>();
        let mut first = None;
        let mut previous_key = None;
        let mut previous_end = None;
        for i in 0..entries {
            let offset = i * idx_size;
            if offset + idx_size > rest.len() {
                corrupt();
                return None;
            }

            let Ok((idx, _)) = Ext4ExtentIdx::ref_from_prefix(&rest[offset..]) else {
                corrupt();
                return None;
            };
            let key = idx.logical_block();
            if previous_key.is_some_and(|previous| key <= previous) {
                corrupt();
                return None;
            }
            previous_key = Some(key);
            let child_block = idx.child_block();
            if child_block == 0 || child_block >= context.max_blocks {
                context
                    .stats
                    .out_of_bounds_blocks
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if !visited.insert(child_block) {
                corrupt();
                continue;
            }
            if !context.tracker.mark_range(child_block, 1) {
                context
                    .stats
                    .duplicate_blocks
                    .fetch_add(1, Ordering::Relaxed);
            }
            if let Some(child_data) = (context.read_block_fn)(child_block) {
                if let Some((child_first, child_end)) = verify_extent_node(
                    &child_data,
                    context,
                    current_depth + 1,
                    Some((depth - 1, key)),
                    visited,
                ) {
                    if previous_end.is_some_and(|end| (child_first as u64) < end) {
                        corrupt();
                    }
                    first.get_or_insert(child_first);
                    previous_end = Some(child_end);
                } else {
                    corrupt();
                }
            } else {
                corrupt();
            }
        }
        let range = first.zip(previous_end);
        if let (Some((_, key)), Some((first, _))) = (expected, range) {
            if first != key {
                corrupt();
                return None;
            }
        }
        range
    }
}

fn metadata_checksum_enabled(context: ExtentTreeChecksumContext) -> bool {
    context.metadata_checksum.enabled
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
    if let Some(dense) = &tracker.dense_words {
        let words = dense.read().unwrap();
        let mut processed = 0u32;
        for disk_chunk in disk_bitmap.chunks(8) {
            if processed >= blocks_in_group {
                break;
            }
            let valid = (blocks_in_group - processed).min(64);
            let mut bytes = [0u8; 8];
            bytes[..disk_chunk.len()].copy_from_slice(disk_chunk);
            let disk_word = u64::from_le_bytes(bytes);
            let absolute = first_block_in_group + processed as u64;
            let word_index = (absolute / 64) as usize;
            let shift = (absolute % 64) as u32;
            let mut tracked = words.get(word_index).copied().unwrap_or(0) >> shift;
            if shift != 0 {
                tracked |= words.get(word_index + 1).copied().unwrap_or(0) << (64 - shift);
            }
            let mask = if valid == 64 {
                u64::MAX
            } else {
                (1u64 << valid) - 1
            };
            discrepancy.false_free_blocks += (tracked & !disk_word & mask).count_ones() as u64;
            discrepancy.leaked_blocks += (!tracked & disk_word & mask).count_ones() as u64;
            processed += valid;
        }
        return discrepancy;
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactDirectoryEntry {
    pub inode: u32,
    pub is_dot_or_dotdot: bool,
}

#[derive(Debug, Default, Clone)]
pub struct CompactDirectoryValidationResult {
    pub entries: Vec<CompactDirectoryEntry>,
    pub corrupt_entries: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HTreeValidationResult {
    pub indexed_blocks: u64,
    pub leaf_blocks: u64,
    pub errors: Vec<String>,
}

pub fn verify_htree_directory<'a, F>(
    root: &[u8],
    logical_block_count: u32,
    read_logical: &F,
) -> HTreeValidationResult
where
    F: Fn(u32) -> Option<&'a [u8]>,
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

fn validate_dx_entries<'a, F>(
    block: &[u8],
    count_offset: usize,
    levels: u8,
    logical_block_count: u32,
    read_logical: &F,
    visited: &mut std::collections::HashSet<u32>,
    result: &mut HTreeValidationResult,
) where
    F: Fn(u32) -> Option<&'a [u8]>,
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
            if verify_directory_block_compact(child_data, u32::MAX).corrupt_entries > 0 {
                result.errors.push(format!(
                    "H-Tree leaf logical block {child} contains invalid entries"
                ));
            }
        } else {
            validate_dx_entries(
                child_data,
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

/// Allocation-light directory parsing for the checker hot path. Names are
/// validated in place and only the information needed by later passes survives.
pub fn verify_directory_block_compact(
    block_bytes: &[u8],
    max_inodes: u32,
) -> CompactDirectoryValidationResult {
    let mut result = CompactDirectoryValidationResult::default();
    let mut offset = 0;
    while offset + 8 <= block_bytes.len() {
        let Ok((header, _)) = Ext4DirEntry2Header::ref_from_prefix(&block_bytes[offset..]) else {
            result.corrupt_entries += 1;
            break;
        };
        let rec_len = header.record_len() as usize;
        let name_len = header.name_length() as usize;
        if rec_len < 8 || rec_len & 3 != 0 || offset + rec_len > block_bytes.len() {
            result.corrupt_entries += 1;
            break;
        }
        let inode = header.inode_number();
        if inode != 0 {
            if header.entry_file_type() == EXT4_FT_DIR_CSUM {
                break;
            }
            if inode > max_inodes || name_len == 0 || 8 + name_len > rec_len {
                result.corrupt_entries += 1;
            } else {
                let name = &block_bytes[offset + 8..offset + 8 + name_len];
                if name.contains(&b'/') || name.contains(&0) {
                    result.corrupt_entries += 1;
                } else {
                    result.entries.push(CompactDirectoryEntry {
                        inode,
                        is_dot_or_dotdot: name == b"." || name == b"..",
                    });
                }
            }
        }
        offset += rec_len;
    }
    result
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
        let result = verify_htree_directory(&root, 2, &|logical| {
            (logical == 1).then_some(leaf.as_slice())
        });
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
mod extent_semantic_tests {
    use super::*;

    fn root(depth: u16, entries: u16, max: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 60];
        bytes[0..2].copy_from_slice(&nexfsck_core::EXT4_EXTENT_MAGIC.to_le_bytes());
        bytes[2..4].copy_from_slice(&entries.to_le_bytes());
        bytes[4..6].copy_from_slice(&max.to_le_bytes());
        bytes[6..8].copy_from_slice(&depth.to_le_bytes());
        bytes
    }

    fn context() -> ExtentTreeChecksumContext {
        let raw = [0u8; 1024];
        let sb = *Ext4Superblock::ref_from_prefix(&raw).unwrap().0;
        ExtentTreeChecksumContext {
            inode_number: 12,
            inode_generation: 1,
            metadata_checksum: Ext4MetadataChecksum::new(&sb),
        }
    }

    #[test]
    fn accepts_initialized_extent_at_length_boundary() {
        let mut bytes = root(0, 1, 4);
        bytes[12..16].copy_from_slice(&3u32.to_le_bytes());
        bytes[16..18].copy_from_slice(&0x8000u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        let tracker = BlockAllocationTracker::new(40_000);
        let stats = InodeVerificationStats::new();
        verify_extent_block(&bytes, context(), 40_000, &tracker, &stats, &|_| None, 0);
        assert_eq!(stats.extent_corruptions.load(Ordering::Relaxed), 0);
        assert_eq!(tracker.allocated_count(), 0x8000);
    }

    #[test]
    fn rejects_zero_length_extent_and_reused_child_node() {
        let mut leaf = root(0, 1, 4);
        leaf[12..16].copy_from_slice(&1u32.to_le_bytes());
        leaf[20..24].copy_from_slice(&8u32.to_le_bytes());
        let stats = InodeVerificationStats::new();
        verify_extent_block(
            &leaf,
            context(),
            100,
            &BlockAllocationTracker::new(100),
            &stats,
            &|_| None,
            0,
        );
        assert_eq!(stats.extent_corruptions.load(Ordering::Relaxed), 1);

        let mut inode_root = root(2, 1, 4);
        inode_root[12..16].copy_from_slice(&0u32.to_le_bytes());
        inode_root[16..20].copy_from_slice(&10u32.to_le_bytes());
        let mut child = vec![0u8; 4096];
        child[0..2].copy_from_slice(&nexfsck_core::EXT4_EXTENT_MAGIC.to_le_bytes());
        child[2..4].copy_from_slice(&1u16.to_le_bytes());
        child[4..6].copy_from_slice(&340u16.to_le_bytes());
        child[6..8].copy_from_slice(&1u16.to_le_bytes());
        child[12..16].copy_from_slice(&0u32.to_le_bytes());
        child[16..20].copy_from_slice(&10u32.to_le_bytes());
        let stats = InodeVerificationStats::new();
        verify_extent_block(
            &inode_root,
            context(),
            100,
            &BlockAllocationTracker::new(100),
            &stats,
            &|block| (block == 10).then(|| child.clone()),
            0,
        );
        assert!(stats.extent_corruptions.load(Ordering::Relaxed) > 0);
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

    #[test]
    fn dense_word_reconciliation_matches_scalar_reference() {
        let total = 131_173u64;
        let tracker = BlockAllocationTracker::new(total);
        let mut state = 0x1234_5678_9abc_def0u64;
        for _ in 0..4_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let start = state % (total - 33);
            tracker.mark_range(start, (state as u32 % 32) + 1);
        }
        for first in [0, 1, 32_768, 65_537, 100_000] {
            let count = ((total - first).min(32_768)) as u32;
            let mut disk = vec![0u8; count.div_ceil(8) as usize];
            for index in 0..count {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                if state & 3 != 0 {
                    disk[(index / 8) as usize] |= 1 << (index % 8);
                }
            }
            let fast = reconcile_block_bitmap(&disk, &tracker, first, count);
            let mut scalar = BitmapDiscrepancy::default();
            for index in 0..count {
                let on_disk = disk[(index / 8) as usize] & (1 << (index % 8)) != 0;
                let reconstructed = tracker.is_allocated(first + index as u64);
                scalar.false_free_blocks += u64::from(reconstructed && !on_disk);
                scalar.leaked_blocks += u64::from(!reconstructed && on_disk);
            }
            assert_eq!(fast.false_free_blocks, scalar.false_free_blocks);
            assert_eq!(fast.leaked_blocks, scalar.leaked_blocks);
        }
    }
}
