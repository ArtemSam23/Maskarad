//! The adaptive limit of the short lane: a gradient controller in the spirit
//! of Netflix concurrency-limits `GradientLimit`, engaged only while the
//! runtime is short of CPU:
//!
//! ```text
//! L_new  = L × clamp(target / rtt_short, 0.5, 1) + sqrt(L)
//! L      = L + SMOOTHING × (L_new − L),  L ≥ rate_max × target,  min_limit ≤ L ≤ ceiling
//! target = max(target_floor, min(TOLERANCE × rtt_noload, TARGET_CAP × target_floor))
//! ```
//!
//! Why a gradient and not AIMD or Vegas. The overload we saw in production is
//! a queue for CPU: at a fixed arrival rate latency grows with the number of
//! requests let in, so the ratio of the target to the current latency says
//! how much to cut, and the cut is proportional instead of AIMD's fixed step.
//! Vegas estimates the queue from the same two numbers and needs alpha/beta
//! thresholds in requests on top; the gradient needs none.
//!
//! **Engaged or not.** Disengaged, the lane is bounded by the ceiling
//! (`server.max_inflight`) and nothing else. The limit engages when a woken
//! task waits for the CPU `LAG_CONGESTED_US` or longer, `ENGAGE_PROBES`
//! probes in a row (`limits::probe_lag`), and is released after the smoothed
//! wait has stayed under `LAG_CLEAR_US` for `EXIT_HOLD`; after `RAISE_AFTER`
//! of that it already leaves room for twice what the lane holds. Latency from
//! anything but the CPU is not ours to cut: a store that stopped answering
//! fills the lane with requests that wait for it while the CPU idles, and
//! fewer of them would not wait less. With the limit always on and 256 as its
//! ceiling, a 3.5 s Valkey pause at 625 requests/s lost 135-145 pairs to 429s;
//! the static 2048 lost none (measured in review).
//! Engaging starts the limit at twice what is in flight, and at least twice
//! what the recent peak rate needs at the target: under a sustained overload
//! the lane fills within tens of milliseconds and the gradient takes it from
//! there, while a backlog left by a stall drains without being shed.
//!
//! **When it moves.** Only in a window where demand kept up with throughput:
//! the lane did not shrink by more than it shed. A backlog that drains on its
//! own (the host took the CPU away, a store came back) keeps the limit, and
//! grows it if the limit sheds meanwhile; a standing queue (a closed-loop
//! client, a generator at its own in-flight cap) and an overload that the
//! limit sheds both count. Such a window is cut toward the target if the
//! probe saw the CPU wait `LAG_CLEAR_US` or longer during it, and grows
//! otherwise.
//!
//! `rtt_short` is an EWMA of window means: time from admission to response.
//! Single samples are not capped. A demask served from the L1 cache finishes
//! in one poll, tens of microseconds at any load; with half the samples like
//! that, a cap at 2 × target held the mean at the target while mask requests
//! waited 60 ms (measured, `maskarad bench` at 10 000 requests/s on one core).
//! What is filtered is the window mean: the EWMA takes the lower of the last
//! two, capped at `target / MIN_GRADIENT`, where the gradient is at its floor
//! anyway. A sustained queue passes unchanged. Without the filter one window
//! of 170 ms (the host took the CPU away for a moment, measured) cut the limit
//! from 60 to 8 over the next windows; it then took ten seconds to grow back
//! while the retries of everything shed meanwhile kept arriving, and p99 of
//! admitted requests went from 35 to 170-390 ms.
//!
//! `rtt_noload` is the mean latency with no queue of our own: the minimum of
//! window means over the last one to two `NOLOAD_PERIOD`s of clean windows,
//! disengaged, nothing shed and the lag under `LAG_CLEAR_US` throughout. A
//! minimum of single samples would not do: on `/process` the fastest request
//! is an L1 hit, and the mean of a normal mix is many times that without any
//! queue. The first version took every window whose lag was under
//! `LAG_CONGESTED_US` at the moment of the update. Under a controlled overload
//! the lag sits right at that threshold and dips under it now and then; the
//! mean of such a window, about the target, became the baseline, every period
//! rotation doubled the target, and within two minutes the limit was back at
//! the ceiling (measured in review: `rtt_noload` 0.16 → 26.6 ms). An overload
//! now contributes no windows at all, and the minimum, rotated only when a
//! window is observed, outlives it. `TARGET_CAP` bounds the damage of any
//! other contamination: a store that slow is not something a concurrency
//! limit can hide.
//!
//! `target_floor` (`server.admission.target_ms`) is the mean latency the
//! limit never tries to go below: a short request takes tenths of a
//! millisecond, and cutting at 2 × that would shed load nobody notices.
//!
//! And the limit is never cut below `rate_max × target`, where `rate_max` is
//! the completion rate of the lane with a memory of a few seconds. By Little's
//! law that many requests in flight at that rate wait `target` in our own
//! queue; if the latency is higher at that point, the rest is not ours (the
//! runtime busy shedding a retry wave, the host taking the CPU away), and
//! cutting further only loses throughput. Without this guard a disturbance of
//! a few hundred milliseconds took the limit from 67 to 9; it grew back over
//! ten seconds while everything shed meanwhile came back as retries, and p99
//! of admitted requests went from ~40 to 390 ms (measured on one core at 1.75×
//! overload). In a plain CPU overload the guard is below the point the
//! gradient settles at and changes nothing.

