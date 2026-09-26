//! Calibration corpus for the SSIM decision policy.
//!
//! Scenes are rasterized with supersampling, so they have real anti-aliasing and can be shifted
//! by sub-pixel amounts to emulate rendering noise across machines, GPUs and font engines.
//! Every benign variant must pass and every regression must fail under the default policy.
//! Run `cargo test -p gleon-engine --test ssim_corpus -- --nocapture` to print the metrics table.
//!
//! Existing crates were evaluated on this corpus first (2026-09, none separated benign noise from
//! regressions with any threshold): `image-compare` hybrid SSIM (mean: regressions score up to
//! 0.9996, above benign 0.955; map minimum: a 0.3px shift scores like a missing icon),
//! `dify`/pixelmatch with anti-aliasing detection (0.3px shift: 1032 px vs a 3x3 badge: 4 px),
//! `butteraugli` (bolder stems 10.1 vs removed divider 5.9). `dssim` is AGPL-3.0 and unusable.
#![cfg(not(miri))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]

use gleon_engine::{
    ComparisonResult, compare_images,
    config::{DiffConfig, Mode},
    ssim::{SsimPolicy, analyze},
};
use image::{Rgba, RgbaImage};

const W: u32 = 240;
const H: u32 = 160;

#[derive(Clone, Copy)]
enum Shape {
    Rect { x: f32, y: f32, w: f32, h: f32 },
    Circle { cx: f32, cy: f32, r: f32 },
}

impl Shape {
    fn contains(self, px: f32, py: f32) -> bool {
        match self {
            Self::Rect { x, y, w, h } => px >= x && px < x + w && py >= y && py < y + h,
            Self::Circle { cx, cy, r } => (px - cx).powi(2) + (py - cy).powi(2) <= r * r,
        }
    }

    fn bounds(self) -> (f32, f32, f32, f32) {
        match self {
            Self::Rect { x, y, w, h } => (x, y, x + w, y + h),
            Self::Circle { cx, cy, r } => (cx - r, cy - r, cx + r, cy + r),
        }
    }
}

/// Supersampled canvas with straight-alpha source-over blending.
struct Canvas {
    ss: u32,
    rgba: Vec<[f32; 4]>,
}

impl Canvas {
    fn new(ss: u32, background: [f32; 4]) -> Self {
        Self {
            ss,
            rgba: vec![background; (W * ss * H * ss) as usize],
        }
    }

    fn fill(&mut self, shape: Shape, color: [u8; 3], alpha: f32) {
        let ss = self.ss as f32;
        let (x0, y0, x1, y1) = shape.bounds();
        let sx0 = ((x0 * ss).floor().max(0.0)) as u32;
        let sy0 = ((y0 * ss).floor().max(0.0)) as u32;
        let sx1 = ((x1 * ss).ceil() as u32).min(W * self.ss);
        let sy1 = ((y1 * ss).ceil() as u32).min(H * self.ss);
        let src = [
            f32::from(color[0]),
            f32::from(color[1]),
            f32::from(color[2]),
        ];
        for sy in sy0..sy1 {
            for sx in sx0..sx1 {
                if !shape.contains((sx as f32 + 0.5) / ss, (sy as f32 + 0.5) / ss) {
                    continue;
                }
                let dst = &mut self.rgba[(sy * W * self.ss + sx) as usize];
                let out_a = alpha + dst[3] * (1.0 - alpha);
                for c in 0..3 {
                    dst[c] = if out_a > 0.0 {
                        (src[c] * alpha + dst[c] * dst[3] * (1.0 - alpha)) / out_a
                    } else {
                        0.0
                    };
                }
                dst[3] = out_a;
            }
        }
    }

