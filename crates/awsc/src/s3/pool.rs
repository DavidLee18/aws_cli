//! An adaptive worker pool.
//!
//! The reference uses a fixed ten threads. A fixed number is wrong in both directions: on
//! a fast link with many small objects it under-uses the connection, and on a slow or
//! throttled one it piles on requests that only add latency and 503s.
//!
//! This starts small, measures throughput, and ramps while throughput is still improving —
//! backing off when it degrades or the service throttles. The ceiling comes from the
//! machine rather than a constant.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How often the supervisor re-measures.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(150);
/// Ramp while throughput improves by at least this fraction.
const IMPROVEMENT: f64 = 1.05;
/// Back off when it falls by more than this fraction.
const DEGRADATION: f64 = 0.90;
/// The reference's fixed worker count, used as our floor rather than our ceiling.
const DEFAULT_WORKERS: usize = 10;
/// The next worker count while throughput is still improving.
fn grown(current: usize, ceiling: usize) -> usize {
    (current + (current / 2).max(2)).min(ceiling)
}

/// The hard ceiling when the caller does not pin one.
///
/// Not derived from the core count. Transfers are IO-bound -- a worker spends its life
/// waiting on a socket -- so cores are the wrong unit, and scaling by them capped a
/// 4-vCPU instance at 16 workers. Measured in-region on a c7g.xlarge against S3, 2 GiB
/// per run: 10 workers gave 607 MB/s up and 744 MB/s down, 20 gave 1049/1152, 40 gave
/// 1096/1165, 80 gave 1072/1183, and 160 fell back to 1049/1078. The gain is gone by 40
/// and turns negative past 80, so 64 is the ceiling with room for the ramp to overshoot
/// slightly and settle.
const MAX_WORKERS: usize = 64;

pub struct Pool {
    /// How many workers may run right now.
    target: AtomicUsize,
    /// The hard ceiling.
    max: usize,
    /// Bytes moved so far, fed by the transfer code.
    bytes: AtomicU64,
    /// Jobs completed so far.
    ///
    /// This, not bytes, is what the controller steers on. Bytes are a useless signal for
    /// a directory of small files — throughput stays near zero however many workers are
    /// running, so a bytes-based controller concludes it is over-provisioned and ratchets
    /// itself down to a single worker. Completed jobs per second rises with concurrency
    /// in both regimes.
    units: AtomicU64,
    /// Set when the service asks us to slow down.
    throttled: AtomicBool,
    /// Set once the work is done, to stop the supervisor.
    done: AtomicBool,
}

impl Pool {
    /// `explicit` pins the worker count; otherwise the pool adapts.
    pub fn new(explicit: Option<usize>) -> Pool {
        let max = explicit.unwrap_or(MAX_WORKERS);
        // Start at the reference's fixed default rather than below it: adapting must
        // never make a short transfer slower than not adapting at all. The ramp tunes
        // upward from here, and throttling tunes it back down.
        let start = explicit.unwrap_or_else(|| DEFAULT_WORKERS.min(max));
        Pool {
            target: AtomicUsize::new(start),
            max,
            bytes: AtomicU64::new(0),
            units: AtomicU64::new(0),
            throttled: AtomicBool::new(false),
            done: AtomicBool::new(false),
        }
    }

    pub fn record_bytes(&self, count: u64) {
        self.bytes.fetch_add(count, Ordering::Relaxed);
    }

    fn record_unit(&self) {
        self.units.fetch_add(1, Ordering::Relaxed);
    }

    /// Called when a request came back throttled, so the next sample backs off.
    pub fn note_throttle(&self) {
        self.throttled.store(true, Ordering::Relaxed);
    }

    /// The current worker count, for tests and diagnostics.
    #[allow(dead_code)]
    pub fn workers(&self) -> usize {
        self.target.load(Ordering::Relaxed)
    }

