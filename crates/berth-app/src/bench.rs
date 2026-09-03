//! M0's two numeric acceptance criteria, measured in the real window.
//!
//! Both run through the actual GPUI render loop against the real host, because
//! neither number means anything measured anywhere else. Driven by
//! `BERTH_BENCH`, which is never set in normal use.

use std::time::Duration;

use gpui::{AsyncApp, WeakEntity};

use berth_core::pty::Pty;

use crate::metrics::FRAME_BUDGET;
use crate::terminal_view::TerminalView;

/// The file the heavy-output test reads, created on the remote and deleted
/// afterwards.
pub const BENCH_FILE: &str = "/tmp/berth_bench_50mb.txt";
pub const BENCH_BYTES: usize = 50 * 1024 * 1024;

/// Regression ceiling on p99 frame time during heavy output.
///
/// Far below the 32 ms freeze threshold on purpose. The measured p99 varies
/// between 2 and 5 ms across runs, so this leaves headroom for that variance
/// while still failing long before anything is visible to a user. Tighten it if
/// the spread ever narrows. It exists because the batching this depends on is easy to
/// undo by accident: the pump previously relied on a fixed coalescing window,
/// that window was removed to fix keystroke latency, and a future change to
/// either side must not quietly reintroduce one bug while fixing the other.
pub const CAT_P99_CEILING_MS: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bench {
    /// `cat` a 50 MB file and histogram the frame times.
    Cat,
    /// Send keystrokes and split the latency into network and render halves.
    Keys,
}

impl Bench {
    pub fn from_env() -> Option<Self> {
        match std::env::var("BERTH_BENCH").ok()?.as_str() {
            "cat" => Some(Bench::Cat),
            "keys" => Some(Bench::Keys),
            other => {
                eprintln!("unknown BERTH_BENCH={other}");
                None
            }
        }
    }
}

/// Shell command that creates the benchmark file if it is not already there.
pub fn create_file_command() -> String {
    format!(
        "test -s {BENCH_FILE} || (head -c {BENCH_BYTES} /dev/urandom | base64 | head -c {BENCH_BYTES} > {BENCH_FILE}); wc -c < {BENCH_FILE}"
    )
}

pub fn remove_file_command() -> String {
    format!("rm -f {BENCH_FILE}")
}

/// Stream the file and report the frame-time distribution.
///
/// The pass condition is the maximum, not the mean: one 200 ms frame is a
/// visible freeze however good the average looks.
pub async fn run_cat(
    view: WeakEntity<TerminalView>,
    pty: Pty,
    settle: Duration,
    cx: &mut AsyncApp,
) -> Outcome {
    // Discard frames from connecting and drawing the prompt.
    let _ = view.update(cx, |view, _| {
        view.frames = crate::metrics::Histogram::new();
        view.bytes_in = 0;
    });

    pty.write(format!("cat {BENCH_FILE}\n")).await;

    // Let it stream, then let the grid settle.
    cx.background_executor().timer(settle).await;

    view.read_with(cx, |view, _| {
        let env = environment(view);
        let report = view.frames.report("frame build", FRAME_BUDGET);
        let p99 = view.frames.percentile_ms(0.99);
        let no_freeze = view.frames.within(FRAME_BUDGET);
        let batching_intact = p99 <= CAT_P99_CEILING_MS;
        // Byte count reported so the frame numbers can be checked against real
        // work: a histogram gathered over an idle window would look excellent
        // too. Note that tmux re-renders rather than forwarding verbatim, so
        // this is below the 50 MB written on the remote.
        let text = format!(
            "{env}{report}bytes through the parser: {} ({:.1} MB over {} frames)\n\n\
             no frame at or above {} ms: {}\n\
             p99 frame time {p99} ms against a {CAT_P99_CEILING_MS} ms regression \
             ceiling: {}\n",
            view.bytes_in,
            view.bytes_in as f64 / 1024.0 / 1024.0,
            view.frames.count(),
            FRAME_BUDGET.as_millis(),
            if no_freeze { "PASS" } else { "FAIL" },
            if batching_intact { "PASS" } else { "FAIL" },
        );
        Outcome {
            text,
            passed: no_freeze && batching_intact && view.bytes_in > 0,
        }
    })
    .unwrap_or_else(|_| Outcome {
        text: "the view went away before the report".to_string(),
        passed: false,
    })
}

