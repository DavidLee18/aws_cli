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
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// How often the supervisor re-measures.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(150);
/// How long an idle worker waits before re-checking the target unprompted.
///
/// A backstop against a missed wakeup, not the mechanism -- growth notifies. Measured on
/// this machine, 64 threads idling for 2s: a 25ms poll loop costs 94.6ms of CPU, the
/// condvar with this backstop costs 11.4ms.
const IDLE_BACKSTOP: Duration = Duration::from_millis(250);
/// Ramp while throughput improves by at least this fraction.
const IMPROVEMENT: f64 = 1.05;
/// Back off when it falls by more than this fraction.
const DEGRADATION: f64 = 0.90;
/// The reference's fixed worker count, used as our floor rather than our ceiling.
const DEFAULT_WORKERS: usize = 10;
/// The next worker count while throughput is still improving.
fn grown(current: usize, ceiling: usize, growth_percent: usize) -> usize {
    let step = (current * growth_percent.saturating_sub(100) / 100).max(2);
    (current + step).min(ceiling)
}

/// How the ramp is shaped. The defaults are the shipped behaviour; the environment
/// variables exist so the shape can be swept in-region rather than argued about, because
/// the ramp is measurably the pool's remaining cost -- an adaptive transfer runs 18% below
/// a pinned 64 on a 2.5s upload, purely because it is still climbing when the work ends.
#[derive(Clone, Copy, Debug)]
struct Tuning {
    /// Workers at the start of a transfer.
    start: usize,
    /// Percent of the current count to grow to, e.g. 150 for x1.5. Always at least +2.
    growth_percent: usize,
    /// Consecutive degrading samples required before giving capacity back. At 1 -- the
    /// shipped value -- a single noisy sample undoes a step of the climb, which the
    /// in-region traces show happening twice on the way up.
    patience: usize,
}

impl Default for Tuning {
    fn default() -> Tuning {
        Tuning { start: DEFAULT_WORKERS, growth_percent: 150, patience: 1 }
    }
}

impl Tuning {
    fn from_environment() -> Tuning {
        let read = |name: &str, fallback: usize| {
            std::env::var(name).ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(fallback)
        };
        let d = Tuning::default();
        Tuning {
            start: read("AWSC_POOL_START", d.start),
            growth_percent: read("AWSC_POOL_GROWTH", d.growth_percent),
            patience: read("AWSC_POOL_PATIENCE", d.patience),
        }
    }
}

/// How the controller gives capacity back when throughput degrades.
///
/// Back off by one worker per sample.
///
/// A proportional back-off (a quarter of the current count) was carried alongside this
/// one and replayed against the measured curve: it holds 9% fewer workers for equal
/// throughput once the samples are noisy, which is worth having only if connections
/// provoke throttling. Measured in-region against real S3 -- 30 transfers of 2 GiB at up
/// to 64 connections -- they do not: zero SlowDown responses, counting the ones retry
/// absorbs. The throttle branch below halves unconditionally and never consults this
/// function, so the proportional variant could not have changed the throttled path
/// either. It was removed rather than left switched off.
fn shrunk(current: usize, floor: usize) -> usize {
    current.saturating_sub(1).max(floor)
}

/// What the controller carries between samples.
#[derive(Clone, Copy, Debug)]
struct Control {
    target: usize,
    best_rate: f64,
    floor: usize,
    /// Consecutive samples that came in below `DEGRADATION`, for `Tuning::patience`.
    degrading: usize,
}

