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

/// Fixed-bucket histogram. Bucket `i` counts samples in
/// `[i * BUCKET_MS, (i+1) * BUCKET_MS)`; everything at or above the top edge
/// lands in the last bucket.
///
/// The buckets are a tenth of a millisecond because a whole millisecond is too
/// coarse for the thing they feed. The missed-frame threshold is one refresh
/// period plus the typical paint, and at 120 Hz that period is 8.3 ms, so a
/// paint estimate rounded to a whole millisecond moved the threshold by more
/// than a tenth of it. Memory is unchanged in character: still a fixed array,
/// still no per-sample storage, because `view.frames` records every frame of a
/// long-running session and must not grow.
#[derive(Debug, Clone)]
pub struct Histogram {
    buckets: Vec<u32>,
    count: u64,
    total: Duration,
    max: Duration,
}

/// Bucket width. Also the precision of every percentile this reports.
const BUCKET_MS: f64 = 0.1;
/// The same width in microseconds. Bucketing is done in integers because
/// `5.9 / 0.1` is 58.999... in binary and would file a sample one bucket low.
const BUCKET_US: u128 = 100;
/// Covering 0 to 129 ms, the same range as when buckets were 1 ms wide.
const BUCKETS: usize = 1290;
/// Bucket count per millisecond, for the one-line-per-millisecond display.
const BUCKETS_PER_MS: usize = 10;

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
        let index = ((sample.as_micros() / BUCKET_US) as usize).min(BUCKETS - 1);
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
    pub fn percentile_ms(&self, quantile: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let target = (quantile.clamp(0.0, 1.0) * self.count as f64).ceil() as u64;
        let mut seen = 0u64;
        for (index, bucket) in self.buckets.iter().enumerate() {
            seen += *bucket as u64;
            if seen >= target {
                return index as f64 * BUCKET_MS;
            }
        }
        (BUCKETS - 1) as f64 * BUCKET_MS
    }

    /// How many samples were at or above `limit`.
    pub fn over(&self, limit: Duration) -> u64 {
        let cutoff = (limit.as_micros() / BUCKET_US) as usize;
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
    ///
    /// `budget` is optional because not every distribution has a criterion.
    /// The round trip is the network's, not Berth's, so printing a verdict on
    /// it would claim a pass Berth did not earn.
    pub fn report(&self, title: &str, budget: Option<Duration>) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{title}: {} frames, mean {:.1} ms, p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms, \
             max {:.1} ms\n",
            self.count,
            self.mean().as_secs_f64() * 1000.0,
            self.percentile_ms(0.50),
            self.percentile_ms(0.95),
            self.percentile_ms(0.99),
            self.max().as_secs_f64() * 1000.0,
        ));
        if let Some(budget) = budget {
            let over = self.over(budget);
            out.push_str(&format!(
                "  over {} ms: {over} ({})\n",
                budget.as_millis(),
                if over == 0 { "PASS" } else { "FAIL" }
            ));
        }

        // Drawn one line per millisecond, not one per bucket: the extra
        // resolution is there for the arithmetic, not for a reader.
        let per_ms: Vec<u32> = self
            .buckets
            .chunks(BUCKETS_PER_MS)
            .map(|chunk| chunk.iter().sum())
            .collect();
        let peak = per_ms.iter().copied().max().unwrap_or(0).max(1);
        let last = per_ms.len() - 1;
        for (index, count) in per_ms.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            let width = (*count as f64 / peak as f64 * 40.0).round() as usize;
            let label = if index == last {
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
    /// Echo arrival until that frame started being built. Almost all of this
    /// is the wait for the next vsync, which is not Berth's to shorten.
    pub wait: Duration,
    /// Building and painting that frame. Berth's actual work.
    pub paint: Duration,
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
    /// The two halves of `local_render`, kept apart so a tail can be blamed on
    /// frame pacing or on Berth rather than argued about.
    pub wait: Histogram,
    pub paint: Histogram,
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
    /// Every sample, kept whole so a late one can be decomposed after the fact.
    /// Forty per run, so this costs nothing.
    late_samples: Vec<KeystrokeLatency>,
}