    fn resolve(&self) -> RgbaImage {
        let ss = self.ss;
        RgbaImage::from_fn(W, H, |x, y| {
            // Average premultiplied samples, then un-premultiply.
            let mut acc = [0.0f32; 4];
            for dy in 0..ss {
                for dx in 0..ss {
                    let p = self.rgba[((y * ss + dy) * W * ss + x * ss + dx) as usize];
                    for c in 0..3 {
                        acc[c] += p[c] * p[3];
                    }
                    acc[3] += p[3];
                }
            }
            let n = (ss * ss) as f32;
            let a = acc[3] / n;
            let ch = |c: usize| {
                if a > 0.0 {
                    (acc[c] / n / a).round().clamp(0.0, 255.0) as u8
                } else {
                    0
                }
            };
            Rgba([ch(0), ch(1), ch(2), (a * 255.0).round() as u8])
        })
    }
}

/// Parameters of the synthetic UI scene.
#[derive(Clone, Copy)]
struct Scene {
    ss: u32,
    offset: (f32, f32),
    text_offset: f32,
    stem_extra: f32,
    drift: i16,
    card_dx: f32,
    card_color: [u8; 3],
    panel_color: [u8; 3],
    border_color: [u8; 3],
    avatar_alpha: f32,
    icon: bool,
    badge: bool,
    divider: bool,
    missing_glyph: Option<usize>,
}

impl Default for Scene {
    fn default() -> Self {
        Self {
            ss: 4,
            offset: (0.0, 0.0),
            text_offset: 0.0,
            stem_extra: 0.0,
            drift: 0,
            card_dx: 0.0,
            card_color: [0x21, 0x96, 0xF3],
            panel_color: [0xFF, 0xFF, 0xFF],
            border_color: [0xBD, 0xBD, 0xBD],
            avatar_alpha: 1.0,
            icon: true,
            badge: false,
            divider: true,
            missing_glyph: None,
        }
    }
}

/// Glyph advance widths of a fake text line ("Hello, gleon user").
const GLYPHS: [f32; 16] = [
    6.0, 5.0, 3.0, 3.0, 5.5, 2.5, 5.0, 3.0, 5.0, 5.0, 5.5, 5.0, 5.0, 4.5, 5.0, 4.0,
];

fn render(scene: Scene) -> RgbaImage {
    let d = |c: [u8; 3]| c.map(|v| (i16::from(v) + scene.drift).clamp(0, 255) as u8);
    let (ox, oy) = scene.offset;
    let mut canvas = Canvas::new(scene.ss, [245.0, 245.0, 245.0, 1.0]);

    // Panel with a 1px border.
    canvas.fill(
        Shape::Rect {
            x: 10.0 + ox,
            y: 10.0 + oy,
            w: 220.0,
            h: 140.0,
        },
        d(scene.border_color),
        1.0,
    );
    canvas.fill(
        Shape::Rect {
            x: 11.0 + ox,
            y: 11.0 + oy,
            w: 218.0,
            h: 138.0,
        },
        d(scene.panel_color),
        1.0,
    );
    // Card.
    canvas.fill(
        Shape::Rect {
            x: 24.0 + ox + scene.card_dx,
            y: 22.0 + oy,
            w: 120.0,
            h: 36.0,
        },
        d(scene.card_color),
        1.0,
    );
    // Avatar.
    canvas.fill(
        Shape::Circle {
            cx: 190.0 + ox,
            cy: 40.0 + oy,
            r: 16.0,
        },
        d([0x9C, 0x27, 0xB0]),
        scene.avatar_alpha,
    );
    if scene.badge {
        canvas.fill(
            Shape::Circle {
                cx: 203.0 + ox,
                cy: 27.0 + oy,
                r: 1.6,
            },
            [0xF4, 0x43, 0x36],
            1.0,
        );
    }
    // Icon: ring with a cross, 16x16.
    if scene.icon {
        canvas.fill(
            Shape::Circle {
                cx: 40.0 + ox,
                cy: 90.0 + oy,
                r: 8.0,
            },
            d([0x42, 0x42, 0x42]),
            1.0,
        );
        canvas.fill(
            Shape::Circle {
                cx: 40.0 + ox,
                cy: 90.0 + oy,
                r: 5.5,
            },
            d(scene.panel_color),
            1.0,
        );
        canvas.fill(
            Shape::Rect {
                x: 39.0 + ox,
                y: 84.0 + oy,
                w: 2.0,
                h: 12.0,
            },
            d([0x42, 0x42, 0x42]),
            1.0,
        );
    }
    // Text line: each glyph is a stem plus a top bar.
    let mut x = 60.0 + ox + scene.text_offset;
    for (i, advance) in GLYPHS.iter().enumerate() {
        if scene.missing_glyph != Some(i) {
            let stem = 1.4 + scene.stem_extra;
            canvas.fill(
                Shape::Rect {
                    x,
                    y: 84.0 + oy,
                    w: stem,
                    h: 11.0,
                },
                d([0x21, 0x21, 0x21]),
                1.0,
            );
            canvas.fill(
                Shape::Rect {
                    x,
                    y: 84.0 + oy,
                    w: advance - 1.5,
                    h: stem,
                },
                d([0x21, 0x21, 0x21]),
                1.0,
            );
        }
        x += advance;
    }
    // Divider.
    if scene.divider {
        canvas.fill(
            Shape::Rect {
                x: 24.0 + ox,
                y: 120.0 + oy,
                w: 192.0,
                h: 1.0,
            },
            d([0xE0, 0xE0, 0xE0]),
            1.0,
        );
    }
    canvas.resolve()
}

