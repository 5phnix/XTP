//! Windows caption preferences, the title's enabled override, and high-contrast mode.

use std::sync::atomic::{AtomicU8, Ordering};

use super::{HRESULT, S_OK, This, put};

// Read ShowSounds until the title makes a choice of its own.
const FOLLOW_WINDOWS: u8 = 2;
static ENABLED: AtomicU8 = AtomicU8::new(FOLLOW_WINDOWS);

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Properties {
    background: [u8; 4],
    text: [u8; 4],
    window: [u8; 4],
    edge: u32,
    style: u32,
    scale: f32,
    enabled: u8,
    reserved: [u8; 3],
}

impl Default for Properties {
    fn default() -> Self {
        Self {
            background: [255, 0, 0, 0],
            text: [255; 4],
            window: [0; 4],
            edge: 0,
            style: 0,
            scale: 1.0,
            enabled: 0,
            reserved: [0; 3],
        }
    }
}

impl Properties {
    fn write(mut self, out: *mut Self, enabled: u8) -> HRESULT {
        if enabled != FOLLOW_WINDOWS {
            self.enabled = enabled;
        }
        put(out, self)
    }
}

pub(super) extern "system" fn properties(_: This, out: *mut Properties) -> HRESULT {
    #[cfg(windows)]
    let settings = host::captions();
    #[cfg(not(windows))]
    let settings = Properties::default();
    settings.write(out, ENABLED.load(Ordering::Relaxed))
}

pub(super) extern "system" fn set_enabled(_: This, enabled: u8) -> HRESULT {
    ENABLED.store(u8::from(enabled != 0), Ordering::Relaxed);
    S_OK
}

pub(super) extern "system" fn high_contrast(_: This, out: *mut u32) -> HRESULT {
    #[cfg(windows)]
    let mode = host::contrast();
    #[cfg(not(windows))]
    let mode = 0;
    put(out, mode)
}

