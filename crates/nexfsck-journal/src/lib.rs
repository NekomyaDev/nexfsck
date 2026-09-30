//! `nexfsck-journal`
//!
//! Atomic Undo Journal (Rollback Log) and JBD2 forensic crash analysis.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use thiserror::Error;

pub const UNDO_MAGIC: u32 = 0x554E444F; // "UNDO"

#[derive(Error, Debug)]
pub enum JournalError {
    #[error("I/O error: {0}")]
    StdIo(#[from] std::io::Error),

    #[error("Corrupted undo log: {0}")]
    CorruptLog(String),
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
    pub fn new<P: AsRef<Path>>(path: Option<P>) -> Result<Self, JournalError> {
        let file = if let Some(p) = path {
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(p)?;
            f.write_all(&UNDO_MAGIC.to_le_bytes())?;
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
    pub fn record_mutation(&mut self, physical_block: u64, original_data: &[u8]) -> Result<(), JournalError> {
        let entry = UndoEntry {
            physical_block,
            original_data: original_data.to_vec(),
        };

        if let Some(ref mut f) = self.file {
            f.write_all(&physical_block.to_le_bytes())?;
            f.write_all(&(original_data.len() as u32).to_le_bytes())?;
            f.write_all(original_data)?;
            f.flush()?;
        }

        self.entries.push(entry);
        Ok(())
    }

    pub fn total_entries(&self) -> usize {
        self.entries.len()
    }
}
