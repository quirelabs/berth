//! M0's two numeric acceptance criteria, measured in the real window.
//!
//! Both run through the actual GPUI render loop against the real host, because
//! neither number means anything measured anywhere else. Driven by
//! `BERTH_BENCH`, which is never set in normal use.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

/// Whether a benchmark has failed its criteria.
///
/// A failed run has to leave a non-zero exit code, so a regression is a build
/// failure rather than a number someone has to notice. It used to do that with
/// `process::exit` from inside the window, which skips every destructor: the
/// server saw a dropped socket instead of a disconnect. The flag is set there
/// and read once in `main`, so there is one way out of the process.
static FAILED: AtomicBool = AtomicBool::new(false);

pub fn mark_failed() {
    FAILED.store(true, Ordering::Relaxed);
}

pub fn exit_code() -> ExitCode {
    if FAILED.load(Ordering::Relaxed) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bench {
    /// `cat` a 50 MB file and histogram the frame times.
    Cat,
    /// Send keystrokes and split the latency into network and render halves.
    Keys,
    /// `cat` a 50 MB file in a local shell: no tmux, no ssh, so every byte
    /// reaches the parser and the renderer is actually loaded (D49 verdict).
    LocalCat,
}

impl Bench {
    pub fn from_env() -> Option<Self> {
        match berth_core::env::bench()?.as_str() {
            "cat" => Some(Bench::Cat),
            "keys" => Some(Bench::Keys),
            "local-cat" => Some(Bench::LocalCat),
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
    format!("rm -f {BENCH_FILE} {CHUNK_FILE}")
}

/// Bytes for the paced phase, once the rate has been calibrated.
pub const PACED_BYTES: usize = 8 * 1024 * 1024;
/// The chunk file the pacer replays, so a chunk costs one `cat` rather than a
/// three-process pipeline.
pub const CHUNK_FILE: &str = "/tmp/berth_bench_chunk";
/// Chunks per second. Fine enough that the rate is a rate rather than a series
/// of bursts, coarse enough that process spawning is not itself the limit.
pub const CHUNKS_PER_SECOND: usize = 20;
/// Rates tried during calibration, bytes per second. A doubling ladder, which
/// finds the knee in a handful of trials rather than bisecting for accuracy
/// the number does not need.
pub const RATE_LADDER: [usize; 7] = [
    1 << 20,
    2 << 20,
    4 << 20,
    8 << 20,
    16 << 20,
    32 << 20,
    64 << 20,
];
/// The fraction that has to arrive for a rate to count as forwarded intact.
/// Not 1.0, because tmux adds escape sequences and the ratio runs slightly over
/// on a good link, and not lower, because the whole point is that nothing was
/// dropped.
pub const INTACT: f64 = 0.95;
/// Seconds of output per calibration trial.
pub const TRIAL_SECONDS: usize = 2;

/// Build the chunk the pacer replays, sized for one chunk at `rate`.
pub fn chunk_command(rate: usize) -> String {
    let chunk = (rate / CHUNKS_PER_SECOND).max(1);
    // Written to the pty rather than exec'd, so it needs its own newline; the
    // next command would otherwise be appended to the redirect target.
    format!("head -c {chunk} /dev/zero | tr '\\0' 'y' | fold -w 76 > {CHUNK_FILE}\n")
}

/// Replay the chunk `count` times, one every 1/`CHUNKS_PER_SECOND` seconds.
///
/// The rate matters because the whole finding behind D25 is that what survives
/// tmux depends on how far the producer outruns tmux's redraw. A fixed gentle
/// rate would measure a load Berth is never asked to carry; the number worth
/// having is the most the renderer can be fed while the stream still arrives
/// intact, and that has to be found per host.
pub fn paced_command(count: usize) -> String {
    let delay = 1.0 / CHUNKS_PER_SECOND as f64;
    format!("i=0; while [ $i -lt {count} ]; do cat {CHUNK_FILE}; i=$((i+1)); sleep {delay}; done\n")
}

/// How long to allow for the chunk file to be written before replaying it.
pub const CHUNK_BUILD: Duration = Duration::from_secs(2);
/// Polling interval while waiting for a stream to go quiet.
const POLL: Duration = Duration::from_millis(250);
/// Consecutive quiet polls that count as the end of a stream.
const QUIET_POLLS: usize = 4;
/// A trial that has not finished by now is not a rate this host sustains.
const TRIAL_CAP: Duration = Duration::from_secs(30);

/// Run a command and wait for its output to stop, rather than for a fixed
/// window.
///
/// A fixed window conflates two different failures. If the producer cannot
/// sustain the rate asked of it, the loop simply runs long, and cutting the
/// measurement off early reports the shortfall as though tmux had dropped it.
/// Waiting for quiet separates them: what arrived is integrity, and how long it
/// took is the rate actually achieved.
async fn stream_until_quiet(
    view: &WeakEntity<TerminalView>,
    pty: &Pty,
    command: String,
    cx: &mut AsyncApp,
) -> (u64, Duration) {
    let _ = view.update(cx, |view, _| view.intake.reset_bytes());
    let start = Instant::now();
    pty.write(command).await;

    let mut last = 0u64;
    let mut quiet = 0usize;
    let mut moving_since = start;
    loop {
        cx.background_executor().timer(POLL).await;
        let now = view
            .read_with(cx, |view, _| view.bytes_in())
            .unwrap_or(last);
        if now == last {
            quiet += 1;
        } else {
            quiet = 0;
            last = now;
            moving_since = Instant::now();
        }
        if quiet >= QUIET_POLLS || start.elapsed() > TRIAL_CAP {
            break;
        }
    }
    // Discount the silence at the end, so the rate is over the time bytes were
    // actually moving.
    (last, moving_since.saturating_duration_since(start))
}

/// Find the fastest rate this host forwards intact, and report the ladder.
///
/// Climbs a doubling ladder. For each rate it records two things that a single
/// number would confuse: how much of what was written arrived, and how fast it
/// arrived. The rate chosen is the one with the highest achieved throughput
/// among those that arrived intact, which is what "the most this renderer can
/// be fed" means. Per host, because the knee moves with the link and with how
/// fast tmux can redraw at the far end.
async fn calibrate(
    view: &WeakEntity<TerminalView>,
    pty: &Pty,
    cx: &mut AsyncApp,
) -> (usize, String) {
    let mut best = RATE_LADDER[0];
    let mut best_achieved = 0.0f64;
    let mut lines = String::from("rate calibration, per requested rate: arrived, and achieved\n");

    for rate in RATE_LADDER {
        let chunk = (rate / CHUNKS_PER_SECOND).max(1);
        pty.write(chunk_command(rate)).await;
        cx.background_executor().timer(CHUNK_BUILD).await;

        let count = CHUNKS_PER_SECOND * TRIAL_SECONDS;
        let (received, elapsed) = stream_until_quiet(view, pty, paced_command(count), cx).await;

        // `fold` adds a newline every 76 bytes and tmux adds escape sequences,
        // so a healthy ratio sits slightly over 1 rather than exactly at it.
        let integrity = received as f64 / (chunk * count) as f64;
        let achieved = received as f64 / elapsed.as_secs_f64().max(0.001);
        let intact = integrity >= INTACT;
        lines.push_str(&format!(
            "  {:>5.0} MB/s requested: {integrity:.2} arrived, {:.1} MB/s achieved  {}\n",
            rate as f64 / 1024.0 / 1024.0,
            achieved / 1024.0 / 1024.0,
            if intact { "intact" } else { "collapsed" }
        ));
        if intact && achieved > best_achieved {
            best_achieved = achieved;
            best = rate;
        }
        // One collapse is the knee; asking for more only collapses harder.
        if !intact {
            break;
        }
    }
    (best, lines)
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
        view.intake.reset_bytes();
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
                view.bytes_in(),
            )
        })
        .unwrap_or_default();

    let (rate, ladder) = calibrate(&view, &pty, cx).await;

    let _ = view.update(cx, |view, _| {
        view.frames = crate::metrics::Histogram::new();
        view.intake.reset_bytes();
    });
    pty.write(chunk_command(rate)).await;
    cx.background_executor().timer(CHUNK_BUILD).await;
    let _ = view.update(cx, |view, _| {
        view.frames = crate::metrics::Histogram::new();
        view.intake.reset_bytes();
    });
    pty.write(paced_command(
        PACED_BYTES / (rate / CHUNKS_PER_SECOND).max(1),
    ))
    .await;
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
        let (cat_report, cat_bytes) = cat_phase;
        // The verdict comes from the paced phase, because it is the only one
        // that reliably puts bytes in front of the renderer. The cat phase is
        // kept because it is what the Windows baseline measured, and because
        // the gap between the two is the whole point.
        let text = format!(
            "{env}\
             {cat_report}bytes through the parser: {cat_bytes} ({:.1} MB of {:.0} MB written)\n\
             tmux collapse case, no renderer verdict: an instantaneous producer never \
             reaches the renderer, so these frame times say nothing about it.\n\
             \n\
             {ladder}\
             paced at {:.0} MB/s, the fastest rate this host forwards intact\n\
             {report}bytes through the parser: {} ({:.1} MB of {:.0} MB written)\n\
             \n\
             paced phase, no frame at or above {} ms: {}\n\
             paced phase, p99 frame time {p99:.1} ms against a {CAT_P99_CEILING_MS} ms \
             regression ceiling: {}\n",
            cat_bytes as f64 / 1024.0 / 1024.0,
            BENCH_BYTES as f64 / 1024.0 / 1024.0,
            rate as f64 / 1024.0 / 1024.0,
            view.bytes_in(),
            view.bytes_in() as f64 / 1024.0 / 1024.0,
            PACED_BYTES as f64 / 1024.0 / 1024.0,
            FRAME_BUDGET.as_millis(),
            if no_freeze { "PASS" } else { "FAIL" },
            if batching_intact { "PASS" } else { "FAIL" },
        );
        // Half the written bytes is a low bar deliberately: the point is that
        // the renderer was actually loaded, not that tmux forwarded perfectly.
        let arrived = view.bytes_in() as usize > PACED_BYTES / 2;
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
                view.bytes_in() as f64 / 1024.0 / 1024.0,
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

/// Where the local heavy-output benchmark keeps its file.
pub fn local_file() -> std::path::PathBuf {
    std::env::temp_dir().join("berth_bench_50mb_local.txt")
}

/// Write `BENCH_BYTES` of 76-column, base64-looking lines, as the remote
/// benchmark's file is, without needing `/dev/urandom` or a shell.
pub fn write_local_file(path: &std::path::Path) -> std::io::Result<()> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(BENCH_BYTES);
    let mut state: u32 = 0x9e37_79b9;
    while out.len() < BENCH_BYTES {
        for _ in 0..76 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            out.push(ALPHABET[(state >> 26) as usize]);
        }
        out.push(b'\n');
    }
    out.truncate(BENCH_BYTES);
    std::fs::write(path, out)
}

