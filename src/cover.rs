use anyhow::Result;
use image::{imageops::FilterType, DynamicImage};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

pub const ART_CHARS: usize = 20;

// Bayer 8×8 ordered dither matrix (values / 64.0)
#[rustfmt::skip]
const BAYER: [[f32; 8]; 8] = [
    [ 0./64., 32./64.,  8./64., 40./64.,  2./64., 34./64., 10./64., 42./64.],
    [48./64., 16./64., 56./64., 24./64., 50./64., 18./64., 58./64., 26./64.],
    [12./64., 44./64.,  4./64., 36./64., 14./64., 46./64.,  6./64., 38./64.],
    [60./64., 28./64., 52./64., 20./64., 62./64., 30./64., 54./64., 22./64.],
    [ 3./64., 35./64., 11./64., 43./64.,  1./64., 33./64.,  9./64., 41./64.],
    [51./64., 19./64., 59./64., 27./64., 49./64., 17./64., 57./64., 25./64.],
    [15./64., 47./64.,  7./64., 39./64., 13./64., 45./64.,  5./64., 37./64.],
    [63./64., 31./64., 55./64., 23./64., 61./64., 29./64., 53./64., 21./64.],
];

// Braille dot layout: (pixel_row_in_cell, pixel_col_in_cell, unicode_bit)
const BRAILLE_DOTS: [(usize, usize, u32); 8] = [
    (0, 0, 0x01),
    (1, 0, 0x02),
    (2, 0, 0x04),
    (3, 0, 0x40),
    (0, 1, 0x08),
    (1, 1, 0x10),
    (2, 1, 0x20),
    (3, 1, 0x80),
];

/// Rendered cover in monochrome and its natural image colours.
pub struct CoverArt {
    /// White Braille art (normal/no-colour mode).
    pub mono: Vec<Line<'static>>,
    /// Braille art with each character tinted by its cell's average colour.
    pub color: Vec<Line<'static>>,
}

impl Default for CoverArt {
    fn default() -> Self {
        Self {
            mono: Vec::new(),
            color: Vec::new(),
        }
    }
}

/// Render a dim wave placeholder with the same dimensions as a real cover.
///
/// Looks like a side-on breaking wave (barrel view), scaled to `art_chars`:
///
///   ░░░╭───────────────╮
///   ░╭─╯░░░░░░░░░░░░░░╰╮
///   ╭╯░░░░░░░░░░░░░░░░░│
///   │░░░░░░░░░░░░░░░░░░│
///   │░░░░░░░░░░░░░░░░░░│
///   ╰╮░░░░░░░░░░░░░░░░░│
///   ░╰──╮░░░░░░░░░░░░░░│
///   ░░░╰──╮░░░░░░░░░░░│
///   ░░░░░░╰────────────╯
///   ░░░░░░░░░░░░░░░░░░░░
pub fn render_placeholder(art_chars: usize) -> CoverArt {
    let char_h = art_chars / 2;
    let dim = Style::new().fg(Color::DarkGray);
    let blank = "░".repeat(art_chars);

    // Each row is generated to be exactly art_chars wide.
    // n = interior fill width for that row.
    let wave: Vec<String> = vec![
        format!("░░░╭{}╮", "─".repeat(art_chars.saturating_sub(5))), // crest top
        format!("░╭─╯{}╰╮", "░".repeat(art_chars.saturating_sub(6))), // barrel ceiling
        format!("╭╯{}│", "░".repeat(art_chars.saturating_sub(3))),   // barrel opens
        format!("│{}│", "░".repeat(art_chars.saturating_sub(2))),    // barrel body
        format!("│{}│", "░".repeat(art_chars.saturating_sub(2))),    // barrel body
        format!("╰╮{}│", "░".repeat(art_chars.saturating_sub(3))),   // curl starts
        format!("░╰──╮{}│", "░".repeat(art_chars.saturating_sub(6))), // curl mid
        format!("░░░╰──╮{}│", "░".repeat(art_chars.saturating_sub(8))), // curl lower
        format!("░░░░░░╰{}╯", "─".repeat(art_chars.saturating_sub(8))), // base closes
    ];

    let lines: Vec<Line<'static>> = (0..char_h)
        .map(|i| {
            let s = wave.get(i).cloned().unwrap_or_else(|| blank.clone());
            Line::from(Span::styled(s, dim))
        })
        .collect();

    CoverArt {
        mono: lines.clone(),
        color: lines,
    }
}

