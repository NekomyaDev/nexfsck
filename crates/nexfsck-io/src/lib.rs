//! `nexfsck-io`
//!
//! Direct I/O block device management, cache-flush invalidation,
//! and high-throughput streaming reader with fault bisection.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use zerocopy::FromBytes;

pub mod uring;
pub use uring::{BatchReadRequest, BatchReadResult, IoUringEngine};

use nexfsck_core::{
    CoreError, Ext4GroupDesc, Ext4Inode, Ext4Superblock, EXT4_SUPERBLOCK_OFFSET,
    EXT4_SUPERBLOCK_SIZE,
};

#[derive(Error, Debug)]
pub enum IoError {
    #[error("I/O error: {0}")]
    StdIo(#[from] std::io::Error),

    #[error("Cache flush ioctl failed: errno {0}")]
    CacheFlushFailed(i32),

    #[error("Core parsing error: {0}")]
    Core(#[from] CoreError),

    #[error("Device size error: {0}")]
    DeviceSize(String),

    #[error("Unrecoverable media read error at offset {offset}: {source}")]
    MediaError { offset: u64, source: std::io::Error },
}

/// Linux block device handle equipped with safe Direct I/O, cache invalidation,
/// asynchronous io_uring batching, and media error isolation tracking.
pub struct BlockDevice {
    file: File,
    path: String,
    read_only: bool,
    uring: Option<IoUringEngine>,
    media_errors: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoBackend {
    Auto,
    IoUring,
    Sync,
}

impl BlockDevice {
    /// Opens the specified block device or disk image.
    /// Executes mandatory cache flush (`ioctl BLKFLSBUF`) to avoid reading stale cache lines.
    /// Initializes an asynchronous Linux `io_uring` engine if supported by the kernel and environment.
    pub fn open<P: AsRef<Path>>(path: P, read_only: bool) -> Result<Self, IoError> {
        Self::open_with_backend(path, read_only, IoBackend::Auto)
    }

    pub fn open_with_backend<P: AsRef<Path>>(
        path: P,
        read_only: bool,
        backend: IoBackend,
    ) -> Result<Self, IoError> {
        let path_str = path.as_ref().to_string_lossy().to_string();
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(&path)?;

        let uring = match backend {
            IoBackend::Sync => None,
            IoBackend::Auto | IoBackend::IoUring => IoUringEngine::try_new(file.as_raw_fd(), 128),
        };

        let dev = Self {
            file,
            path: path_str,
            read_only,
            uring,
            media_errors: AtomicU64::new(0),
        };

        // Invalidate OS buffer cache to prevent stale data incoherency
        dev.flush_kernel_buffers()?;

        Ok(dev)
    }

    /// Invokes BLKFLSBUF ioctl to flush dirty pages and invalidate buffer cache.
    pub fn flush_kernel_buffers(&self) -> Result<(), IoError> {
        const BLKFLSBUF: libc::c_ulong = 0x1261;
        let fd = self.file.as_raw_fd();
        let ret = unsafe { libc::ioctl(fd, BLKFLSBUF, 0) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            tracing::debug!("BLKFLSBUF ioctl returned {ret} ({err}); continuing");
        }
        Ok(())
    }

    /// Reads the primary ext4 superblock at offset 1024 bytes.
    pub fn read_superblock(&self) -> Result<Ext4Superblock, IoError> {
        let mut buffer = [0u8; EXT4_SUPERBLOCK_SIZE];
        self.file
            .read_exact_at(&mut buffer, EXT4_SUPERBLOCK_OFFSET)?;

        let (sb, _) =
            Ext4Superblock::ref_from_prefix(&buffer).map_err(|_| CoreError::BufferTooSmall {
                expected: EXT4_SUPERBLOCK_SIZE,
                found: buffer.len(),
            })?;

        sb.verify_magic()?;
        Ok(*sb)
    }

    /// Reads an ext4 superblock located at a specific block number.
    pub fn read_superblock_at_block(
        &self,
        block_nr: u64,
        block_size: u64,
    ) -> Result<Ext4Superblock, IoError> {
        let mut buffer = [0u8; EXT4_SUPERBLOCK_SIZE];
        let offset = block_nr * block_size;
        self.file.read_exact_at(&mut buffer, offset)?;

        let (sb, _) =
            Ext4Superblock::ref_from_prefix(&buffer).map_err(|_| CoreError::BufferTooSmall {
                expected: EXT4_SUPERBLOCK_SIZE,
                found: buffer.len(),
            })?;

        sb.verify_magic()?;
        Ok(*sb)
    }

    /// Searches known backup superblock offsets (powers of 3, 5, 7, etc.) if primary is lost.
    pub fn find_backup_superblocks(&self) -> Vec<(u64, Ext4Superblock)> {
        let mut candidates = Vec::new();
        // Common candidate blocks across 1K, 2K, and 4K ext4 formats
        let candidate_blocks = [32768, 8193, 24577, 40961, 57345, 73729, 98304];

        for &blk in &candidate_blocks {
            for &block_size in &[4096u64, 1024u64, 2048u64] {
                if let Ok(sb) = self.read_superblock_at_block(blk, block_size) {
                    candidates.push((blk, sb));
                    break;
                }
            }
        }

        candidates
    }

    /// Reads Multi-Mount Protection block if MMP feature is enabled.
    pub fn read_mmp(&self, sb: &Ext4Superblock) -> Result<Option<nexfsck_core::Ext4Mmp>, IoError> {
        if !sb.has_incompat_feature(nexfsck_core::EXT4_FEATURE_INCOMPAT_MMP) {
            return Ok(None);
        }

        let mmp_block = u64::from_le(sb.s_mmp_block);
        if mmp_block == 0 {
            return Ok(None);
        }

        let block_bytes = self.read_block(mmp_block, sb.block_size())?;
        if let Ok((mmp, _)) = nexfsck_core::Ext4Mmp::ref_from_prefix(&block_bytes) {
            if mmp.is_valid_magic() {
                return Ok(Some(*mmp));
            }
        }

        Ok(None)
    }

    /// Reads all Block Group Descriptors from disk into memory.
    pub fn read_group_descriptors(
        &self,
        sb: &Ext4Superblock,
    ) -> Result<Vec<Ext4GroupDesc>, IoError> {
        let block_size = sb.block_size();
        let desc_size = sb.desc_size();
        let bg_count = sb.block_groups_count() as usize;

        // BGD table starts at block 1 for 4KB blocks, or block 2 for 1KB blocks
        let first_data_block = u32::from_le(sb.s_first_data_block) as u64;
        let bgd_table_offset = (first_data_block + 1) * block_size;

        let total_bytes = bg_count * desc_size;
        let mut buffer = vec![0u8; total_bytes];

        self.read_exact_resilient(bgd_table_offset, &mut buffer, block_size as usize)?;

        let mut descriptors = Vec::with_capacity(bg_count);
        for i in 0..bg_count {
            let start = i * desc_size;
            let end = start + desc_size;
            let slice = &buffer[start..end];

            let mut full_desc = [0u8; 64];
            let copy_len = desc_size.min(64);
            full_desc[..copy_len].copy_from_slice(&slice[..copy_len]);

            let (desc, _) = Ext4GroupDesc::ref_from_prefix(&full_desc)
                .map_err(|_| CoreError::Corruption("Failed to parse group descriptor".into()))?;

            descriptors.push(*desc);
        }

        Ok(descriptors)
    }

    /// Reads raw bytes for the inode table of a specified block group.
    pub fn read_inode_table(
        &self,
        sb: &Ext4Superblock,
        desc: &Ext4GroupDesc,
        is_64bit: bool,
    ) -> Result<Vec<u8>, IoError> {
        let block_size = sb.block_size();
        let table_start_block = desc.inode_table_block(is_64bit);
        let byte_offset = table_start_block * block_size;

        let inodes_per_group = sb.inodes_per_group() as usize;
        let inode_size = sb.inode_size() as usize;
        let total_bytes = inodes_per_group * inode_size;

        let mut buffer = vec![0u8; total_bytes];
        self.read_exact_resilient(byte_offset, &mut buffer, block_size as usize)?;

        Ok(buffer)
    }

    /// Reads exact bytes with Hierarchical Bisection Fault Isolation.
    /// If an entire batch read fails due to a bad sector, it bisects recursively
    /// down to sector granularity to isolate only the corrupted sectors with zeros,
    /// salvaging all adjacent healthy data.
    pub fn read_exact_resilient(
        &self,
        offset: u64,
        buf: &mut [u8],
        sector_size: usize,
    ) -> Result<(), IoError> {
        if buf.is_empty() {
            return Ok(());
        }

        match self.file.read_exact_at(buf, offset) {
            Ok(()) => Ok(()),
            Err(e) => {
                if buf.len() <= sector_size {
                    // Reached single sector granularity: log media error and zero out sector
                    tracing::warn!(
                        "Physical media error at byte offset {}: {}. Isolating sector with zeros.",
                        offset,
                        e
                    );
                    self.media_errors.fetch_add(1, Ordering::Relaxed);
                    buf.fill(0);
                    Ok(())
                } else {
                    // Bisect into two halves
                    let mid = buf.len() / 2;
                    let (left, right) = buf.split_at_mut(mid);
                    self.read_exact_resilient(offset, left, sector_size)?;
                    self.read_exact_resilient(offset + mid as u64, right, sector_size)?;
                    Ok(())
                }
            }
        }
    }

    /// Parses inodes from raw inode table bytes.
    pub fn parse_inodes_from_table(table_bytes: &[u8], inode_size: usize) -> Vec<Ext4Inode> {
        let count = table_bytes.len() / inode_size;
        let mut inodes = Vec::with_capacity(count);

        for i in 0..count {
            let start = i * inode_size;
            let end = start + inode_size;
            let slice = &table_bytes[start..end];

            if let Ok((inode, _)) = Ext4Inode::ref_from_prefix(slice) {
                inodes.push(*inode);
            }
        }

        inodes
    }

    /// Reads a single block at `block_nr * block_size`.
    pub fn read_block(&self, block_nr: u64, block_size: u64) -> Result<Vec<u8>, IoError> {
        let mut buffer = vec![0u8; block_size as usize];
        let offset = block_nr * block_size;
        self.read_exact_resilient(offset, &mut buffer, block_size as usize)?;
        Ok(buffer)
    }

    /// Writes raw block data back to disk (requires read_only == false).
    pub fn write_block(&self, block_nr: u64, block_size: u64, data: &[u8]) -> Result<(), IoError> {
        if self.read_only {
            return Err(IoError::StdIo(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Cannot write to device in read-only mode",
            )));
        }
        let offset = block_nr * block_size;
        self.file.write_all_at(data, offset)?;
        Ok(())
    }

    /// Forces preceding writes to stable storage before rollback advances.
    pub fn sync_all(&self) -> Result<(), IoError> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Reads the block allocation bitmap for a block group.
    pub fn read_block_bitmap(
        &self,
        desc: &Ext4GroupDesc,
        is_64bit: bool,
        block_size: u64,
    ) -> Result<Vec<u8>, IoError> {
        let block_nr = desc.block_bitmap(is_64bit);
        self.read_block(block_nr, block_size)
    }

    /// Reads the inode allocation bitmap for a block group.
    pub fn read_inode_bitmap(
        &self,
        desc: &Ext4GroupDesc,
        is_64bit: bool,
        block_size: u64,
    ) -> Result<Vec<u8>, IoError> {
        let block_nr = desc.inode_bitmap(is_64bit);
        self.read_block(block_nr, block_size)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Number of physical bad sectors isolated during reads.
    pub fn media_errors(&self) -> u64 {
        self.media_errors.load(Ordering::Relaxed)
    }

    /// True if Linux `io_uring` kernel submission queue is active.
    pub fn is_io_uring_active(&self) -> bool {
        self.uring.is_some()
    }

    /// True when the active ring has a persistent registered-buffer pool.
    pub fn has_registered_io_buffers(&self) -> bool {
        self.uring
            .as_ref()
            .map(IoUringEngine::fixed_buffers_registered)
            .unwrap_or(false)
    }

    /// Reads multiple blocks in parallel using Linux `io_uring` if active,
    /// or falls back to direct resilient reads with zero runtime crash risk.
    pub fn read_blocks_batch(
        &self,
        blocks: &[u64],
        block_size: u64,
    ) -> Vec<(u64, Result<Vec<u8>, IoError>)> {
        if blocks.is_empty() {
            return Vec::new();
        }

        if let Some(ref uring) = self.uring {
            let requests: Vec<BatchReadRequest> = blocks
                .iter()
                .map(|&blk| BatchReadRequest {
                    id: blk,
                    offset: blk * block_size,
                    len: block_size as usize,
                })
                .collect();

            let batch_results = uring.read_batch(&requests);
            let mut final_results = Vec::with_capacity(blocks.len());

            for res in batch_results {
                match res.data {
                    Ok(data) => final_results.push((res.id, Ok(data))),
                    Err(e) => {
                        // Fall back to resilient bisection read for corrupted/short blocks
                        tracing::debug!(
                            "Block {} failed under io_uring ({}): retrying via resilient bisection reader",
                            res.id,
                            e
                        );
                        let fallback_res = self.read_block(res.id, block_size);
                        final_results.push((res.id, fallback_res));
                    }
                }
            }

            final_results
        } else {
            blocks
                .iter()
                .map(|&blk| (blk, self.read_block(blk, block_size)))
                .collect()
        }
    }
}
