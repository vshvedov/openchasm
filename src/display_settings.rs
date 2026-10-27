//! Bounded scene resolution and desktop-fullscreen preferences.
use sdl2::video::{FullscreenType, Window};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

pub const MAX_PIXELS: u32 = 1920 * 1200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Default for Resolution {
    fn default() -> Self {
        Self {
            width: 640,
            height: 400,
        }
    }
}

/// Common 16:9, 16:10 and 4:3 scene sizes, in width/height order.
pub const RESOLUTIONS: &[Resolution] = &[
    Resolution {
        width: 320,
        height: 200,
    },
    Resolution {
        width: 640,
        height: 360,
    },
    Resolution {
        width: 640,
        height: 400,
    },
    Resolution {
        width: 640,
        height: 480,
    },
    Resolution {
        width: 800,
        height: 450,
    },
    Resolution {
        width: 800,
        height: 500,
    },
    Resolution {
        width: 800,
        height: 600,
    },
    Resolution {
        width: 960,
        height: 540,
    },
    Resolution {
        width: 1024,
        height: 640,
    },
    Resolution {
        width: 1024,
        height: 768,
    },
    Resolution {
        width: 1280,
        height: 720,
    },
    Resolution {
        width: 1280,
        height: 800,
    },
    Resolution {
        width: 1280,
        height: 960,
    },
    Resolution {
        width: 1440,
        height: 900,
    },
    Resolution {
        width: 1600,
        height: 900,
    },
    Resolution {
        width: 1600,
        height: 1000,
    },
    Resolution {
        width: 1600,
        height: 1200,
    },
    Resolution {
        width: 1920,
        height: 1080,
    },
    Resolution {
        width: 1920,
        height: 1200,
    },
];

impl Resolution {
    pub fn validate(self) -> Result<(), &'static str> {
        if !(320..=1920).contains(&self.width)
            || !(200..=1200).contains(&self.height)
            || self
                .width
                .checked_mul(self.height)
                .is_none_or(|pixels| pixels > MAX_PIXELS)
        {
            return Err("resolution must be 320x200 through 1920x1200");
        }
        Ok(())
    }
    pub fn dimensions(self) -> Result<(usize, usize), &'static str> {
        self.validate()?;
        Ok((self.width as usize, self.height as usize))
    }
    pub fn shifted(self, step: i32) -> Self {
        let insertion =
            RESOLUTIONS.partition_point(|r| (r.width, r.height) < (self.width, self.height));
        let index = if RESOLUTIONS.get(insertion) == Some(&self) {
            insertion as i64 + i64::from(step)
        } else if step < 0 {
            insertion as i64 - 1
        } else {
            insertion as i64
        };
        RESOLUTIONS[index.rem_euclid(RESOLUTIONS.len() as i64) as usize]
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

impl FromStr for Resolution {
    type Err = &'static str;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (width, height) = text.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT")?;
        let resolution = Self {
            width: width.parse().map_err(|_| "invalid resolution width")?,
            height: height.parse().map_err(|_| "invalid resolution height")?,
        };
        resolution.validate()?;
        Ok(resolution)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayConfig {
    pub fullscreen: bool,
    /// Internal scene size, and requested window size when windowed.
    pub width: u32,
    pub height: u32,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        let resolution = Resolution::default();
        Self {
            fullscreen: false,
            width: resolution.width,
            height: resolution.height,
        }
    }
}

impl DisplayConfig {
    pub fn resolution(self) -> Resolution {
        Resolution {
            width: self.width,
            height: self.height,
        }
    }
    pub fn set_resolution(&mut self, resolution: Resolution) -> Result<(), &'static str> {
        resolution.validate()?;
        self.width = resolution.width;
        self.height = resolution.height;
        Ok(())
    }
    pub fn validate(self) -> Result<(), &'static str> {
        self.resolution().validate()
    }
    pub fn render_dimensions(self) -> Result<(usize, usize), &'static str> {
        self.resolution().dimensions()
    }
    /// Apply only SDL output state. The host must prepare scene buffers and
    /// update presentation for render_dimensions separately. Window clones
    /// share SDL ownership, allowing this while Presentation borrows the window.
    /// Fullscreen uses the desktop mode; selected scene size remains unchanged.
    pub fn apply(self, window: &Window) -> Result<(), String> {
        self.validate().map_err(str::to_owned)?;
        let mut window = window.clone();
        if self.fullscreen {
            window.set_fullscreen(FullscreenType::Desktop)
        } else {
            let old_mode = window.fullscreen_state();
            window.set_fullscreen(FullscreenType::Off)?;
            if let Err(error) = window.set_size(self.width, self.height) {
                let restore = window.set_fullscreen(old_mode);
                return Err(match restore {
                    Ok(()) => error.to_string(),
                    Err(restore) => {
                        format!("{error}; restoring previous fullscreen mode failed: {restore}")
                    }
                });
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_parser_and_bounds_reject_invalid_allocations() {
        assert_eq!(
            "640x400".parse::<Resolution>().unwrap(),
            Resolution::default()
        );
        assert_eq!(
            "1920X1200".parse::<Resolution>().unwrap().dimensions(),
            Ok((1920, 1200))
        );
        for text in [
            "0x0",
            "319x200",
            "640x199",
            "1921x1080",
            "1920x1201",
            "4294967295x4294967295",
            "640x400x2",
            "640 400",
            "640xnan",
        ] {
            assert!(text.parse::<Resolution>().is_err(), "accepted {text}");
        }
        for resolution in RESOLUTIONS {
            assert!(resolution.validate().is_ok());
            assert_eq!(
                resolution.to_string().parse::<Resolution>(),
                Ok(*resolution)
            );
        }
    }

    #[test]
    fn presets_wrap_and_custom_sizes_step_to_neighbor() {
        assert_eq!(RESOLUTIONS[0].shifted(-1), *RESOLUTIONS.last().unwrap());
        assert_eq!(RESOLUTIONS.last().unwrap().shifted(1), RESOLUTIONS[0]);
        let custom = Resolution {
            width: 1000,
            height: 600,
        };
        assert_eq!(
            custom.shifted(1),
            Resolution {
                width: 1024,
                height: 640
            }
        );
        assert_eq!(
            custom.shifted(-1),
            Resolution {
                width: 960,
                height: 540
            }
        );
    }

    #[test]
    fn config_defaults_are_partial_compatible_and_invalid_change_is_atomic() {
        let mut config: DisplayConfig = serde_json::from_str("{\"fullscreen\":true}").unwrap();
        assert!(config.fullscreen);
        assert_eq!(config.render_dimensions(), Ok((640, 400)));
        let before = config;
        assert!(config
            .set_resolution(Resolution {
                width: u32::MAX,
                height: 400
            })
            .is_err());
        assert_eq!(config, before);
        assert!(serde_json::from_str::<DisplayConfig>("{\"unknown\":1}").is_err());
        config
            .set_resolution(Resolution {
                width: 1280,
                height: 720,
            })
            .unwrap();
        let restored: DisplayConfig =
            serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(restored, config);
    }
}
