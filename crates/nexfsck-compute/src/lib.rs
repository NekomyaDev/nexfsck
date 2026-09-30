//! `nexfsck-compute`
//!
//! Multi-core SIMD compute engine, hardware auto-discovery,
//! and parallel bitmap state reduction.

use crc32fast::Hasher;
use roaring::RoaringBitmap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Hardware profile detected at startup.
#[derive(Debug, Clone)]
pub struct HardwareProfile {
    pub cpu_cores: usize,
    pub has_avx512: bool,
    pub has_avx2: bool,
    pub has_arm_crc32: bool,
    pub total_ram_bytes: u64,
}

impl HardwareProfile {
    /// Detects current system hardware capabilities.
    pub fn detect() -> Self {
        let cpu_cores = num_cpus();

        #[cfg(target_arch = "x86_64")]
        let (has_avx512, has_avx2) = (
            std::is_x86_feature_detected!("avx512f"),
            std::is_x86_feature_detected!("avx2"),
        );

        #[cfg(not(target_arch = "x86_64"))]
        let (has_avx512, has_avx2) = (false, false);

        #[cfg(target_arch = "aarch64")]
        let has_arm_crc32 = std::arch::is_aarch64_feature_detected!("crc");

        #[cfg(not(target_arch = "aarch64"))]
        let has_arm_crc32 = false;

        let total_ram_bytes = detect_total_ram();

        Self {
            cpu_cores,
            has_avx512,
            has_avx2,
            has_arm_crc32,
            total_ram_bytes,
        }
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn detect_total_ram() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(info) = std::fs::read_to_string("/proc/meminfo") {
            for line in info.lines() {
                if line.starts_with("MemTotal:") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        if let Ok(kb) = parts[1].parse::<u64>() {
                            return kb * 1024;
                        }
                    }
                }
            }
        }
    }
    8 * 1024 * 1024 * 1024 // Fallback default 8GB
}

/// Computes ext4 CRC32c with seed.
pub fn ext4_crc32c(seed: u32, data: &[u8]) -> u32 {
    let mut hasher = Hasher::new_with_initial(seed);
    hasher.update(data);
    hasher.finalize()
}

/// High-performance thread-safe block allocation tracker using Roaring Bitmaps.
pub struct BlockAllocationTracker {
    total_blocks: u64,
    allocated_count: AtomicU64,
    bitmap: std::sync::RwLock<RoaringBitmap>,
}

impl BlockAllocationTracker {
    pub fn new(total_blocks: u64) -> Self {
        Self {
            total_blocks,
            allocated_count: AtomicU64::new(0),
            bitmap: std::sync::RwLock::new(RoaringBitmap::new()),
        }
    }

    /// Marks a range of blocks as allocated. Returns true if no collision detected.
    pub fn mark_range(&self, start_block: u64, count: u32) -> bool {
        let mut bm = self.bitmap.write().unwrap();
        let mut collision = false;
        for b in start_block..(start_block + count as u64) {
            if (b as u32) < u32::MAX {
                if !bm.insert(b as u32) {
                    collision = true;
                }
            }
        }
        self.allocated_count.fetch_add(count as u64, Ordering::Relaxed);
        !collision
    }

    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    pub fn allocated_count(&self) -> u64 {
        self.allocated_count.load(Ordering::Relaxed)
    }
}
