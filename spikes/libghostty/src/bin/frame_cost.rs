//! The per-frame cost D49 asked about: handing the renderer a copy of the
//! screen, through berth-term's own interface, on whichever engine it was
//! built with. Also what creating a terminal costs.

use std::time::Instant;

use berth_term::size::{CellSize, TermSize};
use berth_term::Terminal;

fn main() {
    let engine = if cfg!(feature = "ghostty-term") { "libghostty-vt" } else { "alacritty" };
    println!("engine: {engine}");

    let start = Instant::now();
    for _ in 0..20 {
        let t = Terminal::new(TermSize::new(100, 30), CellSize::new(8.0, 16.0));
        drop(t);
    }
    println!("  new terminal: {:.2} ms each", start.elapsed().as_secs_f64() * 1000.0 / 20.0);

    for (cols, rows) in [(100u16, 30u16), (220, 60)] {
        let mut term = Terminal::new(TermSize::new(cols, rows), CellSize::new(8.0, 16.0));
        let line: String = (0..cols).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        for n in 0..rows as usize * 3 {
            term.advance(format!("\x1b[38;5;{}m{}\x1b[0m\r\n", n % 256, &line[..line.len() - 1]).as_bytes());
        }
        // Warm up, then time many frames; one keystroke echo between frames,
        // as in the latency benchmark.
        for _ in 0..50 {
            let _ = term.snapshot();
        }
        let mut samples = Vec::new();
        for i in 0..2000 {
            term.advance(if i % 2 == 0 { b"x" } else { b"\x08" });
            let t = Instant::now();
            let snap = term.snapshot();
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box(snap);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p = |q: f64| samples[((samples.len() as f64 - 1.0) * q) as usize];
        println!(
            "  {cols}x{rows} snapshot per frame: p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms",
            p(0.5),
            p(0.99),
            samples[samples.len() - 1]
        );
        let t = Instant::now();
        for _ in 0..2000 {
            term.advance(b"x");
        }
        println!("  one keystroke echo parsed: {:.4} ms each", t.elapsed().as_secs_f64() * 1000.0 / 2000.0);
    }
}
