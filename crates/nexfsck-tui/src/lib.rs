//! `nexfsck-tui`
//!
//! Telemetry display and progress visualization.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Progress statistics during filesystem checking.
#[derive(Debug)]
pub struct ProgressStats {
    pub start_time: Instant,
    pub total_groups: u64,
    pub processed_groups: u64,
    pub errors_found: u64,
    pub bytes_scanned: u64,
    pub io_operations: u64,
    pub group_errors: Vec<bool>,
}

impl ProgressStats {
    pub fn new(total_groups: u64) -> Self {
        Self {
            start_time: Instant::now(),
            total_groups,
            processed_groups: 0,
            errors_found: 0,
            bytes_scanned: 0,
            io_operations: 0,
            group_errors: vec![false; total_groups as usize],
        }
    }

    pub fn percent_complete(&self) -> f64 {
        if self.total_groups == 0 {
            100.0
        } else {
            (self.processed_groups as f64 / self.total_groups as f64) * 100.0
        }
    }

    pub fn elapsed_secs(&self) -> f64 {
        self.start_time.elapsed().as_secs_f64()
    }

    pub fn throughput_bytes_per_sec(&self) -> f64 {
        self.bytes_scanned as f64 / self.elapsed_secs().max(f64::EPSILON)
    }

    pub fn iops(&self) -> f64 {
        self.io_operations as f64 / self.elapsed_secs().max(f64::EPSILON)
    }

    pub fn record_group(&mut self, group: usize, bytes: u64, io_operations: u64, error: bool) {
        self.processed_groups += 1;
        self.bytes_scanned += bytes;
        self.io_operations += io_operations;
        if let Some(slot) = self.group_errors.get_mut(group) {
            *slot = error;
        }
    }

    /// Compact block-group heatmap: `.` pending, `#` checked, `!` error.
    pub fn heatmap(&self, width: usize) -> String {
        let width = width.max(1);
        (0..self.total_groups as usize)
            .map(|group| {
                if self.group_errors.get(group).copied().unwrap_or(false) {
                    '!'
                } else if group < self.processed_groups as usize {
                    '#'
                } else {
                    '.'
                }
            })
            .collect::<Vec<_>>()
            .chunks(width)
            .map(|row| row.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn print_summary(&self) {
        println!("--------------------------------------------------");
        println!("nexfsck Verification Summary");
        println!("--------------------------------------------------");
        println!("Total Block Groups : {}", self.total_groups);
        println!("Groups Checked     : {}", self.processed_groups);
        println!("Errors Detected    : {}", self.errors_found);
        println!("Elapsed Time       : {:.2}s", self.elapsed_secs());
        println!(
            "Throughput         : {:.2} MiB/s",
            self.throughput_bytes_per_sec() / 1_048_576.0
        );
        println!(
            "I/O Operations     : {} ({:.0} IOPS)",
            self.io_operations,
            self.iops()
        );
        println!("Block Group Map    :\n{}", self.heatmap(64));
        println!("--------------------------------------------------");
    }
}

#[derive(Debug, Default)]
pub struct LiveMetrics {
    total_groups: AtomicU64,
    processed_groups: AtomicU64,
    errors: AtomicU64,
    bytes_scanned: AtomicU64,
    io_operations: AtomicU64,
    started_millis: AtomicU64,
}

impl LiveMetrics {
    pub fn new(total_groups: u64) -> Self {
        Self {
            total_groups: AtomicU64::new(total_groups),
            started_millis: AtomicU64::new(unix_millis()),
            ..Self::default()
        }
    }

    pub fn update(&self, stats: &ProgressStats) {
        self.processed_groups
            .store(stats.processed_groups, Ordering::Relaxed);
        self.errors.store(stats.errors_found, Ordering::Relaxed);
        self.bytes_scanned
            .store(stats.bytes_scanned, Ordering::Relaxed);
        self.io_operations
            .store(stats.io_operations, Ordering::Relaxed);
    }

    pub fn prometheus(&self) -> String {
        format!(
            "# TYPE nexfsck_groups_total gauge\nnexfsck_groups_total {}\n\
# TYPE nexfsck_groups_processed gauge\nnexfsck_groups_processed {}\n\
# TYPE nexfsck_errors_total gauge\nnexfsck_errors_total {}\n\
# TYPE nexfsck_bytes_scanned_total counter\nnexfsck_bytes_scanned_total {}\n\
# TYPE nexfsck_io_operations_total counter\nnexfsck_io_operations_total {}\n\
# TYPE nexfsck_started_milliseconds gauge\nnexfsck_started_milliseconds {}\n",
            self.total_groups.load(Ordering::Relaxed),
            self.processed_groups.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.bytes_scanned.load(Ordering::Relaxed),
            self.io_operations.load(Ordering::Relaxed),
            self.started_millis.load(Ordering::Relaxed),
        )
    }
}

pub struct MetricsServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MetricsServer {
    pub fn start(address: &str, metrics: Arc<LiveMetrics>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => serve_metrics(&mut stream, &metrics),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for MetricsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_metrics(stream: &mut TcpStream, metrics: &LiveMetrics) {
    let mut request = [0u8; 1024];
    let read = stream.read(&mut request).unwrap_or(0);
    let metrics_path = request[..read].starts_with(b"GET /metrics ");
    let (status, content_type, body) = if metrics_path {
        ("200 OK", "text/plain; version=0.0.4", metrics.prometheus())
    } else {
        ("404 Not Found", "text/plain", "not found\n".to_string())
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heatmap_and_rates_are_reported() {
        let mut stats = ProgressStats::new(3);
        stats.record_group(0, 4096, 1, false);
        stats.record_group(1, 4096, 1, true);
        assert_eq!(stats.heatmap(64), "#!.");
        assert!(stats.iops() > 0.0);
        assert!(stats.throughput_bytes_per_sec() > 0.0);
    }

    #[test]
    fn prometheus_contains_all_counters() {
        let metrics = LiveMetrics::new(8);
        let mut stats = ProgressStats::new(8);
        stats.record_group(0, 4096, 2, false);
        metrics.update(&stats);
        let text = metrics.prometheus();
        assert!(text.contains("nexfsck_groups_processed 1"));
        assert!(text.contains("nexfsck_bytes_scanned_total 4096"));
    }
}
