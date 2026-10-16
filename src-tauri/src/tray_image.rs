//! Menu bar image for the "Limits" display: per provider, its logo with the session and weekly
//! usage stacked next to it, like OpenUsage. Drawn as a template image (black + alpha), so macOS
//! tints it for light, dark and highlighted menu bars.

use fontdue::{Font, FontSettings};
use std::sync::OnceLock;
use tiny_skia::{FillRule, Paint, Path, PathBuilder, Pixmap, Transform};

/// Drawn at 2x, for Retina.
const SCALE: f32 = 2.0;
/// tray-icon scales every menu bar image to 18 pt tall, so draw at exactly that size.
const HEIGHT_PT: f32 = 18.0;
const ICON_PT: f32 = 13.0;
const FONT_PT: f32 = 9.5;
/// Space between the session and weekly lines.
const LINE_GAP_PT: f32 = 2.4;
/// Helvetica Neue's cap height, as a fraction of the font size; digits are cap height.
const CAP: f32 = 0.714;
/// Extra space between characters, so "100%" doesn't read as one blob.
const TRACKING_PT: f32 = 0.8;
const GAP_PT: f32 = 3.5;
const SEGMENT_GAP_PT: f32 = 7.5;

/// Provider logos, the same 100×100 artwork OpenUsage ships.
const CLAUDE_SVG: &str = include_str!("../icons/provider-claude.svg");
const CODEX_SVG: &str = include_str!("../icons/provider-codex.svg");
/// The logos fill roughly 10–90 of their 100-unit viewBox.
const LOGO_EXTENT: f32 = 80.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Claude,
    Codex,
}

pub struct Segment {
    pub glyph: Glyph,
    /// Session then weekly, as whole percentages.
    pub lines: [Option<u32>; 2],
}

fn font() -> Option<&'static Font> {
    static FONT: OnceLock<Option<Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        let bytes = std::fs::read("/System/Library/Fonts/HelveticaNeue.ttc").ok()?;
        // Index 1 is the bold face in macOS' collection; fall back to whatever loads.
        [1u32, 0].into_iter().find_map(|i| {
            Font::from_bytes(bytes.as_slice(), FontSettings { collection_index: i, scale: 40.0, ..Default::default() }).ok()
        })
    })
    .as_ref()
}

fn logo(glyph: Glyph) -> Option<&'static Path> {
    static CLAUDE: OnceLock<Option<Path>> = OnceLock::new();
    static CODEX: OnceLock<Option<Path>> = OnceLock::new();
    let (cell, svg) = match glyph {
        Glyph::Claude => (&CLAUDE, CLAUDE_SVG),
        Glyph::Codex => (&CODEX, CODEX_SVG),
    };
    cell.get_or_init(|| {
        let d = svg.split(" d=\"").nth(1)?.split('"').next()?;
        parse_path(d)
    })
    .as_ref()
}

/// Enough of SVG path syntax for the logos: M L H V C Z, absolute and relative.
fn parse_path(d: &str) -> Option<Path> {
    let mut tokens: Vec<String> = Vec::new();
    let mut num = String::new();
    let flush = |num: &mut String, tokens: &mut Vec<String>| {
        if !num.is_empty() {
            tokens.push(std::mem::take(num));
        }
    };
    for c in d.chars() {
        match c {
            c if c.is_ascii_alphabetic() && c != 'e' && c != 'E' => {
                flush(&mut num, &mut tokens);
                tokens.push(c.to_string());
            }
            '-' if !num.is_empty() && !num.ends_with(['e', 'E']) => {
                flush(&mut num, &mut tokens);
                num.push(c);
            }
            '.' if num.contains('.') => {
                flush(&mut num, &mut tokens);
                num.push(c);
            }
            ',' | ' ' | '\n' | '\t' => flush(&mut num, &mut tokens),
            c => num.push(c),
        }
    }
    flush(&mut num, &mut tokens);

    let mut pb = PathBuilder::new();
    let (mut x, mut y, mut sx, mut sy) = (0f32, 0f32, 0f32, 0f32);
    let mut cmd = 'M';
    let mut i = 0;
    let take = |i: &mut usize| -> Option<f32> {
        let v = tokens.get(*i)?.parse().ok()?;
        *i += 1;
        Some(v)
    };
    while i < tokens.len() {
        if let Some(c) = tokens[i].chars().next().filter(|c| c.is_ascii_alphabetic()) {
            cmd = c;
            i += 1;
        }
        let rel = cmd.is_ascii_lowercase();
        let (ox, oy) = if rel { (x, y) } else { (0.0, 0.0) };
        match cmd.to_ascii_uppercase() {
            'M' => {
                (x, y) = (ox + take(&mut i)?, oy + take(&mut i)?);
                (sx, sy) = (x, y);
                pb.move_to(x, y);
                // Further pairs after a moveto are linetos.
                cmd = if rel { 'l' } else { 'L' };
            }
            'L' => {
                (x, y) = (ox + take(&mut i)?, oy + take(&mut i)?);
                pb.line_to(x, y);
            }
            'H' => {
                x = ox + take(&mut i)?;
                pb.line_to(x, y);
            }
            'V' => {
                y = oy + take(&mut i)?;
                pb.line_to(x, y);
            }
            'C' => {
                let (x1, y1) = (ox + take(&mut i)?, oy + take(&mut i)?);
                let (x2, y2) = (ox + take(&mut i)?, oy + take(&mut i)?);
                (x, y) = (ox + take(&mut i)?, oy + take(&mut i)?);
                pb.cubic_to(x1, y1, x2, y2, x, y);
            }
            'Z' => {
                pb.close();
                (x, y) = (sx, sy);
            }
            _ => return None,
        }
    }
    pb.finish()
}