use std::time::Duration;

/// How often the limit is recomputed.
pub const UPDATE_INTERVAL: Duration = Duration::from_millis(100);
/// A window with fewer samples is extended until it has them: a mean of a
/// few requests is noise.
pub const MIN_SAMPLES: u64 = 10;
/// An idle runtime answers the probe in tens of microseconds, a loaded one
/// below capacity in up to ~0.3 ms (measured at 7 000 requests/s on one core
/// and 14 000 on two). A millisecond per wake-up is a queue for the CPU: a
/// `/process` request waits like that three times (to be admitted, after the
/// store read, after the write), which is most of the 5 ms target.
pub const LAG_CONGESTED_US: u64 = 1_000;
/// Below this the CPU has headroom: the limit is released after `EXIT_HOLD`
/// of it, and only windows below it feed `rtt_noload`. Half of
/// `LAG_CONGESTED_US`, so that a lag hovering at that threshold under a
/// controlled overload neither releases the limit nor counts as clean.
pub const LAG_CLEAR_US: u64 = 500;
/// Engaging also needs this many waits in a row at `LAG_CONGESTED_US` or
/// longer. A host that takes the CPU away for a moment (or a process next to
/// ours doing the same) shows up as one or two long waits, and the EWMA jumps
/// over the threshold on the first. Engaging on the EWMA alone shed 183
/// requests after one such stall at 4 000 requests/s, half the capacity of
/// the core (measured); the start was below the backlog, most likely because
/// the backlog was not in the lane yet. Three in a row is 15-20 ms: a backlog
/// that keeps the CPU busy that long has been admitted by then, and on one
/// core at 2× overload a real overload's queue grows by ~150 requests
/// meanwhile. With three, the same warm-up shed nothing.
pub const ENGAGE_PROBES: usize = 3;
/// A saturated CPU does not leave the lag under `LAG_CLEAR_US` for this
/// long, a finished overload does.
const EXIT_HOLD: Duration = Duration::from_secs(1);
/// After this long under `LAG_CLEAR_US` the limit keeps twice what the lane
/// holds until it is released: ten probes in a row with the CPU free, which
/// a saturated one does not give. Without it the store-stall test failed
/// now and then under a CPU quota: a pause of the quota engaged the limit
/// while the stalled store filled the lane, and with no request completing
/// nothing moved the limit, so the lane shed the rest of the pile-up until
/// the release.
const RAISE_AFTER: Duration = Duration::from_millis(50);
/// Weight of the newest window mean in `rtt_short`.
const RTT_ALPHA: f64 = 0.5;
const NOLOAD_PERIOD: Duration = Duration::from_secs(30);
const TOLERANCE: f64 = 2.0;
/// `TOLERANCE × rtt_noload` counts up to this many `target_floor`s.
const TARGET_CAP: f64 = 10.0;
/// One update takes at most half the limit away, so a burst of outliers does
/// not empty the lane.
const MIN_GRADIENT: f64 = 0.5;
const SMOOTHING: f64 = 0.5;
/// Engaging starts at this many times the demand of the moment.
const ENGAGE_HEADROOM: f64 = 2.0;
/// `rate_max` loses 1 % per 100 ms: after 7 s a faster past counts half. The
/// executor stalls seen on the bench lasted 2-3 s; with 3 % the floor halved
/// during one and the limit still fell from 57 to 12. A real loss of capacity
/// keeps the limit up to twice too high for those seconds, latency 2 × target.
const RATE_DECAY: f64 = 0.99;

