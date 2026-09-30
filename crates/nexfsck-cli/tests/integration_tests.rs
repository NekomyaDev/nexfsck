use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
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