    /// Run `work` over every job, adapting the worker count as it goes.
    pub fn run<J: Sync>(&self, jobs: &[J], adaptive: bool, work: impl Fn(&J) + Sync) {
        if jobs.is_empty() {
            return;
        }
        let next = AtomicUsize::new(0);
        let ceiling = self.max.min(jobs.len());

        self.done.store(false, Ordering::Relaxed);
        std::thread::scope(|outer| {
            if adaptive {
                outer.spawn(|| self.supervise(ceiling));
            }
            // The workers get their own scope so they are joined *before* `done` is set;
            // setting it after the outer scope would deadlock, since the outer scope is
            // itself waiting on the supervisor that watches the flag.
            std::thread::scope(|scope| {
            for id in 0..ceiling {
                let next = &next;
                let work = &work;
                scope.spawn(move || loop {
                    // Workers above the current target idle instead of exiting, so the
                    // pool can grow again without re-spawning.
                    if id >= self.target.load(Ordering::Relaxed) {
                        if next.load(Ordering::Relaxed) >= jobs.len() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                        continue;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else { return };
                    work(job);
                    self.record_unit();
                });
            }
            });
            self.done.store(true, Ordering::Relaxed);
        });
    }

    /// Sample throughput and steer the worker count.
    fn supervise(&self, ceiling: usize) {
        let mut last_units = 0u64;
        let mut best_rate = 0.0f64;
        let mut last_at = Instant::now();
        // Never drop below the reference's fixed count unless the service has actually
        // asked us to slow down. Adapting must not make anything slower than not
        // adapting; the ramp is there to go faster, not to second-guess the baseline.
        let mut floor = DEFAULT_WORKERS.min(ceiling);

        while !self.done.load(Ordering::Relaxed) {
            std::thread::sleep(SAMPLE_INTERVAL);
            if self.done.load(Ordering::Relaxed) {
                return;
            }

            let now = Instant::now();
            let units = self.units.load(Ordering::Relaxed);
            let elapsed = now.duration_since(last_at).as_secs_f64().max(0.001);
            let rate = (units - last_units) as f64 / elapsed;
            last_units = units;
            last_at = now;

            let current = self.target.load(Ordering::Relaxed);

            // Throttling is a direct instruction; obey it before looking at throughput.
            if self.throttled.swap(false, Ordering::Relaxed) {
                // Throttling overrides the floor: the service is the authority here.
                floor = 1;
                self.target.store((current / 2).max(1), Ordering::Relaxed);
                best_rate = 0.0;
                continue;
            }

            if rate > best_rate * IMPROVEMENT {
                // Still getting faster — add capacity, by half again rather than by a
                // fixed step. A 2 GiB upload on a fast link is over in two seconds, or
                // roughly thirteen samples; stepping by two from ten never reached the
                // optimum before the transfer ended, which is why adapting measured
                // slower than pinning 40. Growing multiplicatively gets there in four.
                best_rate = rate.max(best_rate);
                if current < ceiling {
                    self.target.store(grown(current, ceiling), Ordering::Relaxed);
                }
            } else if rate < best_rate * DEGRADATION && current > floor {
                // Slower than our best despite more workers: we have overshot.
                self.target.store(current - 1, Ordering::Relaxed);
                // Let the new, lower level set the benchmark rather than chasing a peak
                // we can no longer reach.
                best_rate = rate;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// Every job runs exactly once, whatever the worker count does.
    #[test]
    fn runs_every_job_once() {
        let jobs: Vec<u32> = (0..500).collect();
        let seen: Vec<AtomicU32> = (0..500).map(|_| AtomicU32::new(0)).collect();
        let pool = Pool::new(None);
        pool.run(&jobs, true, |job| {
            seen[*job as usize].fetch_add(1, Ordering::Relaxed);
            pool.record_bytes(1024);
        });
        for (index, count) in seen.iter().enumerate() {
            assert_eq!(count.load(Ordering::Relaxed), 1, "job {index}");
        }
    }

    #[test]
    fn an_empty_job_list_is_a_no_op() {
        let pool = Pool::new(None);
        let jobs: Vec<u32> = Vec::new();
        pool.run(&jobs, true, |_| panic!("should not run"));
    }

    /// The ramp has to arrive before the transfer ends. A fast 2 GiB transfer is about
    /// thirteen 150ms samples long, and the old fixed step of two reached 36 only at the
    /// very end -- measured as adapting being 45% slower than pinning 40 workers.
    #[test]
    fn the_ramp_reaches_the_measured_optimum_within_a_few_samples() {
        let mut workers = DEFAULT_WORKERS;
        let mut samples = 0;
        while workers < 40 {
            workers = grown(workers, MAX_WORKERS);
            samples += 1;
            assert!(samples <= 6, "still at {workers} workers after {samples} samples");
        }
        assert!(samples <= 4, "took {samples} samples to reach {workers}");
    }

    /// The ceiling is a property of the service and the link, not of the machine. Scaling
    /// it by core count capped a 4-vCPU instance at 16 workers, well under the measured
    /// optimum of 20-40.
    #[test]
    fn the_default_ceiling_does_not_depend_on_the_core_count() {
        let pool = Pool::new(None);
        assert_eq!(pool.max, MAX_WORKERS);
    }

    /// An explicit count pins the pool and disables the ramp.
    #[test]
    fn honours_an_explicit_worker_count() {
        let pool = Pool::new(Some(3));
        assert_eq!(pool.workers(), 3);
        let jobs: Vec<u32> = (0..50).collect();
        let peak = AtomicUsize::new(0);
        let running = AtomicUsize::new(0);
        pool.run(&jobs, false, |_| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
            running.fetch_sub(1, Ordering::SeqCst);
        });
        assert!(peak.load(Ordering::SeqCst) <= 3, "ran {} at once", peak.load(Ordering::SeqCst));
    }

    /// Throttling halves the worker count rather than waiting for throughput to sag.
    #[test]
    fn backs_off_when_throttled() {
        let pool = Pool::new(None);
        pool.target.store(16, Ordering::Relaxed);
        pool.note_throttle();
        // Drive one supervisor sample directly.
        let handle = std::thread::scope(|scope| {
            let done = scope.spawn(|| {
                std::thread::sleep(SAMPLE_INTERVAL * 2);
                pool.done.store(true, Ordering::Relaxed);
            });
            scope.spawn(|| pool.supervise(64));
            done.join().is_ok()
        });
        assert!(handle);
        assert!(pool.workers() <= 8, "target was {}", pool.workers());
    }

    /// The ceiling never exceeds the number of jobs, so ten workers are not spawned for
    /// two files.
    #[test]
    fn never_spawns_more_workers_than_jobs() {
        let pool = Pool::new(Some(32));
        let jobs: Vec<u32> = (0..2).collect();
        let peak = AtomicUsize::new(0);
        let running = AtomicUsize::new(0);
        pool.run(&jobs, false, |_| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(10));
            running.fetch_sub(1, Ordering::SeqCst);
        });
        assert!(peak.load(Ordering::SeqCst) <= 2);
    }
}
