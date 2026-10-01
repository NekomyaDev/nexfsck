use nexfsck_compute::{reconcile_block_bitmap, BlockAllocationTracker};
use std::time::Instant;

fn main() {
    let groups = 80u64;
    let blocks_per_group = 32_768u32;
    for tracker in [
        BlockAllocationTracker::new(groups * blocks_per_group as u64),
        BlockAllocationTracker::new(u64::MAX),
    ] {
        for group in 0..groups {
            let first = group * blocks_per_group as u64;
            for offset in (0..blocks_per_group).step_by(3) {
                tracker.mark_range(first + offset as u64, 1);
            }
        }
        let mut disk = vec![0u8; blocks_per_group as usize / 8];
        for offset in (0..blocks_per_group).step_by(3) {
            disk[offset as usize / 8] |= 1 << (offset % 8);
        }
        let started = Instant::now();
        for group in 0..groups {
            let result = reconcile_block_bitmap(
                &disk,
                &tracker,
                group * blocks_per_group as u64,
                blocks_per_group,
            );
            assert_eq!(result.false_free_blocks + result.leaked_blocks, 0);
        }
        let elapsed = started.elapsed();
        println!(
            "representation={} groups={} blocks={} elapsed_ms={:.3} ns_per_group={:.1}",
            tracker.representation_name(),
            groups,
            groups * blocks_per_group as u64,
            elapsed.as_secs_f64() * 1000.0,
            elapsed.as_nanos() as f64 / groups as f64,
        );
    }
}
