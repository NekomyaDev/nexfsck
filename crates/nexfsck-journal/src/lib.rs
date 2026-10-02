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
pub const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x1;
pub const JBD2_FEATURE_INCOMPAT_64BIT: u32 = 0x2;
pub const JBD2_FEATURE_INCOMPAT_CSUM_V2: u32 = 0x8;
pub const JBD2_FEATURE_INCOMPAT_CSUM_V3: u32 = 0x10;

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

/// Result of inspecting the real filesystem journal inode. Transaction blocks
/// are validated read-only; this type never implies that replay is supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalTransactionState {
    Clean,
    CommittedTransactions,
    IncompleteTail,
}

#[derive(Debug, Clone, Copy)]
pub struct JournalInspection {
    pub superblock: Jbd2Superblock,
    pub state: JournalTransactionState,
    pub transaction_blocks_checked: u64,
    pub committed_transactions: u64,
}

/// Descriptor/revoke encoding selected from the JBD2 superblock. Unknown
/// incompatibility bits must be rejected by the caller rather than guessed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalFeatures {
    pub block_64bit: bool,
    pub checksum_v3: bool,
    pub uuid: [u8; 16],
}

impl JournalFeatures {
    pub fn from_superblock_bytes(block: &[u8]) -> Result<Self, JournalError> {
        if block.len() < 64 {
            return Err(JournalError::CorruptLog(
                "truncated JBD2 superblock features".into(),
            ));
        }
        let incompat = u32::from_be_bytes(block[40..44].try_into().unwrap());
        let supported = JBD2_FEATURE_INCOMPAT_REVOKE
            | JBD2_FEATURE_INCOMPAT_64BIT
            | JBD2_FEATURE_INCOMPAT_CSUM_V3;
        if incompat & !supported != 0 {
            return Err(JournalError::CorruptLog(format!(
                "unsupported JBD2 incompatibility flags 0x{:x}",
                incompat & !supported
            )));
        }
        let mut uuid = [0; 16];
        uuid.copy_from_slice(&block[48..64]);
        Ok(Self {
            block_64bit: incompat & JBD2_FEATURE_INCOMPAT_64BIT != 0,
            checksum_v3: incompat & JBD2_FEATURE_INCOMPAT_CSUM_V3 != 0,
            uuid,
        })
    }
}

fn validate_inspected_journal_features(block: &[u8]) -> Result<JournalFeatures, JournalError> {
    let features = JournalFeatures::from_superblock_bytes(block)?;
    let compat = u32::from_be_bytes(
        block
            .get(36..40)
            .ok_or_else(|| JournalError::CorruptLog("truncated JBD2 compat flags".into()))?
            .try_into()
            .unwrap(),
    );
    if compat & 0x1 != 0 {
        return Err(JournalError::CorruptLog(
            "JBD2 compat checksum metadata is not fully validated".into(),
        ));
    }
    let ro_compat = u32::from_be_bytes(
        block
            .get(44..48)
            .ok_or_else(|| JournalError::CorruptLog("truncated JBD2 ro_compat flags".into()))?
            .try_into()
            .unwrap(),
    );
    if ro_compat != 0 {
        return Err(JournalError::CorruptLog(format!(
            "unsupported JBD2 read-only-compatible flags 0x{ro_compat:x}"
        )));
    }
    Ok(features)
}

