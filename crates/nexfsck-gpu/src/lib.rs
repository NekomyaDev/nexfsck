//! `nexfsck-gpu`
//!
//! GPU device discovery.
//!
//! CUDA collision candidates are produced by a PTX kernel and revalidated on CPU.

#[cfg(target_os = "linux")]
mod cuda;

/// Detected GPU device family. This does not indicate an active compute backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuBackend {
    None,
    DrmDeviceDetected,
    CudaCompute,
}

/// A block interval representing contiguous physical blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockInterval {
    pub start_block: u64,
    pub block_count: u32,
}

#[derive(Debug, Default, Clone)]
pub struct CollisionProfile {
    pub sort: std::time::Duration,
    pub host_preparation_and_copy: std::time::Duration,
    pub kernel_launch: std::time::Duration,
    pub gpu_synchronization: std::time::Duration,
    pub copy_back: std::time::Duration,
    pub cpu_revalidation: std::time::Duration,
}

/// GPU device probe with CPU interval processing.
pub struct GpuAccelerator {
    backend: GpuBackend,
    device_name: String,
    vram_bytes: u64,
    #[cfg(target_os = "linux")]
    cuda: Option<cuda::CudaContext>,
}

impl GpuAccelerator {
    /// A zero-cost CPU-only accelerator. This avoids loading the CUDA driver.
    pub fn cpu_only() -> Self {
        Self {
            backend: GpuBackend::None,
            device_name: "CPU collision backend".into(),
            vram_bytes: 0,
            #[cfg(target_os = "linux")]
            cuda: None,
        }
    }

    /// Probes for visible GPU devices and queries model/memory information where possible.
    pub fn probe() -> Self {
        #[cfg(target_os = "linux")]
        {
            if let Ok(cuda) = cuda::CudaContext::new() {
                let (model, vram) = probe_nvidia().unwrap_or_else(|| ("NVIDIA CUDA GPU".into(), 0));
                return Self {
                    backend: GpuBackend::CudaCompute,
                    device_name: model,
                    vram_bytes: vram,
                    cuda: Some(cuda),
                };
            }

            if let Some((model, vram)) = probe_drm_vulkan() {
                return Self {
                    backend: GpuBackend::DrmDeviceDetected,
                    device_name: model,
                    vram_bytes: vram,
                    cuda: None,
                };
            }
        }

        Self {
            backend: GpuBackend::None,
            device_name: "No GPU device detected".into(),
            vram_bytes: 0,
            #[cfg(target_os = "linux")]
            cuda: None,
        }
    }

