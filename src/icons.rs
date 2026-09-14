//! Tray icons drawn as raw ARGB32, with no drawing library.
//!
//! StatusNotifierItem accepts an IconPixmap (`a(iiay)`) instead of a themed icon
//! name. Generating the pixmap here means the battery level is exact, updates the
//! instant we read it, and never touches the icon theme cache -- and it needs no
//! drawing library, so the tray process can stay free of GTK.

/// Signed-distance rounded rectangle; negative inside, positive outside.
fn sd_round_rect(px: f64, py: f64, cx: f64, cy: f64, half_w: f64, half_h: f64, radius: f64) -> f64 {
    let dx = (px - cx).abs() - (half_w - radius);
    let dy = (py - cy).abs() - (half_h - radius);
    let outside = dx.max(0.0).hypot(dy.max(0.0));
    outside + dx.max(dy).min(0.0) - radius
}

type Rgb = [f64; 3];

fn blend(dst: Rgb, src: Rgb, alpha: f64) -> Rgb {
    [
        dst[0] + (src[0] - dst[0]) * alpha,
        dst[1] + (src[1] - dst[1]) * alpha,
        dst[2] + (src[2] - dst[2]) * alpha,
    ]
}

pub fn level_color(percent: u8, charging: bool) -> Rgb {
    if charging {
        return [0x4F as f64, 0xC3 as f64, 0xF7 as f64]; // blue while charging
    }
    match percent {
        0..=15 => [0xE1 as f64, 0x06 as f64, 0x00 as f64], // red
        16..=30 => [0xE8 as f64, 0xA3 as f64, 0x3D as f64], // amber
        _ => [0x36 as f64, 0xAD as f64, 0x6A as f64],      // Ninjutso green
    }
}

const BOLT: [(f64, f64); 7] = [
    (12.6, 4.0),
    (7.6, 13.0),
    (11.0, 13.0),
    (9.4, 20.0),
    (15.4, 10.6),
    (11.8, 10.6),
    (13.6, 4.0),
];

fn in_polygon(x: f64, y: f64, poly: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// A mouse silhouette filled from the bottom to `percent`.
///
/// Returns `(width, height, argb32)` in network byte order, which is exactly
/// what the SNI IconPixmap property expects.
pub fn battery_pixmap(percent: u8, charging: bool, size: i32) -> (i32, i32, Vec<u8>) {
    battery_pixmap_sampled(percent, charging, size, 3)
}

fn battery_pixmap_sampled(
    percent: u8,
    charging: bool,
    size: i32,
    supersample: i32,
) -> (i32, i32, Vec<u8>) {
    let size_f = size as f64;
    let scale = size_f / 24.0;
    let (cx, cy) = (size_f / 2.0, size_f / 2.0);
    let (half_w, half_h) = (7.0 * scale, 10.5 * scale);
    let radius = 7.0 * scale;
    let track: Rgb = [0x3A as f64, 0x41 as f64, 0x4A as f64];
    let fill = level_color(percent, charging);
    let edge: Rgb = [0x10 as f64, 0x13 as f64, 0x17 as f64];

    // Fill line: bottom of the body up to `percent` of its height.
    let (top, bottom) = (cy - half_h, cy + half_h);
    let water = bottom - (bottom - top) * f64::from(percent.min(100)) / 100.0;

    let mut out = Vec::with_capacity((size * size * 4) as usize);
    let step = 1.0 / f64::from(supersample);
    let offset = step / 2.0;
    let samples = f64::from(supersample * supersample);

    for y in 0..size {
        for x in 0..size {
            let mut acc_rgb: Rgb = [0.0; 3];
            let mut acc_a = 0.0;
            for sy in 0..supersample {
                for sx in 0..supersample {
                    let px = f64::from(x) + offset + f64::from(sx) * step;
                    let py = f64::from(y) + offset + f64::from(sy) * step;
                    let dist = sd_round_rect(px, py, cx, cy, half_w, half_h, radius);
                    if dist > 1.0 {
                        continue;
                    }
                    let coverage = (1.0 - dist).clamp(0.0, 1.0);
                    let mut colour = if py >= water { fill } else { track };
                    // Darken the outer rim so the silhouette reads at 22px.
                    if dist > -1.2 {
                        colour = blend(colour, edge, 0.45);
                    }
                    // Scroll-wheel notch, and the bolt when charging.
                    if charging && in_polygon(px / scale, py / scale, &BOLT) {
                        colour = [255.0, 255.0, 255.0];
                    } else if !charging
                        && (px - cx).abs() < 1.1 * scale
                        && py > 3.2 * scale
                        && py < 9.0 * scale
                    {
                        colour = blend(colour, edge, 0.55);
                    }
                    for (slot, channel) in acc_rgb.iter_mut().zip(colour) {
                        *slot += channel * coverage;
                    }
                    acc_a += coverage;
                }
            }
            let alpha = acc_a / samples;
            if alpha <= 0.002 {
                out.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            out.push((alpha * 255.0).round() as u8);
            for channel in acc_rgb {
                out.push((channel / acc_a).round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    (size, size, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixmap_is_argb32_of_the_requested_size() {
        let (w, h, data) = battery_pixmap(50, false, 22);
        assert_eq!((w, h), (22, 22));
        assert_eq!(data.len(), 22 * 22 * 4);
    }

    #[test]
    fn corners_are_transparent_and_the_centre_is_not() {
        let size = 24;
        let (_, _, data) = battery_pixmap(100, false, size);
        let at = |x: i32, y: i32| data[((y * size + x) * 4) as usize];
        assert_eq!(at(0, 0), 0, "top-left corner should be outside the silhouette");
        assert!(at(12, 12) > 200, "centre should be opaque");
    }

    #[test]
    fn level_thresholds_match_the_documented_bands() {
        assert_eq!(level_color(10, false), [0xE1 as f64, 0x06 as f64, 0x00 as f64]);
        assert_eq!(level_color(25, false), [0xE8 as f64, 0xA3 as f64, 0x3D as f64]);
        assert_eq!(level_color(80, false), [0x36 as f64, 0xAD as f64, 0x6A as f64]);
        // Charging wins over the level band.
        assert_eq!(level_color(5, true), [0x4F as f64, 0xC3 as f64, 0xF7 as f64]);
    }
}
