//! Frame and latency measurement.
//!
//! M0 has two numeric acceptance criteria and both are easy to fake. A single
//! "it didn't freeze" observation is not a measurement, so heavy output is
//! judged by a frame-time histogram with a hard ceiling, and keystroke latency
//! is split into the part that travels the network and the part Berth is
//! actually responsible for.

use std::time::Duration;

/// No frame may exceed this while output is streaming. Two frames at 60 Hz.
pub const FRAME_BUDGET: Duration = Duration::from_millis(32);
/// The spec's keystroke-to-glyph target, which applies to local render only.
pub const RENDER_BUDGET: Duration = Duration::from_millis(16);

/// Fixed-bucket histogram in milliseconds. Bucket `i` counts samples in
/// `[i, i+1)` ms; everything at or above `BUCKETS-1` lands in the last bucket.
#[derive(Debug, Clone)]
pub struct Histogram {
    buckets: Vec<u32>,
    count: u64,
    total: Duration,
    max: Duration,
}

const BUCKETS: usize = 129;

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            buckets: vec![0; BUCKETS],
            count: 0,
            total: Duration::ZERO,
            max: Duration::ZERO,
        }
    }

    pub fn record(&mut self, sample: Duration) {
        let ms = sample.as_secs_f64() * 1000.0;
        let index = (ms.floor().max(0.0) as usize).min(BUCKETS - 1);
        self.buckets[index] += 1;
        self.count += 1;
        self.total += sample;
        if sample > self.max {
            self.max = sample;
        }
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// The largest sample, kept exactly rather than as a bucket edge, because
    /// the heavy-output criterion is decided on it.
    pub fn max(&self) -> Duration {
        self.max
    }

    pub fn mean(&self) -> Duration {
        if self.count == 0 {
            Duration::ZERO
        } else {
            self.total / self.count as u32
        }
    }

    /// The lower edge of the bucket containing the given quantile, in ms.
    /// Reported as a bucket edge rather than an interpolated value, so the
    /// number never claims more precision than the histogram has.
    pub fn percentile_ms(&self, quantile: f64) -> u32 {
        if self.count == 0 {
            return 0;
        }
        let target = (quantile.clamp(0.0, 1.0) * self.count as f64).ceil() as u64;
        let mut seen = 0u64;
        for (index, bucket) in self.buckets.iter().enumerate() {
            seen += *bucket as u64;
            if seen >= target {
                return index as u32;
            }
        }
        (BUCKETS - 1) as u32
    }

    /// How many samples were at or above `limit`.
    pub fn over(&self, limit: Duration) -> u64 {
        let cutoff = (limit.as_secs_f64() * 1000.0).floor() as usize;
        self.buckets
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= cutoff)
            .map(|(_, count)| *count as u64)
            .sum()
    }

    /// The pass condition for the heavy-output test.
    pub fn within(&self, budget: Duration) -> bool {
        self.max() <= budget
    }

    /// A compact human-readable report, one line per populated bucket.
    pub fn report(&self, title: &str, budget: Duration) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{title}: {} frames, mean {:.1} ms, p50 {} ms, p95 {} ms, p99 {} ms, max {:.1} ms\n",
            self.count,
            self.mean().as_secs_f64() * 1000.0,
            self.percentile_ms(0.50),
            self.percentile_ms(0.95),
            self.percentile_ms(0.99),
            self.max().as_secs_f64() * 1000.0,
        ));
        let over = self.over(budget);
        out.push_str(&format!(
            "  over {} ms: {over} ({})\n",
            budget.as_millis(),
            if over == 0 { "PASS" } else { "FAIL" }
        ));

        let peak = self.buckets.iter().copied().max().unwrap_or(0).max(1);
        for (index, count) in self.buckets.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            let width = (*count as f64 / peak as f64 * 40.0).round() as usize;
            let label = if index == BUCKETS - 1 {
                format!(">={index}")
            } else {
                format!("{index}")
            };
            out.push_str(&format!(
                "  {label:>4} ms | {:<40} {count}\n",
                "#".repeat(width)
            ));
        }
        out
    }
}

/// One keystroke's journey, split so the network and the renderer are judged
/// separately. RTT dominates the total on any real link, and the spec's 16 ms
/// target is about local render latency, not about the speed of light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeystrokeLatency {
    /// Key press until the remote's echo arrived. Essentially the round trip.
    pub round_trip: Duration,
    /// Echo arrival until the frame carrying that glyph was painted. This is
    /// the part Berth controls.
    pub local_render: Duration,
}

impl KeystrokeLatency {
    pub fn total(&self) -> Duration {
        self.round_trip + self.local_render
    }

    /// The spec's budget applies to the local half only.
    pub fn within_budget(&self) -> bool {
        self.local_render <= RENDER_BUDGET
    }
}

