//! Skew correction for images on their way to Tesseract (never for display).
//!
//! The angle is found with a projection profile: ink pixels of a downscaled,
//! Otsu-binarized copy are projected onto the vertical axis for each candidate
//! angle; text lines that are level give the sharpest profile (largest sum of
//! squared row counts). A coarse 1° sweep over ±15° is refined in 0.1° steps.
//! The full-resolution page is then rotated with bilinear sampling onto an
//! enlarged white canvas so no corner is cut off.

use image::{GrayImage, Luma};

use crate::ocr::{otsu, shrink};

/// Largest skew searched, in degrees.
pub const MAX_SKEW_DEGREES: f32 = 15.0;
/// Below this the page is left untouched (resampling blurs glyphs).
pub const MIN_CORRECTION_DEGREES: f32 = 0.3;
/// The winning angle must sharpen the profile at least this much over 0°,
/// otherwise the estimate is noise (blank pages, photos, tables of dots).
const MIN_GAIN: f64 = 1.02;
/// Cap on sampled ink points per estimate.
const MAX_POINTS: usize = 150_000;

/// Estimated skew in degrees: the angle the page's text lines are rotated by,
/// counter-clockwise positive in image coordinates (y down). 0 when unsure.
pub fn estimate_skew(page: &GrayImage) -> f32 {
    let small = shrink(page, 1200, 1600);
    let threshold = otsu(&small);
    let (width, height) = small.dimensions();
    let mut points: Vec<(f32, f32)> = Vec::new();
    for (x, y, pixel) in small.enumerate_pixels() {
        if pixel[0] <= threshold {
            points.push((x as f32 - width as f32 / 2.0, y as f32 - height as f32 / 2.0));
        }
    }
    let ink_ratio = points.len() as f64 / (width as f64 * height as f64).max(1.0);
    // Empty pages and near-solid images carry no line structure.
    if points.len() < 200 || ink_ratio > 0.5 {
        return 0.0;
    }
    if points.len() > MAX_POINTS {
        let step = points.len().div_ceil(MAX_POINTS);
        points = points.into_iter().step_by(step).collect();
    }
    let diagonal = ((width * width + height * height) as f32).sqrt();
    let score = |degrees: f32| profile_sharpness(&points, degrees, diagonal);

    let level = score(0.0);
    let mut best = (0.0f32, level);
    let mut coarse = -MAX_SKEW_DEGREES;
    while coarse <= MAX_SKEW_DEGREES {
        let value = score(coarse);
        if value > best.1 {
            best = (coarse, value);
        }
        coarse += 1.0;
    }
    let center = best.0;
    let mut fine = center - 1.0;
    while fine <= center + 1.0 {
        let value = score(fine);
        if value > best.1 {
            best = (fine, value);
        }
        fine += 0.1;
    }
    if level <= 0.0 || best.1 / level < MIN_GAIN {
        return 0.0;
    }
    (best.0 * 10.0).round() / 10.0
}

/// Sum of squared row counts after rotating every point by `-degrees`.
fn profile_sharpness(points: &[(f32, f32)], degrees: f32, diagonal: f32) -> f64 {
    let (sin, cos) = (-degrees).to_radians().sin_cos();
    let rows = diagonal.ceil() as usize + 2;
    let offset = diagonal / 2.0 + 1.0;
    let mut histogram = vec![0u32; rows];
    for &(x, y) in points {
        let row = (x * sin + y * cos + offset) as usize;
        if let Some(slot) = histogram.get_mut(row) {
            *slot += 1;
        }
    }
    histogram.iter().map(|&c| (c as f64) * (c as f64)).sum()
}

