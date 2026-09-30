use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Command;

#[test]
fn test_clean_ext4_image_verification() {
    let img_path = "/tmp/test_clean_unique.img";
    let _ = std::fs::remove_file(img_path);

    // Create 32MB ext4 test filesystem
    let truncate_status = Command::new("truncate")
        .args(&["-s", "32M", img_path])
        .status()
        .expect("Failed to execute truncate");
    assert!(truncate_status.success());

    let mkfs_status = Command::new("mkfs.ext4")
        .args(&["-F", img_path])
        .status()
        .expect("Failed to execute mkfs.ext4");
    assert!(mkfs_status.success());

    // Run nexfsck binary
    let nexfsck_bin = env!("CARGO_BIN_EXE_nexfsck");
    let check_status = Command::new(nexfsck_bin)
        .arg(img_path)
        .status()
        .expect("Failed to run nexfsck");

    // Standard LSB exit code 0 indicates clean filesystem
    assert_eq!(check_status.code(), Some(0));

    // Cleanup
    let _ = std::fs::remove_file(img_path);
}

#[test]
fn test_corrupted_magic_detection() {
    let img_path = "/tmp/test_corrupt_magic.img";
    let _ = std::fs::remove_file(img_path);

    // 1. Create a 32MB ext4 filesystem
    let _ = Command::new("truncate")
        .args(&["-s", "32M", img_path])
        .status();

    let _ = Command::new("mkfs.ext4").args(&["-F", img_path]).status();

    // 2. Corrupt superblock magic (offset 1024 + 56 in ext4 superblock)
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(img_path)
        .expect("Failed to open image for tampering");

    file.seek(SeekFrom::Start(1024 + 56)).unwrap();
    file.write_all(&[0x00, 0x00]).unwrap();
    file.sync_all().unwrap();
    drop(file);

    // 3. Run nexfsck binary
    let nexfsck_bin = env!("CARGO_BIN_EXE_nexfsck");
    let check_status = Command::new(nexfsck_bin)
        .arg(img_path)
        .status()
        .expect("Failed to run nexfsck");

    // Exit code must be non-zero (FSCK_EXIT_ERRORS_UNCORRECTED = 4)
    assert_eq!(check_status.code(), Some(4));

    // Cleanup
    let _ = std::fs::remove_file(img_path);
}

#[test]
fn test_atomic_rollback_and_restore() {
    let img_path = "/tmp/test_rollback.img";
    let undo_log = "/tmp/test_rollback.undo";
    let _ = std::fs::remove_file(img_path);
    let _ = std::fs::remove_file(undo_log);

    // 1. Create clean ext4 filesystem
    let _ = Command::new("truncate")
        .args(&["-s", "32M", img_path])
        .status();

    let _ = Command::new("mkfs.ext4").args(&["-F", img_path]).status();

    // 2. Save block 0 (original data containing valid superblock) into undo journal
    let mut dev_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(img_path)
        .unwrap();
    let mut original_block_0 = vec![0u8; 4096];
    dev_file.read_exact(&mut original_block_0).unwrap();

    let mut journal = nexfsck_journal::AtomicUndoJournal::new(Some(undo_log), 4096).unwrap();
    journal.record_mutation(0, &original_block_0).unwrap();
    drop(journal);

    // 3. Corrupt block 0 on disk
    dev_file.seek(SeekFrom::Start(1024 + 56)).unwrap();
    dev_file.write_all(&[0x00, 0x00]).unwrap(); // destroy magic
    dev_file.sync_all().unwrap();
    drop(dev_file);

    // Verify it is currently broken
    let nexfsck_bin = env!("CARGO_BIN_EXE_nexfsck");
    let corrupt_status = Command::new(nexfsck_bin)
        .arg(img_path)
        .status()
        .expect("Failed to run nexfsck");
    assert_eq!(corrupt_status.code(), Some(4));

    // 4. Run nexfsck with --rollback
    let rollback_status = Command::new(nexfsck_bin)
        .args(&["--rollback", "--undo-file", undo_log, img_path])
        .status()
        .expect("Failed to execute rollback");
    assert!(rollback_status.success());

    // 5. Verify the disk is completely restored and passes with code 0!
    let verify_status = Command::new(nexfsck_bin)
        .arg(img_path)
        .status()
        .expect("Failed to verify restored image");
    assert_eq!(verify_status.code(), Some(0));

    // Cleanup
    let _ = std::fs::remove_file(img_path);
    let _ = std::fs::remove_file(undo_log);
}

#[test]
fn test_json_telemetry_output() {
    let img_path = "/tmp/test_json.img";
    let _ = std::fs::remove_file(img_path);

    let _ = Command::new("truncate")
        .args(&["-s", "32M", img_path])
        .status();
    let _ = Command::new("mkfs.ext4").args(&["-F", img_path]).status();

    let nexfsck_bin = env!("CARGO_BIN_EXE_nexfsck");
    let output = Command::new(nexfsck_bin)
        .args(&["--json", img_path])
        .output()
        .expect("Failed to run nexfsck --json");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"total_block_groups\":"));
    assert!(stdout.contains("\"inodes_scanned\":"));
    assert!(stdout.contains("\"journal_dirty\": false"));
    assert!(stdout.contains("\"corrupt_directories\": 0"));
    assert!(stdout.contains("\"errors_detected\": 0"));

    let _ = std::fs::remove_file(img_path);
}