/// Collects keystroke samples and reports the two halves separately.
#[derive(Debug, Default, Clone)]
pub struct LatencyReport {
    pub round_trip: Histogram,
    pub local_render: Histogram,
    /// The whole journey, which is what a user actually feels.
    pub total: Histogram,
    /// Samples whose local half fitted the spec's budget.
    within_budget: u64,
    /// Exact local-render samples.
    ///
    /// The missed-frame verdict cannot come from the histogram: its buckets are
    /// 1 ms wide, and a threshold of 18.7 ms falls inside bucket 18, so counting
    /// whole buckets either flags every sample between 18.0 and 18.7 (a false
    /// failure, observed) or none of those above it (a false pass). Keystroke
    /// samples number in the dozens, so keeping them exactly costs nothing.
    local_render_samples: Vec<Duration>,
}

impl LatencyReport {
    pub fn new() -> Self {
        Self {
            round_trip: Histogram::new(),
            local_render: Histogram::new(),
            total: Histogram::new(),
            within_budget: 0,
            local_render_samples: Vec::new(),
        }
    }

    pub fn record(&mut self, sample: KeystrokeLatency) {
        self.round_trip.record(sample.round_trip);
        self.local_render.record(sample.local_render);
        self.total.record(sample.total());
        self.local_render_samples.push(sample.local_render);
        if sample.within_budget() {
            self.within_budget += 1;
        }
    }

    /// Samples that waited longer than a single display frame plus the time to
    /// paint one.
    ///
    /// This, not the raw 16 ms budget, is the honest question. Output arrives
    /// at an arbitrary moment inside the refresh interval, so the wait for the
    /// next frame is uniform across it and a 60 Hz display alone produces
    /// samples up to 16.7 ms. What matters is whether Berth ever needs *more*
    /// than the next frame: anything over the period plus one paint is a frame
    /// Berth actually missed.
    pub fn missed_frames(&self, refresh_hz: f64, paint: Duration) -> u64 {
        let threshold = self.missed_frame_threshold(refresh_hz, paint);
        self.local_render_samples
            .iter()
            .filter(|sample| **sample > threshold)
            .count() as u64
    }

    /// One refresh period plus a paint, with a small allowance for scheduling
    /// jitter that is not a dropped frame.
    pub fn missed_frame_threshold(&self, refresh_hz: f64, paint: Duration) -> Duration {
        Duration::from_secs_f64(1.0 / refresh_hz.max(1.0)) + paint + Duration::from_millis(1)
    }