impl LatencyReport {
    pub fn new() -> Self {
        Self {
            round_trip: Histogram::new(),
            local_render: Histogram::new(),
            wait: Histogram::new(),
            paint: Histogram::new(),
            total: Histogram::new(),
            within_budget: 0,
            local_render_samples: Vec::new(),
            late_samples: Vec::new(),
        }
    }

    pub fn record(&mut self, sample: KeystrokeLatency) {
        self.round_trip.record(sample.round_trip);
        self.local_render.record(sample.local_render);
        self.wait.record(sample.wait);
        self.paint.record(sample.paint);
        self.total.record(sample.total());
        self.local_render_samples.push(sample.local_render);
        self.late_samples.push(sample);
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

    /// Two refresh periods plus a paint. The ceiling on the worst sample.
    pub fn two_frame_threshold(&self, refresh_hz: f64, paint: Duration) -> Duration {
        self.missed_frame_threshold(refresh_hz, paint)
            + Duration::from_secs_f64(1.0 / refresh_hz.max(1.0))
    }

    /// The exact quantile of the local half, from the samples rather than the
    /// histogram, for the same reason `missed_frames` uses them.
    pub fn local_render_percentile(&self, quantile: f64) -> Duration {
        if self.local_render_samples.is_empty() {
            return Duration::ZERO;
        }
        let mut sorted = self.local_render_samples.clone();
        sorted.sort_unstable();
        let rank = (quantile.clamp(0.0, 1.0) * sorted.len() as f64).ceil() as usize;
        sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
    }

    /// The keystroke criterion: the bulk of keystrokes make the next frame,
    /// and none of them takes more than two. See decisions.md D19.
    pub fn meets_criterion(&self, refresh_hz: f64, paint: Duration) -> bool {
        !self.local_render_samples.is_empty()
            && self.local_render_percentile(0.95) <= self.missed_frame_threshold(refresh_hz, paint)
            && self.local_render.max() <= self.two_frame_threshold(refresh_hz, paint)
    }

    /// Where a late sample's time actually went.
    ///
    /// `local_render` is the wait for the next vsync plus the frame Berth
    /// builds. Only the second is Berth's, so a tail is worth chasing only if
    /// it shows up there. Printed per sample rather than as a distribution
    /// because there are only ever a handful.
    fn late_sample_breakdown(&self, refresh_hz: f64, paint: Duration) -> String {
        let threshold = self.missed_frame_threshold(refresh_hz, paint);
        let late: Vec<&KeystrokeLatency> = self
            .late_samples
            .iter()
            .filter(|s| s.local_render > threshold)
            .collect();
        if late.is_empty() {
            return String::new();
        }
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let mut out = format!(
            "\nlate samples, against a {:.2} ms threshold:\n",
            ms(threshold)
        );
        for s in &late {
            out.push_str(&format!(
                "  local {:.2} ms = wait {:.2} + paint {:.2}, over by {:.2}\n",
                ms(s.local_render),
                ms(s.wait),
                ms(s.paint),
                ms(s.local_render) - ms(threshold),
            ));
        }
        // Judged against the frame period, not against the median paint.
        // Doubling a median of a few hundred microseconds is not a meaningful
        // bar, and calling 0.6 ms "slow" against an 8.3 ms period was wrong.
        let period = Duration::from_secs_f64(1.0 / refresh_hz.max(1.0));
        let worst_paint = late.iter().map(|s| s.paint).max().unwrap_or_default();
        let worst_wait = late.iter().map(|s| s.wait).max().unwrap_or_default();
        out.push_str(&format!(
            "  worst wait {:.2} ms against a nominal period of {:.2} ms; \
             worst paint {:.2} ms\n  {}\n",
            ms(worst_wait),
            ms(period),
            ms(worst_paint),
            if worst_paint * 4 < period {
                "the paint is a small fraction of a frame, so the time went into the wait, \
                 which is the display's and not Berth's"
            } else {
                "the paint is a material fraction of a frame, so this is Berth's to fix"
            }
        ));
        out
    }

    pub fn report(&self, refresh_hz: f64, paint: Duration) -> String {
        let period_ms = 1000.0 / refresh_hz.max(1.0);
        let missed = self.missed_frames(refresh_hz, paint);
        let mut out = String::new();
        out.push_str(
            &self
                .local_render
                .report("local render", Some(RENDER_BUDGET)),
        );
        out.push_str(&self.round_trip.report("round trip", None));
        out.push_str(&format!(
            "\ntotal keystroke to glyph: mean {:.1} ms, p50 {:.1} ms, p95 {:.1} ms\n\
             within the {} ms local budget: {} of {}\n",
            self.total.mean().as_secs_f64() * 1000.0,
            self.total.percentile_ms(0.50),
            self.total.percentile_ms(0.95),
            RENDER_BUDGET.as_millis(),
            self.within_budget,
            self.local_render.count(),
        ));
        out.push_str(&self.late_sample_breakdown(refresh_hz, paint));
        out.push_str(&format!(
            "\ndisplay: {refresh_hz:.0} Hz, frame period {period_ms:.1} ms, \
             paint cost {:.1} ms\n\
             frames missed (local render above one frame period plus a paint): {missed}\n\
             p95 {:.2} ms within one frame ({:.2} ms), max {:.2} ms within two ({:.2} ms): {}\n\
             \n\
             Local render is the wait for the next frame plus the paint. On a {refresh_hz:.0} Hz \
             display that wait is uniform over {period_ms:.1} ms whatever Berth does, so samples \
             above the {} ms budget are expected; a missed frame is not.\n\
             Network round trip p50 {:.1} ms is reported separately: it dominates the total a user \
             feels, and it is not Berth's to fix.\n",
            paint.as_secs_f64() * 1000.0,
            self.local_render_percentile(0.95).as_secs_f64() * 1000.0,
            self.missed_frame_threshold(refresh_hz, paint).as_secs_f64() * 1000.0,
            self.local_render.max().as_secs_f64() * 1000.0,
            self.two_frame_threshold(refresh_hz, paint).as_secs_f64() * 1000.0,
            if self.meets_criterion(refresh_hz, paint) {
                "PASS"
            } else {
                "FAIL"
            },
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
        assert_eq!(h.percentile_ms(0.5), 0.0);
        assert!(h.within(FRAME_BUDGET));
    }

    #[test]
    fn samples_land_in_the_right_bucket() {
        let mut h = Histogram::new();
        h.record(ms(5));
        h.record(Duration::from_micros(5_900));
        assert_eq!(h.buckets[50], 1, "5.0 ms belongs to the 5.0 bucket");
        assert_eq!(h.buckets[59], 1, "5.9 ms is no longer in the same bucket");
    }

    /// The reason the buckets are a tenth of a millisecond: this is the paint
    /// estimate, and rounding it to a whole millisecond moved the missed-frame
    /// threshold by more than a tenth of a frame period at 120 Hz.
    #[test]
    fn a_sub_millisecond_median_is_reported_as_itself() {
        let mut h = Histogram::new();
        for _ in 0..10 {
            h.record(Duration::from_micros(400));
        }
        assert_eq!(h.percentile_ms(0.50), 0.4);
    }

    #[test]
    fn the_display_stays_one_line_per_millisecond() {
        let mut h = Histogram::new();
        h.record(Duration::from_micros(4_100));
        h.record(Duration::from_micros(4_800));
        let report = h.report("frames", None);
        let bars: Vec<&str> = report.lines().filter(|l| l.contains('|')).collect();
        assert_eq!(bars.len(), 1, "both samples are in the 4 ms row");
        assert!(
            bars[0].contains(" 2"),
            "and the row counts both: {}",
            bars[0]
        );
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
        assert_eq!(h.percentile_ms(0.50), 4.0);
        assert_eq!(h.percentile_ms(0.99), 4.0);
        assert_eq!(h.percentile_ms(1.0), 50.0);
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
        assert!(h.report("frames", Some(FRAME_BUDGET)).contains("PASS"));
        // A distribution with no criterion states no verdict.
        let no_budget = h.report("round trip", None);
        assert!(!no_budget.contains("PASS"));
        assert!(!no_budget.contains("over "));
        assert!(no_budget.contains("round trip"));
        h.record(ms(40));
        assert!(h.report("frames", Some(FRAME_BUDGET)).contains("FAIL"));
    }

    /// The amended D19 criterion: a tail of one or two frames is frame pacing,
    /// which Berth cannot shorten, but nothing may take more than two frames.
    #[test]
    fn the_criterion_tolerates_a_thin_tail_but_not_a_second_lost_frame() {
        let sample = |local: Duration| KeystrokeLatency {
            round_trip: ms(30),
            local_render: local,
            wait: local,
            paint: Duration::ZERO,
        };
        let paint = Duration::from_micros(500);

        // 120 Hz: one frame is 9.83 ms with the jitter allowance, two is 18.17.
        let mut clean = LatencyReport::new();
        for _ in 0..39 {
            clean.record(sample(ms(5)));
        }
        clean.record(sample(Duration::from_micros(10_400)));
        assert!(
            clean.missed_frames(120.0, paint) > 0,
            "the late sample is still counted and reported"
        );
        assert!(
            clean.meets_criterion(120.0, paint),
            "but one sample in forty past a frame is pacing, not a failure"
        );

        let mut two_frames_lost = LatencyReport::new();
        for _ in 0..39 {
            two_frames_lost.record(sample(ms(5)));
        }
        two_frames_lost.record(sample(ms(25)));
        assert!(
            !two_frames_lost.meets_criterion(120.0, paint),
            "a sample past two frames is a real stall"
        );

        let mut most_are_late = LatencyReport::new();
        for _ in 0..40 {
            most_are_late.record(sample(Duration::from_micros(11_000)));
        }
        assert!(
            !most_are_late.meets_criterion(120.0, paint),
            "a tail is tolerated; a distribution is not"
        );
    }

    #[test]
    fn keystroke_latency_is_judged_on_the_local_half() {
        // A slow link with a fast renderer passes; that is the point.
        let slow_link = KeystrokeLatency {
            round_trip: ms(200),
            local_render: ms(3),
            wait: Duration::ZERO,
            paint: Duration::ZERO,
        };
        assert!(slow_link.within_budget());
        assert_eq!(slow_link.total(), ms(203));

        let slow_renderer = KeystrokeLatency {
            round_trip: ms(1),
            local_render: ms(40),
            wait: Duration::ZERO,
            paint: Duration::ZERO,
        };
        assert!(!slow_renderer.within_budget());
    }

    #[test]
    fn the_latency_report_separates_the_two_halves() {
        let mut r = LatencyReport::new();
        r.record(KeystrokeLatency {
            round_trip: ms(120),
            local_render: ms(2),
            wait: Duration::ZERO,
            paint: Duration::ZERO,
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
            wait: Duration::ZERO,
            paint: Duration::ZERO,
        });
        assert_eq!(r.missed_frames(60.0, ms(1)), 0, "18.5 ms is under 18.67 ms");
        r.record(KeystrokeLatency {
            round_trip: ms(10),
            local_render: Duration::from_micros(18_900),
            wait: Duration::ZERO,
            paint: Duration::ZERO,
        });
        assert_eq!(r.missed_frames(60.0, ms(1)), 1, "18.9 ms is over 18.67 ms");
    }

    #[test]
    fn a_vsync_wait_is_not_a_missed_frame() {
        let mut r = LatencyReport::new();
        r.record(KeystrokeLatency {
            round_trip: ms(10),
            local_render: ms(15),
            wait: Duration::ZERO,
            paint: Duration::ZERO,
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
            wait: Duration::ZERO,
            paint: Duration::ZERO,
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
            wait: Duration::ZERO,
            paint: Duration::ZERO,
        });
        // 15 ms is fine at 60 Hz and a missed frame at 144 Hz.
        assert_eq!(r.missed_frames(60.0, ms(2)), 0);
        assert_eq!(r.missed_frames(144.0, ms(2)), 1);
    }
}
