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
    Bounds, Context, Element, ElementId, Entity, FocusHandle, Focusable, FontStyle, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, InteractiveElement, IntoElement, LayoutId,
    ParentElement, Pixels, Point, SharedString, StrikethroughStyle, Style, Styled, TextAlign,
    TextRun, UnderlineStyle, Window, fill, point, px, size,
};

use crate::terminal_style::{CursorShape, TerminalStyle};
use berth_core::pty::{Pty, PtyEvent};
use berth_term::size::{CellSize, TermSize};
use berth_term::terminal::Selection;
use berth_term::terminal::{Rgb, Snapshot};
use berth_term::{KeyEvent, MouseButton as TermButton, Terminal};

use crate::metrics::{Histogram, KeystrokeLatency, LatencyReport};

/// Shared between the output pump and the UI. The pump holds the lock only for
/// as long as it takes to feed bytes to the parser.
pub type SharedTerminal = Arc<Mutex<Terminal>>;

pub struct TerminalView {
    terminal: SharedTerminal,
    pty: Option<Pty>,
    focus: FocusHandle,
    style: TerminalStyle,
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
    /// Cell metrics as the platform font system resolved them. Reported by the
    /// benchmarks because font metrics are the most likely thing to differ
    /// between platforms, and a metric difference looks like a latency
    /// difference if it is not shown.
    pub resolved_cell: Option<CellSize>,
    /// The current local selection, if the user is dragging or has dragged.
    selection: Option<Selection>,
    /// Where a drag started, kept so the selection can be rebuilt as it moves.
    drag_anchor: Option<(u16, u16)>,
    /// Grid origin and cell size from the last paint, so a mouse position can
    /// be turned into a cell. Set during paint because only the element knows
    /// the laid-out bounds.
    geometry: Option<(Point<Pixels>, CellSize)>,
}

impl TerminalView {
    pub fn new(
        terminal: SharedTerminal,
        pty: Option<Pty>,
        style: TerminalStyle,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            terminal,
            pty,
            focus: cx.focus_handle(),
            style,
            frames: Histogram::new(),
            dirty: false,
            last_sent_size: None,
            pending_keystroke: None,
            echo_arrived: None,
            latency: crate::metrics::LatencyReport::new(),
            bytes_in: 0,
            resolved_cell: None,
            selection: None,
            drag_anchor: None,
            geometry: None,
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

    pub fn style(&self) -> &TerminalStyle {
        &self.style
    }

    /// Which cell a window position falls on, clamped to the grid.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let (origin, cell) = self.geometry?;
        let column = ((f32::from(position.x) - f32::from(origin.x)) / cell.width).floor();
        let line = ((f32::from(position.y) - f32::from(origin.y)) / cell.height).floor();
        Some((
            line.max(0.0).min(u16::MAX as f32) as u16,
            column.max(0.0).min(u16::MAX as f32) as u16,
        ))
    }

    /// True when the remote asked for mouse events, in which case a click
    /// belongs to the program rather than to local selection.
    fn forwards_mouse(&self) -> bool {
        self.terminal
            .lock()
            .map(|t| t.mouse_reporting())
            .unwrap_or(false)
    }

    fn on_mouse_down(&mut self, position: Point<Pixels>) {
        let Some((line, column)) = self.cell_at(position) else {
            return;
        };
        if self.forwards_mouse() {
            self.send_mouse(TermButton::Left, column, line, true);
            return;
        }
        self.drag_anchor = Some((line, column));
        self.selection = None;
    }

    fn on_mouse_drag(&mut self, position: Point<Pixels>) {
        if self.forwards_mouse() {
            return;
        }
        let (Some((anchor_line, anchor_column)), Some((line, column))) =
            (self.drag_anchor, self.cell_at(position))
        else {
            return;
        };
        self.selection = Some(Selection::new(anchor_line, anchor_column, line, column));
    }

    fn on_mouse_up(&mut self, position: Point<Pixels>) {
        if self.forwards_mouse() {
            if let Some((line, column)) = self.cell_at(position) {
                self.send_mouse(TermButton::Left, column, line, false);
            }
            return;
        }
        self.drag_anchor = None;
    }

    fn send_mouse(&self, button: TermButton, column: u16, line: u16, pressed: bool) {
        let Some(pty) = self.pty.clone() else { return };
        let bytes = berth_term::mouse_sgr(button, column, line, pressed);
        crate::runtime::handle().spawn(async move {
            pty.write(bytes).await;
        });
    }

