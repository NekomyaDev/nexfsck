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

    let _ = Command::new("mkfs.ext4")
        .args(&["-F", img_path])
        .status();

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

    let _ = Command::new("mkfs.ext4")
        .args(&["-F", img_path])
        .status();

    // 2. Save block 0 (original data containing valid superblock) into undo journal
    let mut dev_file = OpenOptions::new().read(true).write(true).open(img_path).unwrap();
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
