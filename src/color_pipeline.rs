//! Software color conversion for the remaster's post-process.
//!
//! Material texels decode from sRGB to linear HDR; light and alpha operations
//! happen in that linear space. Scene and weapon pixels are finalized before
//! drawing the HUD. See `docs/remaster-rendering.md` for executable evidence.
use std::sync::OnceLock;

const LUT_SCALE: f32 = 1024.0;
const LUT_MAX: usize = 16 * 1024;

fn finite_nonnegative(value: f32) -> f32 {
    if value.is_nan() || value <= 0.0 {
        0.0
    } else {
        // Far above the range that maps to display white; bounds also prevent
        // malformed color values from overflowing subsequent operations.
        value.min(1_000_000.0)
    }
}
fn srgb_decode(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}
fn srgb_encode(value: f32) -> f32 {
    if value <= 0.0031308 {
        12.92 * value
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}
fn aces(value: f32) -> f32 {
    // The algebraic form is bounded before squaring to avoid overflow for
    // malformed HDR inputs; above this point the clamped result is already 1.
    let x = value.min(1_000_000.0);
    (x * (2.51 * x + 0.03) / (x * (2.43 * x + 0.59) + 0.14)).clamp(0.0, 1.0)
}
fn decode_table() -> &'static [f32; 256] {
    static TABLE: OnceLock<[f32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| std::array::from_fn(|i| srgb_decode(i as f32 / 255.0)))
}
fn neutral_table(tone_mapping: bool) -> &'static [f32] {
    static ACES: OnceLock<Vec<f32>> = OnceLock::new();
    static LINEAR: OnceLock<Vec<f32>> = OnceLock::new();
    let table = if tone_mapping { &ACES } else { &LINEAR };
    table.get_or_init(|| {
        (0..=LUT_MAX)
            .map(|i| {
                let value = i as f32 / LUT_SCALE;
                srgb_encode(if tone_mapping {
                    aces(value)
                } else {
                    value.min(1.0)
                })
            })
            .collect()
    })
}
#[inline(always)]
fn sample_lut(table: &[f32], value: f32) -> f32 {
    let value = finite_nonnegative(value);
    if value >= 16.0 {
        return 1.0;
    }
    let index = value * LUT_SCALE;
    let lower = index as usize;
    let fraction = index - lower as f32;
    table[lower] + (table[lower + 1] - table[lower]) * fraction
}
#[inline(always)]
fn byte(value: f32) -> u32 {
    let scaled = value.clamp(0.0, 1.0) * 255.0;
    let whole = scaled as u32;
    // Compare the fractional part instead of adding 0.5: f32 addition can
    // round values just below a half-integer up to the boundary.
    whole + u32::from(scaled - whole as f32 >= 0.5)
}

/// Nonnegative scene-linear RGB. Values above one are intentionally retained.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LinearRgb(pub [f32; 3]);
impl LinearRgb {
    pub fn from_srgb8(rgb: u32) -> Self {
        let table = decode_table();
        Self([
            table[((rgb >> 16) & 255) as usize],
            table[((rgb >> 8) & 255) as usize],
            table[(rgb & 255) as usize],
        ])
    }
    pub fn scale(self, light: f32) -> Self {
        let light = finite_nonnegative(light);
        Self(self.0.map(|channel| finite_nonnegative(channel) * light))
    }
    /// Source-over blend in linear HDR space. Invalid alpha becomes zero.
    pub fn over(self, background: Self, alpha: f32) -> Self {
        let alpha = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self(std::array::from_fn(|i| {
            finite_nonnegative(self.0[i]) * alpha
                + finite_nonnegative(background.0[i]) * (1.0 - alpha)
        }))
    }
}

