//! `io_uring` Asynchronous High-Throughput Batch I/O Engine
//!
//! Provides true asynchronous kernel-bypass batch submission using Linux `io_uring`
//! for parallel NVMe queue-depth saturation. If `io_uring` is unavailable or restricted
//! by container seccomp profiles, callers gracefully fall back to resilient direct I/O.

use io_uring::{opcode, types, IoUring};
use std::os::unix::io::RawFd;
use std::sync::Mutex;
use tracing::{debug, warn};

/// High-throughput batch read request.
pub struct BatchReadRequest {
    pub id: u64,
    pub offset: u64,
    pub len: usize,
}

/// Result of a single batch read operation.
pub struct BatchReadResult {
    pub id: u64,
    pub data: Result<Vec<u8>, std::io::Error>,
}

/// Linux `io_uring` batch execution engine.
pub struct IoUringEngine {
    ring: Mutex<IoUring>,
    fd: RawFd,
    queue_depth: u32,
}

impl IoUringEngine {
    /// Attempts to initialize an `io_uring` instance with the requested queue depth.
    /// Returns `None` if the kernel or container seccomp policy disallows `io_uring`.
    pub fn try_new(fd: RawFd, queue_depth: u32) -> Option<Self> {
        match IoUring::new(queue_depth) {
            Ok(ring) => {
                debug!(
                    "Initialized Linux io_uring engine with queue depth {}",
                    queue_depth
                );
                Some(Self {
                    ring: Mutex::new(ring),
                    fd,
                    queue_depth,
                })
            }
            Err(e) => {
                warn!(
                    "io_uring initialization unsupported or restricted: {}. Using POSIX direct I/O fallback.",
                    e
                );
                None
            }
        }
    }

    pub fn queue_depth(&self) -> u32 {
        self.queue_depth
    }

    /// Executes a batch of read requests asynchronously, submitting up to `queue_depth`
    /// concurrent requests to the kernel submission queue.
    pub fn read_batch(&self, requests: &[BatchReadRequest]) -> Vec<BatchReadResult> {
        if requests.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::with_capacity(requests.len());
        let mut ring = self.ring.lock().unwrap();

        for chunk in requests.chunks(self.queue_depth as usize) {
            let mut buffers: Vec<(u64, Vec<u8>)> = chunk
                .iter()
                .map(|req| (req.id, vec![0u8; req.len]))
                .collect();

            // Prepare and submit SQEs
            {
                let mut sq = ring.submission();
                for (idx, req) in chunk.iter().enumerate() {
                    let buf_ptr = buffers[idx].1.as_mut_ptr();
                    let read_e = opcode::Read::new(
                        types::Fd(self.fd),
                        buf_ptr,
                        req.len as u32,
                    )
                    .offset(req.offset)
                    .build()
                    .user_data(idx as u64);

                    unsafe {
                        if sq.push(&read_e).is_err() {
                            break;
                        }
                    }
                }
            }

            // Submit and wait for all completions in this chunk
            let to_wait = chunk.len();
            if let Err(e) = ring.submit_and_wait(to_wait) {
                // If submit_and_wait fails, report errors for entire chunk
                for (id, _) in buffers {
                    results.push(BatchReadResult {
                        id,
                        data: Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("io_uring submit error: {}", e),
                        )),
                    });
                }
                continue;
            }

            // Process CQEs
            let mut chunk_results: Vec<Option<BatchReadResult>> = (0..chunk.len()).map(|_| None).collect();
            {
                let mut cq = ring.completion();
                while let Some(cqe) = cq.next() {
                    let idx = cqe.user_data() as usize;
                    let res = cqe.result();

                    if idx < buffers.len() {
                        let (id, buf) = std::mem::replace(&mut buffers[idx], (0, Vec::new()));
                        let read_res = if res < 0 {
                            let os_err = -res;
                            Err(std::io::Error::from_raw_os_error(os_err))
                        } else if (res as usize) < buf.len() {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                format!(
                                    "io_uring short read: requested {} bytes, got {} bytes",
                                    buf.len(),
                                    res
                                ),
                            ))
                        } else {
                            Ok(buf)
                        };

                        chunk_results[idx] = Some(BatchReadResult { id, data: read_res });
                    }
                }
            }

            for r in chunk_results.into_iter().flatten() {
                results.push(r);
            }
        }

        results
    }
}