/// One control decision, split out from the timing and the atomics so it can be replayed
/// against a measured throughput curve instead of argued about. `supervise` is this
/// function plus a clock.
fn decide(state: Control, rate: f64, ceiling: usize, throttled: bool, tuning: Tuning) -> Control {
    // Throttling is a direct instruction; obey it before looking at throughput.
    if throttled {
        // Throttling overrides the floor: the service is the authority here.
        return Control { target: (state.target / 2).max(1), best_rate: 0.0, floor: 1, degrading: 0 };
    }
    if rate > state.best_rate * IMPROVEMENT {
        let target = if state.target < ceiling {
            grown(state.target, ceiling, tuning.growth_percent)
        } else {
            state.target
        };
        return Control {
            target,
            best_rate: rate.max(state.best_rate),
            degrading: 0,
            ..state
        };
    }
    if rate < state.best_rate * DEGRADATION && state.target > state.floor {
        // Slower than our best despite more workers: we may have overshot. Wait for
        // `patience` consecutive such samples before believing it, so that one noisy
        // sample cannot undo a step of the climb.
        let degrading = state.degrading + 1;
        if degrading < tuning.patience {
            return Control { degrading, ..state };
        }
        // Let the new, lower level set the benchmark rather than chasing a peak we can no
        // longer reach.
        return Control {
            target: shrunk(state.target, state.floor),
            best_rate: rate,
            degrading: 0,
            ..state
        };
    }
    Control { degrading: 0, ..state }
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
    /// Whether to report every control decision on stderr, from `AWSC_POOL_TRACE`.
    ///
    /// The pool's worker count and its throttle events are otherwise unobservable from
    /// outside the process, which makes questions like "does this ceiling provoke
    /// SlowDown from S3" unanswerable except by guessing.
    trace: bool,
    /// Wakes workers idling above the target when it grows, or when the jobs run out.
    /// The counter exists to give the condvar a mutex to pair with; only the signal
    /// matters.
    wake: (Mutex<u64>, Condvar),
    /// The ramp's shape, from the environment.
    tuning: Tuning,
}

impl Pool {
    /// `explicit` pins the worker count; otherwise the pool adapts.
    pub fn new(explicit: Option<usize>) -> Pool {
        let max = explicit.unwrap_or(MAX_WORKERS);
        // Start at the reference's fixed default rather than below it: adapting must
        // never make a short transfer slower than not adapting at all. The ramp tunes
        // upward from here, and throttling tunes it back down.
        let tuning = Tuning::from_environment();
        let start = explicit.unwrap_or_else(|| tuning.start.min(max));
        Pool {
            target: AtomicUsize::new(start),
            max,
            bytes: AtomicU64::new(0),
            units: AtomicU64::new(0),
            throttled: AtomicBool::new(false),
            done: AtomicBool::new(false),
            trace: std::env::var_os("AWSC_POOL_TRACE").is_some(),
            wake: (Mutex::new(0), Condvar::new()),
            tuning,
        }
    }

    /// Let workers idling above the target re-check it.
    fn wake_idle(&self) {
        let (lock, cv) = &self.wake;
        *lock.lock().expect("mutex") += 1;
        cv.notify_all();
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
                        // Wait to be told the target grew rather than polling for it.
                        let (lock, cv) = &self.wake;
                        let guard = lock.lock().expect("mutex");
                        let _ = cv.wait_timeout(guard, IDLE_BACKSTOP).expect("mutex");
                        continue;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        // The jobs are gone; anyone still idling should stop waiting.
                        self.wake_idle();
                        return;
                    };
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
        let mut degrading = 0usize;

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

            let state = Control {
                target: self.target.load(Ordering::Relaxed),
                best_rate,
                floor,
                degrading,
            };
            let throttled = self.throttled.swap(false, Ordering::Relaxed);
            let next = decide(state, rate, ceiling, throttled, self.tuning);
            self.target.store(next.target, Ordering::Relaxed);
            if next.target > state.target {
                self.wake_idle();
            }
            if self.trace && (throttled || next.target != state.target) {
                eprintln!(
                    "pool: workers {} -> {} rate {rate:.1}/s{}",
                    state.target,
                    next.target,
                    if throttled { " THROTTLED" } else { "" }
                );
            }
            best_rate = next.best_rate;
            floor = next.floor;
            degrading = next.degrading;
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