/// A scene pixel target shared by legacy and linear-HDR rasterization.
pub trait RenderPixel: Copy + Default + PartialEq {
    fn from_srgb8(rgb: u32) -> Self;
    fn shade_srgb8(rgb: u32, light: f32) -> Self;
    fn over(self, background: Self, alpha: f32) -> Self;
}
impl RenderPixel for u32 {
    fn from_srgb8(rgb: u32) -> Self {
        rgb & 0x00ff_ffff
    }
    fn shade_srgb8(rgb: u32, light: f32) -> Self {
        let light = finite_nonnegative(light);
        let channel = |shift: u32| ((((rgb >> shift) & 255) as f32 * light) as u32).min(255);
        channel(16) << 16 | channel(8) << 8 | channel(0)
    }
    fn over(self, background: Self, alpha: f32) -> Self {
        let alpha = if alpha.is_finite() {
            (alpha.clamp(0.0, 1.0) * 255.0).round() as u32
        } else {
            0
        };
        let channel = |shift: u32| {
            (((self >> shift) & 255) * alpha + ((background >> shift) & 255) * (255 - alpha)) / 255
        };
        channel(16) << 16 | channel(8) << 8 | channel(0)
    }
}
impl RenderPixel for LinearRgb {
    fn from_srgb8(rgb: u32) -> Self {
        LinearRgb::from_srgb8(rgb)
    }
    fn shade_srgb8(rgb: u32, light: f32) -> Self {
        LinearRgb::from_srgb8(rgb).scale(light)
    }
    fn over(self, background: Self, alpha: f32) -> Self {
        LinearRgb::over(self, background, alpha)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorSettings {
    pub gamma: f32,
    pub brightness: f32,
    pub saturation: f32,
    pub fade: f32,
    pub tone_mapping: bool,
}
impl Default for ColorSettings {
    fn default() -> Self {
        Self {
            gamma: 1.0,
            brightness: 1.0,
            saturation: 1.0,
            fade: 1.0,
            tone_mapping: true,
        }
    }
}
impl ColorSettings {
    /// Bounds prevent undefined/overflowing shader operations on invalid input.
    pub fn validate(self) -> Result<Self, &'static str> {
        if !self.gamma.is_finite() || !(0.05..=16.0).contains(&self.gamma) {
            return Err("gamma outside 0.05..=16");
        }
        if !self.brightness.is_finite() || !(0.0..=16.0).contains(&self.brightness) {
            return Err("brightness outside 0..=16");
        }
        if !self.saturation.is_finite() || !(0.0..=16.0).contains(&self.saturation) {
            return Err("saturation outside 0..=16");
        }
        if !self.fade.is_finite() || !(0.0..=1.0).contains(&self.fade) {
            return Err("fade outside 0..=1");
        }
        Ok(self)
    }
}

pub struct ColorPipeline {
    settings: ColorSettings,
    exposure: f32,
    neutral: bool,
    table: &'static [f32],
}
impl ColorPipeline {
    pub fn exposure(&self) -> f32 {
        self.exposure
    }
    pub fn new(settings: ColorSettings) -> Result<Self, &'static str> {
        let settings = settings.validate()?;
        Ok(Self {
            exposure: 2.0 * settings.brightness * settings.fade.powi(3),
            neutral: settings.gamma == 1.0 && settings.saturation == 1.0,
            table: neutral_table(settings.tone_mapping),
            settings,
        })
    }
    /// Finalize one HDR scene pixel as packed 0xRRGGBB display color.
    pub fn encode(&self, pixel: LinearRgb) -> u32 {
        let input = pixel.0.map(finite_nonnegative);
        let adjusted = if self.neutral {
            input
        } else {
            let y = 0.2126 * input[0] + 0.7152 * input[1] + 0.0722 * input[2];
            if y <= 0.0 || !y.is_finite() {
                [0.0; 3]
            } else {
                let luminance = y.powf(1.0 / self.settings.gamma);
                input.map(|channel| (channel / y).powf(self.settings.saturation) * luminance)
            }
        };
        let channel = |v| {
            byte(sample_lut(
                self.table,
                finite_nonnegative(v * self.exposure),
            ))
        };
        channel(adjusted[0]) << 16 | channel(adjusted[1]) << 8 | channel(adjusted[2])
    }
    /// Reject unequal buffers without modifying either one.
    pub fn encode_frame(
        &self,
        input: &[LinearRgb],
        output: &mut [u32],
    ) -> Result<(), &'static str> {
        if input.len() != output.len() {
            return Err("scene and output lengths differ");
        }
        for (src, dst) in input.iter().zip(output.iter_mut()) {
            *dst = self.encode(*src);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_conversion_matches_round_at_quantization_boundaries() {
        let reference = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
        for n in 0..255 {
            let boundary = (n as f32 + 0.5) / 255.0;
            for bits in boundary.to_bits().saturating_sub(16)..=boundary.to_bits() + 16 {
                let value = f32::from_bits(bits);
                assert_eq!(byte(value), reference(value), "value {value:?}");
            }
        }
        for value in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -1.0,
            0.0,
            1.0,
            2.0,
        ] {
            assert_eq!(byte(value), reference(value));
        }
    }
    #[test]
    fn srgb_decode_matches_standard_breakpoint_and_known_gray() {
        let gray = LinearRgb::from_srgb8(0x808080);
        assert!((gray.0[0] - 0.215_860_5).abs() < 1e-6);
        assert!((srgb_decode(0.04045) - 0.0031308).abs() < 1e-7);
        assert_eq!(LinearRgb::from_srgb8(0xffffff), LinearRgb([1.0; 3]));
    }
    #[test]
    fn pixel_trait_keeps_legacy_blending_and_linear_hdr_separate() {
        assert_eq!(<u32 as RenderPixel>::shade_srgb8(0x804020, 0.5), 0x402010);
        assert_eq!(
            <u32 as RenderPixel>::over(0xff0000, 0x0000ff, 128.0 / 255.0),
            0x80007f
        );
        let lit = <LinearRgb as RenderPixel>::shade_srgb8(0xffffff, 2.0);
        assert_eq!(lit, LinearRgb([2.0; 3]));
        assert_eq!(
            <LinearRgb as RenderPixel>::over(lit, LinearRgb([0.0; 3]), 0.5),
            LinearRgb([1.0; 3])
        );
    }
    #[test]
    fn hdr_is_preserved_through_light_blend_and_tonemap() {
        let foreground = LinearRgb([4.0, 2.0, 0.0]);
        let background = LinearRgb([0.0, 0.0, 2.0]);
        let mix = foreground.over(background, 0.5);
        assert_eq!(mix, LinearRgb([2.0, 1.0, 1.0]));
        assert_eq!(mix.scale(2.0), LinearRgb([4.0, 2.0, 2.0]));
        let pipeline = ColorPipeline::new(ColorSettings::default()).unwrap();
        let bright = pipeline.encode(LinearRgb([1.0, 0.0, 0.0]));
        let brighter = pipeline.encode(LinearRgb([2.0, 0.0, 0.0]));
        assert!(brighter > bright);
    }
    #[test]
    fn neutral_lut_matches_exact_curve_within_one_byte() {
        let pipeline = ColorPipeline::new(ColorSettings::default()).unwrap();
        for i in 0..20000 {
            let linear = i as f32 / 2500.0;
            let actual = pipeline.encode(LinearRgb([linear; 3])) & 255;
            let exact = byte(srgb_encode(aces(2.0 * linear)));
            assert!(actual.abs_diff(exact) <= 1, "{linear}: {actual} {exact}");
        }
    }
    #[test]
    fn chroma_gamma_saturation_and_fade_follow_shader_order() {
        let settings = ColorSettings {
            gamma: 2.0,
            saturation: 0.0,
            fade: 0.5,
            tone_mapping: false,
            ..ColorSettings::default()
        };
        let pipeline = ColorPipeline::new(settings).unwrap();
        let color = LinearRgb([0.25, 0.5, 1.0]);
        let y: f32 = 0.2126 * 0.25 + 0.7152 * 0.5 + 0.0722;
        let expected = byte(srgb_encode(y.sqrt() * 0.25));
        assert_eq!(
            pipeline.encode(color),
            expected << 16 | expected << 8 | expected
        );
        assert_eq!(pipeline.encode(LinearRgb([0.0; 3])), 0);
    }
    #[test]
    fn validation_and_frame_failure_are_bounded() {
        for settings in [
            ColorSettings {
                gamma: 0.0,
                ..ColorSettings::default()
            },
            ColorSettings {
                fade: f32::NAN,
                ..ColorSettings::default()
            },
            ColorSettings {
                brightness: -1.0,
                ..ColorSettings::default()
            },
        ] {
            assert!(ColorPipeline::new(settings).is_err());
        }
        let p = ColorPipeline::new(ColorSettings::default()).unwrap();
        let mut output = [42];
        assert!(p.encode_frame(&[], &mut output).is_err());
        assert_eq!(output, [42]);
        assert_eq!(p.encode(LinearRgb([f32::NAN, -1.0, f32::NEG_INFINITY])), 0);
        assert_eq!(
            LinearRgb([2.0; 3]).over(LinearRgb([1.0; 3]), f32::NAN),
            LinearRgb([1.0; 3])
        );
    }
}
