//! `nexfsck-journal`
//!
//! JBD2 crash recovery analysis and Atomic Undo Journal (Rollback Engine).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use thiserror::Error;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use nexfsck_core::{Ext4Extent, Ext4GroupDesc, Ext4Superblock};
use nexfsck_io::BlockDevice;

pub const UNDO_MAGIC: u32 = 0x554E444F; // "UNDO"
pub const JBD2_MAGIC_NUMBER: u32 = 0xC03B3998;

pub const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
pub const JBD2_COMMIT_BLOCK: u32 = 2;
pub const JBD2_SUPERBLOCK_V1: u32 = 3;
pub const JBD2_SUPERBLOCK_V2: u32 = 4;
pub const JBD2_REVOKE_BLOCK: u32 = 5;

#[derive(Error, Debug)]
pub enum JournalError {
    #[error("I/O error: {0}")]
    StdIo(#[from] std::io::Error),

    #[error("Device I/O error: {0}")]
    DeviceIo(#[from] nexfsck_io::IoError),

    #[error("Corrupted undo log: {0}")]
    CorruptLog(String),

    #[error("Invalid journal magic number: expected 0x{expected:08X}, found 0x{found:08X}")]
    InvalidJournalMagic { expected: u32, found: u32 },
}

/// JBD2 Header layout (12 bytes).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Jbd2Header {
    pub h_magic: u32,
    pub h_blocktype: u32,
    pub h_sequence: u32,
}

impl Jbd2Header {
    pub fn magic(&self) -> u32 {
        u32::from_be(self.h_magic)
    }

    pub fn block_type(&self) -> u32 {
        u32::from_be(self.h_blocktype)
    }

    pub fn sequence(&self) -> u32 {
        u32::from_be(self.h_sequence)
    }
}

/// JBD2 Superblock layout (first 36 bytes).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, packed)]
pub struct Jbd2Superblock {
    pub s_header: Jbd2Header,
    pub s_blocksize: u32,
    pub s_maxlen: u32,
    pub s_first: u32,
    pub s_sequence: u32,
    pub s_start: u32,
    pub s_errno: u32,
}

impl Jbd2Superblock {
    pub fn is_clean(&self) -> bool {
        u32::from_be(self.s_start) == 0
    }

    pub fn block_size(&self) -> u32 {
        u32::from_be(self.s_blocksize)
    }

    pub fn sequence(&self) -> u32 {
        u32::from_be(self.s_sequence)
    }

    pub fn verify_magic(&self) -> Result<(), JournalError> {
        let magic = self.s_header.magic();
        if magic != JBD2_MAGIC_NUMBER {
            return Err(JournalError::InvalidJournalMagic {
                expected: JBD2_MAGIC_NUMBER,
                found: magic,
            });
        }
        Ok(())
    }
}

/// An atomic rollback entry storing pre-mutation physical block data.
#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub physical_block: u64,
    pub original_data: Vec<u8>,
}

/// Atomic Undo Journal file manager.
pub struct AtomicUndoJournal {
    file: Option<File>,
    entries: Vec<UndoEntry>,
}

impl AtomicUndoJournal {
    /// Creates a new in-memory undo journal or links to an on-disk rollback file.
    pub fn new<P: AsRef<Path>>(path: Option<P>, block_size: u32) -> Result<Self, JournalError> {
        let file = if let Some(p) = path {
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(p)?;
            f.write_all(&UNDO_MAGIC.to_le_bytes())?;
            f.write_all(&block_size.to_le_bytes())?;
            Some(f)
        } else {
            None
        };

        Ok(Self {
            file,
            entries: Vec::new(),
        })
    }

    /// Records an impending modification, storing the pristine original sector bytes.
    pub fn record_mutation(
        &mut self,
        physical_block: u64,
        original_data: &[u8],
    ) -> Result<(), JournalError> {
        if original_data.is_empty() {
            return Err(JournalError::CorruptLog(
                "Attempted to record empty mutation into undo journal".into(),
            ));
        }

        let entry = UndoEntry {
            physical_block,
            original_data: original_data.to_vec(),
        };

        if let Some(ref mut f) = self.file {
            f.write_all(&physical_block.to_le_bytes())?;
            f.write_all(&(original_data.len() as u32).to_le_bytes())?;
            f.write_all(original_data)?;
            f.flush()?;
            f.sync_all()?;
        }

        self.entries.push(entry);
        Ok(())
    }

    /// Reads an existing rollback journal and restores all blocks onto the device.
    /// Extracts the recorded block_size directly from the journal header.
    pub fn rollback_from_file<P: AsRef<Path>>(
        path: P,
        dev: &BlockDevice,
    ) -> Result<usize, JournalError> {
        let mut f = OpenOptions::new().read(true).open(path)?;

        let mut magic = [0u8; 4];
        f.read_exact(&mut magic)?;
        if u32::from_le_bytes(magic) != UNDO_MAGIC {
            return Err(JournalError::CorruptLog("Invalid undo journal magic".into()));
        }

        let mut block_size_buf = [0u8; 4];
        f.read_exact(&mut block_size_buf)?;
        let block_size = u32::from_le_bytes(block_size_buf) as u64;

        let mut restored = 0;
        let mut block_buf = [0u8; 8];
        let mut len_buf = [0u8; 4];

        while f.read_exact(&mut block_buf).is_ok() {
            if f.read_exact(&mut len_buf).is_err() {
                break;
            }
            let block = u64::from_le_bytes(block_buf);
            let len = u32::from_le_bytes(len_buf) as usize;

            let mut data = vec![0u8; len];
            f.read_exact(&mut data)?;

            dev.write_block(block, block_size, &data)?;
            restored += 1;
        }

        dev.flush_kernel_buffers()?;
        Ok(restored)
    }

    pub fn total_entries(&self) -> usize {
        self.entries.len()
    }
}

/// Inspects the JBD2 journal from the filesystem if present.
pub fn inspect_journal(
    dev: &BlockDevice,
    sb: &Ext4Superblock,
    descriptors: &[Ext4GroupDesc],
    is_64bit: bool,
) -> Result<Option<Jbd2Superblock>, JournalError> {
    let journal_inum = u32::from_le(sb.s_journal_inum);
    if journal_inum == 0 || descriptors.is_empty() {
        return Ok(None);
    }

    // Inode 8 is in block group 0
    let desc0 = &descriptors[0];
    let inode_table = dev.read_inode_table(sb, desc0, is_64bit)?;
    let inodes = BlockDevice::parse_inodes_from_table(&inode_table, sb.inode_size() as usize);

    let idx = (journal_inum - 1) as usize;
    if idx >= inodes.len() {
        return Ok(None);
    }

    let journal_inode = &inodes[idx];
    if !journal_inode.is_used() || !journal_inode.uses_extents() {
        return Ok(None);
    }

    if let Ok((ext, _)) = Ext4Extent::ref_from_prefix(&journal_inode.i_block[12..]) {
        let first_block = ext.physical_start();
        let block_bytes = dev.read_block(first_block, sb.block_size())?;
        if let Ok((jbd_sb, _)) = Jbd2Superblock::ref_from_prefix(&block_bytes) {
            if jbd_sb.verify_magic().is_ok() {
                return Ok(Some(*jbd_sb));
            }
        }
    }

    Ok(None)
}

