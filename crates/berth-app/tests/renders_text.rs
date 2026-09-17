//! Read the framebuffer back and assert a glyph actually reached it.
//!
//! This exists because B5 was invisible to everything else Berth had: the build
//! succeeded, the window opened, layout produced sensible metrics, paint
//! reported no error, and the screen was empty. The only observable that was
//! wrong was the pixels, so this is the only check that could have caught it.
//!
//! Skipped unless `BERTH_PIXEL_CHECK` is set, because it needs a window server
//! and permission to photograph the screen. CI sets it on the desktop runners.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

/// A pixel counts as drawn if any channel is well clear of the black
/// background. The probe draws pure white, so this needs no tuning.
const LIT: u8 = 96;

struct Probe(Child);

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Window origin and size, then the two bands, all in points.
struct Geometry {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    text: (f32, f32),
    control: (f32, f32),
}

fn parse(line: &str) -> Option<Geometry> {
    let field = |key: &str| -> Option<Vec<f32>> {
        let rest = line.split(key).nth(1)?;
        let value = rest.split_whitespace().next()?;
        value.split(',').map(|n| n.parse().ok()).collect()
    };
    let window = field("window=")?;
    let text = field("text=")?;
    let control = field("control=")?;
    Some(Geometry {
        x: *window.first()?,
        y: *window.get(1)?,
        w: *window.get(2)?,
        h: *window.get(3)?,
        text: (*text.first()?, *text.get(1)?),
        control: (*control.first()?, *control.get(1)?),
    })
}

#[test]
fn a_glyph_reaches_the_framebuffer() {
    if std::env::var(berth_core::env::PIXEL_CHECK).is_err() {
        eprintln!("skipping: BERTH_PIXEL_CHECK is not set");
        return;
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_berth"))
        .arg("--render-probe")
        .stdout(Stdio::piped())
        .spawn()
        .expect("the probe binary should start");

    let stdout = child.stdout.take().expect("piped");
    let probe = Probe(child);

    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("the probe should announce its geometry");
    let g = parse(&line).unwrap_or_else(|| panic!("could not parse {line:?}"));

    // Give the window time to open, take focus and paint. There is nothing to
    // poll for: the point of the check is that the process cannot tell us.
    std::thread::sleep(std::time::Duration::from_secs(4));

    let shot = capture(&g).expect("the window region should be capturable");
    let image = image::load_from_memory(&shot)
        .expect("the capture should decode")
        .to_rgb8();
    drop(probe);

    let (width, height) = image.dimensions();
    assert!(width > 0 && height > 0, "the capture was empty");

    // Scale the probe's points to whatever pixel density the display has.
    let scale = height as f32 / g.h;
    let band = |(top, tall): (f32, f32)| {
        let y0 = (top * scale) as u32;
        let y1 = (((top + tall) * scale) as u32).min(height);
        let mut lit = 0u64;
        let mut total = 0u64;
        for y in y0..y1 {
            for x in 0..width {
                total += 1;
                if image.get_pixel(x, y).0.iter().any(|c| *c > LIT) {
                    lit += 1;
                }
            }
        }
        (lit, total)
    };

    // A capture taken without screen recording permission comes back as a
    // single flat colour. That looks exactly like a renderer that drew nothing,
    // so it is separated here rather than reported as a rendering failure.
    let first = image.get_pixel(0, 0).0;
    if image.pixels().all(|p| p.0 == first) {
        panic!(
            "the capture is a uniform {first:?} across {width}x{height}, so nothing was \
             photographed. On CI this usually means the runner has no screen recording \
             permission, not that the renderer is broken."
        );
    }

    let (text_lit, text_total) = band(g.text);
    let (control_lit, control_total) = band(g.control);
    assert!(
        text_total > 0 && control_total > 0,
        "the bands fell outside a {width}x{height} capture"
    );

    // The control band proves the capture is of the probe's window rather than
    // of whatever else was on screen. Without it a screenshot of a busy desktop
    // would satisfy the text band easily.
    let control_ratio = control_lit as f64 / control_total as f64;
    assert!(
        control_ratio < 0.01,
        "the control band should be background, but {:.1}% of it is lit, \
         so the capture is probably not the probe's window",
        control_ratio * 100.0
    );

    // 40 pt text across a 600 pt window lights a few per cent of its band. The
    // bar sits far below that and far above zero: the failure being caught is
    // nothing at all, not a slightly thin glyph.
    let text_ratio = text_lit as f64 / text_total as f64;
    assert!(
        text_ratio > 0.005,
        "no glyph reached the framebuffer: {:.3}% of the text band is lit. \
         The window drew, and it drew nothing. See decisions.md B5.",
        text_ratio * 100.0
    );

    eprintln!(
        "pixel check ok: text band {:.2}% lit, control {:.2}% lit, capture {width}x{height}",
        text_ratio * 100.0,
        control_ratio * 100.0
    );
    let _ = (g.x, g.y, g.w);
}

#[cfg(target_os = "macos")]
fn capture(g: &Geometry) -> Option<Vec<u8>> {
    let region = format!(
        "{},{},{},{}",
        g.x as i32, g.y as i32, g.w as i32, g.h as i32
    );
    let path = std::env::temp_dir().join("berth_pixel_check.png");
    let ok = Command::new("screencapture")
        .args(["-x", "-o", "-R", &region])
        .arg(&path)
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(bytes)
}

#[cfg(target_os = "windows")]
fn capture(g: &Geometry) -> Option<Vec<u8>> {
    // CopyFromScreen over the probe's known rectangle is the same measurement
    // as PrintWindow with none of the unsafe, and needs nothing built.
    let path = std::env::temp_dir().join("berth_pixel_check.png");
    let script = format!(
        "Add-Type -AssemblyName System.Drawing; \
         $b = New-Object System.Drawing.Bitmap {w},{h}; \
         $gr = [System.Drawing.Graphics]::FromImage($b); \
         $gr.CopyFromScreen({x},{y},0,0,$b.Size); \
         $b.Save('{path}', [System.Drawing.Imaging.ImageFormat]::Png)",
        w = g.w as i32,
        h = g.h as i32,
        x = g.x as i32,
        y = g.y as i32,
        path = path.display().to_string().replace('\\', "\\\\"),
    );
    let ok = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(bytes)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn capture(_g: &Geometry) -> Option<Vec<u8>> {
    None
}
