//! "My Settings" that can be pushed to players with a load-settings command.
//! Port of `org.deepsymmetry.beatlink.PlayerSettings`, expressed as a table so user
//! interfaces can be generated from it.

use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Describes one adjustable player setting.
#[derive(Debug, Clone, Copy)]
pub struct SettingSpec {
    /// Key used when saving settings.
    pub key: &'static str,
    /// Human-oriented label.
    pub label: &'static str,
    /// Address of the byte in the assembled packet.
    pub address: usize,
    /// The choices: display text and protocol value.
    pub choices: &'static [(&'static str, u8)],
    /// Index of the default choice.
    pub default: usize,
}

const TOGGLE: &[(&str, u8)] = &[("Off", 0x80), ("On", 0x81)];
const LCD: &[(&str, u8)] = &[("1", 0x81), ("2", 0x82), ("3", 0x83), ("4", 0x84), ("5", 0x85)];
const AUTO_CUE_LEVEL: &[(&str, u8)] = &[
    ("-36 dB", 0x80),
    ("-42 dB", 0x81),
    ("-48 dB", 0x82),
    ("-54 dB", 0x83),
    ("-60 dB", 0x84),
    ("-66 dB", 0x85),
    ("-72 dB", 0x86),
    ("-78 dB", 0x87),
    ("Memory", 0x88),
];
const LANGUAGE: &[(&str, u8)] = &[
    ("English", 0x81),
    ("French", 0x82),
    ("German", 0x83),
    ("Italian", 0x84),
    ("Dutch", 0x85),
    ("Spanish", 0x86),
    ("Russian", 0x87),
    ("Korean", 0x88),
    ("Chinese (simplified)", 0x89),
    ("Chinese (traditional)", 0x8a),
    ("Japanese", 0x8b),
    ("Portuguese", 0x8c),
    ("Swedish", 0x8d),
    ("Czech", 0x8e),
    ("Magyar", 0x8f),
    ("Danish", 0x90),
    ("Greek", 0x91),
    ("Turkish", 0x92),
];
const ILLUMINATION: &[(&str, u8)] = &[("0 (Off)", 0x80), ("1 (Dark)", 0x81), ("2 (Bright)", 0x82)];
const PLAY_MODE: &[(&str, u8)] = &[("Continue", 0x80), ("Single", 0x81)];
const QUANTIZE: &[(&str, u8)] = &[("Beat", 0x80), ("Half Beat", 0x81), ("Quarter Beat", 0x82), ("Eighth Beat", 0x83)];
const AUTO_LOAD: &[(&str, u8)] = &[("Off", 0x80), ("On", 0x81), ("rekordbox", 0x82)];
const TIME_DISPLAY: &[(&str, u8)] = &[("Elapsed", 0x80), ("Remaining", 0x81)];
const JOG_MODE: &[(&str, u8)] = &[("CDJ", 0x80), ("Vinyl", 0x81)];
const TEMPO_RANGE: &[(&str, u8)] = &[("±6", 0x80), ("±10", 0x81), ("±16", 0x82), ("Wide", 0x83)];
const PHASE_METER: &[(&str, u8)] = &[("Type 1", 0x80), ("Type 2", 0x81)];
const VINYL_SPEED: &[(&str, u8)] = &[("Touch and Release", 0x80), ("Touch", 0x81), ("Release", 0x82)];
const JOG_DISPLAY: &[(&str, u8)] = &[("Auto", 0x80), ("Info", 0x81), ("Simple", 0x82), ("Artwork", 0x83)];
const PAD_BRIGHTNESS: &[(&str, u8)] = &[("1", 0x81), ("2", 0x82), ("3", 0x83), ("4", 0x84)];