/// 3x3 box blur of the 24x24 icon area, emulating a blurry asset.
fn blur_icon(mut img: RgbaImage) -> RgbaImage {
    let src = img.clone();
    for y in 78..102 {
        for x in 28..52 {
            let mut acc = [0u32; 4];
            for dy in 0..3 {
                for dx in 0..3 {
                    let p = src.get_pixel(x + dx - 1, y + dy - 1).0;
                    for c in 0..4 {
                        acc[c] += u32::from(p[c]);
                    }
                }
            }
            img.put_pixel(x, y, Rgba(acc.map(|v| ((v + 4) / 9) as u8)));
        }
    }
    img
}

/// Rendering noise the policy must tolerate.
fn benign() -> Vec<(&'static str, Scene)> {
    let base = Scene::default();
    vec![
        (
            "whole scene shifted by (0.3, 0.2)px",
            Scene {
                offset: (0.3, 0.2),
                ..base
            },
        ),
        (
            "shift (0.25, 0.25)px + drift -2",
            Scene {
                offset: (0.25, 0.25),
                drift: -2,
                ..base
            },
        ),
        ("color drift +2", Scene { drift: 2, ..base }),
        ("anti-aliasing quality 4x vs 8x", Scene { ss: 8, ..base }),
        (
            "glyph stems 0.25px bolder",
            Scene {
                stem_extra: 0.25,
                ..base
            },
        ),
    ]
}

/// Changes the policy must catch.
fn regressions() -> Vec<(&'static str, Scene)> {
    let base = Scene::default();
    vec![
        (
            "icon missing",
            Scene {
                icon: false,
                ..base
            },
        ),
        (
            "glyph missing",
            Scene {
                missing_glyph: Some(6),
                ..base
            },
        ),
        (
            "card shifted by 1px",
            Scene {
                card_dx: 1.0,
                ..base
            },
        ),
        (
            "card hue shift, similar luma",
            Scene {
                card_color: [0x4C, 0xAF, 0x50],
                ..base
            },
        ),
        (
            "panel low-contrast change",
            Scene {
                panel_color: [0xF2, 0xF2, 0xF2],
                ..base
            },
        ),
        (
            "avatar alpha 1.0 -> 0.8",
            Scene {
                avatar_alpha: 0.8,
                ..base
            },
        ),
        (
            "3x3 badge dot added",
            Scene {
                badge: true,
                ..base
            },
        ),
        (
            "1px divider removed",
            Scene {
                divider: false,
                ..base
            },
        ),
    ]
}

/// Documented limits (printed, not asserted either way): glyphs moved by half a pixel are
/// indistinguishable from changed thin content and fail; a low-contrast color change of a
/// one-pixel line passes.
fn known_limits() -> Vec<(&'static str, Scene)> {
    let base = Scene::default();
    vec![
        (
            "LIMIT text shifted by 0.5px",
            Scene {
                text_offset: 0.5,
                ..base
            },
        ),
        (
            "LIMIT 1px border color darker",
            Scene {
                border_color: [0x9E, 0x9E, 0x9E],
                ..base
            },
        ),
    ]
}