/// What the short lane saw since the previous update.
#[derive(Clone, Copy, Debug)]
pub struct WindowStats {
    pub count: u64,
    pub sum_us: u64,
    /// Requests shed because the lane was at its bound.
    pub shed: u64,
    /// Requests in the lane at the end of the window.
    pub in_lane: usize,
    /// The highest smoothed lag the probe reported during the window, µs;
    /// 0 when nobody measures it.
    pub lag_peak_us: u64,
}

#[derive(Debug)]
pub struct Gradient {
    /// `None` while disengaged: the lane is bounded by `ceiling` only.
    limit: Option<f64>,
    min_limit: f64,
    ceiling: f64,
    target_floor_us: f64,
    rtt_short_us: Option<f64>,
    /// The previous window's mean, for the lower-of-two filter.
    prev_mean_us: Option<f64>,
    /// `in_lane` at the previous update.
    prev_in_lane: usize,
    noload: WindowedMin,
    /// Completions per second, the recent maximum decaying by `RATE_DECAY`.
    rate_max: f64,
    last_update: Option<Duration>,
    /// Since when the lag has stayed under `LAG_CLEAR_US`, while engaged.
    clear_since: Option<Duration>,
}

impl Gradient {
    pub fn new(min_limit: usize, ceiling: usize, target_floor: Duration) -> Self {
        Self {
            limit: None,
            min_limit: min_limit as f64,
            ceiling: ceiling.max(min_limit) as f64,
            target_floor_us: target_floor.as_secs_f64() * 1e6,
            rtt_short_us: None,
            prev_mean_us: None,
            prev_in_lane: 0,
            noload: WindowedMin::default(),
            rate_max: 0.0,
            last_update: None,
            clear_since: None,
        }
    }