/// Validate the on-disk JBD2 superblock before using its clean/dirty fields.
/// Linux defines the checksum over the 1024-byte journal_superblock_s, with
/// s_checksum at 0xfc zeroed, using the kernel CRC32c convention.
fn validate_journal_superblock_integrity(
    block: &[u8],
    expected_block_size: u64,
) -> Result<JournalFeatures, JournalError> {
    const JBD2_SUPERBLOCK_V1: u32 = 3;
    const JBD2_SUPERBLOCK_V2: u32 = 4;
    const JBD2_SUPERBLOCK_BYTES: usize = 1024;
    const CHECKSUM_TYPE_OFFSET: usize = 0x50;
    const CHECKSUM_OFFSET: usize = 0xfc;
    if block.len() < JBD2_SUPERBLOCK_BYTES {
        return Err(JournalError::CorruptLog(
            "truncated JBD2 superblock (requires 1024 bytes)".into(),
        ));
    }
    let header = parse_journal_header(block)?;
    if header.magic() != JBD2_MAGIC_NUMBER {
        return Err(JournalError::InvalidJournalMagic {
            expected: JBD2_MAGIC_NUMBER,
            found: header.magic(),
        });
    }
    let block_type = header.block_type();
    if block_type != JBD2_SUPERBLOCK_V1 && block_type != JBD2_SUPERBLOCK_V2 {
        return Err(JournalError::CorruptLog(format!(
            "unexpected JBD2 superblock type {block_type}"
        )));
    }
    let block_size = u32::from_be_bytes(block[12..16].try_into().unwrap()) as u64;
    let maxlen = u32::from_be_bytes(block[16..20].try_into().unwrap());
    let first = u32::from_be_bytes(block[20..24].try_into().unwrap());
    let start = u32::from_be_bytes(block[28..32].try_into().unwrap());
    if block_size != expected_block_size
        || block_size == 0
        || !block_size.is_power_of_two()
        || first == 0
        || first >= maxlen
        || (start != 0 && (start < first || start >= maxlen))
    {
        return Err(JournalError::CorruptLog(format!(
            "invalid JBD2 superblock geometry/state: block_size={block_size}, maxlen={maxlen}, first={first}, start={start}"
        )));
    }
    let features = validate_inspected_journal_features(block)?;
    let incompat = u32::from_be_bytes(block[40..44].try_into().unwrap());
    if incompat & JBD2_FEATURE_INCOMPAT_CSUM_V2 != 0 {
        return Err(JournalError::CorruptLog(
            "JBD2 checksum-v2 transaction format is not supported".into(),
        ));
    }
    if incompat & JBD2_FEATURE_INCOMPAT_CSUM_V3 != 0 {
        if block_type != JBD2_SUPERBLOCK_V2 || block[CHECKSUM_TYPE_OFFSET] != 4 {
            return Err(JournalError::CorruptLog(
                "JBD2 checksum-v3 requires a v2 superblock and CRC32c".into(),
            ));
        }
        let provided = u32::from_be_bytes(
            block[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        let mut bytes = block[..JBD2_SUPERBLOCK_BYTES].to_vec();
        bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].fill(0);
        let calculated = crc32c_kernel(!0, &bytes);
        if provided != calculated {
            return Err(JournalError::CorruptLog(
                "JBD2 superblock checksum mismatch".into(),
            ));
        }
    }
    Ok(features)
}

/// Builds a replay plan from JBD2 log blocks. Only transactions with a matching
/// commit record are returned; revoked and deleted blocks are excluded.
/// Unsupported/truncated layouts fail closed instead of producing partial writes.
pub fn build_replay_plan(
    log_blocks: &[Vec<u8>],
    first_sequence: u32,
    filesystem_blocks: u64,
) -> Result<JournalReplayPlan, JournalError> {
    build_replay_plan_with_features(
        log_blocks,
        first_sequence,
        filesystem_blocks,
        JournalFeatures::default(),
    )
}

pub fn build_replay_plan_with_features(
    log_blocks: &[Vec<u8>],
    first_sequence: u32,
    filesystem_blocks: u64,
    features: JournalFeatures,
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
        if features.checksum_v3 {
            validate_jbd2_v3_block_checksum(&log_blocks[cursor], features.uuid, false)?;
        }
        let tags = parse_descriptor_tags(&log_blocks[cursor], features)?;
        let data_start = cursor + 1;
        let data_count = tags
            .iter()
            .filter(|(_, flags, _)| flags & JBD2_FLAG_DELETED == 0)
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
                JBD2_REVOKE_BLOCK => {
                    if features.checksum_v3 {
                        validate_jbd2_v3_block_checksum(
                            &log_blocks[trailer],
                            features.uuid,
                            false,
                        )?;
                    }
                    parse_revoke_block(
                        &log_blocks[trailer],
                        &mut revoked,
                        features.block_64bit,
                        filesystem_blocks,
                        features.checksum_v3,
                    )?
                }
                JBD2_COMMIT_BLOCK => {
                    if features.checksum_v3 {
                        validate_jbd2_v3_block_checksum(&log_blocks[trailer], features.uuid, true)?;
                    }
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
        for (target, flags, expected_checksum) in tags {
            if flags & JBD2_FLAG_DELETED != 0 {
                continue;
            }
            let mut data = log_blocks[data_index].clone();
            data_index += 1;
            if let Some(expected) = expected_checksum {
                let actual = jbd2_tag_checksum(features.uuid, expected_sequence, &data);
                if actual != expected {
                    return Err(JournalError::CorruptLog(format!(
                        "JBD2 checksum-v3 mismatch for target block {target}"
                    )));
                }
            }
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

fn parse_descriptor_tags(
    block: &[u8],
    features: JournalFeatures,
) -> Result<Vec<(u64, u32, Option<u32>)>, JournalError> {
    let mut tags = Vec::new();
    let mut offset = 12usize;
    let limit = block.len() - usize::from(features.checksum_v3) * 4;
    loop {
        if offset + 8 > limit {
            return Err(JournalError::CorruptLog(
                "truncated JBD2 descriptor tag".into(),
            ));
        }
        let mut target = u32::from_be_bytes(block[offset..offset + 4].try_into().unwrap()) as u64;
        let (flags, checksum) = if features.checksum_v3 {
            let flags = u32::from_be_bytes(block[offset + 4..offset + 8].try_into().unwrap());
            offset += 8;
            if offset + 8 > limit {
                return Err(JournalError::CorruptLog("truncated JBD2 v3 tag".into()));
            }
            if features.block_64bit {
                target |= (u32::from_be_bytes(block[offset..offset + 4].try_into().unwrap())
                    as u64)
                    << 32;
            } else if block[offset..offset + 4] != [0; 4] {
                return Err(JournalError::CorruptLog(
                    "non-zero high block number without JBD2 64-bit feature".into(),
                ));
            }
            let checksum = u32::from_be_bytes(block[offset + 4..offset + 8].try_into().unwrap());
            offset += 8;
            (flags, Some(checksum))
        } else {
            let _checksum = u16::from_be_bytes(block[offset + 4..offset + 6].try_into().unwrap());
            let flags =
                u16::from_be_bytes(block[offset + 6..offset + 8].try_into().unwrap()) as u32;
            offset += 8;
            if features.block_64bit {
                if offset + 4 > block.len() {
                    return Err(JournalError::CorruptLog("truncated JBD2 64-bit tag".into()));
                }
                target |= (u32::from_be_bytes(block[offset..offset + 4].try_into().unwrap())
                    as u64)
                    << 32;
                offset += 4;
            }
            (flags, None)
        };
        const KNOWN_FLAGS: u32 =
            JBD2_FLAG_ESCAPE | JBD2_FLAG_SAME_UUID | JBD2_FLAG_DELETED | JBD2_FLAG_LAST_TAG;
        if flags & !KNOWN_FLAGS != 0 {
            return Err(JournalError::CorruptLog(format!(
                "unsupported JBD2 descriptor tag flags 0x{:x}",
                flags & !KNOWN_FLAGS
            )));
        }
        tags.push((target, flags, checksum));
        if flags & JBD2_FLAG_SAME_UUID == 0 {
            if offset + 16 > limit {
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

fn validate_transaction_stream(
    blocks: &[Vec<u8>],
    first_sequence: u32,
    filesystem_blocks: u64,
    features: JournalFeatures,
) -> Result<(u64, JournalTransactionState, u64), JournalError> {
    let mut cursor = 0usize;
    let mut expected_sequence = first_sequence;
    let mut committed = 0u64;
    let mut incomplete = false;

    while cursor < blocks.len() {
        let first_header = match parse_journal_header(&blocks[cursor]) {
            Ok(header) => header,
            Err(_) if committed != 0 => break,
            Err(error) => return Err(error),
        };
        if first_header.sequence() != expected_sequence {
            if committed == 0 {
                incomplete = true;
            }
            break;
        }

        let sequence = expected_sequence;
        let mut saw_descriptor = false;
        let mut saw_revoke = false;
        let mut transaction_complete = false;
        let mut revoked = std::collections::HashSet::new();

        while cursor < blocks.len() {
            let header = match parse_journal_header(&blocks[cursor]) {
                Ok(header) => header,
                Err(_) => {
                    incomplete = true;
                    break;
                }
            };
            if header.sequence() != sequence {
                // The active log ended before this transaction committed.
                incomplete = true;
                break;
            }
            match header.block_type() {
                JBD2_DESCRIPTOR_BLOCK => {
                    saw_descriptor = true;
                    if features.checksum_v3 {
                        validate_jbd2_v3_block_checksum(&blocks[cursor], features.uuid, false)?;
                    }
                    let tags = parse_descriptor_tags(&blocks[cursor], features)?;
                    cursor += 1;
                    for (target, flags, expected_checksum) in tags {
                        if target >= filesystem_blocks {
                            return Err(JournalError::CorruptLog(format!(
                                "JBD2 target block {target} is out of bounds"
                            )));
                        }
                        if flags & JBD2_FLAG_DELETED != 0 {
                            continue;
                        }
                        let data = blocks.get(cursor).ok_or_else(|| {
                            JournalError::CorruptLog(
                                "JBD2 descriptor references data beyond the active log".into(),
                            )
                        })?;
                        if let Some(expected) = expected_checksum {
                            let actual = jbd2_tag_checksum(features.uuid, sequence, data);
                            if actual != expected {
                                return Err(JournalError::CorruptLog(format!(
                                    "JBD2 checksum-v3 mismatch for target block {target}"
                                )));
                            }
                        }
                        cursor += 1;
                    }
                }
                JBD2_REVOKE_BLOCK => {
                    saw_revoke = true;
                    if features.checksum_v3 {
                        validate_jbd2_v3_block_checksum(&blocks[cursor], features.uuid, false)?;
                    }
                    parse_revoke_block(
                        &blocks[cursor],
                        &mut revoked,
                        features.block_64bit,
                        filesystem_blocks,
                        features.checksum_v3,
                    )?;
                    cursor += 1;
                }
                JBD2_COMMIT_BLOCK => {
                    if !saw_descriptor && !saw_revoke {
                        return Err(JournalError::CorruptLog(
                            "JBD2 commit has no preceding descriptor or revoke block".into(),
                        ));
                    }
                    if features.checksum_v3 {
                        validate_jbd2_v3_block_checksum(&blocks[cursor], features.uuid, true)?;
                    }
                    cursor += 1;
                    transaction_complete = true;
                    break;
                }
                other => {
                    if saw_descriptor || saw_revoke {
                        incomplete = true;
                    }
                    // A non-transaction block terminates the active log, as
                    // in JBD2 recovery scan. It is not treated as a clean log.
                    let _ = other;
                    cursor = blocks.len();
                    break;
                }
            }
        }

        if !transaction_complete {
            incomplete = true;
            break;
        }
        committed += 1;
        expected_sequence = expected_sequence.wrapping_add(1);
    }

    let state = if incomplete || committed == 0 {
        JournalTransactionState::IncompleteTail
    } else {
        JournalTransactionState::CommittedTransactions
    };
    Ok((committed, state, cursor as u64))
}

fn parse_revoke_block(
    block: &[u8],
    revoked: &mut std::collections::HashSet<u64>,
    block_64bit: bool,
    filesystem_blocks: u64,
    has_checksum_tail: bool,
) -> Result<(), JournalError> {
    if block.len() < 16 {
        return Err(JournalError::CorruptLog(
            "truncated JBD2 revoke block".into(),
        ));
    }
    let used = u32::from_be_bytes(block[12..16].try_into().unwrap()) as usize;
    let entry_size = if block_64bit { 8 } else { 4 };
    let limit = block.len() - usize::from(has_checksum_tail) * 4;
    if used < 16 || used > limit || (used - 16) & (entry_size - 1) != 0 {
        return Err(JournalError::CorruptLog(
            "invalid JBD2 revoke length".into(),
        ));
    }
    for entry in block[16..used].chunks_exact(entry_size) {
        let value = if block_64bit {
            u64::from_be_bytes(entry.try_into().unwrap())
        } else {
            u32::from_be_bytes(entry.try_into().unwrap()) as u64
        };
        if value >= filesystem_blocks {
            return Err(JournalError::CorruptLog(format!(
                "JBD2 revoke target block {value} is out of bounds"
            )));
        }
        revoked.insert(value);
    }
    Ok(())
}

/// Verify a JBD2 v2/v3 metadata-block checksum. Descriptor and revoke blocks
/// store the CRC32c in the final word; commit blocks store it at offset 16.
/// Linux seeds these checksums with CRC32c(~0, journal UUID).
fn validate_jbd2_v3_block_checksum(
    block: &[u8],
    uuid: [u8; 16],
    is_commit: bool,
) -> Result<(), JournalError> {
    let mut bytes = block.to_vec();
    let checksum_offset = if is_commit {
        if block.len() < 20 || block[12..16] != [0; 4] {
            return Err(JournalError::CorruptLog(
                "truncated or unsupported JBD2 v2/v3 commit header".into(),
            ));
        }
        16
    } else {
        if block.len() < 16 {
            return Err(JournalError::CorruptLog(
                "truncated JBD2 checksum block".into(),
            ));
        }
        block.len() - 4
    };
    let provided = u32::from_be_bytes(
        block[checksum_offset..checksum_offset + 4]
            .try_into()
            .unwrap(),
    );
    bytes[checksum_offset..checksum_offset + 4].fill(0);
    let seed = crc32c_kernel(!0, &uuid);
    let calculated = crc32c_kernel(seed, &bytes);
    if provided != calculated {
        return Err(JournalError::CorruptLog(if is_commit {
            "JBD2 commit checksum mismatch".into()
        } else {
            "JBD2 descriptor/revoke checksum mismatch".into()
        }));
    }
    Ok(())
}

// Linux's JBD2 v3 tag checksum: crc32c(~0, uuid), then the big-endian
// transaction id, then the complete journal data block. Kernel CRC helpers do
// not apply the conventional final XOR.
fn jbd2_tag_checksum(uuid: [u8; 16], sequence: u32, data: &[u8]) -> u32 {
    let crc = crc32c_kernel(!0, &uuid);
    let crc = crc32c_kernel(crc, &sequence.to_be_bytes());
    crc32c_kernel(crc, data)
}

fn crc32c_kernel(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f63b78 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    crc
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

/// Applies a validated replay plan. Every target pre-image is durably appended
/// to `undo` before its corresponding filesystem write, and each write is
/// synced before the next one begins.
pub fn apply_replay_plan(
    plan: &JournalReplayPlan,
    dev: &BlockDevice,
    undo: &mut AtomicUndoJournal,
    block_size: u64,
) -> Result<usize, JournalError> {
    apply_replay_plan_with_limit(plan, dev, undo, block_size, None)
}

fn apply_replay_plan_with_limit(
    plan: &JournalReplayPlan,
    dev: &BlockDevice,
    undo: &mut AtomicUndoJournal,
    block_size: u64,
    stop_after: Option<usize>,
) -> Result<usize, JournalError> {
    if block_size == 0 || !block_size.is_power_of_two() || block_size > 64 * 1024 {
        return Err(JournalError::CorruptLog(format!(
            "invalid replay block size {block_size}"
        )));
    }
    let mut applied = 0;
    for write in &plan.writes {
        if stop_after == Some(applied) {
            return Err(JournalError::StdIo(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "injected replay interruption",
            )));
        }
        if write.data.len() != block_size as usize {
            return Err(JournalError::CorruptLog(format!(
                "replay data for block {} has length {}, expected {block_size}",
                write.target_block,
                write.data.len()
            )));
        }
        let original = dev.read_block(write.target_block, block_size)?;
        undo.record_mutation(write.target_block, &original)?;
        dev.write_block(write.target_block, block_size, &write.data)?;
        dev.sync_all()?;
        applied += 1;
    }
    dev.flush_kernel_buffers()?;
    Ok(applied)
}

/// Inspects the JBD2 journal from the filesystem if present.
pub fn inspect_journal(
    dev: &BlockDevice,
    sb: &Ext4Superblock,
    descriptors: &[Ext4GroupDesc],
    is_64bit: bool,
) -> Result<Option<JournalInspection>, JournalError> {
    let journal_inum = u32::from_le(sb.s_journal_inum);
    if journal_inum == 0 {
        return Ok(None);
    }
    if descriptors.is_empty() {
        return Err(JournalError::CorruptLog(
            "journal inode is present but the filesystem has no group descriptors".into(),
        ));
    }

    let inodes_per_group = sb.inodes_per_group() as usize;
    if inodes_per_group == 0 {
        return Err(JournalError::CorruptLog(
            "filesystem has zero inodes per group".into(),
        ));
    }
    let journal_index = journal_inum.saturating_sub(1) as usize;
    let group_index = journal_index / inodes_per_group;
    let desc0 = descriptors.get(group_index).ok_or_else(|| {
        JournalError::CorruptLog("journal inode is outside the group descriptor table".into())
    })?;
    let inode_size = sb.inode_size() as usize;
    let idx = journal_index % inodes_per_group;
    if inode_size < 64 {
        return Err(JournalError::CorruptLog(
            "journal inode has a truncated on-disk layout".into(),
        ));
    }
    let inode_table = dev.read_inode_table(sb, desc0, is_64bit)?;
    let inode_start = idx
        .checked_mul(inode_size)
        .ok_or_else(|| JournalError::CorruptLog("journal inode offset overflow".into()))?;
    let raw_inode = inode_table
        .get(inode_start..inode_start + inode_size)
        .ok_or_else(|| JournalError::CorruptLog("journal inode bytes are truncated".into()))?;
    let mode = u16::from_le_bytes(raw_inode[0..2].try_into().unwrap());
    let flags = u32::from_le_bytes(raw_inode[32..36].try_into().unwrap());
    if mode == 0 || flags & nexfsck_core::EXT4_EXTENTS_FL == 0 {
        return Err(JournalError::CorruptLog(
            "journal inode is unused or does not use extent mapping".into(),
        ));
    }

    // JBD2 logical block zero is mapped by the first leaf extent. This compact
    // path intentionally rejects deeper inode roots until their child path is
    // independently validated; interpreting an index as a leaf is unsafe.
    let root = &raw_inode[40..100];
    let header = nexfsck_core::Ext4ExtentHeader::ref_from_prefix(root)
        .map_err(|_| JournalError::CorruptLog("truncated journal extent root".into()))?
        .0;
    if !header.is_valid_magic()
        || header.depth() != 0
        || header.entries() == 0
        || header.entries() > header.max()
        || header.max() > 4
    {
        return Err(JournalError::CorruptLog(
            "journal extent root is malformed or requires unsupported traversal".into(),
        ));
    }
    let mut journal_map = Vec::new();
    let entries = header.entries() as usize;
    for entry in 0..entries {
        let start = 12 + entry * 12;
        let ext = Ext4Extent::ref_from_prefix(&root[start..])
            .map_err(|_| JournalError::CorruptLog("truncated journal extent".into()))?
            .0;
        if ext.block_count() == 0 || ext.is_unwritten() {
            return Err(JournalError::CorruptLog(
                "journal extent has a zero or unwritten range".into(),
            ));
        }
        if entry == 0 && ext.logical_block() != 0 {
            return Err(JournalError::CorruptLog(
                "journal extent map does not begin at logical block zero".into(),
            ));
        }
        let logical_end = ext
            .logical_block()
            .checked_add(ext.block_count())
            .ok_or_else(|| JournalError::CorruptLog("journal logical extent overflow".into()))?;
        if logical_end as usize != journal_map.len() + ext.block_count() as usize {
            return Err(JournalError::CorruptLog(
                "journal extents are unordered or contain a logical gap".into(),
            ));
        }
        let physical_start = ext.physical_start();
        let physical_end = physical_start
            .checked_add(ext.block_count() as u64)
            .ok_or_else(|| JournalError::CorruptLog("journal physical extent overflow".into()))?;
        if physical_end > sb.total_blocks() {
            return Err(JournalError::CorruptLog(
                "journal extent points outside the filesystem".into(),
            ));
        }
        journal_map.extend(physical_start..physical_end);
    }
    let block_bytes = dev.read_block(journal_map[0], sb.block_size())?;
    let (jbd_sb, _) = Jbd2Superblock::ref_from_prefix(&block_bytes)
        .map_err(|_| JournalError::CorruptLog("truncated JBD2 superblock".into()))?;
    jbd_sb.verify_magic()?;
    let features = validate_journal_superblock_integrity(&block_bytes, sb.block_size())?;
    let maxlen = u32::from_be(jbd_sb.s_maxlen) as usize;
    let first = u32::from_be(jbd_sb.s_first) as usize;
    let start = u32::from_be(jbd_sb.s_start) as usize;
    if maxlen > journal_map.len() {
        return Err(JournalError::CorruptLog(format!(
            "journal inode maps {} blocks but superblock declares {maxlen}",
            journal_map.len()
        )));
    }
    if start == 0 {
        return Ok(Some(JournalInspection {
            superblock: *jbd_sb,
            state: JournalTransactionState::Clean,
            transaction_blocks_checked: 0,
            committed_transactions: 0,
        }));
    }

    // Hard memory bound: the parser currently accepts an in-memory block
    // stream. A larger active ring is rejected, never partially scanned.
    const MAX_ACTIVE_JOURNAL_BYTES: usize = 64 * 1024 * 1024;
    // Read at most one complete ring, beginning at s_start. A larger count
    // would revisit s_start and could make a malformed log look cyclic.
    let active_blocks = maxlen - first;
    let block_size = sb.block_size() as usize;
    if active_blocks
        .checked_mul(block_size)
        .is_none_or(|bytes| bytes > MAX_ACTIVE_JOURNAL_BYTES)
    {
        return Err(JournalError::CorruptLog(
            "active journal range exceeds the bounded verifier limit".into(),
        ));
    }
    let mut blocks = Vec::with_capacity(active_blocks);
    let mut logical = start;
    for _ in 0..active_blocks {
        if logical >= maxlen {
            logical = first;
        }
        let physical = *journal_map
            .get(logical)
            .ok_or_else(|| JournalError::CorruptLog("active journal block is not mapped".into()))?;
        blocks.push(dev.read_block(physical, sb.block_size())?);
        logical += 1;
    }
    let sequence = u32::from_be(jbd_sb.s_sequence);
    let (committed_transactions, state, transaction_blocks_checked) =
        validate_transaction_stream(&blocks, sequence, sb.total_blocks(), features)?;
    Ok(Some(JournalInspection {
        superblock: *jbd_sb,
        state,
        transaction_blocks_checked,
        committed_transactions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};

    fn header(kind: u32, sequence: u32, size: usize) -> Vec<u8> {
        let mut block = vec![0u8; size];
        block[0..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
        block[4..8].copy_from_slice(&kind.to_be_bytes());
        block[8..12].copy_from_slice(&sequence.to_be_bytes());
        block
    }

    fn seal_v3_metadata(block: &mut [u8], uuid: [u8; 16], is_commit: bool) {
        let offset = if is_commit { 16 } else { block.len() - 4 };
        block[offset..offset + 4].fill(0);
        if is_commit {
            block[12..16].fill(0);
        }
        let seed = crc32c_kernel(!0, &uuid);
        let checksum = crc32c_kernel(seed, block);
        block[offset..offset + 4].copy_from_slice(&checksum.to_be_bytes());
    }

    #[test]
    fn journal_feature_policy_rejects_unknown_and_unvalidated_checksum_modes() {
        let mut superblock = vec![0u8; 64];
        superblock[40..44].copy_from_slice(&JBD2_FEATURE_INCOMPAT_CSUM_V3.to_be_bytes());
        assert!(validate_inspected_journal_features(&superblock).is_ok());

        superblock[40..44].copy_from_slice(&0x8000_0000u32.to_be_bytes());
        assert!(validate_inspected_journal_features(&superblock).is_err());

        superblock[40..44].copy_from_slice(&0u32.to_be_bytes());
        superblock[36..40].copy_from_slice(&1u32.to_be_bytes());
        assert!(validate_inspected_journal_features(&superblock).is_err());

        superblock[36..40].copy_from_slice(&0u32.to_be_bytes());
        superblock[40..44].copy_from_slice(&JBD2_FEATURE_INCOMPAT_CSUM_V2.to_be_bytes());
        assert!(validate_inspected_journal_features(&superblock).is_err());
        superblock[40..44].copy_from_slice(&0u32.to_be_bytes());
        superblock[44..48].copy_from_slice(&1u32.to_be_bytes());
        assert!(validate_inspected_journal_features(&superblock).is_err());
    }

    #[test]
    fn validates_jbd2_v3_superblock_crc_and_geometry() {
        let mut block = vec![0u8; 4096];
        block[0..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
        block[4..8].copy_from_slice(&4u32.to_be_bytes());
        block[8..12].copy_from_slice(&1u32.to_be_bytes());
        block[12..16].copy_from_slice(&4096u32.to_be_bytes());
        block[16..20].copy_from_slice(&1024u32.to_be_bytes());
        block[20..24].copy_from_slice(&1u32.to_be_bytes());
        block[40..44].copy_from_slice(&JBD2_FEATURE_INCOMPAT_CSUM_V3.to_be_bytes());
        block[80] = 4;
        let checksum = crc32c_kernel(!0, &block[..1024]);
        block[0xfc..0x100].copy_from_slice(&checksum.to_be_bytes());

        assert!(validate_journal_superblock_integrity(&block, 4096).is_ok());
        let mut corrupt = block.clone();
        corrupt[48] ^= 1;
        assert!(validate_journal_superblock_integrity(&corrupt, 4096).is_err());
        assert!(validate_journal_superblock_integrity(&block, 2048).is_err());
    }

    #[test]
    fn plans_only_committed_jbd2_data() {
        let mut descriptor = header(JBD2_DESCRIPTOR_BLOCK, 7, 64);
        descriptor[12..16].copy_from_slice(&42u32.to_be_bytes());
        descriptor[18..20]
            .copy_from_slice(&((JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG) as u16).to_be_bytes());
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
    fn validates_real_transaction_stream_and_reports_incomplete_tail() {
        let mut descriptor = header(JBD2_DESCRIPTOR_BLOCK, 7, 64);
        descriptor[12..16].copy_from_slice(&42u32.to_be_bytes());
        descriptor[18..20]
            .copy_from_slice(&((JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG) as u16).to_be_bytes());
        let data = vec![0x5a; 64];
        let commit = header(JBD2_COMMIT_BLOCK, 7, 64);
        let features = JournalFeatures::default();
        let (committed, state, checked) = validate_transaction_stream(
            &[descriptor.clone(), data.clone(), commit],
            7,
            100,
            features,
        )
        .unwrap();
        assert_eq!(committed, 1);
        assert_eq!(state, JournalTransactionState::CommittedTransactions);
        assert_eq!(checked, 3);

        let (committed, state, checked) =
            validate_transaction_stream(&[descriptor, data], 7, 100, features).unwrap();
        assert_eq!(committed, 0);
        assert_eq!(state, JournalTransactionState::IncompleteTail);
        assert_eq!(checked, 2);
    }

    #[test]
    fn rejects_real_transaction_target_outside_filesystem() {
        let mut descriptor = header(JBD2_DESCRIPTOR_BLOCK, 7, 64);
        descriptor[12..16].copy_from_slice(&100u32.to_be_bytes());
        descriptor[18..20]
            .copy_from_slice(&((JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG) as u16).to_be_bytes());
        let data = vec![0; 64];
        let commit = header(JBD2_COMMIT_BLOCK, 7, 64);
        assert!(validate_transaction_stream(
            &[descriptor, data, commit],
            7,
            100,
            JournalFeatures::default()
        )
        .is_err());
    }

    #[test]
    fn validates_multiple_descriptors_and_revoke_only_transaction_metadata() {
        let mut descriptor_a = header(JBD2_DESCRIPTOR_BLOCK, 12, 64);
        descriptor_a[12..16].copy_from_slice(&9u32.to_be_bytes());
        descriptor_a[18..20]
            .copy_from_slice(&((JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG) as u16).to_be_bytes());
        let data_a = vec![0x11; 64];
        let mut descriptor_b = header(JBD2_DESCRIPTOR_BLOCK, 12, 64);
        descriptor_b[12..16].copy_from_slice(&10u32.to_be_bytes());
        descriptor_b[18..20]
            .copy_from_slice(&((JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG) as u16).to_be_bytes());
        let data_b = vec![0x22; 64];
        let mut revoke = header(JBD2_REVOKE_BLOCK, 12, 64);
        revoke[12..16].copy_from_slice(&16u32.to_be_bytes());
        let commit = header(JBD2_COMMIT_BLOCK, 12, 64);

        let (committed, state, checked) = validate_transaction_stream(
            &[descriptor_a, data_a, descriptor_b, data_b, revoke, commit],
            12,
            100,
            JournalFeatures::default(),
        )
        .unwrap();
        assert_eq!(committed, 1);
        assert_eq!(state, JournalTransactionState::CommittedTransactions);
        assert_eq!(checked, 6);
    }

    #[test]
    fn ignores_uncommitted_tail() {
        let mut descriptor = header(JBD2_DESCRIPTOR_BLOCK, 3, 64);
        descriptor[12..16].copy_from_slice(&9u32.to_be_bytes());
        descriptor[18..20]
            .copy_from_slice(&((JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG) as u16).to_be_bytes());
        let plan = build_replay_plan(&[descriptor, vec![1; 64]], 3, 100).unwrap();
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn revoke_removes_committed_write() {
        let mut descriptor = header(JBD2_DESCRIPTOR_BLOCK, 4, 64);
        descriptor[12..16].copy_from_slice(&11u32.to_be_bytes());
        descriptor[18..20]
            .copy_from_slice(&((JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG) as u16).to_be_bytes());
        let mut revoke = header(JBD2_REVOKE_BLOCK, 4, 64);
        revoke[12..16].copy_from_slice(&20u32.to_be_bytes());
        revoke[16..20].copy_from_slice(&11u32.to_be_bytes());
        let commit = header(JBD2_COMMIT_BLOCK, 4, 64);
        let plan = build_replay_plan(&[descriptor, vec![2; 64], revoke, commit], 4, 100).unwrap();
        assert_eq!(plan.committed_transactions, 1);
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn validates_checksum_v3_and_64bit_target() {
        let features = JournalFeatures {
            block_64bit: true,
            checksum_v3: true,
            uuid: *b"0123456789abcdef",
        };
        let sequence = 19;
        let data = vec![0xa5; 64];
        let target = (1u64 << 32) | 7;
        let mut descriptor = header(JBD2_DESCRIPTOR_BLOCK, sequence, 64);
        descriptor[12..16].copy_from_slice(&(target as u32).to_be_bytes());
        descriptor[16..20]
            .copy_from_slice(&(JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG).to_be_bytes());
        descriptor[20..24].copy_from_slice(&((target >> 32) as u32).to_be_bytes());
        descriptor[24..28]
            .copy_from_slice(&jbd2_tag_checksum(features.uuid, sequence, &data).to_be_bytes());
        seal_v3_metadata(&mut descriptor, features.uuid, false);
        let mut commit = header(JBD2_COMMIT_BLOCK, sequence, 64);
        seal_v3_metadata(&mut commit, features.uuid, true);
        let mut unsupported_commit_header = commit.clone();
        unsupported_commit_header[12] = 4;
        assert!(
            validate_jbd2_v3_block_checksum(&unsupported_commit_header, features.uuid, true)
                .is_err()
        );
        let plan = build_replay_plan_with_features(
            &[descriptor.clone(), data.clone(), commit.clone()],
            sequence,
            target + 1,
            features,
        )
        .unwrap();
        assert_eq!(plan.writes[0].target_block, target);

        let mut damaged = data;
        damaged[31] ^= 1;
        assert!(build_replay_plan_with_features(
            &[descriptor, damaged, commit],
            sequence,
            target + 1,
            features,
        )
        .is_err());
    }

    #[test]
    fn verifies_v3_descriptor_commit_and_revoke_checksums() {
        let features = JournalFeatures {
            block_64bit: false,
            checksum_v3: true,
            uuid: *b"journal-uuid-001",
        };
        let sequence = 27;
        let mut descriptor = header(JBD2_DESCRIPTOR_BLOCK, sequence, 64);
        descriptor[12..16].copy_from_slice(&42u32.to_be_bytes());
        descriptor[16..20]
            .copy_from_slice(&(JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG).to_be_bytes());
        let data = vec![0x6b; 64];
        descriptor[24..28]
            .copy_from_slice(&jbd2_tag_checksum(features.uuid, sequence, &data).to_be_bytes());
        seal_v3_metadata(&mut descriptor, features.uuid, false);

        let mut revoke = header(JBD2_REVOKE_BLOCK, sequence, 64);
        revoke[12..16].copy_from_slice(&20u32.to_be_bytes());
        revoke[16..20].copy_from_slice(&42u32.to_be_bytes());
        seal_v3_metadata(&mut revoke, features.uuid, false);

        let mut commit = header(JBD2_COMMIT_BLOCK, sequence, 64);
        seal_v3_metadata(&mut commit, features.uuid, true);
        let blocks = vec![
            descriptor.clone(),
            data.clone(),
            revoke.clone(),
            commit.clone(),
        ];
        let plan = build_replay_plan_with_features(&blocks, sequence, 100, features).unwrap();
        assert_eq!(plan.committed_transactions, 1);
        assert!(plan.writes.is_empty());

        for mutate in [0usize, 2, 3] {
            let mut corrupt = blocks.clone();
            let last = corrupt[mutate].len() - 1;
            corrupt[mutate][last] ^= 1;
            assert!(build_replay_plan_with_features(&corrupt, sequence, 100, features).is_err());
        }
    }

    #[test]
    fn interrupted_replay_can_be_rolled_back_from_synced_preimages() {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        );
        let image = std::env::temp_dir().join(format!("nexfsck-replay-{unique}.img"));
        let undo_path = std::env::temp_dir().join(format!("nexfsck-replay-{unique}.undo"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&image)
            .unwrap();
        file.set_len(4 * 4096).unwrap();
        drop(file);
        let dev = BlockDevice::open(&image, false).unwrap();
        let original = vec![0u8; 4096];
        let plan = JournalReplayPlan {
            committed_transactions: 1,
            writes: vec![
                JournalReplayWrite {
                    sequence: 1,
                    target_block: 1,
                    data: vec![1; 4096],
                },
                JournalReplayWrite {
                    sequence: 1,
                    target_block: 2,
                    data: vec![2; 4096],
                },
            ],
        };
        let mut undo = AtomicUndoJournal::new(Some(&undo_path), 4096).unwrap();
        assert!(apply_replay_plan_with_limit(&plan, &dev, &mut undo, 4096, Some(1)).is_err());
        drop(undo);
        assert_eq!(dev.read_block(1, 4096).unwrap(), vec![1; 4096]);
        assert_eq!(dev.read_block(2, 4096).unwrap(), original);
        assert_eq!(
            AtomicUndoJournal::rollback_from_file(&undo_path, &dev).unwrap(),
            1
        );
        assert_eq!(dev.read_block(1, 4096).unwrap(), vec![0; 4096]);
        let _ = fs::remove_file(image);
        let _ = fs::remove_file(undo_path);
    }
}
