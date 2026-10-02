use nexfsck_compute::ext4_crc32c;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

fn unique_test_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nexfsck-{name}-{}", std::process::id()))
}

fn inode_location(image: &Path, inode: u32) -> (u64, u64) {
    let output = Command::new("debugfs")
        .args(["-R", &format!("imap <{inode}>"), image.to_str().unwrap()])
        .output()
        .expect("debugfs imap must run");
    assert!(output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let location = text
        .lines()
        .find(|line| line.contains("located at block"))
        .unwrap_or_else(|| panic!("debugfs imap location missing: {text}"));
    let after_block = location.split("located at block").nth(1).unwrap().trim();
    let (block, offset) = after_block.split_once(',').unwrap();
    let block = block.trim().parse::<u64>().unwrap();
    let offset = offset.split("offset").nth(1).unwrap().trim();
    let offset = u64::from_str_radix(offset.trim_start_matches("0x"), 16).unwrap();
    (block, offset)
}

fn run_nexfsck_json(image: &Path) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_nexfsck"))
        .args(["--json", "-n", image.to_str().unwrap()])
        .output()
        .expect("nexfsck must run");
    (
        output.status.code().expect("normal nexfsck exit"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

fn flip_image_byte(image: &Path, byte_offset: u64) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .unwrap();
    file.seek(SeekFrom::Start(byte_offset)).unwrap();
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0x01;
    file.seek(SeekFrom::Start(byte_offset)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
}

fn set_image_bytes(image: &Path, byte_offset: u64, bytes: &[u8]) {
    let mut file = OpenOptions::new().write(true).open(image).unwrap();
    file.seek(SeekFrom::Start(byte_offset)).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn set_superblock_feature_bit(image: &Path, feature_offset: usize, bit: u32) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .unwrap();
    let mut superblock = [0u8; 1024];
    file.seek(SeekFrom::Start(1024)).unwrap();
    file.read_exact(&mut superblock).unwrap();
    let value = u32::from_le_bytes(
        superblock[feature_offset..feature_offset + 4]
            .try_into()
            .unwrap(),
    ) | bit;
    superblock[feature_offset..feature_offset + 4].copy_from_slice(&value.to_le_bytes());
    let checksum = ext4_crc32c(u32::MAX, &superblock[..1020]);
    superblock[1020..1024].copy_from_slice(&checksum.to_le_bytes());
    file.seek(SeekFrom::Start(1024)).unwrap();
    file.write_all(&superblock).unwrap();
    file.sync_all().unwrap();
}

fn make_metadata_csum_image(path: &Path) {
    assert!(Command::new("truncate")
        .args(["-s", "64M", path.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("mkfs.ext4")
        .args(["-q", "-F", "-O", "metadata_csum", path.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
}

fn ext4_layout(image: &Path) -> (u64, u64, u64, u64, u64) {
    let header = Command::new("dumpe2fs")
        .args(["-h", image.to_str().unwrap()])
        .output()
        .unwrap();
    let header = String::from_utf8_lossy(&header.stdout);
    let block_size = header
        .lines()
        .find_map(|line| {
            line.strip_prefix("Block size:")
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap();
    let groups = Command::new("dumpe2fs")
        .args(["-g", image.to_str().unwrap()])
        .output()
        .unwrap();
    let groups = String::from_utf8_lossy(&groups.stdout);
    let fields: Vec<_> = groups
        .lines()
        .find(|line| line.starts_with("0:"))
        .unwrap()
        .split(':')
        .collect();
    let first_data = if block_size == 1024 { 1 } else { 0 };
    let descriptor_table = (first_data + 1) * block_size;
    let descriptor = fields[3].split('-').next().unwrap().parse::<u64>().unwrap();
    let block_bitmap = fields[4].parse::<u64>().unwrap() * block_size;
    let inode_bitmap = fields[5].parse::<u64>().unwrap() * block_size;
    (
        block_size,
        descriptor_table,
        block_bitmap,
        inode_bitmap,
        descriptor,
    )
}

#[test]
fn test_metadata_checksum_corruption_oracle() {
    let clean = unique_test_path("metadata-csum-clean.img");
    make_metadata_csum_image(&clean);
    let (_block_size, descriptor_table, block_bitmap, inode_bitmap, _) = ext4_layout(&clean);
    let (clean_code, clean_json) = run_nexfsck_json(&clean);
    assert_eq!(clean_code, 0, "{clean_json}");
    assert!(Command::new("e2fsck")
        .args(["-f", "-n", clean.to_str().unwrap()])
        .status()
        .unwrap()
        .success());

    let corruptions = [
        (
            "superblock",
            1024 + 1020,
            "\"superblock_checksum_failures\":1",
        ),
        (
            "group-payload",
            descriptor_table,
            "\"group_descriptor_checksum_failures\":1",
        ),
        (
            "group-checksum",
            descriptor_table + 30,
            "\"group_descriptor_checksum_failures\":1",
        ),
        (
            "block-bitmap",
            block_bitmap,
            "\"block_bitmap_checksum_failures\": 1",
        ),
        (
            "block-bitmap-csum",
            descriptor_table + 24,
            "\"group_descriptor_checksum_failures\":1",
        ),
        (
            "inode-bitmap",
            inode_bitmap,
            "\"inode_bitmap_checksum_failures\": 1",
        ),
        (
            "inode-bitmap-csum",
            descriptor_table + 26,
            "\"group_descriptor_checksum_failures\":1",
        ),
    ];
    for (label, offset, counter) in corruptions {
        let corrupt = unique_test_path(&format!("metadata-csum-{label}.img"));
        std::fs::copy(&clean, &corrupt).unwrap();
        flip_image_byte(&corrupt, offset);
        let (code, stdout) = run_nexfsck_json(&corrupt);
        assert_eq!(code, 4, "{label}: {stdout}");
        if !counter.is_empty() {
            assert!(stdout.contains(counter), "{label}: {stdout}");
        }
        let e2fsck = Command::new("e2fsck")
            .args(["-f", "-n", corrupt.to_str().unwrap()])
            .output()
            .unwrap();
        let e2fsck_text = format!(
            "{}{}",
            String::from_utf8_lossy(&e2fsck.stdout),
            String::from_utf8_lossy(&e2fsck.stderr)
        );
        assert!(
            e2fsck.status.code() != Some(0) || e2fsck_text.to_lowercase().contains("checksum"),
            "e2fsck gave no checksum diagnostic for {label}: {e2fsck_text}"
        );
        std::fs::remove_file(corrupt).unwrap();
    }
    std::fs::remove_file(clean).unwrap();
}

#[test]
fn test_directory_checksum_corruption_oracle() {
    let clean = unique_test_path("dir-csum-clean.img");
    make_metadata_csum_image(&clean);
    let (block_size, _, _, _, _) = ext4_layout(&clean);
    let blocks = Command::new("debugfs")
        .args(["-R", "blocks <2>", clean.to_str().unwrap()])
        .output()
        .unwrap();
    let blocks_text = format!(
        "{}{}",
        String::from_utf8_lossy(&blocks.stdout),
        String::from_utf8_lossy(&blocks.stderr)
    );
    let block = blocks_text
        .split_whitespace()
        .filter_map(|part| part.parse::<u64>().ok())
        .next()
        .expect("root directory block");
    let (code, stdout) = run_nexfsck_json(&clean);
    assert_eq!(code, 0, "{stdout}");
    assert!(Command::new("e2fsck")
        .args(["-f", "-n", clean.to_str().unwrap()])
        .status()
        .unwrap()
        .success());

    let corrupt = unique_test_path("dir-csum-corrupt.img");
    std::fs::copy(&clean, &corrupt).unwrap();
    flip_image_byte(&corrupt, block * block_size + block_size - 1);
    let (code, stdout) = run_nexfsck_json(&corrupt);
    assert_eq!(code, 4, "{stdout}");
    assert!(
        stdout.contains("\"directory_checksum_failures\": 1"),
        "{stdout}"
    );
    let e2fsck = Command::new("e2fsck")
        .args(["-f", "-n", corrupt.to_str().unwrap()])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&e2fsck.stdout),
        String::from_utf8_lossy(&e2fsck.stderr)
    );
    assert!(
        text.to_lowercase().contains("checksum") || e2fsck.status.code() != Some(0),
        "{text}"
    );
    std::fs::remove_file(corrupt).unwrap();
    std::fs::remove_file(clean).unwrap();
}

#[test]
fn test_htree_checksum_corruption_oracle() {
    let source = unique_test_path("htree-source");
    std::fs::create_dir(&source).unwrap();
    for index in 0..900u32 {
        std::fs::write(source.join(format!("entry-{index:04}")), b"x").unwrap();
    }
    let image = unique_test_path("htree-csum.img");
    assert!(Command::new("truncate")
        .args(["-s", "128M", image.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-O",
            "metadata_csum,dir_index",
            "-d",
            source.to_str().unwrap(),
            image.to_str().unwrap()
        ])
        .status()
        .unwrap()
        .success());
    let indexed = Command::new("e2fsck")
        .args(["-f", "-y", "-D", image.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(
        matches!(indexed.code(), Some(0) | Some(1)),
        "e2fsck -D failed: {indexed}"
    );
    let stat = Command::new("debugfs")
        .args(["-R", "stat <2>", image.to_str().unwrap()])
        .output()
        .unwrap();
    let stat_text = format!(
        "{}{}",
        String::from_utf8_lossy(&stat.stdout),
        String::from_utf8_lossy(&stat.stderr)
    );
    let inode_flags = stat_text
        .lines()
        .find_map(|line| {
            line.split("Flags: 0x")
                .nth(1)
                .and_then(|value| u32::from_str_radix(value.split_whitespace().next()?, 16).ok())
        })
        .unwrap_or(0);
    assert_ne!(
        inode_flags & 0x1000,
        0,
        "fixture root directory was not indexed: {stat_text}"
    );
    let blocks = Command::new("debugfs")
        .args(["-R", "blocks <2>", image.to_str().unwrap()])
        .output()
        .unwrap();
    let blocks_text = format!(
        "{}{}",
        String::from_utf8_lossy(&blocks.stdout),
        String::from_utf8_lossy(&blocks.stderr)
    );
    let block = blocks_text
        .split_whitespace()
        .filter_map(|part| part.parse::<u64>().ok())
        .nth(1)
        .unwrap();
    let (block_size, _, _, _, _) = ext4_layout(&image);
    let (code, stdout) = run_nexfsck_json(&image);
    assert_eq!(code, 0, "{stdout}");

    let corrupt = unique_test_path("htree-csum-corrupt.img");
    std::fs::copy(&image, &corrupt).unwrap();
    flip_image_byte(&corrupt, block * block_size + block_size - 1);
    let (code, stdout) = run_nexfsck_json(&corrupt);
    assert_eq!(code, 4, "{stdout}");
    assert!(
        stdout.contains("\"directory_checksum_failures\": 1"),
        "{stdout}"
    );
    let e2fsck = Command::new("e2fsck")
        .args(["-f", "-n", corrupt.to_str().unwrap()])
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&e2fsck.stdout),
        String::from_utf8_lossy(&e2fsck.stderr)
    );
    assert!(
        diagnostic.to_lowercase().contains("checksum") || e2fsck.status.code() != Some(0),
        "{diagnostic}"
    );
    std::fs::remove_file(corrupt).unwrap();
    std::fs::remove_file(image).unwrap();
    std::fs::remove_dir_all(source).unwrap();
}

#[test]
fn test_unsupported_feature_fails_closed() {
    let image = unique_test_path("unsupported-inline-data.img");
    assert!(Command::new("truncate")
        .args(["-s", "64M", image.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("mkfs.ext4")
        .args(["-q", "-F", "-O", "inline_data", image.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let output = Command::new(env!("CARGO_BIN_EXE_nexfsck"))
        .args(["--json", "-n", image.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(diagnostic.contains("inline_data"), "{diagnostic}");
    std::fs::remove_file(image).unwrap();
}

#[test]
fn test_mmp_filesystem_is_rejected_before_verification() {
    let image = unique_test_path("mmp-unsupported.img");
    assert!(Command::new("truncate")
        .args(["-s", "64M", image.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-O",
            "metadata_csum,mmp",
            image.to_str().unwrap(),
        ])
        .status()
        .unwrap()
        .success());
    let output = Command::new(env!("CARGO_BIN_EXE_nexfsck"))
        .args(["--json", "-n", image.to_str().unwrap()])
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(4), "{diagnostic}");
    assert!(
        diagnostic.contains("MMP checksum validation is incomplete"),
        "{diagnostic}"
    );
    std::fs::remove_file(image).unwrap();
}

#[test]
fn test_unknown_incompat_and_ro_compat_bits_fail_closed() {
    for (label, offset, bit, expected) in [
        (
            "orphan-file",
            92usize,
            0x0000_1000u32,
            "orphan_file metadata",
        ),
        ("incompat", 96usize, 0x8000_0000u32, "unknown incompat"),
        (
            "ro-compat",
            100usize,
            0x4000_0000u32,
            "unknown or unverified ro_compat",
        ),
    ] {
        let clean = unique_test_path(&format!("unknown-feature-{label}-clean.img"));
        make_metadata_csum_image(&clean);
        let corrupt = unique_test_path(&format!("unknown-feature-{label}.img"));
        std::fs::copy(&clean, &corrupt).unwrap();
        set_superblock_feature_bit(&corrupt, offset, bit);
        let output = Command::new(env!("CARGO_BIN_EXE_nexfsck"))
            .args(["--json", "-n", corrupt.to_str().unwrap()])
            .output()
            .unwrap();
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(4), "{label}: {diagnostic}");
        assert!(diagnostic.contains(expected), "{label}: {diagnostic}");
        std::fs::remove_file(corrupt).unwrap();
        std::fs::remove_file(clean).unwrap();
    }
}

#[test]
fn test_external_extent_block_checksum_corruption_oracle() {
    let source = unique_test_path("extent-csum-source");
    std::fs::create_dir(&source).unwrap();
    let fragmented = source.join("fragmented");
    let mut file = std::fs::File::create(&fragmented).unwrap();
    for logical_block in (0..20u64).step_by(2) {
        file.seek(SeekFrom::Start(logical_block * 4096)).unwrap();
        file.write_all(&vec![logical_block as u8 + 1; 4096])
            .unwrap();
    }
    file.set_len(64 * 1024 * 1024).unwrap();
    file.sync_all().unwrap();

    let image = unique_test_path("extent-csum.img");
    assert!(Command::new("truncate")
        .args(["-s", "128M", image.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-O",
            "metadata_csum",
            "-d",
            source.to_str().unwrap(),
            image.to_str().unwrap()
        ])
        .status()
        .unwrap()
        .success());
    let stat = Command::new("debugfs")
        .args(["-R", "stat /fragmented", image.to_str().unwrap()])
        .output()
        .unwrap();
    let stat_text = format!(
        "{}{}",
        String::from_utf8_lossy(&stat.stdout),
        String::from_utf8_lossy(&stat.stderr)
    );
    let external_block = stat_text
        .split("(ETB0):")
        .nth(1)
        .and_then(|tail| tail.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|number| number.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("fixture did not create an external extent node: {stat_text}"));
    let (block_size, _, _, _, _) = ext4_layout(&image);
    let mut raw_node_header = [0u8; 6];
    let mut image_file = std::fs::File::open(&image).unwrap();
    image_file
        .seek(SeekFrom::Start(external_block * block_size + 4))
        .unwrap();
    image_file.read_exact(&mut raw_node_header[0..2]).unwrap();
    let max_entries = u16::from_le_bytes(raw_node_header[0..2].try_into().unwrap()) as u64;
    let checksum_offset = external_block * block_size + 12 + max_entries * 12;

    let (clean_code, clean_json) = run_nexfsck_json(&image);
    assert_eq!(clean_code, 0, "{clean_json}");
    assert!(clean_json.contains("\"extent_block_checksum_failures\": 0"));
    let clean_e2fsck = Command::new("e2fsck")
        .args(["-f", "-n", image.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(clean_e2fsck.success());

    let corrupt = unique_test_path("extent-csum-corrupt.img");
    std::fs::copy(&image, &corrupt).unwrap();
    flip_image_byte(&corrupt, checksum_offset);
    let (corrupt_code, corrupt_json) = run_nexfsck_json(&corrupt);
    assert_eq!(corrupt_code, 4, "{corrupt_json}");
    assert!(
        corrupt_json.contains("\"extent_block_checksum_failures\": 1"),
        "{corrupt_json}"
    );
    let corrupt_bytes = std::fs::read(&corrupt).unwrap();
    let undo = unique_test_path("extent-csum-undo.log");
    let repair = Command::new(env!("CARGO_BIN_EXE_nexfsck"))
        .args([
            "--repair",
            "--undo-file",
            undo.to_str().unwrap(),
            corrupt.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(repair.status.code(), Some(4));
    let repair_diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&repair.stdout),
        String::from_utf8_lossy(&repair.stderr)
    );
    assert!(
        repair_diagnostic.contains("metadata checksum failure"),
        "{repair_diagnostic}"
    );
    assert!(
        repair_diagnostic.contains("repair_eligible\": false")
            || repair_diagnostic.contains("Repair Blocked"),
        "repair trust decision was not reported: {repair_diagnostic}"
    );
    assert_eq!(std::fs::read(&corrupt).unwrap(), corrupt_bytes);
    assert!(
        !undo.exists(),
        "checksum-uncertain metadata was journaled for repair"
    );
    let corrupt_e2fsck = Command::new("e2fsck")
        .args(["-f", "-n", corrupt.to_str().unwrap()])
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&corrupt_e2fsck.stdout),
        String::from_utf8_lossy(&corrupt_e2fsck.stderr)
    );
    assert!(
        corrupt_e2fsck.status.code() == Some(4)
            || diagnostic.to_lowercase().contains("extent block")
                && diagnostic.to_lowercase().contains("checksum"),
        "e2fsck did not identify external extent checksum corruption: {diagnostic}"
    );
    std::fs::remove_file(corrupt).unwrap();
    std::fs::remove_file(image).unwrap();
    std::fs::remove_dir_all(source).unwrap();
}

#[test]
fn test_external_xattr_block_integrity_oracle() {
    let source = unique_test_path("xattr-source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), b"xattr fixture").unwrap();
    let value_file = unique_test_path("xattr-value");
    std::fs::write(&value_file, vec![0x5a; 1024]).unwrap();
    let image = unique_test_path("xattr-clean.img");
    assert!(Command::new("truncate")
        .args(["-s", "64M", image.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-O",
            "metadata_csum",
            "-d",
            source.to_str().unwrap(),
            image.to_str().unwrap()
        ])
        .status()
        .unwrap()
        .success());
    let set_xattr = Command::new("debugfs")
        .args([
            "-w",
            "-R",
            &format!(
                "ea_set -f {} /file user.large",
                value_file.to_string_lossy()
            ),
            image.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(set_xattr.status.success());
    let stat = Command::new("debugfs")
        .args(["-R", "stat /file", image.to_str().unwrap()])
        .output()
        .unwrap();
    let stat_text = format!(
        "{}{}",
        String::from_utf8_lossy(&stat.stdout),
        String::from_utf8_lossy(&stat.stderr)
    );
    let xattr_block = stat_text
        .lines()
        .find_map(|line| line.strip_prefix("File ACL:"))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|block| *block != 0)
        .unwrap_or_else(|| panic!("fixture did not allocate an external xattr block: {stat_text}"));
    let (block_size, _, _, _, _) = ext4_layout(&image);
    let mut xattr_entry = [0u8; 36];
    let mut file = std::fs::File::open(&image).unwrap();
    file.seek(SeekFrom::Start(xattr_block * block_size + 32))
        .unwrap();
    file.read_exact(&mut xattr_entry).unwrap();
    let value_offset = u16::from_le_bytes(xattr_entry[2..4].try_into().unwrap()) as u64;
    assert!(value_offset >= 32);

    let (code, clean_json) = run_nexfsck_json(&image);
    assert_eq!(code, 0, "{clean_json}");
    assert!(clean_json.contains("\"xattr_block_corruptions\": 0"));
    assert!(clean_json.contains("\"xattr_checksum_failures\": 0"));
    assert!(clean_json.contains("\"xattr_blocks_checked\": 1"));
    assert!(clean_json.contains("\"xattr_entries_checked\": 1"));
    assert!(clean_json.contains("\"xattr_hash_failures\": 0"));
    assert!(Command::new("e2fsck")
        .args(["-f", "-n", image.to_str().unwrap()])
        .status()
        .unwrap()
        .success());

    let corruptions = [
        ("header", 0u64, vec![0xff]),
        ("entry-bound", 32u64, vec![u8::MAX]),
        ("value-offset", 34u64, u16::MAX.to_le_bytes().to_vec()),
        ("checksum", 16u64, vec![0xff; 4]),
        ("payload", value_offset, vec![0]),
    ];
    for (label, offset, bytes) in corruptions {
        let corrupt = unique_test_path(&format!("xattr-{label}.img"));
        std::fs::copy(&image, &corrupt).unwrap();
        set_image_bytes(&corrupt, xattr_block * block_size + offset, &bytes);
        let (code, json) = run_nexfsck_json(&corrupt);
        assert_eq!(code, 4, "{label}: {json}");
        let e2 = Command::new("e2fsck")
            .args(["-f", "-n", corrupt.to_str().unwrap()])
            .output()
            .unwrap();
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&e2.stdout),
            String::from_utf8_lossy(&e2.stderr)
        );
        assert!(
            e2.status.code() != Some(0) || diagnostic.to_lowercase().contains("extended attribute"),
            "e2fsck did not diagnose {label}: {diagnostic}"
        );
        if label == "checksum" || label == "payload" {
            assert!(
                json.contains("\"xattr_checksum_failures\": 1"),
                "{label}: {json}"
            );
        } else {
            assert!(
                json.contains("\"xattr_block_corruptions\": 1"),
                "{label}: {json}"
            );
        }
        std::fs::remove_file(corrupt).unwrap();
    }
    std::fs::remove_file(image).unwrap();
    std::fs::remove_file(value_file).unwrap();
    std::fs::remove_dir_all(source).unwrap();
}

#[test]
fn test_real_ext4_inode_checksum_clean_layouts_and_seed_modes() {
    for (label, inode_size, features) in [
        ("inode-csum-128", "128", "metadata_csum"),
        ("inode-csum-256", "256", "metadata_csum"),
        ("inode-csum-seed", "256", "metadata_csum,metadata_csum_seed"),
        ("metadata-csum-desc32", "256", "metadata_csum,^64bit"),
        ("legacy-gdt-csum", "256", "uninit_bg,^metadata_csum"),
    ] {
        let image = unique_test_path(&format!("{label}.img"));
        assert!(Command::new("truncate")
            .args(["-s", "64M", image.to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("mkfs.ext4")
            .args([
                "-q",
                "-F",
                "-I",
                inode_size,
                "-O",
                features,
                image.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success());
        let (code, stdout) = run_nexfsck_json(&image);
        assert_eq!(code, 0, "{label}: {stdout}");
        assert!(stdout.contains("\"inode_checksum_failures\": 0"));
        assert!(Command::new("e2fsck")
            .args(["-f", "-n", image.to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        std::fs::remove_file(image).unwrap();
    }
}

#[test]
fn test_real_ext4_inode_checksum_corruption_matches_e2fsprogs() {
    let clean = unique_test_path("inode-csum-clean.img");
    let payload = unique_test_path("inode-csum-payload");
    std::fs::write(&payload, b"nexfsck inode checksum oracle\n").unwrap();
    assert!(Command::new("truncate")
        .args(["-s", "64M", clean.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-I",
            "256",
            "-O",
            "metadata_csum",
            clean.to_str().unwrap(),
        ])
        .status()
        .unwrap()
        .success());
    let debugfs = Command::new("debugfs")
        .args([
            "-w",
            "-R",
            &format!("write {} /checked", payload.display()),
            clean.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(debugfs.status.success());
    let debugfs_text = String::from_utf8_lossy(&debugfs.stdout);
    let inode = debugfs_text
        .split("Allocated inode:")
        .nth(1)
        .expect("allocated inode output")
        .trim()
        .parse::<u32>()
        .unwrap();
    let (block, offset) = inode_location(&clean, inode);
    let inode_start = block * 4096 + offset;

    for (label, relative_offset) in [
        ("mode-type", 0u64),
        ("link-count", 26u64),
        ("extent-root", 40u64),
        ("extent-header", 42u64),
        ("extent-depth", 46u64),
        ("extent-bounds", 52u64),
        ("generation", 100u64),
        ("stored-checksum", 124u64),
    ] {
        let corrupt = unique_test_path(&format!("inode-csum-{label}.img"));
        std::fs::copy(&clean, &corrupt).unwrap();
        flip_image_byte(&corrupt, inode_start + relative_offset);
        let (code, stdout) = run_nexfsck_json(&corrupt);
        assert_eq!(code, 4, "{label}: {stdout}");
        assert!(
            stdout.contains("\"inode_checksum_failures\": 1"),
            "{label}: {stdout}"
        );
        let e2fsck = Command::new("e2fsck")
            .args(["-f", "-n", corrupt.to_str().unwrap()])
            .output()
            .unwrap();
        assert_ne!(e2fsck.status.code(), Some(0), "e2fsck accepted {label}");
        std::fs::remove_file(corrupt).unwrap();
    }
    std::fs::remove_file(clean).unwrap();
    std::fs::remove_file(payload).unwrap();
}

#[test]
fn test_backup_superblocks_are_checked_and_checksum_corruption_blocks_clean() {
    let clean = unique_test_path("backup-clean.img");
    let corrupt = unique_test_path("backup-corrupt.img");
    for path in [&clean, &corrupt] {
        assert!(
            !path.exists(),
            "refusing to overwrite test image {}",
            path.display()
        );
    }
    assert!(Command::new("truncate")
        .args(["-s", "512M", clean.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("mkfs.ext4")
        .args(["-q", "-F", "-O", "metadata_csum", clean.to_str().unwrap()])
        .status()
        .unwrap()
        .success());

    let (status, clean_json) = run_nexfsck_json(&clean);
    assert_eq!(status, 0, "{clean_json}");
    assert!(clean_json.contains("\"backup_superblocks_checked\": 2"));
    assert!(clean_json.contains("\"backup_superblocks_invalid_checksum\": 0"));
    assert!(clean_json.contains("\"backup_superblocks_inconsistent\": 0"));

    std::fs::copy(&clean, &corrupt).unwrap();
    // Default 4 KiB ext4 geometry places group 1 at block 32768. The backup
    // superblock checksum is at byte 1020 within its 1024-byte structure.
    flip_image_byte(&corrupt, 32_768 * 4096 + 1020);
    let (status, corrupt_json) = run_nexfsck_json(&corrupt);
    assert_eq!(status, 4, "{corrupt_json}");
    assert!(corrupt_json.contains("\"backup_superblocks_invalid_checksum\": 1"));
    assert!(corrupt_json.contains("\"repair_eligible\": false"));

    let e2 = Command::new("e2fsck")
        .args(["-f", "-n", corrupt.to_str().unwrap()])
        .output()
        .unwrap();
    eprintln!(
        "backup-superblock differential: nexfsck=4, e2fsck={}, diagnostic={}",
        e2.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&e2.stderr)
    );
    std::fs::remove_file(clean).unwrap();
    std::fs::remove_file(corrupt).unwrap();
}

#[test]
fn test_clean_ext4_image_verification() {
    let img_path = "/tmp/test_clean_unique.img";
    let _ = std::fs::remove_file(img_path);

    // Create 32MB ext4 test filesystem
    let truncate_status = Command::new("truncate")
        .args(["-s", "32M", img_path])
        .status()
        .expect("Failed to execute truncate");
    assert!(truncate_status.success());

    let mkfs_status = Command::new("mkfs.ext4")
        .args(["-F", img_path])
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
        .args(["-s", "32M", img_path])
        .status();

    let _ = Command::new("mkfs.ext4").args(["-F", img_path]).status();

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
        .args(["-s", "32M", img_path])
        .status();

    let _ = Command::new("mkfs.ext4").args(["-F", img_path]).status();

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
        .args(["--rollback", "--undo-file", undo_log, img_path])
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
        .args(["-s", "32M", img_path])
        .status();
    let _ = Command::new("mkfs.ext4").args(["-F", img_path]).status();

    let nexfsck_bin = env!("CARGO_BIN_EXE_nexfsck");
    let output = Command::new(nexfsck_bin)
        .args(["--json", img_path])
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
        .args(["-s", "32M", img_path])
        .status();
    let _ = Command::new("mkfs.ext4")
        .args(["-F", "-O", "^metadata_csum", img_path])
        .status();

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
        .args(["-n", img_path])
        .status()
        .unwrap();
    assert_eq!(verify_status.code(), Some(4));

    // 4. Run with --repair / -y -> must repair and return exit code 1 (FSCK_EXIT_ERRORS_CORRECTED)
    let repair_status = Command::new(nexfsck_bin)
        .args(["--repair", "--undo-file", undo_file, img_path])
        .status()
        .unwrap();
    assert_eq!(repair_status.code(), Some(1));

    // 5. Subsequent verification in read-only mode must now be completely clean (exit code 0)!
    let clean_status = Command::new(nexfsck_bin)
        .args(["-n", img_path])
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
        .args(["-s", "32M", img_path])
        .status();
    let _ = Command::new("mkfs.ext4")
        .args(["-F", "-O", "^metadata_csum", img_path])
        .status();

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
        .args(["-n", img_path])
        .status()
        .unwrap();
    assert_eq!(verify_status.code(), Some(4));

    // 4. Run --repair -> must repair and return exit code 1
    let repair_status = Command::new(nexfsck_bin)
        .args(["--repair", "--undo-file", undo_file, img_path])
        .status()
        .unwrap();
    assert_eq!(repair_status.code(), Some(1));

    // 5. Subsequent read-only check must pass cleanly (exit code 0)
    let clean_status = Command::new(nexfsck_bin)
        .args(["-n", img_path])
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
        .args(["-s", "32M", img_path])
        .status();
    let _ = Command::new("mkfs.ext4").args(["-F", img_path]).status();

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
        .args(["-n", img_path])
        .status()
        .unwrap();
    assert_eq!(verify_status.code(), Some(4));

    // Cleanup
    let _ = std::fs::remove_file(img_path);
}
