//! `io_uring` Asynchronous High-Throughput Batch I/O Engine
//!
//! Provides asynchronous batched submission using Linux `io_uring` and a persistent
//! pool of registered fixed buffers. If registration or `io_uring` itself is unavailable,
//! callers gracefully fall back to resilient positioned I/O.

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
    state: Mutex<IoUringState>,
    fd: RawFd,
    queue_depth: u32,
}

const FIXED_BUFFER_SIZE: usize = 64 * 1024;

struct IoUringState {
    ring: IoUring,
    buffers: Vec<Box<[u8]>>,
    fixed_buffers_registered: bool,
}

impl IoUringEngine {
    /// Attempts to initialize an `io_uring` instance with the requested queue depth.
    /// Returns `None` if the kernel or container seccomp policy disallows `io_uring`.
    pub fn try_new(fd: RawFd, queue_depth: u32) -> Option<Self> {
        match IoUring::new(queue_depth) {
            Ok(ring) => {
                let mut buffers: Vec<Box<[u8]>> = (0..queue_depth)
                    .map(|_| vec![0u8; FIXED_BUFFER_SIZE].into_boxed_slice())
                    .collect();
                let iovecs: Vec<libc::iovec> = buffers
                    .iter_mut()
                    .map(|buffer| libc::iovec {
                        iov_base: buffer.as_mut_ptr().cast(),
                        iov_len: buffer.len(),
                    })
                    .collect();
                // SAFETY: every buffer owns a stable heap allocation and remains in
                // `IoUringState` until after the ring is dropped.
                let fixed_buffers_registered =
                    unsafe { ring.submitter().register_buffers(&iovecs) }
                        .map(|_| true)
                        .unwrap_or_else(|error| {
                            warn!("io_uring fixed-buffer registration failed: {error}");
                            false
                        });
                debug!(
                    "Initialized Linux io_uring engine with queue depth {} (fixed buffers: {})",
                    queue_depth, fixed_buffers_registered
                );
                Some(Self {
                    state: Mutex::new(IoUringState {
                        ring,
                        buffers,
                        fixed_buffers_registered,
                    }),
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

    pub fn fixed_buffers_registered(&self) -> bool {
        self.state.lock().unwrap().fixed_buffers_registered
    }

    /// Executes a batch of read requests asynchronously, submitting up to `queue_depth`
    /// concurrent requests to the kernel submission queue.
    pub fn read_batch(&self, requests: &[BatchReadRequest]) -> Vec<BatchReadResult> {
        if requests.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::with_capacity(requests.len());
        let mut state = self.state.lock().unwrap();

        for chunk in requests.chunks(self.queue_depth as usize) {
            let use_fixed = state.fixed_buffers_registered
                && chunk.iter().all(|request| request.len <= FIXED_BUFFER_SIZE);
            let mut ordinary_buffers: Vec<Vec<u8>> = if use_fixed {
                Vec::new()
            } else {
                chunk.iter().map(|request| vec![0u8; request.len]).collect()
            };

            // Prepare and submit SQEs
            {
                let IoUringState { ring, buffers, .. } = &mut *state;
                let mut sq = ring.submission();
                for (idx, req) in chunk.iter().enumerate() {
                    let read_e = if use_fixed {
                        opcode::ReadFixed::new(
                            types::Fd(self.fd),
                            buffers[idx].as_mut_ptr(),
                            req.len as u32,
                            idx as u16,
                        )
                        .offset(req.offset)
                        .build()
                        .user_data(idx as u64)
                    } else {
                        opcode::Read::new(
                            types::Fd(self.fd),
                            ordinary_buffers[idx].as_mut_ptr(),
                            req.len as u32,
                        )
                        .offset(req.offset)
                        .build()
                        .user_data(idx as u64)
                    };

                    unsafe {
                        if sq.push(&read_e).is_err() {
                            break;
                        }
                    }
                }
            }

            // Submit and wait for all completions in this chunk
            let to_wait = chunk.len();
            if let Err(e) = state.ring.submit_and_wait(to_wait) {
                // If submit_and_wait fails, report errors for entire chunk
                for request in chunk {
                    results.push(BatchReadResult {
                        id: request.id,
                        data: Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("io_uring submit error: {}", e),
                        )),
                    });
                }
                continue;
            }

            // Process CQEs
            let mut chunk_results: Vec<Option<BatchReadResult>> =
                (0..chunk.len()).map(|_| None).collect();
            let completions: Vec<(usize, i32)> = {
                let mut completed = Vec::with_capacity(chunk.len());
                let mut cq = state.ring.completion();
                while let Some(cqe) = cq.next() {
                    completed.push((cqe.user_data() as usize, cqe.result()));
                }
                completed
            };
            for (idx, res) in completions {
                if idx < chunk.len() {
                    let request = &chunk[idx];
                    let read_res = if res < 0 {
                        let os_err = -res;
                        Err(std::io::Error::from_raw_os_error(os_err))
                    } else if (res as usize) < request.len {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            format!(
                                "io_uring short read: requested {} bytes, got {} bytes",
                                request.len, res
                            ),
                        ))
                    } else {
                        let data = if use_fixed {
                            state.buffers[idx][..request.len].to_vec()
                        } else {
                            std::mem::take(&mut ordinary_buffers[idx])
                        };
                        Ok(data)
                    };

                    chunk_results[idx] = Some(BatchReadResult {
                        id: request.id,
                        data: read_res,
                    });
                }
            }

            for r in chunk_results.into_iter().flatten() {
                results.push(r);
            }
        }

        results
    }
}