#[cfg(windows)]
mod host {
    use super::Properties;
    use std::sync::{Mutex, PoisonError};
    use std::time::{Duration, Instant};
    use windows::Media::ClosedCaptioning::{
        ClosedCaptionColor as Color, ClosedCaptionOpacity as Opacity,
        ClosedCaptionProperties as Captions, ClosedCaptionSize as Size,
    };
    use windows::UI::ViewManagement::{AccessibilitySettings, UIElementType, UISettings};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHOWSOUNDS};

    const REFRESH: Duration = Duration::from_secs(1);

    fn cached<T: Copy>(cache: &Mutex<Option<(Instant, T)>>, read: impl FnOnce() -> T) -> T {
        let mut entry = cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, value)) = *entry
            && at.elapsed() < REFRESH
        {
            return value;
        }
        let value = read();
        *entry = Some((Instant::now(), value));
        value
    }

    // The caption ABI stores alpha first. Windows lets the user override the
    // color and opacity independently, including setting just one of them.
    fn argb(
        color: Color,
        computed: impl FnOnce() -> windows::core::Result<windows::UI::Color>,
        opacity: Opacity,
        fallback: [u8; 4],
    ) -> [u8; 4] {
        let [alpha, r, g, b] = fallback;
        let [r, g, b] = if color != Color::Default
            && let Ok(c) = computed()
        {
            [c.R, c.G, c.B]
        } else {
            [r, g, b]
        };
        let alpha = match opacity {
            Opacity::OneHundredPercent => 255,
            Opacity::SeventyFivePercent => 191,
            Opacity::TwentyFivePercent => 64,
            Opacity::ZeroPercent => 0,
            _ => alpha,
        };
        [alpha, r, g, b]
    }

    fn font_scale(size: Size) -> f32 {
        match size {
            Size::FiftyPercent => 0.5,
            Size::OneHundredFiftyPercent => 1.5,
            Size::TwoHundredPercent => 2.0,
            _ => 1.0,
        }
    }

    pub(super) fn captions() -> Properties {
        static CAPTIONS: Mutex<Option<(Instant, Properties)>> = Mutex::new(None);
        cached(&CAPTIONS, read_captions)
    }

    fn read_captions() -> Properties {
        let defaults = Properties::default();
        // The Windows and title enums use the same values for font styles and edges.
        let style = Captions::FontStyle().unwrap_or_default().0;
        let edge = Captions::FontEffect().unwrap_or_default().0;
        Properties {
            background: argb(
                Captions::BackgroundColor().unwrap_or_default(),
                Captions::ComputedBackgroundColor,
                Captions::BackgroundOpacity().unwrap_or_default(),
                defaults.background,
            ),
            text: argb(
                Captions::FontColor().unwrap_or_default(),
                Captions::ComputedFontColor,
                Captions::FontOpacity().unwrap_or_default(),
                defaults.text,
            ),
            window: argb(
                Captions::RegionColor().unwrap_or_default(),
                Captions::ComputedRegionColor,
                Captions::RegionOpacity().unwrap_or_default(),
                defaults.window,
            ),
            style: if (0..=7).contains(&style) {
                style as u32
            } else {
                defaults.style
            },
            edge: if (0..=5).contains(&edge) {
                edge as u32
            } else {
                defaults.edge
            },
            scale: font_scale(Captions::FontSize().unwrap_or_default()),
            // SAFETY: GetSystemMetrics takes only the identifier of the requested metric.
            enabled: u8::from(unsafe { GetSystemMetrics(SM_SHOWSOUNDS) } != 0),
            ..defaults
        }
    }

    pub(super) fn contrast() -> u32 {
        static CONTRAST: Mutex<Option<(Instant, u32)>> = Mutex::new(None);
        cached(&CONTRAST, read_contrast)
    }

    fn read_contrast() -> u32 {
        if !AccessibilitySettings::new()
            .and_then(|s| s.HighContrast())
            .unwrap_or(false)
        {
            return 0;
        }
        let background = UISettings::new().and_then(|s| s.UIElementColor(UIElementType::Window));
        match background {
            Ok(c) => contrast_for_background(c.R, c.G, c.B),
            Err(_) => 3, // Active, but the palette is unknown.
        }
    }

    fn contrast_for_background(r: u8, g: u8, b: u8) -> u32 {
        // Windows UI's perceived brightness weights green most heavily.
        let brightness = 5 * u32::from(g) + 2 * u32::from(r) + u32::from(b);
        if brightness >= 8 * 128 { 2 } else { 1 }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn repeated_polls_reuse_the_value_until_refresh() {
            let mut cache = Mutex::new(None);
            assert_eq!(cached(&cache, || 1), 1);
            assert_eq!(cached(&cache, || panic!("unexpected refresh")), 1);
            *cache.get_mut().unwrap() = Some((Instant::now() - REFRESH, 1));
            assert_eq!(cached(&cache, || 2), 2);
        }

        #[test]
        fn independent_color_opacity_and_size_preferences() {
            let defaults = Properties::default();
            let computed = || {
                Ok(windows::UI::Color {
                    A: 255,
                    R: 23,
                    G: 45,
                    B: 67,
                })
            };
            assert_eq!(
                argb(Color::Cyan, computed, Opacity::Default, defaults.background),
                [255, 23, 45, 67]
            );
            assert_eq!(
                argb(
                    Color::Default,
                    computed,
                    Opacity::TwentyFivePercent,
                    defaults.text
                ),
                [64, 255, 255, 255]
            );
            assert_eq!(
                argb(Color::Red, computed, Opacity::ZeroPercent, defaults.window),
                [0, 23, 45, 67]
            );
            assert_eq!(
                [
                    Size::FiftyPercent,
                    Size::Default,
                    Size::OneHundredFiftyPercent,
                    Size::TwoHundredPercent
                ]
                .map(font_scale),
                [0.5, 1.0, 1.5, 2.0]
            );
        }

        #[test]
        fn distinguishes_dark_and_light_contrast_palettes() {
            assert_eq!(contrast_for_background(15, 20, 60), 1);
            assert_eq!(contrast_for_background(255, 241, 211), 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::{speech::OBJECT, xasync::E_INVALIDARG, xgameruntime::method};

    #[test]
    fn missing_preferences_initialize_the_entire_output() {
        assert_eq!(size_of::<Properties>(), 28);
        let mut words = [0xa5a5_a5a5_u32; 7];
        assert_eq!(
            Properties::default().write(words.as_mut_ptr().cast(), FOLLOW_WINDOWS),
            S_OK
        );
        assert_eq!(words, [255, u32::MAX, 0, 0, 0, 1.0_f32.to_bits(), 0]);
    }

    #[test]
    fn title_can_enable_and_disable_captions_through_the_provider() {
        let get = method::<extern "system" fn(This, *mut Properties) -> HRESULT>(&OBJECT, 0x18);
        let set = method::<extern "system" fn(This, u8) -> HRESULT>(&OBJECT, 0x20);
        let contrast = method::<extern "system" fn(This, *mut u32) -> HRESULT>(&OBJECT, 0x28);
        assert_eq!(get(&OBJECT, std::ptr::null_mut()), E_INVALIDARG);
        assert_eq!(contrast(&OBJECT, std::ptr::null_mut()), E_INVALIDARG);
        let previous = ENABLED.load(Ordering::Relaxed);
        for (choice, expected) in [(0, 0), (2, 1), (0, 0), (0xff, 1), (1, 1), (0, 0)] {
            let mut result = Properties::default();
            assert_eq!(set(&OBJECT, choice), S_OK);
            assert_eq!(get(&OBJECT, &mut result), S_OK);
            assert_eq!(result.enabled, expected);
        }
        ENABLED.store(previous, Ordering::Relaxed);
        let mut mode = u32::MAX;
        assert_eq!(contrast(&OBJECT, &mut mode), S_OK);
        assert!(mode <= 3);
    }
}
