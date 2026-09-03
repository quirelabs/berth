//! The GPUI element that paints a terminal grid.
//!
//! Written against gpui's primitives rather than lifted from Zed, whose
//! element depends on `editor`, `theme`, `settings`, `ui` and `workspace`
//! (see decisions.md D17). The shape is Zed's: paint from an owned snapshot,
//! batch contiguous same-style cells into one shaped line, and never hold the
//! grid lock while painting.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    Bounds, Context, Element, ElementId, Entity, FocusHandle, Focusable, Font, FontFeatures,
    FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId, InteractiveElement,
    IntoElement, LayoutId, ParentElement, Pixels, SharedString, StrikethroughStyle, Style, Styled,
    TextAlign, TextRun, UnderlineStyle, Window, fill, point, px, size,
};

use berth_core::pty::{Pty, PtyEvent};
use berth_term::size::{CellSize, TermSize};
use berth_term::terminal::{Rgb, Snapshot};
use berth_term::{KeyEvent, Terminal};

use crate::metrics::{Histogram, KeystrokeLatency, LatencyReport};

/// Shared between the output pump and the UI. The pump holds the lock only for
/// as long as it takes to feed bytes to the parser.
pub type SharedTerminal = Arc<Mutex<Terminal>>;

pub struct TerminalView {
    terminal: SharedTerminal,
    pty: Option<Pty>,
    focus: FocusHandle,
    font_family: SharedString,
    font_size: Pixels,
    /// Frame build times, the basis of the heavy-output acceptance test.
    pub frames: Histogram,
    /// Set when the pump has seen output that is not yet painted.
    pub dirty: bool,
    /// Last size announced to the remote, so window_change is sent once.
    last_sent_size: Option<TermSize>,
    /// When the most recent keystroke was sent, and when its echo arrived.
    /// Together these split latency into the network half and the render half.
    pending_keystroke: Option<Instant>,
    echo_arrived: Option<(Instant, Duration)>,
    /// Keystroke latency samples, network and local reported separately.
    pub latency: LatencyReport,
    /// Bytes fed to the parser, so throughput claims can be checked.
    pub bytes_in: u64,
}

impl TerminalView {
    pub fn new(
        terminal: SharedTerminal,
        pty: Option<Pty>,
        font_family: impl Into<SharedString>,
        font_size: f32,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            terminal,
            pty,
            focus: cx.focus_handle(),
            font_family: font_family.into(),
            font_size: px(font_size),
            frames: Histogram::new(),
            dirty: false,
            last_sent_size: None,
            pending_keystroke: None,
            echo_arrived: None,
            latency: crate::metrics::LatencyReport::new(),
            bytes_in: 0,
        }
    }

    /// Feed output into the parser. Called from the coalescing pump, never
    /// per byte.
    pub fn ingest(&mut self, event: PtyEvent) {
        if let PtyEvent::Output(bytes) = event
            && let Ok(mut term) = self.terminal.lock()
        {
            term.advance(&bytes);
            self.bytes_in += bytes.len() as u64;
            self.dirty = true;
            // The first output after a keystroke is its echo. The round trip
            // ends here; whatever follows is Berth's own render cost.
            if let Some(sent) = self.pending_keystroke.take() {
                self.echo_arrived = Some((Instant::now(), sent.elapsed()));
            }
        }
    }

    /// Translate a key event and send it to the remote.
    pub fn send_key(&mut self, event: KeyEvent, _cx: &mut Context<Self>) {
        let application_cursor = self
            .terminal
            .lock()
            .map(|t| t.application_cursor())
            .unwrap_or(false);
        let mode = berth_term::TermMode { application_cursor };
        let Some(bytes) = berth_term::encode(event, mode) else {
            return;
        };
        let Some(pty) = self.pty.clone() else { return };
        // Timed from here so keystroke latency measures the whole journey.
        self.pending_keystroke = Some(Instant::now());
        crate::runtime::handle().spawn(async move {
            pty.write(bytes).await;
        });
    }

    /// Tell the remote the grid changed size. Only sent when it actually
    /// changed, since a window_change per frame would flood the channel.
    fn sync_remote_size(&mut self, size: TermSize) {
        if self.last_sent_size == Some(size) {
            return;
        }
        self.last_sent_size = Some(size);
        let Some(pty) = self.pty.clone() else { return };
        crate::runtime::handle().spawn(async move {
            pty.resize(size.cols, size.rows).await;
        });
    }

    fn font(&self) -> Font {
        Font {
            family: self.font_family.clone(),
            features: FontFeatures::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl gpui::Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let element = TerminalElement {
            view: cx.entity(),
            font: self.font(),
            font_size: self.font_size,
        };
        gpui::div()
            .track_focus(&self.focus)
            .key_context("Terminal")
            .size_full()
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    if let Some(key_event) = crate::input::translate(&event.keystroke) {
                        this.send_key(key_event, cx);
                    }
                }),
            )
            .child(element)
    }
}