#[test]
fn test_active_repair_false_free_block() {
    let img_path = "/tmp/test_repair.img";
    let undo_file = "/tmp/test_repair.undo";
    let _ = std::fs::remove_file(img_path);
    let _ = std::fs::remove_file(undo_file);

    // 1. Create clean ext4 filesystem
    let _ = Command::new("truncate")
        .args(&["-s", "32M", img_path])
        .status();
    let _ = Command::new("mkfs.ext4").args(&["-F", img_path]).status();

    // 2. Artificially clear a bit in block bitmap (block 5 is block bitmap in group 0)
    // Clear bit 0 of block 0 (superblock) so it becomes a false-free block
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(img_path)
        .unwrap();
    // Seek to block bitmap at block 5 (5 * 4096 = 20480)
    file.seek(SeekFrom::Start(5 * 4096)).unwrap();
    let mut bm_byte = [0u8; 1];
    file.read_exact(&mut bm_byte).unwrap();
    // Clear the first bit to simulate a false-free block
    bm_byte[0] &= 0xFE;
    file.seek(SeekFrom::Start(5 * 4096)).unwrap();
    file.write_all(&bm_byte).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let nexfsck_bin = env!("CARGO_BIN_EXE_nexfsck");

    // 3. Verification in read-only mode must detect false-free block error (exit code 4)
    let verify_status = Command::new(nexfsck_bin)
        .args(&["-n", img_path])
        .status()
        .unwrap();
    assert_eq!(verify_status.code(), Some(4));

    // 4. Run with --repair / -y -> must repair and return exit code 1 (FSCK_EXIT_ERRORS_CORRECTED)
    let repair_status = Command::new(nexfsck_bin)
        .args(&["--repair", "--undo-file", undo_file, img_path])
        .status()
        .unwrap();
    assert_eq!(repair_status.code(), Some(1));

    // 5. Subsequent verification in read-only mode must now be completely clean (exit code 0)!
    let clean_status = Command::new(nexfsck_bin)
        .args(&["-n", img_path])
        .status()
        .unwrap();
    assert_eq!(clean_status.code(), Some(0));

    // Cleanup
    let _ = std::fs::remove_file(img_path);
    let _ = std::fs::remove_file(undo_file);
}

#[test]
fn test_active_repair_false_free_inode() {
    let img_path = "/tmp/test_repair_inode.img";
    let undo_file = "/tmp/test_repair_inode.undo";
    let _ = std::fs::remove_file(img_path);
    let _ = std::fs::remove_file(undo_file);

    // 1. Create clean ext4 filesystem
    let _ = Command::new("truncate")
        .args(&["-s", "32M", img_path])
        .status();
    let _ = Command::new("mkfs.ext4").args(&["-F", img_path]).status();

    // 2. Artificially clear bit 1 (inode 2) in inode bitmap (block 21 in 32M ext4)
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(img_path)
        .unwrap();
    file.seek(SeekFrom::Start(21 * 4096)).unwrap();
    let mut bm_byte = [0u8; 1];
    file.read_exact(&mut bm_byte).unwrap();
    bm_byte[0] &= 0xFD; // clear bit 1 (inode 2, root dir)
    file.seek(SeekFrom::Start(21 * 4096)).unwrap();
    file.write_all(&bm_byte).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let nexfsck_bin = env!("CARGO_BIN_EXE_nexfsck");

    // 3. Read-only mode must detect false-free inode error (exit code 4)
    let verify_status = Command::new(nexfsck_bin)
        .args(&["-n", img_path])
        .status()
        .unwrap();
    assert_eq!(verify_status.code(), Some(4));

    // 4. Run --repair -> must repair and return exit code 1
    let repair_status = Command::new(nexfsck_bin)
        .args(&["--repair", "--undo-file", undo_file, img_path])
        .status()
        .unwrap();
    assert_eq!(repair_status.code(), Some(1));

    // 5. Subsequent read-only check must pass cleanly (exit code 0)
    let clean_status = Command::new(nexfsck_bin)
        .args(&["-n", img_path])
        .status()
        .unwrap();
    assert_eq!(clean_status.code(), Some(0));

    // Cleanup
    let _ = std::fs::remove_file(img_path);
    let _ = std::fs::remove_file(undo_file);
}

#[test]
fn test_corrupt_directory_entry_detection() {
    let img_path = "/tmp/test_corrupt_dentry.img";
    let _ = std::fs::remove_file(img_path);

    // 1. Create clean ext4 filesystem
    let _ = Command::new("truncate")
        .args(&["-s", "32M", img_path])
        .status();
    let _ = Command::new("mkfs.ext4").args(&["-F", img_path]).status();

    // 2. Corrupt root directory block (block 6 at offset 24576):
    // Inject invalid unaligned rec_len (e.g. 5 bytes instead of 12) at offset 24576 + 4
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(img_path)
        .unwrap();
    file.seek(SeekFrom::Start(6 * 4096 + 4)).unwrap();
    let bad_rec_len = [0x05u8, 0x00u8]; // Invalid unaligned record length
    file.write_all(&bad_rec_len).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let nexfsck_bin = env!("CARGO_BIN_EXE_nexfsck");

    // 3. Read-only verification must detect directory corruption and exit with code 4
    let verify_status = Command::new(nexfsck_bin)
        .args(&["-n", img_path])
        .status()
        .unwrap();
    assert_eq!(verify_status.code(), Some(4));

    // Cleanup
    let _ = std::fs::remove_file(img_path);
}
