//! Display-space UI operations shared by software and GPU presentation.
//!
//! An overlay pixel stores the affine operation `background * (1 - opacity) +
//! add_rgb`. This preserves draw order without reading the post-processed scene
//! back from the GPU. Opaque black is distinct from untouched background.

pub trait UiPixel: Copy + Default {
    fn from_rgb(rgb: u32) -> Self;
    fn dim_shift_bias(self, shift: u32, bias: u32) -> Self;
    fn tint_rgb(self, color: u32, alpha: u8) -> Self;
}

impl UiPixel for u32 {
    fn from_rgb(rgb: u32) -> Self {
        rgb
    }

    fn dim_shift_bias(self, shift: u32, bias: u32) -> Self {
        let shift = shift.min(8);
        let mask = (255 >> shift) * 0x010101;
        ((self >> shift) & mask).wrapping_add(bias)
    }

    fn tint_rgb(self, color: u32, alpha: u8) -> Self {
        if alpha == 0 {
            return self;
        }
        let alpha = u32::from(alpha);
        let channel = |shift: u32| -> u32 {
            let background = (self >> shift) & 255;
            let source = (color >> shift) & 255;
            (background * (255 - alpha) + source * alpha) / 255
        };
        channel(16) << 16 | channel(8) << 8 | channel(0)
    }
}

/// Normalized additive RGB and opacity (`1 - background gain`), in display
/// space. RGB is already weighted by coverage; do not multiply it by A again.
#[repr(transparent)]
#[cfg_attr(feature = "gpu", derive(bytemuck::Pod, bytemuck::Zeroable))]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OverlayPixel(pub [f32; 4]);

impl OverlayPixel {
    #[cfg(test)]
    pub fn packed(self) -> [f32; 4] {
        self.0
    }

    /// Borrow the GPU upload representation without allocating or copying.
    #[cfg(feature = "gpu")]
    pub fn as_packed(pixels: &[Self]) -> &[[f32; 4]] {
        bytemuck::cast_slice(pixels)
    }
}

fn rgb_channels(rgb: u32) -> [f32; 3] {
    [16, 8, 0].map(|shift| ((rgb >> shift) & 255) as f32 / 255.0)
}

impl UiPixel for OverlayPixel {
    fn from_rgb(rgb: u32) -> Self {
        let [r, g, b] = rgb_channels(rgb);
        Self([r, g, b, 1.0])
    }

    fn dim_shift_bias(self, shift: u32, bias: u32) -> Self {
        // Software floors each byte before adding the bias. Deferred affine
        // composition postpones rounding, allowing small byte-level differences.
        let gain = if shift >= 8 {
            0.0
        } else {
            1.0 / (1u32 << shift) as f32
        };
        let bias = rgb_channels(bias);
        Self([
            self.0[0] * gain + bias[0],
            self.0[1] * gain + bias[1],
            self.0[2] * gain + bias[2],
            1.0 - (1.0 - self.0[3]) * gain,
        ])
    }