pub struct TerminalElement {
    view: Entity<TerminalView>,
    font: Font,
    font_size: Pixels,
}

/// Everything painting needs, computed during prepaint.
pub struct PrepaintState {
    snapshot: Snapshot,
    runs: Vec<CellRun>,
    background: Hsla,
    cell: CellSize,
    started: Instant,
}

/// A horizontal stretch of cells sharing one style, shaped as a single line.
struct CellRun {
    text: String,
    line: u16,
    column: u16,
    fg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

fn hsla(c: Rgb) -> Hsla {
    gpui::Rgba {
        r: c.r as f32 / 255.0,
        g: c.g as f32 / 255.0,
        b: c.b as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.0).into();
        style.size.height = gpui::relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> Self::PrepaintState {
        let started = Instant::now();

        // Cell geometry comes from the font, exactly as Zed derives it.
        let font_id = window.text_system().resolve_font(&self.font);
        let advance = window
            .text_system()
            .advance(font_id, self.font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(px(8.0));
        let line_height = self.font_size * 1.3;
        let cell = CellSize::new(f32::from(advance), f32::from(line_height));

        let size = TermSize::from_pixels(
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            cell,
        );

        // One lock, held only long enough to resize and copy the grid out.
        let snapshot = {
            let view = self.view.read(cx);
            let mut term = view.terminal.lock().expect("terminal lock poisoned");
            term.resize(size, cell);
            term.snapshot()
        };

        let runs = batch_runs(&snapshot);
        let background = hsla(
            snapshot
                .cells
                .first()
                .map(|_| Rgb::new(0x10, 0x10, 0x12))
                .unwrap_or(Rgb::new(0x10, 0x10, 0x12)),
        );

        PrepaintState {
            snapshot,
            runs,
            background,
            cell,
            started,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request: &mut Self::RequestLayoutState,
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let cell_w = px(state.cell.width);
        let cell_h = px(state.cell.height);
        let origin = bounds.origin;

        window.paint_quad(fill(bounds, state.background));

        // Backgrounds first, as a separate pass, so glyphs are never clipped by
        // a neighbouring cell's fill.
        for cell in &state.snapshot.cells {
            let colour = hsla(cell.bg);
            if colour == state.background {
                continue;
            }
            let at = point(
                origin.x + cell_w * cell.column as f32,
                origin.y + cell_h * cell.line as f32,
            );
            window.paint_quad(fill(Bounds::new(at, size(cell_w, cell_h)), colour));
        }

        for run in &state.runs {
            let text: SharedString = run.text.clone().into();
            let mut font = self.font.clone();
            if run.bold {
                font.weight = FontWeight::BOLD;
            }
            if run.italic {
                font.style = FontStyle::Italic;
            }
            let text_run = TextRun {
                len: text.len(),
                font,
                color: run.fg,
                background_color: None,
                underline: run.underline.then(|| UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(run.fg),
                    wavy: false,
                }),
                strikethrough: run.strikethrough.then(|| StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(run.fg),
                }),
            };
            let at = point(
                origin.x + cell_w * run.column as f32,
                origin.y + cell_h * run.line as f32,
            );
            // force_width pins every glyph to the cell advance, which is what
            // makes a proportional fallback font still line up in the grid.
            let shaped = window.text_system().shape_line(
                text,
                self.font_size,
                std::slice::from_ref(&text_run),
                Some(cell_w),
            );
            let _ = shaped.paint(at, cell_h, TextAlign::Left, None, window, cx);
        }

        if state.snapshot.cursor.visible && !state.snapshot.is_scrolled_back() {
            let at = point(
                origin.x + cell_w * state.snapshot.cursor.column as f32,
                origin.y + cell_h * state.snapshot.cursor.line as f32,
            );
            window.paint_quad(fill(
                Bounds::new(at, size(cell_w, cell_h)),
                hsla(Rgb::new(0x4c, 0x8d, 0xff)),
            ));
        }

        let elapsed = state.started.elapsed();
        let painted_size = state.snapshot.size;
        self.view.update(cx, |view, _| {
            view.frames.record(elapsed);
            view.dirty = false;
            view.sync_remote_size(painted_size);
            // This frame carried the echoed glyph, so the local half of the
            // keystroke's journey ends now.
            if let Some((arrived, round_trip)) = view.echo_arrived.take() {
                view.latency.record(KeystrokeLatency {
                    round_trip,
                    local_render: arrived.elapsed(),
                });
            }
        });
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Group contiguous cells on a row that share a style into single runs.
///
/// One `shape_line` call per run rather than per glyph is the difference
/// between a comfortable frame and a stalled one when output is streaming.
fn batch_runs(snapshot: &Snapshot) -> Vec<CellRun> {
    let mut runs: Vec<CellRun> = Vec::new();
    let mut current: Option<CellRun> = None;

    for cell in &snapshot.cells {
        // Blanks paint nothing; breaking the run on them also keeps runs short.
        if cell.c == ' ' || cell.c == '\0' {
            if let Some(run) = current.take() {
                runs.push(run);
            }
            continue;
        }

        let continues = current.as_ref().is_some_and(|run| {
            run.line == cell.line
                && run.column + run.text.chars().count() as u16 == cell.column
                && run.fg == hsla(cell.fg)
                && run.bold == cell.bold
                && run.italic == cell.italic
                && run.underline == cell.underline
                && run.strikethrough == cell.strikethrough
        });

        if continues {
            if let Some(run) = current.as_mut() {
                run.text.push(cell.c);
            }
        } else {
            if let Some(run) = current.take() {
                runs.push(run);
            }
            let mut text = String::with_capacity(64);
            text.push(cell.c);
            current = Some(CellRun {
                text,
                line: cell.line,
                column: cell.column,
                fg: hsla(cell.fg),
                bold: cell.bold,
                italic: cell.italic,
                underline: cell.underline,
                strikethrough: cell.strikethrough,
            });
        }
    }
    if let Some(run) = current.take() {
        runs.push(run);
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_term::terminal::Terminal as CoreTerminal;

    fn snapshot_of(input: &[u8], cols: u16, rows: u16) -> Snapshot {
        let mut t = CoreTerminal::new(TermSize::new(cols, rows), CellSize::new(8.0, 16.0));
        t.advance(input);
        t.snapshot()
    }

    #[test]
    fn plain_text_becomes_one_run() {
        let runs = batch_runs(&snapshot_of(b"hello", 20, 3));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "hello");
        assert_eq!((runs[0].line, runs[0].column), (0, 0));
    }

    #[test]
    fn a_colour_change_splits_the_run() {
        let runs = batch_runs(&snapshot_of(b"ab\x1b[31mcd", 20, 3));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "ab");
        assert_eq!(runs[1].text, "cd");
        assert_ne!(runs[0].fg, runs[1].fg);
    }

    #[test]
    fn a_gap_splits_the_run() {
        let runs = batch_runs(&snapshot_of(b"ab  cd", 20, 3));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].column, 0);
        assert_eq!(runs[1].column, 4);
    }

    #[test]
    fn rows_never_merge() {
        let runs = batch_runs(&snapshot_of(b"ab\r\ncd", 20, 3));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].line, 0);
        assert_eq!(runs[1].line, 1);
    }

    #[test]
    fn an_empty_grid_produces_no_runs() {
        assert!(batch_runs(&snapshot_of(b"", 20, 3)).is_empty());
    }

    #[test]
    fn attributes_split_runs() {
        let runs = batch_runs(&snapshot_of(b"a\x1b[1mb", 20, 3));
        assert_eq!(runs.len(), 2);
        assert!(!runs[0].bold);
        assert!(runs[1].bold);
    }

    /// Batching is the performance property; a full row of one style must not
    /// become one shaping call per glyph.
    #[test]
    fn a_uniform_row_is_a_single_run() {
        let runs = batch_runs(&snapshot_of(&b"x".repeat(80), 80, 3));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text.chars().count(), 80);
    }

    #[test]
    fn wide_glyphs_keep_their_column_positions() {
        let runs = batch_runs(&snapshot_of("日本".as_bytes(), 20, 3));
        // The wide glyphs occupy columns 0 and 2, so they cannot be one run.
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].column, 0);
        assert_eq!(runs[1].column, 2);
    }
}