fn default_policy() -> SsimPolicy {
    let config = DiffConfig::default();
    SsimPolicy {
        min_similarity: config.min_similarity,
        color_tolerance: config.color_tolerance,
    }
}

#[test]
fn test_corpus_separates_benign_noise_from_regressions() {
    let golden = render(Scene::default());
    let policy = default_policy();
    let mut failures = Vec::new();
    let mut check = |name: &str, candidate: &RgbaImage, expect_pass: bool| {
        let a = analyze(&golden, candidate, &policy);
        eprintln!(
            "{:<40} {:>9.4} {:>9.5} {:>9.2} {:>8}  {}",
            name,
            a.min_ssim,
            a.mean_ssim,
            a.max_excess,
            a.failing_pixels,
            if a.passed() { "pass" } else { "FAIL" }
        );
        if a.passed() != expect_pass {
            failures.push(format!(
                "{name}: expected {}, min_ssim={:.4}, excess={:.2}",
                if expect_pass { "pass" } else { "fail" },
                a.min_ssim,
                a.max_excess
            ));
        }
    };
    eprintln!(
        "{:<40} {:>9} {:>9} {:>9} {:>8}  verdict",
        "case", "min_ssim", "mean", "excess", "fail_px"
    );
    for (expect_pass, cases) in [(true, benign()), (false, regressions())] {
        for (name, scene) in cases {
            check(name, &render(scene), expect_pass);
        }
    }
    check(
        "icon blurred (3x3 box)",
        &blur_icon(render(Scene::default())),
        false,
    );
    drop(check);
    for (name, scene) in known_limits() {
        let a = analyze(&golden, &render(scene), &policy);
        eprintln!(
            "{:<40} {:>9.4} {:>9.5} {:>9.2} {:>8}  {}",
            name,
            a.min_ssim,
            a.mean_ssim,
            a.max_excess,
            a.failing_pixels,
            if a.passed() { "pass" } else { "FAIL" }
        );
    }
    assert!(
        failures.is_empty(),
        "policy misclassified:\n{}",
        failures.join("\n")
    );
}

#[test]
fn test_compare_images_uses_the_same_policy() {
    let golden = render(Scene::default());
    let config = DiffConfig::default();
    assert_eq!(
        compare_images(
            &golden,
            &render(Scene {
                offset: (0.3, 0.2),
                ..Scene::default()
            }),
            Mode::Ssim,
            &config
        ),
        ComparisonResult::Match
    );
    assert!(matches!(
        compare_images(
            &golden,
            &render(Scene {
                icon: false,
                ..Scene::default()
            }),
            Mode::Ssim,
            &config
        ),
        ComparisonResult::Mismatch { .. }
    ));
}

/// Worst case for a full `MaterialApp` golden (2400x1800) where every pixel differs.
/// Run with `cargo test --release -p gleon-engine --test ssim_corpus -- --ignored --nocapture`.
#[test]
#[ignore = "timing report; run manually in release mode"]
fn perf_full_screen_worst_case() {
    let (w, h) = (2400u32, 1800u32);
    let base = RgbaImage::from_fn(w, h, |x, y| {
        Rgba([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 239) as u8, 255])
    });
    let cand = RgbaImage::from_fn(w, h, |x, y| {
        Rgba([
            (x % 251) as u8 ^ 1,
            (y % 241) as u8,
            ((x ^ y) % 239) as u8,
            255,
        ])
    });
    let policy = default_policy();
    let start = std::time::Instant::now();
    let a = analyze(&base, &cand, &policy);
    eprintln!(
        "full-screen worst case: {:?}, passed={}",
        start.elapsed(),
        a.passed()
    );
    let mut small = base.clone();
    small.put_pixel(1200, 900, Rgba([0, 0, 0, 255]));
    let start = std::time::Instant::now();
    let a = analyze(&base, &small, &policy);
    eprintln!(
        "full-screen single-pixel change: {:?}, passed={}",
        start.elapsed(),
        a.passed()
    );
}