fn text_width(font: &Font, text: &str, px: f32) -> f32 {
    let n = text.chars().count() as f32;
    text.chars().map(|c| font.metrics(c, px).advance_width).sum::<f32>() + (n - 1.0).max(0.0) * TRACKING_PT * SCALE
}

fn draw_text(pix: &mut Pixmap, font: &Font, text: &str, x: f32, baseline: f32, px: f32) {
    let width = pix.width() as i32;
    let height = pix.height() as i32;
    let mut pen = x;
    for c in text.chars() {
        let (m, bitmap) = font.rasterize(c, px);
        let gx = (pen + m.xmin as f32).round() as i32;
        let gy = (baseline - m.height as f32 - m.ymin as f32).round() as i32;
        for row in 0..m.height {
            for col in 0..m.width {
                let (px_x, px_y) = (gx + col as i32, gy + row as i32);
                if px_x < 0 || px_y < 0 || px_x >= width || px_y >= height {
                    continue;
                }
                let a = bitmap[row * m.width + col];
                let idx = (px_y * width + px_x) as usize * 4 + 3;
                let data = pix.data_mut();
                data[idx] = data[idx].max(a);
            }
        }
        pen += m.advance_width + TRACKING_PT * SCALE;
    }
}

fn draw_logo(pix: &mut Pixmap, glyph: Glyph, cx: f32, cy: f32, size: f32) {
    let Some(path) = logo(glyph) else { return };
    let mut paint = Paint::default();
    paint.set_color_rgba8(0, 0, 0, 255);
    paint.anti_alias = true;
    let s = size / LOGO_EXTENT;
    let t = Transform::from_translate(cx, cy).pre_scale(s, s).pre_translate(-50.0, -50.0);
    pix.fill_path(path, &paint, FillRule::Winding, t, None);
}

/// RGBA pixels, width, height of the rendered image; `None` when nothing can be drawn.
pub fn render(segments: &[Segment]) -> Option<(Vec<u8>, u32, u32)> {
    let font = font()?;
    if segments.is_empty() {
        return None;
    }
    let px = FONT_PT * SCALE;
    let text_of = |v: Option<u32>| v.map(|p| format!("{p}%")).unwrap_or_else(|| "–".into());

    let widths: Vec<f32> = segments
        .iter()
        .map(|s| {
            let t = s.lines.iter().map(|l| text_width(font, &text_of(*l), px)).fold(0.0, f32::max);
            (ICON_PT + GAP_PT) * SCALE + t
        })
        .collect();
    let total = widths.iter().sum::<f32>() + SEGMENT_GAP_PT * SCALE * (segments.len() - 1) as f32;
    let (w, h) = (total.ceil() as u32 + 2, (HEIGHT_PT * SCALE) as u32);
    let mut pix = Pixmap::new(w, h)?;

    // Centre the two lines (cap height each, plus the gap) vertically.
    let cap = FONT_PT * CAP;
    let margin = (HEIGHT_PT - 2.0 * cap - LINE_GAP_PT) / 2.0;
    let (base1, base2) = ((margin + cap) * SCALE, (HEIGHT_PT - margin) * SCALE);

    let mut x = 1.0;
    for (seg, seg_w) in segments.iter().zip(&widths) {
        draw_logo(&mut pix, seg.glyph, x + ICON_PT * SCALE / 2.0, h as f32 / 2.0, ICON_PT * SCALE);
        // Right-align both lines to the segment's edge, so the % signs line up.
        let right = x + seg_w;
        for (line, base) in seg.lines.iter().zip([base1, base2]) {
            let text = text_of(*line);
            draw_text(&mut pix, font, &text, right - text_width(font, &text, px), base, px);
        }
        x += seg_w + SEGMENT_GAP_PT * SCALE;
    }
    // tiny-skia stores premultiplied RGBA; the image is pure black, so only alpha varies.
    Some((pix.take(), w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logos_parse() {
        assert!(logo(Glyph::Claude).is_some());
        assert!(logo(Glyph::Codex).is_some());
    }

    /// `TRAY_DUMP=<dir> cargo test tray_image` writes a sample image to look at.
    #[test]
    fn renders_and_dumps() {
        let segs = [
            Segment { glyph: Glyph::Claude, lines: [Some(99), Some(90)] },
            Segment { glyph: Glyph::Codex, lines: [Some(100), Some(99)] },
        ];
        let Some((data, w, h)) = render(&segs) else { return };
        let mut pix = Pixmap::new(w, h).unwrap();
        pix.data_mut().copy_from_slice(&data);
        if let Ok(dir) = std::env::var("TRAY_DUMP") {
            pix.save_png(format!("{dir}/tray-limits.png")).unwrap();
        }
    }
}
