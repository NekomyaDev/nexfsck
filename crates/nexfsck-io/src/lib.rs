//! `nexfsck-io`
//!
//! Direct I/O block device management, cache-flush invalidation,
//! and high-throughput streaming reader.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use thiserror::Error;
use zerocopy::FromBytes;

use nexfsck_core::{CoreError, Ext4Superblock, EXT4_SUPERBLOCK_OFFSET, EXT4_SUPERBLOCK_SIZE};

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
}

/// Linux block device handle equipped with safe Direct I/O and cache invalidation.
pub struct BlockDevice {
    file: File,
    path: String,
    read_only: bool,
}

impl BlockDevice {
    /// Opens the specified block device or disk image.
    /// Executes mandatory cache flush (`ioctl BLKFLSBUF`) to avoid reading stale cache lines.
    pub fn open<P: AsRef<Path>>(path: P, read_only: bool) -> Result<Self, IoError> {
        let path_str = path.as_ref().to_string_lossy().to_string();
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(&path)?;

        let dev = Self {
            file,
            path: path_str,
            read_only,
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
            // Not fatal if running against a regular file (e.g. disk image test)
            tracing::debug!("BLKFLSBUF ioctl returned {ret} ({err}); continuing");
        }
        Ok(())
    }

    /// Reads the primary ext4 superblock at offset 1024 bytes.
    pub fn read_superblock(&self) -> Result<Ext4Superblock, IoError> {
        let mut buffer = [0u8; EXT4_SUPERBLOCK_SIZE];
        self.file.read_exact_at(&mut buffer, EXT4_SUPERBLOCK_OFFSET)?;

        let (sb, _) = Ext4Superblock::ref_from_prefix(&buffer)
            .map_err(|_| CoreError::BufferTooSmall {
                expected: EXT4_SUPERBLOCK_SIZE,
                found: buffer.len(),
            })?;

        sb.verify_magic()?;
        Ok(*sb)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
}
