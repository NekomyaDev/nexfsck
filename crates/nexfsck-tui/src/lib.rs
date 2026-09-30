//! `nexfsck-tui`
//!
//! Telemetry display and progress visualization.

use std::time::Instant;

/// Progress statistics during filesystem checking.
#[derive(Debug)]
pub struct ProgressStats {
    pub start_time: Instant,
    pub total_groups: u64,
    pub processed_groups: u64,
    pub errors_found: u64,
    pub bytes_scanned: u64,
}

impl ProgressStats {
    pub fn new(total_groups: u64) -> Self {
        Self {
            start_time: Instant::now(),
            total_groups,
            processed_groups: 0,
            errors_found: 0,
            bytes_scanned: 0,
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

    pub fn print_summary(&self) {
        println!("--------------------------------------------------");
        println!("nexfsck Verification Summary");
        println!("--------------------------------------------------");
        println!("Total Block Groups : {}", self.total_groups);
        println!("Groups Checked     : {}", self.processed_groups);
        println!("Errors Detected    : {}", self.errors_found);
        println!("Elapsed Time       : {:.2}s", self.elapsed_secs());
        println!("--------------------------------------------------");
    }
}
