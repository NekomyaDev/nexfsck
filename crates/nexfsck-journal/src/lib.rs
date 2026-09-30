//! `nexfsck-journal`
//!
//! JBD2 metadata inspection and pre-image undo journal support.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use thiserror::Error;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use nexfsck_core::{Ext4Extent, Ext4GroupDesc, Ext4Superblock};
use nexfsck_io::BlockDevice;

pub const UNDO_MAGIC: u32 = 0x554E444F; // "UNDO"
pub const UNDO_VERSION: u32 = 2;
pub const JBD2_MAGIC_NUMBER: u32 = 0xC03B3998;

pub const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
pub const JBD2_COMMIT_BLOCK: u32 = 2;
pub const JBD2_SUPERBLOCK_V1: u32 = 3;
pub const JBD2_SUPERBLOCK_V2: u32 = 4;
pub const JBD2_REVOKE_BLOCK: u32 = 5;
pub const JBD2_FLAG_ESCAPE: u32 = 1;
pub const JBD2_FLAG_SAME_UUID: u32 = 2;
pub const JBD2_FLAG_DELETED: u32 = 4;
pub const JBD2_FLAG_LAST_TAG: u32 = 8;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalReplayWrite {
    pub sequence: u32,
    pub target_block: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalReplayPlan {
    pub committed_transactions: u64,
    pub writes: Vec<JournalReplayWrite>,
}

/// Builds a replay plan from JBD2 log blocks. Only transactions with a matching
/// commit record are returned; revoked and deleted blocks are excluded.
/// Unsupported/truncated layouts fail closed instead of producing partial writes.
pub fn build_replay_plan(
    log_blocks: &[Vec<u8>],
    first_sequence: u32,
    filesystem_blocks: u64,
) -> Result<JournalReplayPlan, JournalError> {
    let mut plan = JournalReplayPlan::default();
    let mut cursor = 0usize;
    let mut expected_sequence = first_sequence;
    while cursor < log_blocks.len() {
        let header = parse_journal_header(&log_blocks[cursor])?;
        if header.block_type() != JBD2_DESCRIPTOR_BLOCK || header.sequence() != expected_sequence {
            cursor += 1;
            continue;
        }
        let tags = parse_descriptor_tags(&log_blocks[cursor])?;
        let data_start = cursor + 1;
        let data_count = tags
            .iter()
            .filter(|(_, flags)| flags & JBD2_FLAG_DELETED == 0)
            .count();
        if data_start + data_count > log_blocks.len() {
            return Err(JournalError::CorruptLog(
                "truncated JBD2 transaction data".into(),
            ));
        }
        let mut revoked = std::collections::HashSet::new();
        let mut trailer = data_start + data_count;
        let mut committed = false;
        while trailer < log_blocks.len() {
            let trailer_header = parse_journal_header(&log_blocks[trailer])?;
            if trailer_header.sequence() != expected_sequence {
                break;
            }
            match trailer_header.block_type() {
                JBD2_REVOKE_BLOCK => parse_revoke_block(&log_blocks[trailer], &mut revoked)?,
                JBD2_COMMIT_BLOCK => {
                    committed = true;
                    trailer += 1;
                    break;
                }
                _ => break,
            }
            trailer += 1;
        }
        if !committed {
            // An incomplete tail is intentionally ignored: it was never committed.
            break;
        }
        let mut data_index = data_start;
        for (target, flags) in tags {
            if flags & JBD2_FLAG_DELETED != 0 {
                continue;
            }
            let mut data = log_blocks[data_index].clone();
            data_index += 1;
            if target >= filesystem_blocks {
                return Err(JournalError::CorruptLog(format!(
                    "JBD2 target block {target} is out of bounds"
                )));
            }
            if flags & JBD2_FLAG_ESCAPE != 0 && data.len() >= 4 {
                data[..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
            }
            if !revoked.contains(&target) {
                plan.writes.push(JournalReplayWrite {
                    sequence: expected_sequence,
                    target_block: target,
                    data,
                });
            }
        }
        plan.committed_transactions += 1;
        expected_sequence = expected_sequence.wrapping_add(1);
        cursor = trailer;
    }
    Ok(plan)
}

fn parse_journal_header(block: &[u8]) -> Result<Jbd2Header, JournalError> {
    let (header, _) = Jbd2Header::ref_from_prefix(block)
        .map_err(|_| JournalError::CorruptLog("truncated JBD2 header".into()))?;
    if header.magic() != JBD2_MAGIC_NUMBER {
        return Err(JournalError::InvalidJournalMagic {
            expected: JBD2_MAGIC_NUMBER,
            found: header.magic(),
        });
    }
    Ok(*header)
}

fn parse_descriptor_tags(block: &[u8]) -> Result<Vec<(u64, u32)>, JournalError> {
    let mut tags = Vec::new();
    let mut offset = 12usize;
    loop {
        if offset + 8 > block.len() {
            return Err(JournalError::CorruptLog(
                "truncated JBD2 descriptor tag".into(),
            ));
        }
        let target = u32::from_be_bytes(block[offset..offset + 4].try_into().unwrap()) as u64;
        let flags = u32::from_be_bytes(block[offset + 4..offset + 8].try_into().unwrap());
        tags.push((target, flags));
        offset += 8;
        if flags & JBD2_FLAG_SAME_UUID == 0 {
            if offset + 16 > block.len() {
                return Err(JournalError::CorruptLog("truncated JBD2 tag UUID".into()));
            }
            offset += 16;
        }
        if flags & JBD2_FLAG_LAST_TAG != 0 {
            break;
        }
    }
    Ok(tags)
}

fn parse_revoke_block(
    block: &[u8],
    revoked: &mut std::collections::HashSet<u64>,
) -> Result<(), JournalError> {
    if block.len() < 16 {
        return Err(JournalError::CorruptLog(
            "truncated JBD2 revoke block".into(),
        ));
    }
    let used = u32::from_be_bytes(block[12..16].try_into().unwrap()) as usize;
    if used < 16 || used > block.len() || (used - 16) % 4 != 0 {
        return Err(JournalError::CorruptLog(
            "invalid JBD2 revoke length".into(),
        ));
    }
    for entry in block[16..used].chunks_exact(4) {
        revoked.insert(u32::from_be_bytes(entry.try_into().unwrap()) as u64);
    }
    Ok(())
}

/// A rollback entry storing pre-mutation physical block data.
#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub physical_block: u64,
    pub original_data: Vec<u8>,
}

/// Pre-image undo journal file manager.
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
            f.write_all(&UNDO_VERSION.to_le_bytes())?;
            f.write_all(&block_size.to_le_bytes())?;
            f.flush()?;
            f.sync_all()?;
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
            f.write_all(&crc32fast::hash(original_data).to_le_bytes())?;
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
            return Err(JournalError::CorruptLog(
                "Invalid undo journal magic".into(),
            ));
        }

        let mut version_buf = [0u8; 4];
        f.read_exact(&mut version_buf)?;
        let version = u32::from_le_bytes(version_buf);
        if version != UNDO_VERSION {
            return Err(JournalError::CorruptLog(format!(
                "Unsupported undo journal version {version}"
            )));
        }

        let mut block_size_buf = [0u8; 4];
        f.read_exact(&mut block_size_buf)?;
        let block_size = u32::from_le_bytes(block_size_buf) as u64;
        if block_size == 0 || !block_size.is_power_of_two() || block_size > 64 * 1024 {
            return Err(JournalError::CorruptLog(format!(
                "Invalid undo journal block size {block_size}"
            )));
        }

        let mut restored = 0;
        let mut block_buf = [0u8; 8];
        let mut len_buf = [0u8; 4];

        loop {
            match f.read(&mut block_buf[..1])? {
                0 => break,
                1 => f.read_exact(&mut block_buf[1..])?,
                _ => unreachable!(),
            }
            f.read_exact(&mut len_buf)?;
            let block = u64::from_le_bytes(block_buf);
            let len = u32::from_le_bytes(len_buf) as usize;
            if len != block_size as usize {
                return Err(JournalError::CorruptLog(format!(
                    "Entry for block {block} has length {len}, expected {block_size}"
                )));
            }

            let mut data = vec![0u8; len];
            f.read_exact(&mut data)?;
            let mut checksum_buf = [0u8; 4];
            f.read_exact(&mut checksum_buf)?;
            let expected = u32::from_le_bytes(checksum_buf);
            let actual = crc32fast::hash(&data);
            if actual != expected {
                return Err(JournalError::CorruptLog(format!(
                    "Checksum mismatch for block {block}"
                )));
            }

            dev.write_block(block, block_size, &data)?;
            // Each record is idempotent. Persist it before advancing so an
            // interrupted rollback can safely restart from the beginning.
            dev.sync_all()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: u32, sequence: u32, size: usize) -> Vec<u8> {
        let mut block = vec![0u8; size];
        block[0..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
        block[4..8].copy_from_slice(&kind.to_be_bytes());
        block[8..12].copy_from_slice(&sequence.to_be_bytes());
        block
    }

    #[test]
    fn plans_only_committed_jbd2_data() {
        let mut descriptor = header(JBD2_DESCRIPTOR_BLOCK, 7, 64);
        descriptor[12..16].copy_from_slice(&42u32.to_be_bytes());
        descriptor[16..20]
            .copy_from_slice(&(JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG).to_be_bytes());
        let data = vec![0x5a; 64];
        let commit = header(JBD2_COMMIT_BLOCK, 7, 64);
        let plan = build_replay_plan(&[descriptor, data.clone(), commit], 7, 100).unwrap();
        assert_eq!(plan.committed_transactions, 1);
        assert_eq!(
            plan.writes,
            vec![JournalReplayWrite {
                sequence: 7,
                target_block: 42,
                data
            }]
        );
    }

    #[test]
    fn ignores_uncommitted_tail() {
        let mut descriptor = header(JBD2_DESCRIPTOR_BLOCK, 3, 64);
        descriptor[12..16].copy_from_slice(&9u32.to_be_bytes());
        descriptor[16..20]
            .copy_from_slice(&(JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG).to_be_bytes());
        let plan = build_replay_plan(&[descriptor, vec![1; 64]], 3, 100).unwrap();
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn revoke_removes_committed_write() {
        let mut descriptor = header(JBD2_DESCRIPTOR_BLOCK, 4, 64);
        descriptor[12..16].copy_from_slice(&11u32.to_be_bytes());
        descriptor[16..20]
            .copy_from_slice(&(JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG).to_be_bytes());
        let mut revoke = header(JBD2_REVOKE_BLOCK, 4, 64);
        revoke[12..16].copy_from_slice(&20u32.to_be_bytes());
        revoke[16..20].copy_from_slice(&11u32.to_be_bytes());
        let commit = header(JBD2_COMMIT_BLOCK, 4, 64);
        let plan = build_replay_plan(&[descriptor, vec![2; 64], revoke, commit], 4, 100).unwrap();
        assert_eq!(plan.committed_transactions, 1);
        assert!(plan.writes.is_empty());
    }
}
