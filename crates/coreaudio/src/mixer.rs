//! The mixer unit: many inputs in, one stream out.
//!
//! A game that places sounds in space builds a graph whose middle is a mixer -
//! `kAudioUnitSubType_3DMixer` and its relatives - with one input element per
//! voice. Each element has a format and a render callback of its own, and the
//! mixer is pulled by whatever it is connected to, which is the output unit.
//!
//! What the 3D mixer does that a plain one does not is *position*: azimuth,
//! elevation and distance per element, which Apple's turns into a filter and a
//! pan. This one pans and attenuates and does no filtering, because a game that
//! gets the direction and the loudness right sounds placed, and one that gets
//! silence sounds broken.

use crate::render::Callback;
use crate::stream::Format;

/// `kAudioUnitType_Mixer`.
pub const TYPE_MIXER: u32 = u32::from_be_bytes(*b"aumx");

/// The mixer subtypes this crate answers to. All of them mix; the 3D ones also
/// place, which is what [`Element::pan`] is.
pub const SUBTYPES: &[u32] = &[
    // `kAudioUnitSubType_3DMixer`, the one a game built against the older
    // headers asks for, and `kAudioUnitSubType_SpatialMixer`, its replacement.
    u32::from_be_bytes(*b"3dmx"),
    u32::from_be_bytes(*b"smxr"),
    // The two that only mix.
    u32::from_be_bytes(*b"mcmx"),
    u32::from_be_bytes(*b"mxmx"),
];

/// `k3DMixerParam_*` and `kMultiChannelMixerParam_*`, which overlap in id and
/// not in meaning - so they are read against the unit's own subtype.
pub mod parameter {
    /// `k3DMixerParam_Azimuth`: degrees clockwise from straight ahead.
    pub const AZIMUTH: u32 = 0;
    /// `k3DMixerParam_Elevation`: degrees above the horizontal.
    pub const ELEVATION: u32 = 1;
    /// `k3DMixerParam_Distance`, in the unit's own distance scale.
    pub const DISTANCE: u32 = 2;
    /// `k3DMixerParam_Gain`, in decibels.
    pub const GAIN: u32 = 3;
    /// `k3DMixerParam_PlaybackRate`, a multiplier.
    pub const PLAYBACK_RATE: u32 = 4;
    /// `k3DMixerParam_Enable`, nonzero to hear the element at all.
    pub const ENABLE: u32 = 5;
}

/// One input element of a mixer.
#[derive(Clone, Copy)]
pub struct Element {
    /// The format the element's callback fills, which need not be the mixer's
    /// own output format.
    pub format: Format,
    pub callback: Option<Callback>,
    /// The unit connected to this element by a graph, as its handle, or zero.
    pub source: usize,
    pub azimuth: f32,
    pub elevation: f32,
    pub distance: f32,
    /// Decibels, as the parameter is given.
    pub gain: f32,
    pub enabled: bool,
    /// Frames pulled from this element, which is the `mSampleTime` its callback
    /// is given. Per element rather than per mixer: a callback that renders
    /// from the timestamp - and a game streaming music does - is handed
    /// nonsense if the count jumps by the number of *other* voices each buffer.
    pub rendered: u64,
}

impl Default for Element {
    fn default() -> Self {
        Self {
            format: Format::default(),
            callback: None,
            source: 0,
            azimuth: 0.0,
            elevation: 0.0,
            distance: 0.0,
            gain: 0.0,
            enabled: true,
            rendered: 0,
        }
    }
}

// SAFETY: `callback` is a host-callable pointer this runtime was handed, and is
// only ever called from the audio thread with the unit's lock held.
unsafe impl Send for Element {}