    fn tint_rgb(self, color: u32, alpha: u8) -> Self {
        let opacity = f32::from(alpha) / 255.0;
        let gain = 1.0 - opacity;
        let source = rgb_channels(color);
        Self([
            self.0[0] * gain + source[0] * opacity,
            self.0[1] * gain + source[1] * opacity,
            self.0[2] * gain + source[2] * opacity,
            1.0 - (1.0 - self.0[3]) * gain,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(overlay: OverlayPixel, background: u32) -> u32 {
        let background = rgb_channels(background);
        let out: [u32; 3] = std::array::from_fn(|i| {
            ((background[i] * (1.0 - overlay.0[3]) + overlay.0[i]).clamp(0.0, 1.0) * 255.0).round()
                as u32
        });
        out[0] << 16 | out[1] << 8 | out[2]
    }
    fn close(actual: u32, expected: u32, tolerance: i32) {
        for shift in [16, 8, 0] {
            let a = ((actual >> shift) & 255) as i32;
            let e = ((expected >> shift) & 255) as i32;
            assert!((a - e).abs() <= tolerance, "{actual:06x} != {expected:06x}");
        }
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn packed_slice_borrows_storage_and_roundtrips_bytes() {
        let pixels = [
            OverlayPixel::default(),
            OverlayPixel::from_rgb(0),
            OverlayPixel::from_rgb(0x123456).dim_shift_bias(1, 0),
        ];
        let packed = OverlayPixel::as_packed(&pixels);
        assert_eq!(std::mem::size_of::<OverlayPixel>(), 16);
        assert_eq!(
            std::mem::align_of::<OverlayPixel>(),
            std::mem::align_of::<[f32; 4]>()
        );
        assert_eq!(packed.as_ptr().cast::<u8>(), pixels.as_ptr().cast::<u8>());
        assert_eq!(packed, &pixels.map(OverlayPixel::packed));
        assert_eq!(packed[0], [0.0; 4]);
        assert_eq!(packed[1], [0.0, 0.0, 0.0, 1.0]);
        let bytes: &[u8] = bytemuck::cast_slice(packed);
        assert_eq!(bytemuck::cast_slice::<u8, OverlayPixel>(bytes), &pixels);
        assert_eq!(
            <OverlayPixel as bytemuck::Zeroable>::zeroed(),
            OverlayPixel::default()
        );
        assert!(OverlayPixel::as_packed(&[]).is_empty());
    }

    #[test]
    fn untouched_pixels_and_opaque_black_have_distinct_coverage() {
        let untouched = OverlayPixel::default();
        let black = OverlayPixel::from_rgb(0);
        assert_eq!(untouched.packed(), [0.0; 4]);
        assert_eq!(black.packed(), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(resolve(untouched, 0xabcdef), 0xabcdef);
        assert_eq!(resolve(black, 0xabcdef), 0);
    }

    #[test]
    fn integer_menu_console_and_damage_operations_remain_exact() {
        for value in 0..=255u32 {
            let rgb = value << 16 | (255 - value) << 8 | (value * 37 % 256);
            assert_eq!(rgb.dim_shift_bias(1, 0), ((rgb & 0xfefefe) >> 1) & 0x7f7f7f);
            assert_eq!(
                rgb.dim_shift_bias(4, 0x0c1014),
                ((rgb >> 4) & 0x0f0f0f) + 0x0c1014
            );
            for alpha in [0, 1, 32, 127, 254, 255] {
                let a = u32::from(alpha);
                let red = ((rgb >> 16) & 255) * (255 - a) / 255 + a;
                let green = ((rgb >> 8) & 255) * (255 - a) / 255;
                let blue = (rgb & 255) * (255 - a) / 255;
                assert_eq!(rgb.tint_rgb(0xff0000, alpha), red << 16 | green << 8 | blue);
            }
        }
    }

    #[test]
    fn two_pass_dim_and_bias_stay_within_byte_rounding_tolerance() {
        for value in 0..=255u32 {
            let background = value << 16 | (255 - value) << 8 | (value * 53 % 256);
            for console_first in [false, true] {
                let operations = if console_first {
                    [(4, 0x0c1014), (1, 0)]
                } else {
                    [(1, 0), (4, 0x0c1014)]
                };
                let mut software = background;
                let mut overlay = OverlayPixel::default();
                for (shift, bias) in operations {
                    software = software.dim_shift_bias(shift, bias);
                    overlay = overlay.dim_shift_bias(shift, bias);
                }
                close(resolve(overlay, background), software, 1);
            }
        }
    }

    #[test]
    fn opaque_writes_dims_and_tints_preserve_draw_order() {
        let background = 0x123456;
        let overlay = OverlayPixel::from_rgb(0xabcdef)
            .dim_shift_bias(4, 0x0c1014)
            .tint_rgb(0xff0000, 80);
        let software = 0xabcdefu32
            .dim_shift_bias(4, 0x0c1014)
            .tint_rgb(0xff0000, 80);
        close(resolve(overlay, background), software, 2);
        assert_eq!(overlay.0[3], 1.0);
        let dim_then_tint = OverlayPixel::default()
            .dim_shift_bias(1, 0)
            .tint_rgb(0xff0000, 128);
        let tint_then_dim = OverlayPixel::default()
            .tint_rgb(0xff0000, 128)
            .dim_shift_bias(1, 0);
        assert!(dim_then_tint.0[0] > tint_then_dim.0[0] + 0.2);
        assert_eq!(OverlayPixel::from_rgb(0xffffff).packed(), [1.0; 4]);
        assert_eq!(
            resolve(OverlayPixel::default().tint_rgb(0x102030, 255), background),
            0x102030
        );
    }
}