    pub fn backend(&self) -> GpuBackend {
        self.backend
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn vram_bytes(&self) -> u64 {
        self.vram_bytes
    }

    /// Returns whether a real CUDA compute context is active.
    pub fn is_available(&self) -> bool {
        self.backend == GpuBackend::CudaCompute
    }

    /// Sorts intervals and detects adjacent overlaps on the CPU.
    pub fn find_interval_collisions(
        &self,
        intervals: &mut [BlockInterval],
    ) -> Vec<(BlockInterval, BlockInterval)> {
        self.find_interval_collisions_profiled(intervals).0
    }

    pub fn find_interval_collisions_profiled(
        &self,
        intervals: &mut [BlockInterval],
    ) -> (Vec<(BlockInterval, BlockInterval)>, CollisionProfile) {
        let mut profile = CollisionProfile::default();
        if intervals.len() < 2 {
            return (Vec::new(), profile);
        }

        // Sorting remains on the host; collision candidate generation is dispatched
        // to CUDA when available and deterministically revalidated on the CPU.
        let started = std::time::Instant::now();
        intervals.sort_unstable_by_key(|i| i.start_block);
        profile.sort = started.elapsed();

        #[cfg(target_os = "linux")]
        if let Some(cuda) = &self.cuda {
            if let Ok((candidate_indices, timings)) =
                cuda.find_collision_candidates_profiled(intervals)
            {
                profile.host_preparation_and_copy = timings[0];
                profile.kernel_launch = timings[1];
                profile.gpu_synchronization = timings[2];
                profile.copy_back = timings[3];
                let started = std::time::Instant::now();
                let collisions = candidate_indices
                    .into_iter()
                    .filter_map(|index| {
                        let current = intervals.get(index).copied()?;
                        let next = intervals.get(index + 1).copied()?;
                        (current
                            .start_block
                            .saturating_add(current.block_count as u64)
                            > next.start_block)
                            .then_some((current, next))
                    })
                    .collect();
                profile.cpu_revalidation = started.elapsed();
                return (collisions, profile);
            }
        }

        let mut collisions = Vec::new();
        for i in 0..intervals.len() - 1 {
            let curr = intervals[i];
            let next = intervals[i + 1];

            if curr.start_block.saturating_add(curr.block_count as u64) > next.start_block {
                collisions.push((curr, next));
            }
        }

        (collisions, profile)
    }
}

#[cfg(target_os = "linux")]
fn probe_nvidia() -> Option<(String, u64)> {
    if !std::path::Path::new("/dev/nvidia0").exists()
        && !std::path::Path::new("/dev/nvidiactl").exists()
    {
        return None;
    }

    let mut model_name = "NVIDIA CUDA GPU".to_string();
    let mut vram_bytes: u64 = 0;

    // 1. Read real model name from /proc/driver/nvidia/gpus/*/information
    if let Ok(entries) = std::fs::read_dir("/proc/driver/nvidia/gpus") {
        for entry in entries.flatten() {
            let info_file = entry.path().join("information");
            if let Ok(content) = std::fs::read_to_string(&info_file) {
                for line in content.lines() {
                    if let Some(rest) = line.strip_prefix("Model:") {
                        let trimmed = rest.trim();
                        if !trimmed.is_empty() {
                            model_name = trimmed.to_string();
                            break;
                        }
                    }
                }
            }
        }
    }

    // Querying nvidia-smi adds a process launch to every fsck. The PCI BAR is
    // sufficient for diagnostics and keeps discovery entirely in-process.
    if vram_bytes == 0 {
        if let Ok(entries) = std::fs::read_dir("/sys/bus/pci/devices") {
            for entry in entries.flatten() {
                let dev_path = entry.path();
                let vendor_path = dev_path.join("vendor");
                if let Ok(vendor) = std::fs::read_to_string(&vendor_path) {
                    if vendor.trim().eq_ignore_ascii_case("0x10de") {
                        let res_path = dev_path.join("resource");
                        if let Ok(res_content) = std::fs::read_to_string(&res_path) {
                            for line in res_content.lines() {
                                let parts: Vec<&str> = line.split_whitespace().collect();
                                if parts.len() >= 3 {
                                    if let (Ok(start), Ok(end), Ok(flags)) = (
                                        u64::from_str_radix(parts[0].trim_start_matches("0x"), 16),
                                        u64::from_str_radix(parts[1].trim_start_matches("0x"), 16),
                                        u64::from_str_radix(parts[2].trim_start_matches("0x"), 16),
                                    ) {
                                        if end > start {
                                            let size = end - start + 1;
                                            if size >= 128 * 1024 * 1024 && (flags & 0x200) != 0 {
                                                vram_bytes = size;
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Some((model_name, vram_bytes))
}

#[cfg(target_os = "linux")]
fn probe_drm_vulkan() -> Option<(String, u64)> {
    if !std::path::Path::new("/dev/dri/renderD128").exists() {
        return None;
    }

    let mut model_name = "DRM / Vulkan Accelerator".to_string();
    let mut vram_bytes: u64 = 0;

    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|s| s.starts_with("card"))
            {
                let vram_path = path.join("device/mem_info_vram_total");
                if let Ok(content) = std::fs::read_to_string(&vram_path) {
                    if let Ok(bytes) = content.trim().parse::<u64>() {
                        vram_bytes = bytes;
                    }
                }
                let vendor_path = path.join("device/vendor");
                if let Ok(vendor) = std::fs::read_to_string(&vendor_path) {
                    let v = vendor.trim();
                    if v.eq_ignore_ascii_case("0x1002") {
                        model_name = "AMD Radeon GPU (Vulkan)".to_string();
                    } else if v.eq_ignore_ascii_case("0x8086") {
                        model_name = "Intel Graphics GPU (Vulkan)".to_string();
                    }
                }
                if vram_bytes > 0 {
                    break;
                }
            }
        }
    }

    Some((model_name, vram_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collision_detection_is_deterministic() {
        #[cfg(target_os = "linux")]
        if std::env::var_os("NEXFSCK_REQUIRE_CUDA").is_some() {
            cuda::CudaContext::new().expect("CUDA context and PTX module must initialize");
        }
        let accelerator = GpuAccelerator::probe();
        if std::env::var_os("NEXFSCK_REQUIRE_CUDA").is_some() {
            assert_eq!(accelerator.backend(), GpuBackend::CudaCompute);
        }
        let mut intervals = [
            BlockInterval {
                start_block: 20,
                block_count: 2,
            },
            BlockInterval {
                start_block: 10,
                block_count: 5,
            },
            BlockInterval {
                start_block: 14,
                block_count: 3,
            },
            BlockInterval {
                start_block: 30,
                block_count: 1,
            },
        ];
        let collisions = accelerator.find_interval_collisions(&mut intervals);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].0.start_block, 10);
        assert_eq!(collisions[0].1.start_block, 14);
    }
}
