//! Where libghostty-vt's parsing time goes: the same 64 MB fed in different
//! shapes, with history off and with a 10,000-line limit, next to Berth's
//! current engine. Linux parsed three times slower than the current engine in
//! the spike's first report; this separates the likely causes.

use std::time::Instant;

const MB: usize = 1024 * 1024;

fn stream(unit: &[u8]) -> Vec<u8> {
    let mut s = Vec::with_capacity(64 * MB + unit.len());
    while s.len() < 64 * MB {
        s.extend_from_slice(unit);
    }
    s
}

fn plain() -> Vec<u8> {
    let mut u = Vec::new();
    for n in 0..200 {
        u.extend_from_slice(format!("{n:>6} the quick brown fox jumps over the lazy dog, again\r\n").as_bytes());
    }
    stream(&u)
}

fn coloured() -> Vec<u8> {
    let mut u = Vec::new();
    for n in 0..200 {
        u.extend_from_slice(
            format!("\x1b[38;5;{}m{n:>6}\x1b[0m \x1b[1mthe quick\x1b[0m brown \x1b[3;4mfox\x1b[0m jumps\r\n", n % 256)
                .as_bytes(),
        );
    }
    stream(&u)
}

fn mixed() -> Vec<u8> {
    let mut u = coloured()[..20_000].to_vec();
    u.extend_from_slice(b"\x1b]0;a title\x07\x1b]7;file://h/p\x07\x1b]8;;https://x.y\x1b\\link\x1b]8;;\x1b\\\r\n");
    stream(&u)
}

fn unicode() -> Vec<u8> {
    let mut u = Vec::new();
    for n in 0..200 {
        u.extend_from_slice(format!("{n:>6} 日本語のテキスト 中文 한국어 wide text\r\n").as_bytes());
    }
    stream(&u)
}

fn emoji() -> Vec<u8> {
    let mut u = Vec::new();
    for n in 0..200 {
        u.extend_from_slice(format!("{n:>6} 😀 👍🏽 🇳🇱 e\u{301} a\u{308} combining and emoji\r\n").as_bytes());
    }
    stream(&u)
}

fn rate(bytes: &[u8], mut write: impl FnMut(&[u8])) -> f64 {
    let start = Instant::now();
    for chunk in bytes.chunks(8192) {
        write(chunk);
    }
    bytes.len() as f64 / MB as f64 / start.elapsed().as_secs_f64()
}

fn main() {
    println!("{:<10} {:>16} {:>18} {:>14}", "stream", "ghostty, no hist", "ghostty, 10k lines", "alacritty 10k");
    for (name, bytes) in [
        ("plain", plain()),
        ("coloured", coloured()),
        ("mixed", mixed()),
        ("cjk wide", unicode()),
        ("emoji", emoji()),
    ] {
        let mut none = libghostty_vt::Terminal::new(100, 30).expect("terminal");
        let _ = none.set_scrollback_max_bytes(None);
        let _ = none.set_scrollback_max_lines(Some(0));
        let a = rate(&bytes, |c| none.vt_write(c));

        let mut hist = libghostty_vt::Terminal::new(100, 30).expect("terminal");
        let _ = hist.set_scrollback_max_bytes(None);
        let _ = hist.set_scrollback_max_lines(Some(10_000));
        let b = rate(&bytes, |c| hist.vt_write(c));

        let mut ala = berth_term::Terminal::new(
            berth_term::size::TermSize::new(100, 30),
            berth_term::size::CellSize::new(8.0, 16.0),
        );
        let c = rate(&bytes, |x| ala.advance(x));
        println!("{name:<10} {a:>13.0} MB/s {b:>15.0} MB/s {c:>11.0} MB/s");
    }
}
