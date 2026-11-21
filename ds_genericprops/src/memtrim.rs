//! Hands the memory freed by Horizon and the plugins back to the system.
//!
//! The whole process allocates through glibc malloc, which keeps one arena per thread
//! (up to 8 per core) and only ever returns the top end of each 64 MB arena heap. A heap
//! whose few live chunks sit in the middle stays resident for good. On preprod
//! (2026-10-16) horizon sat at 26.7 GB with no player connected after a 1300-NPC test:
//! walking the malloc chunks of sampled heaps, ~80 % of them was free memory, left by the
//! full-world snapshots and hand-overs of the mesh ops (free chunks of 20-43 MB). It grew
//! by 2-3 GB per wave of splits and never came back, toward the 40 Gi limit.
//!
//! `malloc_trim(0)` releases the free pages of every arena, wherever they are in the heap.
//! It runs here every TRIM_EVERY, from its own thread, and logs what it gave back.

use std::time::{Duration, Instant};
use tracing::{error, info};

/// How often the free memory is handed back. Each arena is locked only while it is
/// trimmed; the `[mem]` line logs how long a pass took.
const TRIM_EVERY: Duration = Duration::from_secs(60);
/// Below this much released, the pass is not logged.
const LOG_ABOVE_MB: u64 = 64;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
extern "C" {
    fn malloc_trim(pad: usize) -> std::os::raw::c_int;
}

/// Resident memory of the process, in MB (`VmRSS`).
fn rss_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024)
}

/// Start the trim loop. Call once, at plugin startup.
pub fn start() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let spawned = std::thread::Builder::new()
            .name("genericprops-trim".to_string())
            .spawn(|| loop {
                std::thread::sleep(TRIM_EVERY);
                let before = rss_mb();
                let started = Instant::now();
                // SAFETY: glibc's malloc_trim takes no pointer and locks each arena itself.
                unsafe { malloc_trim(0) };
                let took = started.elapsed();
                if let (Some(before), Some(after)) = (before, rss_mb()) {
                    if before.saturating_sub(after) >= LOG_ABOVE_MB {
                        info!(
                            "[mem] malloc_trim gave back {} MB: rss {} -> {} MB in {} ms",
                            before - after, before, after, took.as_millis()
                        );
                    }
                }
            });
        match spawned {
            Ok(_) => info!("[mem] malloc_trim every {} s", TRIM_EVERY.as_secs()),
            Err(e) => error!("[mem] ❌ failed to start the malloc_trim thread: {}", e),
        }
    }
}
