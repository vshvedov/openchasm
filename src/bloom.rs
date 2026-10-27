//! Optional enhancement bloom, independent of the original renderer.
//!
//! Apply to scene-linear world and weapon pixels before tone mapping and HUD
//! composition. Original appearance uses strength zero; 0.35 is a restrained
//! optional enhancement. Exposure selects highlights without baking exposure
//! into the returned scene values.

use crate::color_pipeline::LinearRgb;

const MAX_SIDE: usize = 16_384;
const MAX_PIXELS: usize = 16_777_216;
const MAX_CHANNEL: f32 = 1.0e12;
const WEIGHTS: [f32; 5] = [0.0625, 0.25, 0.375, 0.25, 0.0625];

/// Reusable half-resolution bright-pass and Gaussian blur storage.
pub struct Bloom {
    width: usize,
    height: usize,
    half_width: usize,
    half_height: usize,
    bright: Vec<LinearRgb>,
    scratch: Vec<LinearRgb>,
}

impl Bloom {
    /// Reject zero dimensions, sides above 16384, and more than 16M pixels.
    pub fn new(width: usize, height: usize) -> Result<Self, &'static str> {
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return Err("bloom dimensions outside 1..=16384");
        }
        let pixels = width.checked_mul(height).ok_or("bloom size overflow")?;
        if pixels > MAX_PIXELS {
            return Err("bloom image exceeds 16M pixels");
        }
        let half_width = width.div_ceil(2);
        let half_height = height.div_ceil(2);
        let count = half_width
            .checked_mul(half_height)
            .ok_or("bloom buffer size overflow")?;
        let buffer = || {
            let mut data = Vec::new();
            data.try_reserve_exact(count)
                .map_err(|_| "unable to allocate bloom buffers")?;
            data.resize(count, LinearRgb::default());
            Ok::<_, &'static str>(data)
        };
        Ok(Self {
            width,
            height,
            half_width,
            half_height,
            bright: buffer()?,
            scratch: buffer()?,
        })
    }

    /// Add blurred highlights in linear HDR without clipping at display white.
    ///
    /// Strength must be finite in 0..=16, exposure in 0..=64, and input
    /// channels in 0..=1e12. Validation completes before changing any pixel.
    /// Zero strength or zero exposure leaves the frame exactly unchanged.
    pub fn apply(
        &mut self,
        frame: &mut [LinearRgb],
        strength: f32,
        exposure: f32,
    ) -> Result<(), &'static str> {
        if frame.len() != self.width * self.height {
            return Err("bloom frame length differs from dimensions");
        }
        if !strength.is_finite() || !(0.0..=16.0).contains(&strength) {
            return Err("bloom strength outside 0..=16");
        }
        if !exposure.is_finite() || !(0.0..=64.0).contains(&exposure) {
            return Err("bloom exposure outside 0..=64");
        }
        if frame.iter().any(|pixel| {
            pixel
                .0
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=MAX_CHANNEL).contains(value))
        }) {
            return Err("bloom requires finite nonnegative bounded HDR channels");
        }
        if strength == 0.0 || exposure == 0.0 {
            return Ok(());
        }

        for hy in 0..self.half_height {
            for hx in 0..self.half_width {
                let x0 = hx * 2;
                let y0 = hy * 2;
                let x1 = (x0 + 2).min(self.width);
                let y1 = (y0 + 2).min(self.height);
                let weight = 1.0 / ((x1 - x0) * (y1 - y0)) as f32;
                let mut sum = [0.0; 3];
                for y in y0..y1 {
                    for x in x0..x1 {
                        let color = frame[y * self.width + x].0;
                        let luminance = 0.2126 * color[0] + 0.7152 * color[1] + 0.0722 * color[2];
                        let exposed = luminance * exposure;
                        let factor = if exposed > 1.0 {
                            (1.0 - 1.0 / exposed) * weight
                        } else {
                            0.0
                        };
                        for channel in 0..3 {
                            sum[channel] += color[channel] * factor;
                        }
                    }
                }
                self.bright[hy * self.half_width + hx] = LinearRgb(sum);
            }
        }

        // Clamp-to-edge convolution preserves uniform fields, including tiny
        // images. Both passes use the same normalized five-tap Gaussian.
        for y in 0..self.half_height {
            for x in 0..self.half_width {
                let mut sum = [0.0; 3];
                for (tap, weight) in WEIGHTS.iter().enumerate() {
                    let sx = (x + tap).saturating_sub(2).min(self.half_width - 1);
                    let color = self.bright[y * self.half_width + sx].0;
                    for channel in 0..3 {
                        sum[channel] += color[channel] * weight;
                    }
                }
                self.scratch[y * self.half_width + x] = LinearRgb(sum);
            }
        }
        for y in 0..self.half_height {
            for x in 0..self.half_width {
                let mut sum = [0.0; 3];
                for (tap, weight) in WEIGHTS.iter().enumerate() {
                    let sy = (y + tap).saturating_sub(2).min(self.half_height - 1);
                    let color = self.scratch[sy * self.half_width + x].0;
                    for channel in 0..3 {
                        sum[channel] += color[channel] * weight;
                    }
                }
                self.bright[y * self.half_width + x] = LinearRgb(sum);
            }
        }

        for y in 0..self.height {
            let (y0, y1, fy) = sample_axis(y, self.half_height);
            for x in 0..self.width {
                let (x0, x1, fx) = sample_axis(x, self.half_width);
                let a = self.bright[y0 * self.half_width + x0].0;
                let b = self.bright[y0 * self.half_width + x1].0;
                let c = self.bright[y1 * self.half_width + x0].0;
                let d = self.bright[y1 * self.half_width + x1].0;
                let pixel = &mut frame[y * self.width + x].0;
                for channel in 0..3 {
                    let top = a[channel] + (b[channel] - a[channel]) * fx;
                    let bottom = c[channel] + (d[channel] - c[channel]) * fx;
                    pixel[channel] += (top + (bottom - top) * fy) * strength;
                }
            }
        }
        Ok(())
    }
}