/// Render album cover art from raw JPEG bytes (embedded or fetched).
pub fn render_cover(cover_bytes: &[u8], art_chars: usize) -> CoverArt {
    match render_inner(cover_bytes, art_chars) {
        Ok(art) => art,
        Err(_) => render_placeholder(art_chars),
    }
}

fn render_inner(bytes: &[u8], art_chars: usize) -> Result<CoverArt> {
    let img = image::load_from_memory(bytes)?;

    let px_w = art_chars * 2;
    let px_h = art_chars * 2;

    // Resize for colour extraction and dithering
    let img_rgb = img
        .resize_exact(px_w as u32, px_h as u32, FilterType::Lanczos3)
        .to_rgb8();

    // ── Grayscale + contrast enhancement ─────────────────────────────────────
    let gray_dyn = DynamicImage::ImageRgb8(img_rgb.clone()).to_luma8();
    let (gw, gh) = gray_dyn.dimensions();
    let avg: f32 = gray_dyn.pixels().map(|p| p[0] as f32).sum::<f32>() / (gw * gh) as f32;
    let enhanced: Vec<f32> = gray_dyn
        .pixels()
        .map(|p| (avg + 1.5 * (p[0] as f32 - avg)).clamp(0.0, 255.0) / 255.0)
        .collect();

    // ── Bayer dithering ───────────────────────────────────────────────────────
    let mut pixels = vec![false; (px_w * px_h) as usize];
    for py in 0..gh as usize {
        for px in 0..gw as usize {
            let v = enhanced[py * gw as usize + px];
            let threshold = BAYER[py % 8][px % 8];
            pixels[py * gw as usize + px] = v > threshold;
        }
    }

    // ── Braille rendering ─────────────────────────────────────────────────────
    let char_h = px_h / 4;
    let mut mono_lines: Vec<Line<'static>> = Vec::new();
    let mut color_lines: Vec<Line<'static>> = Vec::new();

    for char_row in 0..char_h {
        let mut mono_spans: Vec<Span<'static>> = Vec::new();
        let mut color_spans: Vec<Span<'static>> = Vec::new();

        for char_col in 0..art_chars {
            // Build Braille character
            let mut val: u32 = 0;
            for &(dr, dc, bit) in &BRAILLE_DOTS {
                let pr = char_row * 4 + dr;
                let pc = char_col * 2 + dc;
                if pr < px_h && pc < px_w && pixels[pr * px_w + pc] {
                    val |= bit;
                }
            }
            let ch = char::from_u32(0x2800 + val).unwrap_or(' ');
            let ch_str: String = ch.to_string();

            mono_spans.push(Span::styled(ch_str.clone(), Style::new().fg(Color::White)));

            // Average RGB of the 4×2 pixel cell for colour tinting
            let pr0 = char_row * 4;
            let pc0 = char_col * 2;
            let mut r_sum = 0u32;
            let mut g_sum = 0u32;
            let mut b_sum = 0u32;
            let mut count = 0u32;
            for dr in 0..4 {
                for dc in 0..2 {
                    let pr = (pr0 + dr) as u32;
                    let pc = (pc0 + dc) as u32;
                    if pr < gh && pc < gw {
                        let pixel = img_rgb.get_pixel(pc, pr);
                        r_sum += pixel[0] as u32;
                        g_sum += pixel[1] as u32;
                        b_sum += pixel[2] as u32;
                        count += 1;
                    }
                }
            }
            let (r, g, b) = if count > 0 {
                (
                    (r_sum / count) as u8,
                    (g_sum / count) as u8,
                    (b_sum / count) as u8,
                )
            } else {
                (255, 255, 255)
            };
            color_spans.push(Span::styled(ch_str, Style::new().fg(Color::Rgb(r, g, b))));
        }

        mono_lines.push(Line::from(mono_spans));
        color_lines.push(Line::from(color_spans));
    }

    Ok(CoverArt {
        mono: mono_lines,
        color: color_lines,
    })
}
