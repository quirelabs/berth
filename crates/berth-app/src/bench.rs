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

/// Bytes for the paced phase. Smaller than the file because all of it arrives.
pub const PACED_BYTES: usize = 8 * 1024 * 1024;

/// A producer that tmux forwards instead of collapsing.
///
/// `cat` of a cached file is instantaneous, so tmux gets only a few dozen
/// chances to redraw and the client receives about a megabyte of a fifty
/// megabyte file however fast the link is. That measures tmux, not Berth. A
/// CPU-bound pipeline emits at roughly the rate tmux can forward, so what is
/// written is what arrives: `berth-core` reading exactly this receives 114% of
/// it on localhost. This is the phase that actually loads the renderer.
pub fn paced_command() -> String {
    format!("head -c {PACED_BYTES} /dev/zero | tr '\\0' 'y' | fold -w 76\n")
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

    // What survived tmux for the instantaneous producer, before the second
    // phase overwrites the counters.
    let cat_phase = view
        .read_with(cx, |view, _| {
            (
                view.frames.report("frame build, cat", Some(FRAME_BUDGET)),
                view.bytes_in,
                view.frames.within(FRAME_BUDGET),
                view.frames.percentile_ms(0.99),
            )
        })
        .unwrap_or_default();

    let _ = view.update(cx, |view, _| {
        view.frames = crate::metrics::Histogram::new();
        view.bytes_in = 0;
    });
    pty.write(paced_command()).await;
    cx.background_executor().timer(settle).await;

    view.read_with(cx, |view, _| {
        let env = environment(view);
        let report = view.frames.report("frame build, paced", Some(FRAME_BUDGET));
        let p99 = view.frames.percentile_ms(0.99);
        let ceiling = f64::from(CAT_P99_CEILING_MS);
        let no_freeze = view.frames.within(FRAME_BUDGET);
        let batching_intact = p99 <= ceiling;
        // Byte count reported so the frame numbers can be checked against real
        // work: a histogram gathered over an idle window would look excellent
        // too. Note that tmux re-renders rather than forwarding verbatim, so
        // this is below the 50 MB written on the remote.
        let (cat_report, cat_bytes, cat_no_freeze, cat_p99) = cat_phase;
        // The verdict comes from the paced phase, because it is the only one
        // that reliably puts bytes in front of the renderer. The cat phase is
        // kept because it is what the Windows baseline measured, and because
        // the gap between the two is the whole point.
        let text = format!(
            "{env}\
             {cat_report}bytes through the parser: {cat_bytes} ({:.1} MB of {:.0} MB written)\n\
             \n\
             {report}bytes through the parser: {} ({:.1} MB of {:.0} MB written)\n\
             \n\
             cat phase, no frame at or above {} ms: {} (p99 {cat_p99:.1} ms)\n\
             paced phase, no frame at or above {} ms: {}\n\
             paced phase, p99 frame time {p99:.1} ms against a {CAT_P99_CEILING_MS} ms \
             regression ceiling: {}\n",
            cat_bytes as f64 / 1024.0 / 1024.0,
            BENCH_BYTES as f64 / 1024.0 / 1024.0,
            view.bytes_in,
            view.bytes_in as f64 / 1024.0 / 1024.0,
            PACED_BYTES as f64 / 1024.0 / 1024.0,
            FRAME_BUDGET.as_millis(),
            if cat_no_freeze { "PASS" } else { "FAIL" },
            FRAME_BUDGET.as_millis(),
            if no_freeze { "PASS" } else { "FAIL" },
            if batching_intact { "PASS" } else { "FAIL" },
        );
        // Half the written bytes is a low bar deliberately: the point is that
        // the renderer was actually loaded, not that tmux forwarded perfectly.
        let arrived = view.bytes_in as usize > PACED_BYTES / 2;
        // A handful of frames over twenty seconds is not "painting"; it is a
        // starved window. The bar is low but not zero.
        let painted = view.frames.count() >= 10;
        // Same trap as the keystroke bench: an unfocused window paints rarely,
        // the pump starves behind it, tmux collapses what it cannot hand over,
        // and every frame-time criterion then passes on almost no work.
        let text = if arrived && painted {
            text
        } else {
            format!(
                "{text}\nINVALID: the paced phase delivered {:.1} MB of {:.0} MB over {} \
                 frames.\n{}\n",
                view.bytes_in as f64 / 1024.0 / 1024.0,
                PACED_BYTES as f64 / 1024.0 / 1024.0,
                view.frames.count(),
                if painted {
                    "Frames were being painted, so this is the link: tmux collapsed what it \
                     could not hand over in time. Run against a host with no network in the \
                     way."
                } else {
                    "Nothing painted, so the window was probably not frontmost. gpui paints \
                     an inactive window rarely, the pump starves behind it, and every \
                     frame-time criterion then passes on almost no work. Re-run focused."
                },
            )
        };
        Outcome {
            text,
            passed: no_freeze && batching_intact && arrived && painted,
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
/// The font lines matter most: JetBrains Mono is not installed everywhere, so
/// the two platforms may be measuring different metrics entirely, and that
/// would otherwise be invisible in a latency number.
///
/// Naming the requested family is not enough, because Core Text and DirectWrite
/// both substitute silently for a family that is not installed. So this reports
/// whether the request was honoured, which family the chain actually landed on,
/// and the advance of `m` against `i`. That last pair is the one that cannot be
/// argued with: they are equal in every monospace face, and a difference means
/// the grid has been built on a proportional font.
pub fn environment(view: &TerminalView) -> String {
    let style = view.style();
    let cell = view.resolved_cell;
    let mut out = format!(
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
    );

    match &view.resolved_font {
        None => out.push_str("face: not painted yet\n"),
        Some(facts) => {
            out.push_str(&format!(
                "face metrics: units/em {}, ascent {:.2} px, descent {:.2} px, \
                 cap {:.2} px, x {:.2} px\n",
                facts.units_per_em, facts.ascent, facts.descent, facts.cap_height, facts.x_height,
            ));
            out.push_str(&format!(
                "monospace check: m {:.2} px, i {:.2} px, advance ratio {:.2} ({})\n",
                facts.advance_m,
                facts.advance_i,
                facts.advance_m / style.font_size,
                if facts.is_monospace() {
                    "OK"
                } else {
                    "PROPORTIONAL, the grid is wrong and these numbers are not comparable"
                },
            ));
        }
    }
    out
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
        // threshold and turn a genuinely missed frame into a pass. Taken at the
        // histogram's full resolution and not rounded up to a millisecond: a
        // paint here is well under one, and rounding it up loosened the
        // threshold by more than a tenth of a frame period at 120 Hz.
        let paint = Duration::from_secs_f64(view.frames.percentile_ms(0.50) / 1000.0);
        // Every keystroke must have produced a sample. A short run is not a
        // good run: gpui drops frames for a window that is not frontmost, and
        // D19 records that silently costing 33 of 40 samples once already.
        // Reporting a percentile over whatever survived hides exactly the
        // conditions that produce the worst numbers.
        let collected = view.latency.local_render.count() as usize;
        let complete = collected == samples;
        Outcome {
            text: format!(
                "{}{}{}",
                environment(view),
                view.latency.report(refresh_hz, paint),
                if complete {
                    String::new()
                } else {
                    format!(
                        "\nINVALID: {collected} of {samples} keystrokes produced a frame. \
                         The window was probably not frontmost. Re-run with it focused.\n"
                    )
                },
            ),
            // p95 within the next frame and max within two, not the raw 16 ms
            // line and no longer zero missed frames. See decisions.md D19.
            passed: complete && view.latency.meets_criterion(refresh_hz, paint),
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
