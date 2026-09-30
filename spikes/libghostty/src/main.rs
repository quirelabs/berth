//! D49 step 1: can libghostty-vt be Berth's engine on both ends?
//!
//! Prints a report. Each section answers one question the direction asked:
//! does a snapshot round-trip, is dirty tracking usable for diffs, can it
//! answer programs' queries, what are its thread limits, and how fast is it
//! next to the engine Berth uses today.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::time::Instant;

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::terminal::{Mode, ScrollViewport};
use libghostty_vt::{RenderState, Terminal};

const COLS: u16 = 100;
const ROWS: u16 = 30;

fn terminal() -> Terminal<'static, 'static> {
    let mut term = Terminal::new(COLS, ROWS).expect("a terminal");
    // Both limits apply and the first reached wins, so the default byte
    // limit has to go for the line limit to be the one that counts.
    term.set_scrollback_max_bytes(None).expect("no byte limit");
    term.set_scrollback_max_lines(Some(10_000)).expect("line limit");
    term
}

/// Ghostty's own snapshot: encode, then decode into a fresh terminal.
fn snapshot_copy(term: &mut Terminal<'static, 'static>) -> (Terminal<'static, 'static>, usize) {
    let mut bytes = Vec::new();
    term.encode_snapshot(&mut bytes).expect("encoded");
    let size = bytes.len();
    let decoded = libghostty_vt::snapshot::Decoder::new_buf(Box::leak(bytes.into_boxed_slice()))
        .expect("decoder")
        .decode()
        .expect("decoded");
    (decoded, size)
}

/// Every visible cell: text, colours and the style flags that matter.
fn visible(term: &Terminal) -> Vec<String> {
    let mut state = RenderState::new().expect("render state");
    let snapshot = state.update(term).expect("snapshot");
    let mut rows = RowIterator::new().expect("rows");
    let mut cells = CellIterator::new().expect("cells");
    let mut out = Vec::new();
    let mut iteration = rows.update(&snapshot).expect("row iteration");
    while let Some(row) = iteration.next() {
        let mut line = String::new();
        let mut cell_iteration = cells.update(row).expect("cell iteration");
        while let Some(cell) = cell_iteration.next() {
            let text: String = cell.graphemes().unwrap_or_default().into_iter().collect();
            let style = cell
                .style()
                .map(|s| {
                    format!(
                        "{}{}{}{}{:?}",
                        s.bold as u8, s.italic as u8, s.inverse as u8, s.strikethrough as u8, s.underline
                    )
                })
                .unwrap_or_default();
            line.push_str(&format!(
                "[{text}|{:?}|{:?}|{style}]",
                cell.fg_color().ok().flatten(),
                cell.bg_color().ok().flatten()
            ));
        }
        out.push(line);
    }
    out
}

/// Everything else a client needs to be in the same state.
fn facts(term: &Terminal) -> Vec<(String, String)> {
    let modes = [
        ("DECCKM", Mode::DECCKM),
        ("ALT_SCREEN_SAVE", Mode::ALT_SCREEN_SAVE),
        ("BRACKETED_PASTE", Mode::BRACKETED_PASTE),
        ("SGR_MOUSE", Mode::SGR_MOUSE),
        ("BUTTON_MOUSE", Mode::BUTTON_MOUSE),
        ("CURSOR_VISIBLE", Mode::CURSOR_VISIBLE),
        ("ORIGIN", Mode::ORIGIN),
        ("WRAPAROUND", Mode::WRAPAROUND),
        ("FOCUS_EVENT", Mode::FOCUS_EVENT),
    ];
    let mut out: Vec<(String, String)> = modes
        .iter()
        .map(|(name, mode)| (format!("mode {name}"), format!("{:?}", term.mode(*mode))))
        .collect();
    out.push(("cursor".into(), format!("{:?},{:?}", term.cursor_x(), term.cursor_y())));
    out.push(("screen".into(), format!("{:?}", term.active_screen())));
    out.push(("title".into(), format!("{:?}", term.title())));
    out.push(("pwd".into(), format!("{:?}", term.pwd())));
    out.push(("scrollback rows".into(), format!("{:?}", term.scrollback_rows())));
    out
}