    /// The selected text, for copying.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let text = self.terminal.lock().ok()?.selection_text(selection);
        (!text.is_empty()).then_some(text)
    }

    /// Paste, honouring the remote's bracketed paste mode.
    pub fn paste(&self, text: &str) {
        let bracketed = self
            .terminal
            .lock()
            .map(|t| t.bracketed_paste())
            .unwrap_or(false);
        let Some(pty) = self.pty.clone() else { return };
        let bytes = berth_term::paste_bytes(text, bracketed);
        crate::runtime::handle().spawn(async move {
            pty.write(bytes).await;
        });
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
            style: self.style.clone(),
        };
        gpui::div()
            .track_focus(&self.focus)
            .key_context("Terminal")
            .size_full()
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    // Copy and paste are chords the terminal never sees; a
                    // bare Ctrl+C must still reach the remote as an interrupt,
                    // which is why copy is Ctrl+Shift+C.
                    match crate::input::clipboard_action(&event.keystroke) {
                        Some(crate::input::Clipboard::Copy) => {
                            if let Some(text) = this.selected_text() {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                            }
                        }
                        Some(crate::input::Clipboard::Paste) => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                this.paste(&text);
                            }
                        }
                        None => {
                            if let Some(key_event) = crate::input::translate(&event.keystroke) {
                                this.send_key(key_event, cx);
                            }
                        }
                    }
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                    this.on_mouse_down(event.position);
                    cx.notify();
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                    if event.dragging() {
                        this.on_mouse_drag(event.position);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, _window, cx| {
                    this.on_mouse_up(event.position);
                    cx.notify();
                }),
            )
            .child(element)
    }
}

pub struct TerminalElement {
    view: Entity<TerminalView>,
    style: TerminalStyle,
}

/// Everything painting needs, computed during prepaint.
pub struct PrepaintState {
    snapshot: Snapshot,
    selection: Option<Selection>,
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
        let font = self.style.font();
        let font_id = window.text_system().resolve_font(&font);
        let advance = window
            .text_system()
            .advance(font_id, self.style.font_size_px(), 'm')
            .map(|s| f32::from(s.width))
            .unwrap_or(self.style.fallback_advance);
        let cell = CellSize::new(
            self.style.cell_rounding.apply(advance),
            self.style
                .cell_rounding
                .apply(f32::from(self.style.line_height_px())),
        );

        // Padding is inside the pane, so the grid is measured against what is
        // left after it.
        let size = TermSize::from_pixels(
            f32::from(bounds.size.width) - self.style.padding_x * 2.0,
            f32::from(bounds.size.height) - self.style.padding_y * 2.0,
            cell,
        );

        // One lock, held only long enough to resize and copy the grid out.
        let snapshot = {
            let view = self.view.read(cx);
            let mut term = view.terminal.lock().expect("terminal lock poisoned");
            term.resize(size, cell);
            term.snapshot()
        };

        let selection = self.view.read(cx).selection;
        let runs = batch_runs(&snapshot);
        let background = self.style.colour(self.style.background);

        PrepaintState {
            snapshot,
            selection,
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
        let origin = point(
            bounds.origin.x + px(self.style.padding_x),
            bounds.origin.y + px(self.style.padding_y),
        );

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

        // Selection sits above the cell backgrounds and below the glyphs, so
        // the text under it stays readable.
        if let Some(selection) = state.selection.map(Selection::normalised)
            && !selection.is_empty()
        {
            let colour = self.style.selection_colour();
            for cell in &state.snapshot.cells {
                if !selection_covers(selection, cell.line, cell.column) {
                    continue;
                }
                let at = point(
                    origin.x + cell_w * cell.column as f32,
                    origin.y + cell_h * cell.line as f32,
                );
                window.paint_quad(fill(Bounds::new(at, size(cell_w, cell_h)), colour));
            }
        }

        for run in &state.runs {
            let text: SharedString = run.text.clone().into();
            let mut font = self.style.font();
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
                    thickness: px(self.style.underline_thickness),
                    color: Some(run.fg),
                    wavy: false,
                }),
                strikethrough: run.strikethrough.then(|| StrikethroughStyle {
                    thickness: px(self.style.strikethrough_thickness),
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
                self.style.font_size_px(),
                std::slice::from_ref(&text_run),
                Some(cell_w),
            );
            let _ = shaped.paint(at, cell_h, TextAlign::Left, None, window, cx);
        }

        if state.snapshot.cursor.visible && !state.snapshot.is_scrolled_back() {
            let cursor = &self.style.cursor;
            let at = point(
                origin.x + cell_w * state.snapshot.cursor.column as f32,
                origin.y + cell_h * state.snapshot.cursor.line as f32,
            );
            let thickness = px(cursor.thickness);
            let (at, cursor_size) = match cursor.shape {
                CursorShape::Block => (at, size(cell_w, cell_h)),
                CursorShape::Bar => (at, size(thickness, cell_h)),
                CursorShape::Underline => (
                    point(at.x, at.y + cell_h - thickness),
                    size(cell_w, thickness),
                ),
            };
            window.paint_quad(fill(
                Bounds::new(at, cursor_size),
                self.style.colour(cursor.colour),
            ));
        }

        let elapsed = state.started.elapsed();
        let painted_size = state.snapshot.size;
        let geometry = (origin, state.cell);
        let painted_cell = state.cell;
        self.view.update(cx, |view, _| {
            view.frames.record(elapsed);
            view.dirty = false;
            view.sync_remote_size(painted_size);
            view.geometry = Some(geometry);
            view.resolved_cell = Some(painted_cell);
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

/// Whether a selection covers a cell. Mirrors the rule in berth-term, which
/// owns the text extraction; this is the painting half of the same shape.
fn selection_covers(selection: Selection, line: u16, column: u16) -> bool {
    if line < selection.start_line || line > selection.end_line {
        return false;
    }
    if line == selection.start_line && column < selection.start_column {
        return false;
    }
    if line == selection.end_line && column > selection.end_column {
        return false;
    }
    true
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