    /// Throughput at a given worker count, in MB/s, from the in-region measurement on a
    /// c7g.xlarge against S3 (2 GiB per run, uploads): 10 -> 607, 20 -> 1049, 40 -> 1096,
    /// 80 -> 1072, 160 -> 1049. Linearly interpolated between the measured points, held
    /// flat outside them. This is the plant the controller is steering; using the real
    /// curve is what makes the simulation evidence rather than an opinion.
    fn measured_rate(workers: usize) -> f64 {
        const CURVE: [(usize, f64); 5] =
            [(10, 607.0), (20, 1049.0), (40, 1096.0), (80, 1072.0), (160, 1049.0)];
        let w = workers as f64;
        if workers <= CURVE[0].0 {
            // Below the measured floor, assume it scales down proportionally: one worker
            // cannot be doing 607 MB/s.
            return CURVE[0].1 * w / CURVE[0].0 as f64;
        }
        for pair in CURVE.windows(2) {
            let ((w0, r0), (w1, r1)) = (pair[0], pair[1]);
            if workers <= w1 {
                let t = (w - w0 as f64) / (w1 - w0) as f64;
                return r0 + (r1 - r0) * t;
            }
        }
        CURVE[CURVE.len() - 1].1
    }

    /// Replay the controller against that curve and report the worker trajectory.
    fn replay(samples: usize, throttle_at: Option<usize>) -> Vec<usize> {
        replay_tuned(Tuning::default(), samples, throttle_at)
    }

    fn replay_tuned(tuning: Tuning, samples: usize, throttle_at: Option<usize>) -> Vec<usize> {
        let ceiling = MAX_WORKERS;
        let mut state = Control {
            target: tuning.start,
            best_rate: 0.0,
            floor: DEFAULT_WORKERS,
            degrading: 0,
        };
        let mut trace = vec![state.target];
        for i in 0..samples {
            let throttled = throttle_at == Some(i);
            // The rate the plant returns for the worker count currently in force.
            let rate = measured_rate(state.target);
            state = decide(state, rate, ceiling, throttled, tuning);
            trace.push(state.target);
        }
        trace
    }

    /// The same replay with jitter on each sample. A noiseless curve never overshoots the
    /// knee, so the degradation branch never fires on one; sampling noise is what pushes
    /// `best_rate` above what the plant can repeat, and only then does the pool back off
    /// at all. This is the replay that chose the back-off shape.
    fn replay_noisy(samples: usize, percent: u64) -> Vec<usize> {
        replay_noisy_tuned(Tuning::default(), samples, percent)
    }