    /// The bound of the lane: the adaptive limit while engaged, the ceiling
    /// otherwise.
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(self.ceiling) as usize
    }

    #[cfg(test)]
    fn engaged(&self) -> bool {
        self.limit.is_some()
    }

    pub fn rtt_short(&self) -> Option<Duration> {
        self.rtt_short_us.map(us_to_duration)
    }

    pub fn rtt_noload(&self) -> Option<Duration> {
        self.noload.get().map(us_to_duration)
    }

    fn target_us(&self) -> f64 {
        let floor = self.target_floor_us;
        self.noload.get().map_or(floor, |noload| {
            floor.max((TOLERANCE * noload).min(TARGET_CAP * floor))
        })
    }

    /// Feeds one reading of the probe: `lag_us` smoothed, `sustained_us` the
    /// shortest of the last `ENGAGE_PROBES` waits, with `in_lane` requests in
    /// the lane at that moment. Engages or releases the limit; the new bound
    /// when it changed.
    pub fn observe_lag(
        &mut self,
        lag_us: u64,
        sustained_us: u64,
        in_lane: usize,
        now: Duration,
    ) -> Option<usize> {
        match self.limit {
            None if lag_us >= LAG_CONGESTED_US && sustained_us >= LAG_CONGESTED_US => {
                let for_rate = self.rate_max * self.target_us() / 1e6;
                let start = ENGAGE_HEADROOM * (in_lane as f64).max(for_rate);
                self.limit = Some(start.clamp(self.min_limit, self.ceiling));
                self.prev_in_lane = in_lane;
                self.clear_since = None;
            }
            None => return None,
            Some(_) if lag_us >= LAG_CLEAR_US => {
                self.clear_since = None;
                return None;
            }
            Some(limit) => {
                let since = *self.clear_since.get_or_insert(now);
                let clear = now.saturating_sub(since);
                if clear >= EXIT_HOLD {
                    self.limit = None;
                    self.clear_since = None;
                } else if clear >= RAISE_AFTER {
                    // The CPU is free and the lane keeps filling: its requests
                    // wait for something else (a store that stopped
                    // answering), and no update comes while none of them
                    // completes.
                    let room =
                        (ENGAGE_HEADROOM * in_lane as f64).clamp(self.min_limit, self.ceiling);
                    if room <= limit {
                        return None;
                    }
                    self.limit = Some(room);
                } else {
                    return None;
                }
            }
        }
        Some(self.limit())
    }

    /// Feeds one window and returns the new bound.
    pub fn update(&mut self, w: WindowStats, now: Duration) -> usize {
        if w.count == 0 {
            return self.limit();
        }
        let mean = w.sum_us as f64 / w.count as f64;
        if self.limit.is_none() && w.shed == 0 && w.lag_peak_us < LAG_CLEAR_US {
            self.noload.observe(mean, now);
        }
        if let Some(last) = self.last_update {
            let elapsed = now.saturating_sub(last).as_secs_f64();
            if elapsed > 0.0 {
                let decay = RATE_DECAY.powf(elapsed / UPDATE_INTERVAL.as_secs_f64());
                self.rate_max = (w.count as f64 / elapsed).max(self.rate_max * decay);
            }
        }
        self.last_update = Some(now);
        let filtered = self
            .prev_mean_us
            .map_or(mean, |prev| prev.min(mean))
            .min(self.target_us() / MIN_GRADIENT);
        self.prev_mean_us = Some(mean);
        let rtt = match self.rtt_short_us {
            Some(prev) => prev + RTT_ALPHA * (filtered - prev),
            None => filtered,
        };
        self.rtt_short_us = Some(rtt);
        // Arrivals minus completions over the window: what the lane gained
        // plus what it turned away.
        let demand = w.in_lane as i64 - self.prev_in_lane as i64 + w.shed as i64;
        self.prev_in_lane = w.in_lane;

        let Some(limit) = self.limit else {
            return self.limit();
        };
        let gradient = if demand < 0 {
            if w.shed == 0 {
                return self.limit();
            }
            1.0
        } else if w.lag_peak_us >= LAG_CLEAR_US {
            (self.target_us() / rtt).clamp(MIN_GRADIENT, 1.0)
        } else {
            1.0
        };
        let proposed = limit * gradient + limit.sqrt();
        let floor = (self.rate_max * self.target_us() / 1e6).min(limit);
        let next = (limit + SMOOTHING * (proposed - limit))
            .max(floor)
            .clamp(self.min_limit, self.ceiling);
        self.limit = Some(next);
        self.limit()
    }
}

fn us_to_duration(us: f64) -> Duration {
    Duration::from_secs_f64(us / 1e6)
}

/// Minimum over the current and the previous period: a window of one to two
/// periods without keeping every sample.
#[derive(Debug, Default)]
struct WindowedMin {
    current: Option<f64>,
    previous: Option<f64>,
    period_started: Option<Duration>,
}

impl WindowedMin {
    fn observe(&mut self, value: f64, now: Duration) {
        let started = *self.period_started.get_or_insert(now);
        if now.saturating_sub(started) >= NOLOAD_PERIOD {
            self.previous = self.current.take();
            self.period_started = Some(now);
        }
        self.current = Some(self.current.map_or(value, |c| c.min(value)));
    }

