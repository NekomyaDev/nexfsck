use nexfsck_compute::BlockAllocationTracker;
use std::time::Instant;

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("VmRSS:")?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
        })
        .unwrap_or(0)
}

fn main() {
    let entries = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1_000_000);
    let stride = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1_000_003);
    let before = rss_kib();
    let tracker = BlockAllocationTracker::new(u64::MAX);
    let started = Instant::now();
    for index in 0..entries {
        tracker.mark_range(index.saturating_mul(stride), 1);
    }
    let after = rss_kib();
    println!(
        "entries={entries} stride={stride} chunks={} elapsed_ms={} rss_delta_kib={}",
        tracker.chunks.read().unwrap().len(),
        started.elapsed().as_millis(),
        after.saturating_sub(before)
    );
}