/// The whole state as VT sequences, every option on.
fn snapshot_bytes(term: &Terminal) -> Vec<u8> {
    let options = FormatterOptions::new()
        .with_format(Format::Vt)
        .with_palette(true)
        .with_modes(true)
        .with_scrolling_region(true)
        .with_tabstops(true)
        .with_pwd(true)
        .with_keyboard(true)
        .with_cursor(true)
        .with_style(true)
        .with_hyperlink(true)
        .with_protection(true)
        .with_kitty_keyboard(true)
        .with_charsets(true);
    let mut formatter = Formatter::new(term, options).expect("formatter");
    formatter.format_alloc(None).expect("formatted").to_vec()
}

fn plain(term: &Terminal) -> String {
    let mut formatter =
        Formatter::new(term, FormatterOptions::new().with_format(Format::Plain)).expect("formatter");
    String::from_utf8_lossy(&formatter.format_alloc(None).expect("formatted")).into_owned()
}

fn compare(label: &str, source: &Terminal) {
    let bytes = snapshot_bytes(source);
    let mut copy = terminal();
    copy.vt_write(&bytes);
    // The formatter's cursor workaround, as Berth would apply it.
    let (x, y) = (source.cursor_x().unwrap_or(0), source.cursor_y().unwrap_or(0));
    copy.vt_write(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
    report(label, source, &copy, bytes.len());
}

fn report(label: &str, source: &Terminal, copy: &Terminal, size: usize) {

    let (a, b) = (visible(source), visible(copy));
    let differing: Vec<usize> = (0..a.len().max(b.len()))
        .filter(|i| a.get(*i) != b.get(*i))
        .collect();
    let (fa, fb) = (facts(source), facts(copy));
    let fact_diffs: Vec<String> = fa
        .iter()
        .zip(fb.iter())
        .filter(|(x, y)| x != y)
        .map(|(x, y)| format!("{}: {} -> {}", x.0, x.1, y.1))
        .collect();
    let (pa, pb) = (plain(source), plain(copy));

    println!("  {label}: {size} bytes");
    println!(
        "    visible cells: {}",
        if differing.is_empty() {
            "identical".to_string()
        } else {
            format!("{} of {} rows differ, first at row {}", differing.len(), a.len(), differing[0])
        }
    );
    if let Some(row) = differing.first() {
        let show = |s: Option<&String>| s.map(|s| s.chars().take(240).collect::<String>()).unwrap_or_default();
        println!("      source: {}", show(a.get(*row)));
        println!("      copy:   {}", show(b.get(*row)));
    }
    println!(
        "    modes, cursor, screen, title, pwd, scrollback: {}",
        if fact_diffs.is_empty() { "identical".to_string() } else { fact_diffs.join("; ") }
    );
    println!(
        "    full text incl. history: {} ({} vs {} bytes)",
        if pa == pb { "identical" } else { "DIFFERS" },
        pa.len(),
        pb.len()
    );
}

fn shell_session() -> Vec<u8> {
    let mut s = Vec::new();
    for n in 0..200 {
        s.extend_from_slice(format!("\x1b[38;5;{}mline {n}\x1b[0m history\r\n", n % 256).as_bytes());
    }
    s.extend_from_slice(b"\x1b]0;my title\x07\x1b]7;file://host/Users/me/work\x07");
    s.extend_from_slice("\x1b[1;32m➜\x1b[0m \x1b[1;36m~/work\x1b[0m ls\r\n".as_bytes());
    s.extend_from_slice("\x1b[34mdir\x1b[0m  \x1b[1;4mbold under\x1b[0m  \x1b[3;9mitalic strike\x1b[0m  \x1b[7minverse\x1b[0m\r\n".as_bytes());
    s.extend_from_slice("日本語 wide  e\u{301} combining  😀 emoji  \x1b[38;2;255;100;0mtruecolor\x1b[0m\r\n".as_bytes());
    s.extend_from_slice(b"\x1b]8;;https://example.com\x1b\\a link\x1b]8;;\x1b\\\r\n");
    s.extend_from_slice(b"\x1b[?2004h\x1b[1;32m\xe2\x9e\x9c\x1b[0m half-typed com");
    s
}

fn full_screen_app() -> Vec<u8> {
    let mut s = shell_session();
    s.extend_from_slice(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?1h\x1b[?1002h\x1b[?1006h\x1b[?25l\x1b[2;28r");
    for n in 1..=ROWS {
        s.extend_from_slice(format!("\x1b[{n};1H\x1b[48;5;236m~ vim row {n}\x1b[K\x1b[0m").as_bytes());
    }
    s.extend_from_slice(b"\x1b[30;1H\x1b[7m-- INSERT --\x1b[0m\x1b[5;10H");
    s
}

struct Probe<T>(PhantomData<T>);
trait NotSend {
    fn is_send() -> bool {
        false
    }
}
impl<T> NotSend for Probe<T> {}
#[allow(dead_code)]
impl<T: Send> Probe<T> {
    fn is_send() -> bool {
        true
    }
}
trait NotSync {
    fn is_sync() -> bool {
        false
    }
}
impl<T> NotSync for Probe<T> {}
#[allow(dead_code)]
impl<T: Sync> Probe<T> {
    fn is_sync() -> bool {
        true
    }
}

fn main() {
    println!("libghostty-vt {}", libghostty_vt::build_info::version_string().unwrap_or("?"));
    println!(
        "  simd {:?}, kitty graphics {:?}, tmux control mode {:?}, optimize {:?}",
        libghostty_vt::build_info::supports_simd(),
        libghostty_vt::build_info::supports_kitty_graphics(),
        libghostty_vt::build_info::supports_tmux_control_mode(),
        libghostty_vt::build_info::optimize_mode()
    );

    println!("\n1. Snapshot round-trip into a fresh engine");
    let scenarios: Vec<(&str, Vec<u8>)> = vec![
        ("shell with 200 lines of history", shell_session()),
        ("the same, ending on a scroll (Ghostty #14148)", {
            let mut s = shell_session();
            s.extend_from_slice(b"\r\n");
            s
        }),
        ("full-screen app on the alternate screen", full_screen_app()),
    ];
    for (label, input) in &scenarios {
        let mut source = terminal();
        source.vt_write(input);
        println!(" {label}");
        compare("   formatter (VT, every option, cursor appended)", &source);
        let (copy, size) = snapshot_copy(&mut source);
        report("   Ghostty snapshot API", &source, &copy, size);
    }

    println!("\n1a. Leaving the full-screen app after the round-trip: is the screen underneath intact?");
    let mut source = terminal();
    source.vt_write(&full_screen_app());
    let (mut copy, _) = snapshot_copy(&mut source);
    source.vt_write(b"\x1b[?1049l");
    copy.vt_write(b"\x1b[?1049l");
    report("   snapshot API, after leaving", &source, &copy, 0);
    let mut formatted = terminal();
    formatted.vt_write(&snapshot_bytes(&{
        let mut t = terminal();
        t.vt_write(&full_screen_app());
        t
    }));
    formatted.vt_write(b"\x1b[?1049l");
    report("   formatter, after leaving", &source, &formatted, 0);

    println!("\n1c. Incremental restore, 10,000 lines of history: the screen first, history after");
    let mut source = terminal();
    for n in 0..10_000 {
        source.vt_write(format!("\x1b[38;5;{}m{n:>6} {}\x1b[0m\r\n", n % 256, "abcdefghij".repeat(9)).as_bytes());
    }
    source.vt_write(&shell_session());
    let mut bytes = Vec::new();
    source.encode_snapshot(&mut bytes).expect("encoded");
    let decoder = libghostty_vt::snapshot::Decoder::new_buf(&bytes).expect("decoder");
    let dump = std::env::temp_dir().join("berth-spike-snapshot.bin");
    let _ = std::fs::write(&dump, &bytes);
    let _ = std::fs::write(std::env::temp_dir().join("berth-spike-ready.bin"), &bytes[..372_748.min(bytes.len())]);
    let mut incremental = decoder.ready().expect("ready");
    println!(
        "  at READY ({} bytes in all): screen renderable, {:?} history rows so far, visible rows match source: {}",
        bytes.len(),
        incremental.terminal().scrollback_rows(),
        visible(incremental.terminal()) == visible(&source)
    );
    let mut steps = 0;
    let mut offsets = Vec::new();
    while let Some(progress) = incremental.next().expect("next") {
        steps += 1;
        offsets.push(progress.as_decoder().source_offset().unwrap_or(0));
    }
    println!(
        "  decoded terminal limits: bytes {:?}, lines {:?}",
        incremental.terminal().scrollback_max_bytes(),
        incremental.terminal().scrollback_max_lines()
    );
    println!(
        "  then {} history pages, arriving at byte offsets {:?} ... {:?}",
        offsets.len(),
        offsets.first(),
        offsets.last()
    );
    println!(
        "  then {steps} history steps; history rows now {:?} (source {:?})",
        incremental.terminal().scrollback_rows(),
        source.scrollback_rows()
    );

    let mut shell = terminal();
    shell.vt_write(&shell_session());
    let mut app = terminal();
    app.vt_write(&full_screen_app());

    println!("\n1b. Details");
    let (a, b) = (visible(&app), {
        let mut copy = terminal();
        copy.vt_write(&snapshot_bytes(&app));
        visible(&copy)
    });
    if let (Some(ra), Some(rb)) = (a.first(), b.first()) {
        let ca: Vec<&str> = ra.split(']').collect();
        let cb: Vec<&str> = rb.split(']').collect();
        if let Some(i) = (0..ca.len().max(cb.len())).find(|i| ca.get(*i) != cb.get(*i)) {
            println!("  alt row 0 first differing cell {i}: source {:?} copy {:?}", ca.get(i), cb.get(i));
        }
    }
    let raw = snapshot_bytes(&shell);
    let tail = String::from_utf8_lossy(&raw[raw.len().saturating_sub(160)..]).replace('\x1b', "ESC");
    println!("  end of the shell snapshot: {tail}");
    let raw = snapshot_bytes(&app);
    let head = String::from_utf8_lossy(&raw[..raw.len().min(200)]).replace('\x1b', "ESC");
    println!("  start of the app snapshot: {head}");
    let mut copy = terminal();
    copy.vt_write(&raw);
    println!("  app copy plain text starts: {:?}", plain(&copy).chars().take(80).collect::<String>());
    println!("  app source plain text starts: {:?}", plain(&app).chars().take(80).collect::<String>());

    println!("\n2. Dirty rows, for diffs, clearing flags after each draw as a renderer does");
    let mut term = terminal();
    term.vt_write(&shell_session());
    let mut state = RenderState::new().expect("render state");
    let mut rows = RowIterator::new().expect("rows");
    fn draw(
        state: &mut RenderState<'static>,
        rows: &mut RowIterator<'static>,
        term: &Terminal<'static, 'static>,
        label: &str,
    ) {
        let snapshot = state.update(term).expect("snapshot");
        let overall = snapshot.dirty();
        let mut iteration = rows.update(&snapshot).expect("rows");
        let (mut dirty, mut total) = (0, 0);
        while let Some(row) = iteration.next() {
            total += 1;
            if row.dirty().unwrap_or(false) {
                dirty += 1;
                let _ = row.set_dirty(false);
            }
        }
        let _ = snapshot.set_dirty(libghostty_vt::render::Dirty::Clean);
        println!("  {label}: overall {overall:?}, {dirty} of {total} rows dirty");
    }
    draw(&mut state, &mut rows, &term, "first draw");
    term.vt_write(b"x");
    draw(&mut state, &mut rows, &term, "one character on the current line");
    term.vt_write(b"\x1b[5;1Hchanged");
    draw(&mut state, &mut rows, &term, "one row rewritten elsewhere");
    term.vt_write(b"\r\n");
    draw(&mut state, &mut rows, &term, "a newline at the bottom (scrolls)");
    term.vt_write(b"\x1b[?1049h");
    draw(&mut state, &mut rows, &term, "switch to the alternate screen");

    println!("\n3. Replies to programs' queries (answering as Berth)");
    let replies = RefCell::new(Vec::<Vec<u8>>::new());
    {
        let mut term = Terminal::new(COLS, ROWS).expect("a terminal");
        term.on_pty_write(|_t, data| replies.borrow_mut().push(data.to_vec()))
            .expect("callback");
        term.vt_write(b"\x1b[c\x1b[6n\x1b[>q\x1b]10;?\x1b\\");
    }
    for reply in replies.borrow().iter() {
        println!("  {:?}", String::from_utf8_lossy(reply));
    }
    if replies.borrow().is_empty() {
        println!("  none");
    }

    println!("\n4. Thread limits");
    println!(
        "  Terminal: Send {}, Sync {}",
        Probe::<Terminal<'static, 'static>>::is_send(),
        Probe::<Terminal<'static, 'static>>::is_sync()
    );
    println!(
        "  RenderState: Send {}, Sync {}",
        Probe::<RenderState<'static>>::is_send(),
        Probe::<RenderState<'static>>::is_sync()
    );

    println!("\n5. Throughput, parsing only, next to Berth's engine today");
    let mut stream = Vec::new();
    while stream.len() < 64 * 1024 * 1024 {
        stream.extend_from_slice(&shell_session());
    }
    let mb = stream.len() as f64 / (1024.0 * 1024.0);
    let mut term = terminal();
    let start = Instant::now();
    for chunk in stream.chunks(8192) {
        term.vt_write(chunk);
    }
    let ghostty = start.elapsed().as_secs_f64();
    let mut alacritty = berth_term::Terminal::new(
        berth_term::size::TermSize::new(COLS, ROWS),
        berth_term::size::CellSize::new(8.0, 16.0),
    );
    let start = Instant::now();
    for chunk in stream.chunks(8192) {
        alacritty.advance(chunk);
    }
    let berth = start.elapsed().as_secs_f64();
    println!("  {mb:.0} MB: libghostty-vt {:.0} MB/s, berth-term (alacritty) {:.0} MB/s", mb / ghostty, mb / berth);
    term.scroll_viewport(ScrollViewport::Bottom);

    println!("\n6. Memory per terminal, 100x30 with 10,000 lines of coloured history each");
    let history: Vec<u8> = (0..10_000)
        .flat_map(|n| {
            format!(
                "\x1b[38;5;{}m{:>6} \x1b[1m{}\x1b[0m\r\n",
                n % 256,
                n,
                "abcdefghij".repeat(9)
            )
            .into_bytes()
        })
        .collect();
    const N: usize = 20;
    let before = rss_kb();
    let mut ghostties: Vec<Terminal<'static, 'static>> = Vec::new();
    for _ in 0..N {
        let mut t = terminal();
        t.vt_write(&history);
        ghostties.push(t);
    }
    let ghostty_kb = rss_kb().saturating_sub(before) / N as u64;
    println!(
        "  history kept: libghostty-vt {:?} rows",
        ghostties[0].scrollback_rows()
    );
    drop(ghostties);
    let before = rss_kb();
    let mut alacritties = Vec::new();
    for _ in 0..N {
        let mut t = berth_term::Terminal::new(
            berth_term::size::TermSize::new(COLS, ROWS),
            berth_term::size::CellSize::new(8.0, 16.0),
        );
        t.advance(&history);
        alacritties.push(t);
    }
    let alacritty_rows = {
        let t = &mut alacritties[0];
        t.scroll(berth_term::Scroll::Lines(i32::MAX / 2));
        t.display_offset()
    };
    println!("  history kept: berth-term {alacritty_rows} rows");
    let alacritty_kb = rss_kb().saturating_sub(before) / N as u64;
    drop(alacritties);
    println!(
        "  resident per terminal: libghostty-vt {:.1} MB, berth-term (alacritty) {:.1} MB",
        ghostty_kb as f64 / 1024.0,
        alacritty_kb as f64 / 1024.0
    );
}

/// This process's resident set, in KB, as `ps` reports it. Zero where there
/// is no `ps` (Windows); the section then reports nothing useful there.
fn rss_kb() -> u64 {
    if cfg!(windows) {
        return 0;
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}