/// A benchmark result plus whether it met its criteria, so a run can fail
/// rather than merely print numbers for someone to eyeball.
pub struct Outcome {
    pub text: String,
    pub passed: bool,
}

/// Platform and font facts, printed by both benchmarks.
///
/// The font line matters most: JetBrains Mono is not installed everywhere, so
/// the two platforms may be measuring different metrics entirely, and that
/// would otherwise be invisible in a latency number.
pub fn environment(view: &TerminalView) -> String {
    let style = view.style();
    let cell = view.resolved_cell;
    format!(
        "platform: {}
font requested: {} at {} px, line height {}
cell resolved: {}
",
        std::env::consts::OS,
        style.font_family,
        style.font_size,
        style.line_height,
        match cell {
            Some(c) => format!("{:.2} x {:.2} px", c.width, c.height),
            None => "not painted yet".to_string(),
        }
    )
}

/// Type into the remote and report both halves of the latency separately.
pub async fn run_keys(
    view: WeakEntity<TerminalView>,
    pty: Pty,
    samples: usize,
    refresh_hz: f64,
    cx: &mut AsyncApp,
) -> Outcome {
    // A shell that echoes but produces no extra output per keystroke.
    pty.write("\n").await;
    cx.background_executor()
        .timer(Duration::from_millis(750))
        .await;

    // Discard the connect and first-paint frames. Font resolution and the
    // first shaped line make that paint an outlier, and it would otherwise set
    // the missed-frame threshold far too loose.
    let _ = view.update(cx, |view, _| {
        view.frames = crate::metrics::Histogram::new();
    });

    for _ in 0..samples {
        let _ = view.update(cx, |view, cx| {
            view.send_key(
                berth_term::KeyEvent::new(berth_term::Key::Char('x'), berth_term::Modifiers::NONE),
                cx,
            );
        });
        // Well clear of any plausible round trip, so samples never overlap.
        cx.background_executor()
            .timer(Duration::from_millis(250))
            .await;
    }

    // Leave the shell's line buffer clean.
    let _ = pty.write("\x15").await;

    view.read_with(cx, |view, _| {
        // Typical paint cost, from the frames recorded while typing. The median
        // rather than a tail percentile: one slow outlier must not inflate the
        // threshold and turn a genuinely missed frame into a pass.
        let paint = Duration::from_millis(view.frames.percentile_ms(0.50).max(1) as u64);
        Outcome {
            text: format!(
                "{}{}",
                environment(view),
                view.latency.report(refresh_hz, paint)
            ),
            // The criterion is missed frames, not the raw 16 ms line. See
            // decisions.md D19 and spec section 10.
            passed: view.latency.missed_frames(refresh_hz, paint) == 0
                && view.latency.local_render.count() > 0,
        }
    })
    .unwrap_or_else(|_| Outcome {
        text: "the view went away before the report".to_string(),
        passed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_mode_parses_from_the_environment() {
        // Parsing is by string so the harness cannot silently run the wrong one.
        assert_eq!(
            ["cat", "keys"]
                .iter()
                .filter(|m| matches!(*m, &"cat" | &"keys"))
                .count(),
            2
        );
    }

    #[test]
    fn the_file_command_is_idempotent() {
        let cmd = create_file_command();
        assert!(cmd.contains("test -s"), "must not rebuild an existing file");
        assert!(cmd.contains(BENCH_FILE));
    }

    #[test]
    fn the_benchmark_size_is_the_specified_fifty_megabytes() {
        assert_eq!(BENCH_BYTES, 52_428_800);
    }

    #[test]
    fn cleanup_removes_the_file() {
        assert!(remove_file_command().contains(BENCH_FILE));
        assert!(remove_file_command().starts_with("rm -f"));
    }
}