    fn replay_noisy_tuned(tuning: Tuning, samples: usize, percent: u64) -> Vec<usize> {
        let ceiling = MAX_WORKERS;
        let mut state = Control {
            target: tuning.start,
            best_rate: 0.0,
            floor: DEFAULT_WORKERS,
            degrading: 0,
        };
        let mut trace = vec![state.target];
        // A fixed LCG, so the comparison between back-offs sees identical noise.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..samples {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let swing = (seed >> 33) % (2 * percent + 1);
            let factor = 1.0 + (swing as f64 - percent as f64) / 100.0;
            state = decide(state, measured_rate(state.target) * factor, ceiling, false, tuning);
            trace.push(state.target);
        }
        trace
    }

    /// With noisy samples, does the pool oscillate badly, and what does it sustain?
    #[test]
    fn the_pool_holds_its_level_on_noisy_samples() {
        for percent in [5u64, 15] {
            let trace = replay_noisy(200, percent);
            let tail = &trace[100..];
            let lo = *tail.iter().min().expect("non-empty");
            let hi = *tail.iter().max().expect("non-empty");
            let mean_rate: f64 =
                tail.iter().map(|w| measured_rate(*w)).sum::<f64>() / tail.len() as f64;
            let mean_workers: f64 =
                tail.iter().map(|w| *w as f64).sum::<f64>() / tail.len() as f64;
            println!(
                "noise +/-{percent}%: workers {lo}-{hi} (mean {mean_workers:.1}), sustained {mean_rate:.0} MB/s"
            );
            assert!(lo >= DEFAULT_WORKERS, "fell to {lo}, under the floor");
        }
    }

    /// Where the controller settles, and how long it takes, against the measured curve.
    /// The optimum is 40-80 workers; anything that parks far above that is running hotter
    /// than the link rewards, and anything far below is leaving throughput unused.
    #[test]
    fn the_controller_settles_near_the_measured_optimum() {
        let trace = replay(40, None);
        let settled = *trace.last().expect("non-empty");
        assert!((20..=MAX_WORKERS).contains(&settled), "settled at {settled}: {trace:?}");
        println!("settled at {settled} workers ({:.0} MB/s)", measured_rate(settled));
    }

    /// After the service throttles, how long before throughput is back?
    ///
    /// Measured in throughput, not in worker count: the curve is flat from ~36 workers
    /// up, so "back to 40 workers" is the wrong question -- 36 workers already deliver 99%
    /// of what 40 do, and the controller correctly stops climbing there.
    ///
    /// Caveat on the model: the plant is quasi-static, i.e. the rate is assumed to follow
    /// the worker count within one sample. Real throughput lags, so these sample counts
    /// are a lower bound on recovery, useful for comparing the two back-offs against each
    /// other rather than as absolute timings.
    #[test]
    fn recovery_after_a_throttle_is_measured_not_assumed() {
        let peak = measured_rate(40);
        let trace = replay(60, Some(12));
        let recovered =
            trace[13..].iter().position(|w| measured_rate(*w) >= peak * 0.95).map(|i| i + 1);
        let settled = *trace.last().expect("non-empty");
        println!(
            "pre-throttle {}, cut to {}, {:?} samples to 95% of peak, settled {settled}",
            trace[12], trace[13], recovered
        );
        assert!(recovered.is_some(), "never regained 95% of peak");
    }

    /// The ramp has to arrive before the transfer ends. A fast 2 GiB transfer is about
    /// thirteen 150ms samples long, and the old fixed step of two reached 36 only at the
    /// very end -- measured as adapting being 45% slower than pinning 40 workers.
    #[test]
    fn the_ramp_reaches_the_measured_optimum_within_a_few_samples() {
        let tuning = Tuning::default();
        let mut workers = tuning.start;
        let mut samples = 0;
        while workers < 40 {
            workers = grown(workers, MAX_WORKERS, tuning.growth_percent);
            samples += 1;
            assert!(samples <= 6, "still at {workers} workers after {samples} samples");
        }
        assert!(samples <= 4, "took {samples} samples to reach {workers}");
    }

    /// How long each ramp shape takes to arrive, replayed against the measured curve with
    /// noisy samples. This is the cheap half of the ramp question: it says which shapes
    /// are worth spending in-region link time on, and it cannot answer what a fast ramp
    /// costs on a slow link, because the curve it replays is a fast one.
    #[test]
    fn ramp_shapes_reach_the_optimum_in_different_numbers_of_samples() {
        let peak = measured_rate(40);
        let shapes = [
            ("shipped 10/150%/1", Tuning::default()),
            ("start 32", Tuning { start: 32, ..Tuning::default() }),
            ("growth 200%", Tuning { growth_percent: 200, ..Tuning::default() }),
            ("patience 2", Tuning { patience: 2, ..Tuning::default() }),
            ("start 32 + patience 2", Tuning { start: 32, patience: 2, ..Tuning::default() }),
        ];
        // A 2.5s transfer is about sixteen 150ms samples. What it is paid is the *mean*
        // rate over its whole life, not the rate it eventually reaches -- the in-region
        // run measured adapting at 18% below a pinned 64 while the target itself arrived
        // in well under a second.
        const SHORT: usize = 16;
        for (name, tuning) in shapes {
            let trace = replay_noisy_tuned(tuning, 40, 15);
            let arrived = trace.iter().position(|w| measured_rate(*w) >= peak * 0.95);
            let short: f64 =
                trace[..SHORT].iter().map(|w| measured_rate(*w)).sum::<f64>() / SHORT as f64;
            println!(
                "{name}: {:?} samples to 95% of peak, {short:.0} MB/s over a {}-sample transfer ({:.0}% of pinned peak), trace {:?}",
                arrived,
                SHORT,
                100.0 * short / peak,
                &trace[..6.min(trace.len())]
            );
            assert!(arrived.is_some(), "{name} never reached 95% of peak");
        }
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
