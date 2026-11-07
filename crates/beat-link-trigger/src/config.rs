//! The saved configuration: triggers, global expressions, and application settings.
//! Saved as JSON (`.bltj`); the original EDN `.blt` files can be imported with
//! [`crate::legacy`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The file extension for trigger files written by this version.
pub const TRIGGER_FILE_EXTENSION: &str = "bltj";

/// Which player(s) a trigger watches. Serialized as the original numbers:
/// -1 is Any Player, 0 is the Master Player, otherwise a player number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "i8", into = "i8")]
pub enum PlayerChoice {
    Any,
    Master,
    Player(u8),
}

impl From<i8> for PlayerChoice {
    fn from(n: i8) -> Self {
        match n {
            n if n < 0 => PlayerChoice::Any,
            0 => PlayerChoice::Master,
            n => PlayerChoice::Player(n as u8),
        }
    }
}

impl From<PlayerChoice> for i8 {
    fn from(p: PlayerChoice) -> i8 {
        match p {
            PlayerChoice::Any => -1,
            PlayerChoice::Master => 0,
            PlayerChoice::Player(n) => n as i8,
        }
    }
}

impl std::fmt::Display for PlayerChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlayerChoice::Any => write!(f, "Any Player"),
            PlayerChoice::Master => write!(f, "Master Player"),
            PlayerChoice::Player(n) => write!(f, "Player {n}"),
        }
    }
}

impl PlayerChoice {
    /// The choices offered in the Watch menu.
    pub fn choices() -> Vec<PlayerChoice> {
        let mut v = vec![PlayerChoice::Any, PlayerChoice::Master];
        v.extend((1..=6).map(PlayerChoice::Player));
        v
    }
}

/// What a trigger sends when it activates and deactivates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum MessageKind {
    #[default]
    Note,
    #[serde(rename = "CC")]
    Cc,
    Clock,
    Link,
    Custom,
}

impl MessageKind {
    pub const ALL: [MessageKind; 5] = [MessageKind::Note, MessageKind::Cc, MessageKind::Clock, MessageKind::Link, MessageKind::Custom];
    pub fn label(self) -> &'static str {
        match self {
            MessageKind::Note => "Note",
            MessageKind::Cc => "CC",
            MessageKind::Clock => "Clock",
            MessageKind::Link => "Link",
            MessageKind::Custom => "Custom",
        }
    }
    pub fn from_label(s: &str) -> Option<Self> {
        MessageKind::ALL.iter().copied().find(|m| m.label() == s)
    }
}

/// When a trigger is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum EnabledMode {
    #[default]
    Never,
    #[serde(rename = "On-Air")]
    OnAir,
    Custom,
    Always,
}

impl EnabledMode {
    pub const ALL: [EnabledMode; 4] = [EnabledMode::Never, EnabledMode::OnAir, EnabledMode::Custom, EnabledMode::Always];
    pub fn label(self) -> &'static str {
        match self {
            EnabledMode::Never => "Never",
            EnabledMode::OnAir => "On-Air",
            EnabledMode::Custom => "Custom",
            EnabledMode::Always => "Always",
        }
    }
    pub fn from_label(s: &str) -> Option<Self> {
        EnabledMode::ALL.iter().copied().find(|m| m.label() == s)
    }
}

/// Whether a clock trigger sends Start or Continue when activating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ClockStart {
    #[default]
    Start,
    Continue,
}

fn default_note() -> u8 {
    127
}
fn default_channel() -> u8 {
    1
}
fn yes() -> bool {
    true
}
fn new_uuid() -> String {
    // A random-enough identifier without pulling in a UUID crate.
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let a = h.finish();
    let mut h2 = RandomState::new().build_hasher();
    h2.write_u64(a);
    format!("{:016x}{:016x}", a, h2.finish())
}

/// One row in the Triggers window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TriggerConfig {
    #[serde(default = "new_uuid")]
    pub uuid: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default = "default_players")]
    pub players: PlayerChoice,
    /// The MIDI output name (None if no output chosen).
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub message: MessageKind,
    #[serde(default = "default_note")]
    pub note: u8,
    #[serde(default = "default_channel")]
    pub channel: u8,
    #[serde(default)]
    pub enabled: EnabledMode,
    /// Clock: send Start/Continue when activated.
    #[serde(default = "yes")]
    pub send: bool,
    #[serde(default)]
    pub start: ClockStart,
    /// Clock: send Stop when deactivated.
    #[serde(default = "yes")]
    pub stop: bool,
    /// Link: align bars, not just beats.
    #[serde(default = "yes")]
    pub bar: bool,
    /// Link: start and stop the Link transport.
    #[serde(default)]
    pub start_stop: bool,
    /// Rhai expressions by kind (see [`crate::expressions::TRIGGER_EXPRESSIONS`]).
    #[serde(default)]
    pub expressions: BTreeMap<String, String>,
    /// Clojure expressions imported from a Java-era file, kept for reference.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub legacy_expressions: BTreeMap<String, String>,
}

fn default_players() -> PlayerChoice {
    PlayerChoice::Any
}

impl Default for TriggerConfig {
    fn default() -> Self {
        TriggerConfig {
            uuid: new_uuid(),
            comment: String::new(),
            players: PlayerChoice::Any,
            output: None,
            message: MessageKind::Note,
            note: 127,
            channel: 1,
            enabled: EnabledMode::Never,
            send: true,
            start: ClockStart::Start,
            stop: true,
            bar: true,
            start_stop: false,
            expressions: BTreeMap::new(),
            legacy_expressions: BTreeMap::new(),
        }
    }
}