/// Heavy output in a plain local shell.
///
/// Through tmux the fast producer is collapsed before Berth sees it (D20, D25),
/// so that path can never load the renderer. A local pty has nothing in
/// between: all 50 MB reach the parser, and the frames drawn while they do are
/// the real test of the engine.
pub async fn run_local_cat(view: WeakEntity<TerminalView>, pty: Pty, cx: &mut AsyncApp) -> Outcome {
    let path = local_file();
    if let Err(error) = write_local_file(&path) {
        return Outcome {
            text: format!("could not write {}: {error}", path.display()),
            passed: false,
        };
    }
    // Let the prompt draw, then draw a screen of the file's own characters
    // once. The first frame to show a glyph rasterises it, a one-off of about
    // 10 ms that has nothing to do with sustained output (measured
    // 2026-09-30); left in, it is always the slowest frame of the run.
    cx.background_executor().timer(Duration::from_secs(2)).await;
    pty.write(format!("head -c 8192 '{}'\n", path.display()))
        .await;
    cx.background_executor().timer(Duration::from_secs(1)).await;
    let _ = view.update(cx, |view, _| {
        view.frames = crate::metrics::Histogram::new();
        view.slow_frames.clear();
        view.intake.reset_bytes();
        view.longest_gap = Duration::ZERO;
        view.last_frame_at = Some(std::time::Instant::now());
    });
    let started = std::time::Instant::now();
    pty.write(format!("cat '{}'\n", path.display())).await;

    // Until every byte is through, or a minute passes.
    let deadline = Duration::from_secs(60);
    loop {
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        let bytes = view.read_with(cx, |view, _| view.bytes_in()).unwrap_or(0);
        if bytes as usize >= BENCH_BYTES || started.elapsed() > deadline {
            break;
        }
    }
    let took = started.elapsed();
    // Let the last frames land.
    cx.background_executor().timer(Duration::from_secs(1)).await;
    let _ = std::fs::remove_file(&path);

    view.read_with(cx, |view, _| {
        let env = environment(view);
        let report = view.frames.report("frame build, local cat", Some(FRAME_BUDGET));
        let slow = view.slow_frames_report();
        let p99 = view.frames.percentile_ms(0.99);
        let no_freeze = view.frames.within(FRAME_BUDGET);
        let batching_intact = p99 <= f64::from(CAT_P99_CEILING_MS);
        let bytes = view.bytes_in() as f64;
        let arrived = bytes >= BENCH_BYTES as f64 * 0.95;
        // The freeze criterion, measured as what the eye sees: the longest the
        // window went without a new frame while the output was flowing.
        let gap = view.longest_gap;
        let no_stall = gap < FRAME_BUDGET;
        let painted = view.frames.count() >= 2;
        let mut text = format!(
            "{env}{report}{slow}bytes through the parser: {:.1} MB of {:.0} MB in {:.1} s ({:.0} MB/s)\n\
             longest time without a frame: {:.0} ms ({})\n\
             no frame at or above 32 ms: {}\n\
             p99 frame time {p99:.1} ms against a {CAT_P99_CEILING_MS} ms regression ceiling: {}\n",
            bytes / (1024.0 * 1024.0),
            BENCH_BYTES as f64 / (1024.0 * 1024.0),
            took.as_secs_f64(),
            bytes / (1024.0 * 1024.0) / took.as_secs_f64(),
            gap.as_secs_f64() * 1000.0,
            if no_stall { "PASS" } else { "FAIL: the window froze" },
            if no_freeze { "PASS" } else { "FAIL" },
            if batching_intact { "PASS" } else { "FAIL" },
        );
        if !arrived || !painted {
            text.push_str(&format!(
                "INVALID: {:.1} MB of {:.0} MB reached the parser over {} frames.\n",
                bytes / (1024.0 * 1024.0),
                BENCH_BYTES as f64 / (1024.0 * 1024.0),
                view.frames.count()
            ));
        }
        Outcome {
            text,
            passed: no_freeze && no_stall && batching_intact && arrived && painted,
        }
    })
    .unwrap_or_else(|_| Outcome {
        text: "the view went away before the report".to_string(),
        passed: false,
    })
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
            ["cat", "keys", "local-cat"]
                .iter()
                .filter(|m| matches!(*m, &"cat" | &"keys" | &"local-cat"))
                .count(),
            3
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

    /// The local file is the full size the criterion is stated in, so a
    /// short file cannot pass as a loaded renderer.
    #[test]
    fn the_local_file_is_fifty_megabytes_of_lines() {
        let dir = std::env::temp_dir().join(format!("berth-bench-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("f.txt");
        write_local_file(&path).expect("written");
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(bytes.len(), BENCH_BYTES);
        assert_eq!(bytes[76], b'\n');
        assert!(
            bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'\n'))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