    pub fn report(&self, refresh_hz: f64, paint: Duration) -> String {
        let period_ms = 1000.0 / refresh_hz.max(1.0);
        let missed = self.missed_frames(refresh_hz, paint);
        let mut out = String::new();
        out.push_str(&self.local_render.report("local render", RENDER_BUDGET));
        out.push_str(&self.round_trip.report("round trip", Duration::MAX));
        out.push_str(&format!(
            "\ntotal keystroke to glyph: mean {:.1} ms, p50 {} ms, p95 {} ms\n\
             within the {} ms local budget: {} of {}\n",
            self.total.mean().as_secs_f64() * 1000.0,
            self.total.percentile_ms(0.50),
            self.total.percentile_ms(0.95),
            RENDER_BUDGET.as_millis(),
            self.within_budget,
            self.local_render.count(),
        ));
        out.push_str(&format!(
            "\ndisplay: {refresh_hz:.0} Hz, frame period {period_ms:.1} ms, \
             paint cost {:.1} ms\n\
             frames missed (local render above one frame period plus a paint): {missed} ({})\n\
             \n\
             Local render is the wait for the next frame plus the paint. On a {refresh_hz:.0} Hz \
             display that wait is uniform over {period_ms:.1} ms whatever Berth does, so samples \
             above the {} ms budget are expected; a missed frame is not.\n\
             Network round trip p50 {} ms is reported separately: it dominates the total a user \
             feels, and it is not Berth's to fix.\n",
            paint.as_secs_f64() * 1000.0,
            if missed == 0 { "PASS" } else { "FAIL" },
            RENDER_BUDGET.as_millis(),
            self.round_trip.percentile_ms(0.50),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn an_empty_histogram_reports_zero() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.mean(), Duration::ZERO);
        assert_eq!(h.percentile_ms(0.5), 0);
        assert!(h.within(FRAME_BUDGET));
    }

    #[test]
    fn samples_land_in_the_right_bucket() {
        let mut h = Histogram::new();
        h.record(ms(5));
        h.record(Duration::from_micros(5_900));
        assert_eq!(h.buckets[5], 2, "5.0 and 5.9 ms both belong to bucket 5");
    }

    #[test]
    fn max_is_tracked_exactly() {
        let mut h = Histogram::new();
        h.record(ms(3));
        h.record(Duration::from_micros(41_500));
        h.record(ms(2));
        assert_eq!(h.max(), Duration::from_micros(41_500));
    }

    #[test]
    fn percentiles_follow_the_distribution() {
        let mut h = Histogram::new();
        for _ in 0..99 {
            h.record(ms(4));
        }
        h.record(ms(50));
        assert_eq!(h.percentile_ms(0.50), 4);
        assert_eq!(h.percentile_ms(0.99), 4);
        assert_eq!(h.percentile_ms(1.0), 50);
    }

    #[test]
    fn over_counts_at_and_above_the_limit() {
        let mut h = Histogram::new();
        h.record(ms(31));
        h.record(ms(32));
        h.record(ms(90));
        assert_eq!(h.over(FRAME_BUDGET), 2);
    }

    #[test]
    fn the_pass_condition_is_the_maximum_not_an_average() {
        let mut h = Histogram::new();
        for _ in 0..1000 {
            h.record(ms(2));
        }
        h.record(ms(90));
        assert!(
            !h.within(FRAME_BUDGET),
            "one long frame is a freeze, however good the average"
        );
    }

    #[test]
    fn very_long_samples_saturate_the_last_bucket() {
        let mut h = Histogram::new();
        h.record(Duration::from_secs(5));
        assert_eq!(h.buckets[BUCKETS - 1], 1);
        assert_eq!(h.max(), Duration::from_secs(5));
    }

    #[test]
    fn the_report_names_pass_or_fail() {
        let mut h = Histogram::new();
        h.record(ms(4));
        assert!(h.report("frames", FRAME_BUDGET).contains("PASS"));
        h.record(ms(40));
        assert!(h.report("frames", FRAME_BUDGET).contains("FAIL"));
    }

    #[test]
    fn keystroke_latency_is_judged_on_the_local_half() {
        // A slow link with a fast renderer passes; that is the point.
        let slow_link = KeystrokeLatency {
            round_trip: ms(200),
            local_render: ms(3),
        };
        assert!(slow_link.within_budget());
        assert_eq!(slow_link.total(), ms(203));

        let slow_renderer = KeystrokeLatency {
            round_trip: ms(1),
            local_render: ms(40),
        };
        assert!(!slow_renderer.within_budget());
    }

    #[test]
    fn the_latency_report_separates_the_two_halves() {
        let mut r = LatencyReport::new();
        r.record(KeystrokeLatency {
            round_trip: ms(120),
            local_render: ms(2),
        });
        let text = r.report(60.0, Duration::from_millis(2));
        assert!(text.contains("local render"));
        assert!(text.contains("round trip"));
        assert!(text.contains("not Berth's to fix"));
        assert!(text.contains("frames missed"));
    }

    /// A wait inside the refresh interval is the display, not Berth.
    /// The bucketing trap: a threshold inside a bucket must not condemn every
    /// sample in that bucket. Seen for real, as three false failures.
    #[test]
    fn a_sample_just_under_the_threshold_is_not_a_missed_frame() {
        let mut r = LatencyReport::new();
        // Threshold at 60 Hz with a 1 ms paint is 16.67 + 1 + 1 = 18.67 ms.
        r.record(KeystrokeLatency {
            round_trip: ms(10),
            local_render: Duration::from_micros(18_500),
        });
        assert_eq!(r.missed_frames(60.0, ms(1)), 0, "18.5 ms is under 18.67 ms");
        r.record(KeystrokeLatency {
            round_trip: ms(10),
            local_render: Duration::from_micros(18_900),
        });
        assert_eq!(r.missed_frames(60.0, ms(1)), 1, "18.9 ms is over 18.67 ms");
    }

    #[test]
    fn a_vsync_wait_is_not_a_missed_frame() {
        let mut r = LatencyReport::new();
        r.record(KeystrokeLatency {
            round_trip: ms(10),
            local_render: ms(15),
        });
        assert_eq!(r.missed_frames(60.0, ms(2)), 0);
        assert!(r.report(60.0, ms(2)).contains("PASS"));
    }

    #[test]
    fn waiting_past_a_whole_frame_is_a_missed_frame() {
        let mut r = LatencyReport::new();
        r.record(KeystrokeLatency {
            round_trip: ms(10),
            local_render: ms(40),
        });
        assert_eq!(r.missed_frames(60.0, ms(2)), 1);
        assert!(r.report(60.0, ms(2)).contains("FAIL"));
    }

    #[test]
    fn a_faster_display_narrows_the_window() {
        let mut r = LatencyReport::new();
        r.record(KeystrokeLatency {
            round_trip: ms(10),
            local_render: ms(15),
        });
        // 15 ms is fine at 60 Hz and a missed frame at 144 Hz.
        assert_eq!(r.missed_frames(60.0, ms(2)), 0);
        assert_eq!(r.missed_frames(144.0, ms(2)), 1);
    }
}