impl TriggerConfig {
    /// Give the trigger a fresh identity (after duplication).
    pub fn with_new_uuid(mut self) -> Self {
        self.uuid = new_uuid();
        self
    }
    pub fn expression(&self, kind: &str) -> &str {
        self.expressions.get(kind).map(String::as_str).unwrap_or("")
    }
}

/// Carabiner (Ableton Link bridge) synchronization modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SyncMode {
    #[default]
    Off,
    /// Link tempo is controlled by triggers set to send Link messages ("Triggers").
    Manual,
    /// Link follows the Pro DJ Link tempo master.
    Passive,
    /// Bidirectional: the Virtual CDJ can become tempo master, following Link.
    Full,
}

impl SyncMode {
    pub const ALL: [SyncMode; 4] = [SyncMode::Off, SyncMode::Manual, SyncMode::Passive, SyncMode::Full];
    pub fn label(self) -> &'static str {
        match self {
            SyncMode::Off => "Off",
            SyncMode::Manual => "Triggers",
            SyncMode::Passive => "Passive",
            SyncMode::Full => "Full",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct CarabinerConfig {
    pub host: String,
    pub port: u16,
    pub latency_ms: i64,
    pub sync_mode: SyncMode,
    pub bars: bool,
}

impl Default for CarabinerConfig {
    fn default() -> Self {
        CarabinerConfig { host: "127.0.0.1".into(), port: 17000, latency_ms: 1, sync_mode: SyncMode::Off, bars: false }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct OverlayConfig {
    pub enabled: bool,
    pub port: u16,
    /// A custom template; the built-in one is used when absent.
    pub template: Option<PathBuf>,
    /// Folder of extra static files served under `/public/`.
    pub public_folder: Option<PathBuf>,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        OverlayConfig { enabled: false, port: 17081, template: None, public_folder: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct PlaylistConfig {
    pub enabled: bool,
    pub file: Option<PathBuf>,
    /// Seconds a track must be on the air before it is written.
    pub min_play_seconds: u64,
    /// Only record tracks played by the tempo master.
    pub master_only: bool,
    /// Append to an existing file instead of replacing it.
    pub append: bool,
}

impl Default for PlaylistConfig {
    fn default() -> Self {
        PlaylistConfig { enabled: false, file: None, min_play_seconds: 10, master_only: false, append: false }
    }
}

/// The whole saved configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Settings {
    /// The version of the program that wrote the file.
    pub beat_link_trigger_version: String,
    pub triggers: Vec<TriggerConfig>,
    /// Global expressions by kind (see [`crate::expressions::GLOBAL_EXPRESSIONS`]).
    pub expressions: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub legacy_expressions: BTreeMap<String, String>,
    /// Pose as a real player (1-4) and send status packets.
    pub send_status: bool,
    /// The name the virtual CDJ announces.
    pub device_name: String,
    pub carabiner: CarabinerConfig,
    pub overlay: OverlayConfig,
    pub playlist: PlaylistConfig,
    /// Show files to open at startup.
    pub shows: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            beat_link_trigger_version: crate::VERSION.into(),
            triggers: vec![TriggerConfig::default()],
            expressions: BTreeMap::new(),
            legacy_expressions: BTreeMap::new(),
            send_status: false,
            device_name: "BeatLinkTrigger".into(),
            carabiner: CarabinerConfig::default(),
            overlay: OverlayConfig::default(),
            playlist: PlaylistConfig::default(),
            shows: Vec::new(),
        }
    }
}

impl Settings {
    /// Read settings from a file; `.blt` files from the Java version are imported.
    pub fn load(path: &Path) -> io::Result<Settings> {
        if path.extension().map(|e| e == "blt").unwrap_or(false) {
            return crate::legacy::import_blt(path);
        }
        let text = fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Write settings to a file, atomically.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut copy = self.clone();
        copy.beat_link_trigger_version = crate::VERSION.into();
        let text = serde_json::to_string_pretty(&copy).map_err(io::Error::other)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)
    }

    pub fn global_expression(&self, kind: &str) -> &str {
        self.expressions.get(kind).map(String::as_str).unwrap_or("")
    }
}

/// Where the application keeps its preferences and logs.
pub fn app_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("org", "Deep Symmetry", "Beat Link Trigger")
}

/// The default preferences file (the equivalent of the Java preferences node).
pub fn default_preferences_path() -> PathBuf {
    app_dirs()
        .map(|d| d.config_dir().join("preferences.bltj"))
        .unwrap_or_else(|| PathBuf::from("beat-link-trigger-preferences.bltj"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_json() {
        let mut s = Settings::default();
        s.triggers[0].players = PlayerChoice::Player(3);
        s.triggers[0].message = MessageKind::Cc;
        s.triggers[0].enabled = EnabledMode::OnAir;
        s.triggers[0].expressions.insert("activation".into(), "log_info(\"hi\")".into());
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"players\":3"));
        assert!(text.contains("\"message\":\"CC\""));
        assert!(text.contains("\"enabled\":\"On-Air\""));
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn tolerant_of_missing_fields() {
        let s: Settings = serde_json::from_str(r#"{"triggers":[{"comment":"x"}]}"#).unwrap();
        assert_eq!(s.triggers[0].note, 127);
        assert_eq!(s.triggers[0].players, PlayerChoice::Any);
        assert_eq!(s.carabiner.port, 17000);
    }
}