impl Element {
    /// The linear gain this element's `gain` parameter asks for.
    ///
    /// Decibels, as `k3DMixerParam_Gain` is defined, and clamped: a game that
    /// has not set it yet leaves zero, which is unity, and one that mutes a
    /// voice writes a large negative number rather than negative infinity.
    pub fn amplitude(&self) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        if self.gain <= -120.0 {
            return 0.0;
        }
        10f32.powf(self.gain / 20.0)
    }

    /// Left and right weights for this element's azimuth.
    ///
    /// Constant-power panning: the two weights are the cosine and sine of an
    /// angle that runs a quarter turn from hard left to hard right, so a source
    /// swung across the front keeps its loudness instead of dipping in the
    /// middle the way a linear pan does.
    ///
    /// Azimuth is degrees clockwise from ahead, so -90 is left and +90 right.
    /// Behind the listener it folds back onto the front, which is what a pan
    /// with no filtering can express.
    pub fn pan(&self) -> (f32, f32) {
        let wrapped = self.azimuth.rem_euclid(360.0);
        let signed = match wrapped > 180.0 {
            true => wrapped - 360.0,
            false => wrapped,
        };
        // Fold the back hemisphere onto the front: +150 degrees is as far right
        // as +30 is, and there is no cue here that would tell them apart.
        let folded = match signed.abs() > 90.0 {
            true => signed.signum() * (180.0 - signed.abs()),
            false => signed,
        };
        let angle = (folded / 90.0).clamp(-1.0, 1.0) * std::f32::consts::FRAC_PI_4;
        // Azimuth grows clockwise, so a positive angle moves *away* from the
        // left channel and toward the right.
        let centre = std::f32::consts::FRAC_PI_4;
        ((centre + angle).cos(), (centre - angle).cos())
    }
}

/// The most input elements a mixer grows to on its own.
///
/// A guest is free to set `kAudioUnitProperty_ElementCount`, and Apple's mixer
/// also has a default bus count that a guest may simply use without setting
/// one. This is that default: reaching for bus seven on a mixer nobody sized is
/// how a game with eight voices is written, and answering
/// `kAudioUnitErr_InvalidElement` to it is how that game ends up silent.
pub const DEFAULT_ELEMENTS: usize = 64;

/// The element at `index`, growing the mixer to reach it if it is within the
/// default count.
pub fn element_mut(elements: &mut Vec<Element>, index: u32) -> Option<&mut Element> {
    let index = index as usize;
    if index >= elements.len() && index < DEFAULT_ELEMENTS {
        elements.resize(index + 1, Element::default());
    }
    elements.get_mut(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unity when nothing has set a gain, which is the state a mixer's elements
    /// start in and the one a game that never touches gain stays in.
    #[test]
    fn a_gain_of_zero_decibels_is_unity() {
        let element = Element::default();
        assert!((element.amplitude() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn six_decibels_down_is_about_half() {
        let element = Element {
            gain: -6.0206,
            ..Element::default()
        };
        assert!((element.amplitude() - 0.5).abs() < 1e-3);
    }

    /// A disabled element is silent whatever its gain says.
    #[test]
    fn a_disabled_element_is_silent() {
        let element = Element {
            enabled: false,
            gain: 12.0,
            ..Element::default()
        };
        assert_eq!(element.amplitude(), 0.0);
    }

    /// Straight ahead is equal and, being constant-power, is not one-half each.
    #[test]
    fn straight_ahead_is_centred() {
        let (left, right) = Element::default().pan();
        assert!((left - right).abs() < 1e-6);
        assert!((left - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    /// And the ends are hard over.
    #[test]
    fn a_quarter_turn_each_way_is_hard_over() {
        let left = Element {
            azimuth: -90.0,
            ..Element::default()
        }
        .pan();
        assert!((left.0 - 1.0).abs() < 1e-6, "{left:?}");
        assert!(left.1.abs() < 1e-6, "{left:?}");

        let right = Element {
            azimuth: 90.0,
            ..Element::default()
        }
        .pan();
        assert!(right.0.abs() < 1e-6, "{right:?}");
        assert!((right.1 - 1.0).abs() < 1e-6, "{right:?}");
    }

    /// Panning keeps its power across the sweep, which is the whole reason for
    /// the trigonometry.
    #[test]
    fn panning_keeps_its_power() {
        for degrees in [-90.0, -45.0, 0.0, 30.0, 90.0] {
            let (left, right) = Element {
                azimuth: degrees,
                ..Element::default()
            }
            .pan();
            let power = left * left + right * right;
            assert!((power - 1.0).abs() < 1e-5, "{degrees}: {power}");
        }
    }

    /// Behind folds onto the front rather than wrapping to the wrong side.
    #[test]
    fn the_back_hemisphere_folds_forward() {
        let behind_right = Element {
            azimuth: 150.0,
            ..Element::default()
        }
        .pan();
        let ahead_right = Element {
            azimuth: 30.0,
            ..Element::default()
        }
        .pan();
        assert!((behind_right.0 - ahead_right.0).abs() < 1e-6);
        assert!((behind_right.1 - ahead_right.1).abs() < 1e-6);
    }
}
