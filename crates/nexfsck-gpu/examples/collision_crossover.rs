use nexfsck_gpu::{BlockInterval, GpuAccelerator};
use std::time::Instant;

fn intervals(count: usize) -> Vec<BlockInterval> {
    // Reverse order forces equivalent sorting work in both paths.
    (0..count)
        .rev()
        .map(|i| BlockInterval {
            start_block: (i as u64) * 2,
            block_count: 1,
        })
        .collect()
}

fn main() {
    let sizes: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    let sizes = if sizes.is_empty() {
        vec![100_000, 1_000_000, 5_000_000, 10_000_000]
    } else {
        sizes
    };

    let init = Instant::now();
    let cuda = GpuAccelerator::probe();
    let init_ms = init.elapsed().as_secs_f64() * 1000.0;
    if !cuda.is_available() {
        eprintln!("CUDA unavailable");
        std::process::exit(2);
    }
    let cpu = GpuAccelerator::cpu_only();
    println!("count,cpu_ms,cuda_dispatch_ms,cuda_first_run_total_ms,cuda_init_ms");
    for count in sizes {
        let mut cpu_input = intervals(count);
        let mut cuda_input = cpu_input.clone();
        let started = Instant::now();
        let cpu_result = cpu.find_interval_collisions(&mut cpu_input);
        let cpu_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        let cuda_result = cuda.find_interval_collisions(&mut cuda_input);
        let cuda_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(cpu_result, cuda_result);
        println!(
            "{count},{cpu_ms:.3},{cuda_ms:.3},{:.3},{init_ms:.3}",
            cuda_ms + init_ms
        );
    }
}