/// Rotates `page` by `degrees` (counter-clockwise positive, y down) around
/// its center onto a canvas large enough to hold every source pixel; the
/// uncovered area is white.
pub fn rotate(page: &GrayImage, degrees: f32) -> GrayImage {
    let (width, height) = page.dimensions();
    let (sin, cos) = degrees.to_radians().sin_cos();
    let new_width = (width as f32 * cos.abs() + height as f32 * sin.abs()).ceil() as u32;
    let new_height = (width as f32 * sin.abs() + height as f32 * cos.abs()).ceil() as u32;
    let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
    let (ncx, ncy) = (new_width as f32 / 2.0, new_height as f32 / 2.0);
    let mut out = GrayImage::from_pixel(new_width, new_height, Luma([255]));
    for y in 0..new_height {
        for x in 0..new_width {
            // Inverse mapping: destination → source.
            let dx = x as f32 + 0.5 - ncx;
            let dy = y as f32 + 0.5 - ncy;
            let sx = dx * cos + dy * sin + cx - 0.5;
            let sy = -dx * sin + dy * cos + cy - 0.5;
            if let Some(value) = bilinear(page, sx, sy) {
                out.put_pixel(x, y, Luma([value]));
            }
        }
    }
    out
}

fn bilinear(image: &GrayImage, x: f32, y: f32) -> Option<u8> {
    let (width, height) = image.dimensions();
    if x < -0.5 || y < -0.5 || x > width as f32 - 0.5 || y > height as f32 - 0.5 {
        return None;
    }
    let x0 = x.floor().clamp(0.0, (width - 1) as f32);
    let y0 = y.floor().clamp(0.0, (height - 1) as f32);
    let x1 = (x0 + 1.0).min((width - 1) as f32);
    let y1 = (y0 + 1.0).min((height - 1) as f32);
    let (fx, fy) = ((x - x0).clamp(0.0, 1.0), (y - y0).clamp(0.0, 1.0));
    let at = |px: f32, py: f32| image.get_pixel(px as u32, py as u32)[0] as f32;
    let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
    let bottom = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
    Some((top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8)
}

/// The page rotated level, or `None` when no correction is warranted.
pub fn deskew(page: &GrayImage) -> Option<(GrayImage, f32)> {
    let angle = estimate_skew(page);
    (angle.abs() >= MIN_CORRECTION_DEGREES).then(|| (rotate(page, -angle), angle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page of dashed "text lines".
    fn lined_page() -> GrayImage {
        let mut page = GrayImage::from_pixel(1000, 1300, Luma([255]));
        for line in 0..30 {
            let y = 150 + line * 32;
            for x in 120..880 {
                if (x / 7) % 6 != 0 {
                    for t in 0..6 {
                        page.put_pixel(x, y + t, Luma([20]));
                    }
                }
            }
        }
        page
    }

    #[test]
    fn a_level_page_is_left_alone() {
        assert_eq!(estimate_skew(&lined_page()), 0.0);
        assert!(deskew(&lined_page()).is_none());
        assert_eq!(estimate_skew(&GrayImage::from_pixel(800, 800, Luma([255]))), 0.0, "blank page");
    }

    #[test]
    fn skew_is_measured_and_corrected_in_both_directions() {
        for skew in [4.0f32, -7.5, 12.0] {
            let tilted = rotate(&lined_page(), skew);
            let estimate = estimate_skew(&tilted);
            assert!((estimate - skew).abs() <= 0.3, "skew {skew}: estimated {estimate}");
            let (level, angle) = deskew(&tilted).expect("correction expected");
            assert_eq!(angle, estimate);
            assert!(estimate_skew(&level).abs() <= 0.3, "residual after correcting {skew}");
        }
    }

    #[test]
    fn rotation_keeps_every_corner_on_the_canvas() {
        let page = GrayImage::from_pixel(400, 200, Luma([0]));
        let rotated = rotate(&page, 10.0);
        assert!(rotated.width() > 400 && rotated.height() > 200);
        let ink = rotated.pixels().filter(|p| p[0] < 128).count() as f32;
        assert!((ink / (400.0 * 200.0) - 1.0).abs() < 0.03, "ink area preserved: {ink}");
    }
}