/// Every setting, in the order the original Settings Loader presents them.
pub const SETTINGS: &[SettingSpec] = &[
    SettingSpec { key: "on-air-display", label: "On Air Display", address: 0x2c, choices: TOGGLE, default: 1 },
    SettingSpec { key: "lcd-brightness", label: "LCD Brightness", address: 0x2d, choices: LCD, default: 2 },
    SettingSpec { key: "quantize", label: "Quantize", address: 0x2e, choices: TOGGLE, default: 1 },
    SettingSpec { key: "auto-cue-level", label: "Auto Cue Level", address: 0x2f, choices: AUTO_CUE_LEVEL, default: 8 },
    SettingSpec { key: "language", label: "Language", address: 0x30, choices: LANGUAGE, default: 0 },
    SettingSpec { key: "jog-ring-illumination", label: "Jog Ring Brightness", address: 0x32, choices: ILLUMINATION, default: 2 },
    SettingSpec { key: "jog-ring-indicator", label: "Jog Ring Indicator", address: 0x33, choices: TOGGLE, default: 1 },
    SettingSpec { key: "slip-flashing", label: "Slip Flashing", address: 0x34, choices: TOGGLE, default: 1 },
    SettingSpec { key: "disc-slot-illumination", label: "Disc Slot Illumination", address: 0x38, choices: ILLUMINATION, default: 1 },
    SettingSpec { key: "eject-load-lock", label: "Eject/Load Lock", address: 0x39, choices: TOGGLE, default: 1 },
    SettingSpec { key: "sync", label: "Sync", address: 0x3a, choices: TOGGLE, default: 1 },
    SettingSpec { key: "auto-play-mode", label: "Play Mode / Auto Play", address: 0x3b, choices: PLAY_MODE, default: 1 },
    SettingSpec { key: "quantize-beat-value", label: "Quantize Beat Value", address: 0x3c, choices: QUANTIZE, default: 0 },
    SettingSpec { key: "auto-load-mode", label: "Hot Cue Auto Load", address: 0x3d, choices: AUTO_LOAD, default: 2 },
    SettingSpec { key: "hot-cue-color", label: "Hot Cue Color", address: 0x3e, choices: TOGGLE, default: 1 },
    SettingSpec { key: "needle-lock", label: "Needle Lock", address: 0x41, choices: TOGGLE, default: 1 },
    SettingSpec { key: "time-display-mode", label: "Time Mode", address: 0x44, choices: TIME_DISPLAY, default: 1 },
    SettingSpec { key: "jog-mode", label: "Jog Mode", address: 0x45, choices: JOG_MODE, default: 1 },
    SettingSpec { key: "auto-cue", label: "Auto Cue", address: 0x46, choices: TOGGLE, default: 1 },
    SettingSpec { key: "master-tempo", label: "Master Tempo", address: 0x47, choices: TOGGLE, default: 1 },
    SettingSpec { key: "tempo-range", label: "Tempo Range", address: 0x48, choices: TEMPO_RANGE, default: 0 },
    SettingSpec { key: "phase-meter-type", label: "Phase Meter", address: 0x49, choices: PHASE_METER, default: 0 },
    SettingSpec { key: "vinyl-speed-adjust", label: "Vinyl Speed Adjust", address: 0x4c, choices: VINYL_SPEED, default: 0 },
    SettingSpec { key: "jog-wheel-display", label: "Jog Display Mode", address: 0x4d, choices: JOG_DISPLAY, default: 0 },
    SettingSpec { key: "pad-button-brightness", label: "Pad/Button Brightness", address: 0x4e, choices: PAD_BRIGHTNESS, default: 2 },
    SettingSpec { key: "jog-wheel-lcd-brightness", label: "Jog LCD Brightness", address: 0x4f, choices: LCD, default: 2 },
];

const LOAD_SETTINGS_PAYLOAD: [u8; 0x55] = {
    let mut p = [0u8; 0x55];
    p[0] = 0x02;
    p[1] = 0x0d;
    p[2] = 0x0d;
    p[4] = 0x50;
    p[5] = 0x12;
    p[6] = 0x34;
    p[7] = 0x56;
    p[8] = 0x78;
    p[0x0b] = 0x03;
    p[0x11] = 0x01;
    p[0x15] = 0x01;
    p[0x16] = 0x01;
    p[0x17] = 0x01;
    p
};

/// A complete set of player settings, keyed by [`SettingSpec::key`] with the
/// display text of the chosen value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerSettings {
    pub values: BTreeMap<String, String>,
}

impl Default for PlayerSettings {
    fn default() -> Self {
        PlayerSettings {
            values: SETTINGS.iter().map(|s| (s.key.to_string(), s.choices[s.default].0.to_string())).collect(),
        }
    }
}

impl PlayerSettings {
    /// Set a value by key and display text. Returns false if either is unknown.
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        match SETTINGS.iter().find(|s| s.key == key) {
            Some(spec) if spec.choices.iter().any(|(label, _)| *label == value) => {
                self.values.insert(key.to_string(), value.to_string());
                true
            }
            _ => false,
        }
    }

    /// The protocol byte for a setting, using the default when unset or invalid.
    pub fn protocol_value(&self, spec: &SettingSpec) -> u8 {
        self.values
            .get(spec.key)
            .and_then(|v| spec.choices.iter().find(|(label, _)| label == v))
            .map(|(_, b)| *b)
            .unwrap_or(spec.choices[spec.default].1)
    }

    /// Build the payload for a load-settings command.
    pub fn build_payload(&self, our_number: u8, target_number: u8) -> [u8; 0x55] {
        let mut payload = LOAD_SETTINGS_PAYLOAD;
        util::set_payload_byte(&mut payload, 0x20, our_number);
        util::set_payload_byte(&mut payload, 0x21, target_number);
        for spec in SETTINGS {
            util::set_payload_byte(&mut payload, spec.address, self.protocol_value(spec));
        }
        payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_payload() {
        let mut s = PlayerSettings::default();
        assert!(s.set("jog-mode", "CDJ"));
        assert!(!s.set("jog-mode", "Banana"));
        let p = s.build_payload(5, 2);
        assert_eq!(p[0x20 - 0x1f], 5);
        assert_eq!(p[0x21 - 0x1f], 2);
        assert_eq!(p[0x45 - 0x1f], 0x80);
        assert_eq!(p[0x2d - 0x1f], 0x83);
        assert_eq!(0x1f + p.len(), 0x74);
    }
}