    fn get(&self) -> Option<f64> {
        match (self.current, self.previous) {
            (Some(c), Some(p)) => Some(c.min(p)),
            (c, p) => c.or(p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: Duration = Duration::from_millis(5);
    const CONGESTED: u64 = 1_500;

    /// Engaged and at its bound, the lane full and the CPU congested: 50 more
    /// requests arrived than completed, and what the lane could not take
    /// after a cut was shed.
    fn overloaded(rtt_us: f64, g: &Gradient) -> WindowStats {
        let limit = g.limit();
        WindowStats {
            count: 100,
            sum_us: (100.0 * rtt_us) as u64,
            shed: (50 + g.prev_in_lane as i64 - limit as i64).max(0) as u64,
            in_lane: limit,
            lag_peak_us: CONGESTED,
        }
    }

    /// Engaged at `start` by a lag spike, from a lane holding `start / 2`.
    fn engaged(start: usize, ceiling: usize) -> (Gradient, Duration) {
        let mut g = Gradient::new(8, ceiling, FLOOR);
        let t = Duration::from_secs(1);
        assert_eq!(
            g.observe_lag(CONGESTED, CONGESTED, start / 2, t),
            Some(start)
        );
        (g, t)
    }

    /// `n` updates 100 ms apart after `t`; the latency of each window is
    /// `plant(limit)`, the lane full and shedding, the CPU congested. The
    /// latencies fed.
    fn run(g: &mut Gradient, t: &mut Duration, n: usize, plant: impl Fn(f64) -> f64) -> Vec<f64> {
        let mut seen = Vec::with_capacity(n);
        for _ in 0..n {
            *t += UPDATE_INTERVAL;
            let rtt = plant(g.limit() as f64);
            seen.push(rtt);
            let w = overloaded(rtt, g);
            g.update(w, *t);
        }
        seen
    }

    /// Clean windows of `rtt_us` while disengaged.
    fn idle(g: &mut Gradient, t: &mut Duration, n: usize, rtt_us: u64) {
        for _ in 0..n {
            *t += UPDATE_INTERVAL;
            g.update(
                WindowStats {
                    count: 100,
                    sum_us: 100 * rtt_us,
                    shed: 0,
                    in_lane: 2,
                    lag_peak_us: 60,
                },
                *t,
            );
        }
    }

    #[test]
    fn engages_on_congestion_and_releases_after_a_clear_second() {
        let mut g = Gradient::new(8, 2048, FLOOR);
        let mut t = Duration::ZERO;
        idle(&mut g, &mut t, 20, 300);
        assert_eq!(g.limit(), 2048, "disengaged: the ceiling");
        assert_eq!(g.observe_lag(900, 900, 40, t), None, "under the threshold");
        assert_eq!(g.observe_lag(1_200, 300, 40, t), None, "one long wait");

        // 1 000 completions/s at the 5 ms target need 5 in flight; 40 are.
        assert_eq!(g.observe_lag(1_200, 1_200, 40, t), Some(80));
        assert!(g.engaged());
        let ms = |n: u64| Duration::from_millis(n);
        assert_eq!(g.observe_lag(400, 400, 40, t + ms(100)), None);
        assert_eq!(
            g.observe_lag(700, 700, 40, t + ms(600)),
            None,
            "not clear yet"
        );
        assert_eq!(g.observe_lag(400, 400, 40, t + ms(700)), None);
        assert_eq!(
            g.observe_lag(400, 400, 40, t + ms(1_600)),
            None,
            "0.9 s clear"
        );
        assert_eq!(g.observe_lag(400, 400, 40, t + ms(1_700)), Some(2048));
        assert!(!g.engaged());

        // Engaged, then the CPU goes free while the lane keeps filling (a
        // stalled store): after 50 ms the limit makes room, and never cuts.
        assert_eq!(g.observe_lag(1_200, 1_200, 40, t), Some(80));
        assert_eq!(g.observe_lag(100, 100, 70, t + ms(10)), None, "10 ms clear");
        assert_eq!(g.observe_lag(100, 100, 70, t + ms(60)), Some(140));
        assert_eq!(g.observe_lag(100, 100, 60, t + ms(70)), None);
        assert_eq!(g.observe_lag(100, 100, 90, t + ms(80)), Some(180));
        assert!(g.engaged());

        let mut g = Gradient::new(8, 64, FLOOR);
        assert_eq!(g.observe_lag(5_000, 5_000, 1, t), Some(8), "min_limit");
        let mut g = Gradient::new(8, 64, FLOOR);
        assert_eq!(g.observe_lag(5_000, 5_000, 500, t), Some(64), "the ceiling");
    }

    /// Stalls: the host took the CPU away, a store came back. The backlog
    /// drains faster than requests arrive, and the limit waits for it
    /// instead of shedding it.
    #[test]
    fn a_draining_backlog_keeps_the_limit() {
        let mut g = Gradient::new(8, 4096, FLOOR);
        let mut t = Duration::from_secs(1);
        let mut in_lane = 1400;
        assert_eq!(g.observe_lag(20_000, 20_000, in_lane, t), Some(2800));
        for _ in 0..10 {
            t += UPDATE_INTERVAL;
            in_lane -= 100;
            let w = WindowStats {
                count: 1500,
                sum_us: 1500 * 80_000,
                shed: 0,
                in_lane,
                lag_peak_us: 20_000,
            };
            assert_eq!(g.update(w, t), 2800, "{in_lane} left");
        }
        // Shedding while it drains (a limit below the backlog): it grows
        // rather than cuts.
        t += UPDATE_INTERVAL;
        let w = WindowStats {
            count: 1500,
            sum_us: 1500 * 80_000,
            shed: 30,
            in_lane: in_lane - 100,
            lag_peak_us: 20_000,
        };
        assert!(g.update(w, t) > 2800);
    }

    #[test]
    fn growing_latency_shrinks_the_limit_and_recovery_grows_it() {
        let (mut g, mut t) = engaged(256, 256);
        run(&mut g, &mut t, 5, |_| 300.0);
        assert_eq!(g.limit(), 256, "fast and full: stays at the ceiling");

        run(&mut g, &mut t, 10, |_| 50_000.0);
        let shrunk = g.limit();
        assert!(shrunk < 64, "50 ms against a 5 ms target: {shrunk}");
        run(&mut g, &mut t, 10, |_| 50_000.0);
        let lower = g.limit();
        assert!(lower < shrunk, "keeps shrinking: {lower}");

        run(&mut g, &mut t, 10, |_| 300.0);
        let grown = g.limit();
        assert!(grown > lower, "latency back to normal: {lower} -> {grown}");
        run(&mut g, &mut t, 100, |_| 300.0);
        assert_eq!(g.limit(), 256, "back at the ceiling");
    }

    #[test]
    fn limit_is_clamped_between_min_and_ceiling() {
        let (mut g, mut t) = engaged(64, 64);
        run(&mut g, &mut t, 200, |_| 10_000_000.0);
        assert_eq!(g.limit(), 8);
        run(&mut g, &mut t, 200, |_| 100.0);
        assert_eq!(g.limit(), 64);
    }

    /// A closed-loop client: the same requests in the lane window after
    /// window, nothing shed, a queue for the CPU. It is cut like an overload.
    #[test]
    fn a_standing_queue_is_cut() {
        let (mut g, mut t) = engaged(128, 128);
        for _ in 0..30 {
            t += UPDATE_INTERVAL;
            let w = WindowStats {
                count: 250,
                sum_us: 250 * 40_000,
                shed: 0,
                in_lane: 100,
                lag_peak_us: CONGESTED,
            };
            g.update(w, t);
        }
        assert!(g.limit() < 64, "limit {}", g.limit());
    }

    /// The store answers in 20 ms while the CPU is free: those windows make
    /// the target 40 ms, and an engaged limit is not cut at 20 ms.
    #[test]
    fn latency_without_congestion_does_not_cut_the_limit() {
        let mut g = Gradient::new(8, 2048, FLOOR);
        let mut t = Duration::ZERO;
        idle(&mut g, &mut t, 20, 20_000);
        assert_eq!(g.rtt_noload(), Some(Duration::from_millis(20)));
        assert_eq!(g.observe_lag(CONGESTED, CONGESTED, 64, t), Some(128));
        let before = g.limit();
        run(&mut g, &mut t, 20, |_| 20_000.0);
        assert!(
            g.limit() >= before,
            "congested, but 20 ms is within 2 × 20 ms: {before} -> {}",
            g.limit()
        );
        // Shedding with the CPU free (lag under LAG_CLEAR_US): grows.
        let before = g.limit();
        for _ in 0..5 {
            t += UPDATE_INTERVAL;
            let mut w = overloaded(90_000.0, &g);
            w.lag_peak_us = 100;
            g.update(w, t);
        }
        assert!(g.limit() > before, "{before} -> {}", g.limit());
    }

    #[test]
    fn noload_minimum_expires_after_two_periods() {
        let mut g = Gradient::new(8, 256, FLOOR);
        let clean = |rtt_us: u64| WindowStats {
            count: 10,
            sum_us: 10 * rtt_us,
            shed: 0,
            in_lane: 0,
            lag_peak_us: 0,
        };
        g.update(clean(400), Duration::ZERO);
        g.update(clean(9_000), Duration::from_secs(31));
        assert_eq!(
            g.rtt_noload(),
            Some(Duration::from_micros(400)),
            "previous period still counts"
        );
        g.update(clean(9_000), Duration::from_secs(62));
        assert_eq!(g.rtt_noload(), Some(Duration::from_millis(9)));
    }

    /// Five minutes of an overload the limit holds at the target, the lag
    /// hovering around `LAG_CONGESTED_US` and dipping under it every few
    /// windows, as measured in review. None of those windows is a baseline:
    /// `rtt_noload`, the target and the limit stay where the first seconds
    /// put them.
    #[test]
    fn a_long_overload_does_not_move_the_baseline() {
        let mut g = Gradient::new(8, 2048, FLOOR);
        let mut t = Duration::ZERO;
        idle(&mut g, &mut t, 50, 160);
        let baseline = g.rtt_noload();
        assert_eq!(baseline, Some(Duration::from_micros(160)));
        assert!(g.observe_lag(CONGESTED, CONGESTED, 100, t).is_some());

        let mut limits = Vec::new();
        for i in 0..3_000 {
            t += UPDATE_INTERVAL;
            let limit = g.limit();
            let rtt = limit as f64 * 110.0;
            let lag = if i % 4 == 0 { 900 } else { 1_400 };
            if i % 7 == 0 {
                // A dip under LAG_CLEAR_US too short to release the limit.
                g.observe_lag(300, 300, limit, t);
                g.observe_lag(1_400, 1_400, limit, t + Duration::from_millis(50));
            } else {
                g.observe_lag(lag, lag, limit, t);
            }
            let w = WindowStats {
                lag_peak_us: lag,
                ..overloaded(rtt, &g)
            };
            limits.push(g.update(w, t));
        }
        assert!(g.engaged());
        assert_eq!(g.rtt_noload(), baseline);
        let late = &limits[limits.len() - 600..];
        let (lo, hi) = late
            .iter()
            .fold((usize::MAX, 0), |(lo, hi), &l| (lo.min(l), hi.max(l)));
        assert!(
            lo >= 30 && hi <= 80,
            "limit over the last minute between {lo} and {hi}"
        );
    }

    #[test]
    fn the_target_is_capped() {
        let mut g = Gradient::new(8, 256, FLOOR);
        let mut t = Duration::ZERO;
        idle(&mut g, &mut t, 20, 400_000);
        assert_eq!(g.target_us(), 50_000.0, "10 × 5 ms, not 2 × 400 ms");
    }

    /// Half the requests finish in one poll (a demask from the L1 cache) and
    /// never see the queue; the other half wait 60 ms behind it. The mean is
    /// 30 ms, and the limit has to come down.
    #[test]
    fn requests_that_skip_the_queue_do_not_hide_it() {
        let (mut g, mut t) = engaged(256, 256);
        for _ in 0..10 {
            t += UPDATE_INTERVAL;
            let w = WindowStats {
                count: 1000,
                sum_us: 500 * 20 + 500 * 60_000,
                ..overloaded(0.0, &g)
            };
            g.update(w, t);
        }
        assert!(g.limit() < 64, "limit {}", g.limit());
    }

    /// One window of 170 ms among 4 ms ones against a 5 ms target (the host
    /// took the CPU away for a moment): the limit stays where it was.
    #[test]
    fn a_single_slow_window_does_not_cut_the_limit() {
        let (mut g, mut t) = engaged(128, 128);
        run(&mut g, &mut t, 20, |_| 4_000.0);
        assert_eq!(g.limit(), 128);
        run(&mut g, &mut t, 1, |_| 170_000.0);
        run(&mut g, &mut t, 5, |_| 4_000.0);
        assert_eq!(g.limit(), 128);
        run(&mut g, &mut t, 3, |_| 170_000.0);
        assert!(g.limit() < 128, "three in a row is a queue: {}", g.limit());
    }

    /// A queue for the CPU behind work the limit does not control: 5.5 ms of it,
    /// plus 10 µs per request let in; throughput follows by Little's law.
    /// Then two seconds of 60 ms windows (the executor stalled, as seen on the
    /// bench).
    /// The limit dips and comes back instead of falling to the floor.
    #[test]
    fn a_disturbance_does_not_empty_the_lane() {
        let (mut g, mut t) = engaged(256, 256);
        let feed = |g: &mut Gradient, t: &mut Duration, rtt_us: f64| {
            *t += UPDATE_INTERVAL;
            let limit = g.limit();
            let count = (limit as f64 / (rtt_us / 1e6) * UPDATE_INTERVAL.as_secs_f64()) as u64;
            let w = WindowStats {
                count,
                sum_us: (count as f64 * rtt_us) as u64,
                ..overloaded(0.0, g)
            };
            g.update(w, *t);
        };
        for _ in 0..300 {
            let rtt = 5_500.0 + 10.0 * g.limit() as f64;
            feed(&mut g, &mut t, rtt);
        }
        let before = g.limit();
        let mut lowest = before;
        for _ in 0..20 {
            feed(&mut g, &mut t, 60_000.0);
            lowest = lowest.min(g.limit());
        }
        for _ in 0..30 {
            let rtt = 5_500.0 + 10.0 * g.limit() as f64;
            feed(&mut g, &mut t, rtt);
            lowest = lowest.min(g.limit());
        }
        assert!(
            lowest as f64 >= 0.6 * before as f64,
            "{before} fell to {lowest}"
        );
        assert!(
            g.limit() as f64 >= 0.85 * before as f64,
            "{before} -> {}",
            g.limit()
        );
    }

    /// One core, CPU-bound requests: with the lane full, latency is the limit
    /// times the CPU time of a request. The limit has to find the point where
    /// the mean latency is near the target, not the floor of 8 and not 256.
    #[test]
    fn converges_near_the_target_on_a_saturated_cpu() {
        let (mut g, mut t) = engaged(256, 256);
        let seen = run(&mut g, &mut t, 100, |limit| limit * 110.0);
        let settled = &seen[50..];
        let (lo, hi) = settled
            .iter()
            .fold((f64::MAX, 0f64), |(lo, hi), &r| (lo.min(r), hi.max(r)));
        assert!(
            4_000.0 <= lo && hi <= 7_500.0,
            "latency after 5 s between {lo} and {hi} µs, limit {}",
            g.limit()
        );
    }
}