fn sample_axis(coordinate: usize, half_size: usize) -> (usize, usize, f32) {
    // Full-resolution pixel centers mapped to the 2x2 downsample grid.
    let sample = ((coordinate as f32 + 0.5) * 0.5 - 0.5).max(0.0);
    let lower = (sample.floor() as usize).min(half_size - 1);
    (lower, (lower + 1).min(half_size - 1), sample.fract())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_and_subthreshold_fields_remain_unchanged() {
        let mut bloom = Bloom::new(7, 5).unwrap();
        for value in [0.0, 0.2, 0.5] {
            let mut frame = vec![LinearRgb([value; 3]); 35];
            let before = frame.clone();
            bloom.apply(&mut frame, 0.35, 2.0).unwrap();
            assert_eq!(frame, before);
        }
    }

    #[test]
    fn uniform_highlights_keep_color_and_expected_energy() {
        let color = [4.0, 2.0, 1.0];
        let luminance = 0.2126 * color[0] + 0.7152 * color[1] + 0.0722 * color[2];
        let expected = color.map(|v| v * (1.0 + 0.35 * (1.0 - 1.0 / (2.0 * luminance))));
        for (width, height) in [(1, 1), (1, 7), (7, 1), (2, 2), (7, 5), (8, 6)] {
            let mut bloom = Bloom::new(width, height).unwrap();
            let mut frame = vec![LinearRgb(color); width * height];
            bloom.apply(&mut frame, 0.35, 2.0).unwrap();
            for pixel in frame {
                for (actual, expected) in pixel.0.into_iter().zip(expected) {
                    assert!((actual - expected).abs() < 2e-6);
                }
            }
        }
    }

    #[test]
    fn bright_impulse_spreads_and_retains_hdr() {
        let mut bloom = Bloom::new(13, 13).unwrap();
        let mut frame = vec![LinearRgb::default(); 13 * 13];
        frame[6 * 13 + 6] = LinearRgb([16.0, 8.0, 4.0]);
        bloom.apply(&mut frame, 0.35, 2.0).unwrap();
        assert!(frame[6 * 13 + 6].0[0] > 16.0);
        let neighbor = frame[6 * 13 + 8].0;
        assert!(neighbor[0] > 0.0);
        assert!((neighbor[0] - 2.0 * neighbor[1]).abs() < 1e-6);
        assert!((neighbor[1] - 2.0 * neighbor[2]).abs() < 1e-6);
        assert_eq!(frame[0], LinearRgb::default());
    }

    #[test]
    fn disabled_is_bit_exact_and_storage_is_reused() {
        let mut bloom = Bloom::new(3, 1).unwrap();
        let mut frame = [LinearRgb([-0.0, 0.1, 8.0]); 3];
        let before = frame.map(|p| p.0.map(f32::to_bits));
        let buffers = (bloom.bright.as_ptr(), bloom.scratch.as_ptr());
        bloom.apply(&mut frame, 0.0, 2.0).unwrap();
        assert_eq!(frame.map(|p| p.0.map(f32::to_bits)), before);
        bloom.apply(&mut frame, 0.35, 0.0).unwrap();
        assert_eq!(frame.map(|p| p.0.map(f32::to_bits)), before);
        for _ in 0..3 {
            bloom.apply(&mut frame, 0.35, 2.0).unwrap();
        }
        assert_eq!((bloom.bright.as_ptr(), bloom.scratch.as_ptr()), buffers);
    }

    #[test]
    fn exposure_selects_highlights_without_double_exposure() {
        let mut bloom = Bloom::new(1, 1).unwrap();
        let mut frame = [LinearRgb([0.75; 3])];
        bloom.apply(&mut frame, 1.0, 1.0).unwrap();
        assert_eq!(frame[0], LinearRgb([0.75; 3]));
        bloom.apply(&mut frame, 1.0, 2.0).unwrap();
        assert!((frame[0].0[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn invalid_arguments_are_atomic() {
        for dimensions in [(0, 1), (1, 0), (usize::MAX, 2), (8192, 8192)] {
            assert!(Bloom::new(dimensions.0, dimensions.1).is_err());
        }
        let mut bloom = Bloom::new(2, 1).unwrap();
        for (strength, exposure) in [
            (-1.0, 2.0),
            (17.0, 2.0),
            (f32::NAN, 2.0),
            (1.0, -1.0),
            (1.0, 65.0),
            (1.0, f32::INFINITY),
        ] {
            let mut frame = [LinearRgb([4.0; 3]); 2];
            let before = frame;
            assert!(bloom.apply(&mut frame, strength, exposure).is_err());
            assert_eq!(frame, before);
        }
        let mut short = [LinearRgb([4.0; 3])];
        assert!(bloom.apply(&mut short, 1.0, 2.0).is_err());
        assert_eq!(short, [LinearRgb([4.0; 3])]);
        for invalid in [f32::NAN, f32::INFINITY, -1.0, f32::MAX] {
            let mut frame = [LinearRgb([4.0; 3]), LinearRgb([invalid; 3])];
            let before = frame.map(|p| p.0.map(f32::to_bits));
            assert!(bloom.apply(&mut frame, 1.0, 2.0).is_err());
            assert_eq!(frame.map(|p| p.0.map(f32::to_bits)), before);
        }
        // An error must not poison the reused scratch storage.
        let mut valid = [LinearRgb([4.0; 3]); 2];
        bloom.apply(&mut valid, 16.0, 64.0).unwrap();
        assert!(valid.iter().all(|p| p.0.iter().all(|v| v.is_finite())));
    }
}
