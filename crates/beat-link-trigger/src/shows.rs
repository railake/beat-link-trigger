//! Shows: per-track automation keyed by track signature, with beat-ranged cues.
//! Port of the runtime parts of `show.clj` and `show_cues.clj`.
//!
//! A show contains tracks (identified by the signature of their metadata, waveform
//! and beat grid, so they are recognized wherever they are played from). Each track
//! can send MIDI or run expressions when it is loaded, starts or stops playing, and
//! has cues spanning ranges of beats that report being entered, exited, started (on
//! the cue's first beat or late) and ended.

use crate::config::TriggerConfig;
use crate::expressions::{Compiled, Context, ExpressionEngine, StatusValue, Store};
use crate::legacy::{parse_edn, Edn};
use crate::outputs::Outputs;
use beat_link::packets::CdjStatus;
use beat_link::TrackData;
use parking_lot::{Mutex, RwLock};
use rhai::{Dynamic, Map};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// File extension for shows written by this version.
pub const SHOW_FILE_EXTENSION: &str = "blsj";

/// When a show (or track) is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ShowEnabled {
    /// Tracks only: use the show's setting.
    Default,
    Never,
    #[serde(rename = "On-Air")]
    OnAir,
    Master,
    Custom,
    #[default]
    Always,
}

impl ShowEnabled {
    pub const SHOW_CHOICES: [ShowEnabled; 5] = [ShowEnabled::Never, ShowEnabled::OnAir, ShowEnabled::Master, ShowEnabled::Custom, ShowEnabled::Always];
    pub const TRACK_CHOICES: [ShowEnabled; 6] =
        [ShowEnabled::Default, ShowEnabled::Never, ShowEnabled::OnAir, ShowEnabled::Master, ShowEnabled::Custom, ShowEnabled::Always];
    pub fn label(self) -> &'static str {
        match self {
            ShowEnabled::Default => "Default",
            ShowEnabled::Never => "Never",
            ShowEnabled::OnAir => "On-Air",
            ShowEnabled::Master => "Master",
            ShowEnabled::Custom => "Custom",
            ShowEnabled::Always => "Always",
        }
    }
    pub fn from_label(s: &str) -> Option<Self> {
        Self::TRACK_CHOICES.iter().copied().find(|e| e.label() == s)
    }
}

/// What a track or cue event sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum EventMessage {
    #[default]
    None,
    Note,
    #[serde(rename = "CC")]
    Cc,
    Custom,
    /// Started-late only: do the same as started-on-beat.
    Same,
}

impl EventMessage {
    pub fn label(self) -> &'static str {
        match self {
            EventMessage::None => "None",
            EventMessage::Note => "Note",
            EventMessage::Cc => "CC",
            EventMessage::Custom => "Custom",
            EventMessage::Same => "Same",
        }
    }
    pub fn from_label(s: &str) -> Self {
        match s {
            "Note" => EventMessage::Note,
            "CC" => EventMessage::Cc,
            "Custom" => EventMessage::Custom,
            "Same" => EventMessage::Same,
            _ => EventMessage::None,
        }
    }
}

/// The MIDI details of an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventSpec {
    pub message: EventMessage,
    pub note: u8,
    pub channel: u8,
}

impl Default for EventSpec {
    fn default() -> Self {
        EventSpec { message: EventMessage::None, note: 127, channel: 1 }
    }
}

/// A cue: a range of beats within a track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct ShowCue {
    pub uuid: String,
    pub comment: String,
    /// First beat of the cue.
    pub start: i64,
    /// Beat at which the cue ends (exclusive).
    pub end: i64,
    pub hue: f64,
    pub entered: EventSpec,
    pub started_on_beat: EventSpec,
    pub started_late: EventSpec,
    pub expressions: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub legacy_expressions: BTreeMap<String, String>,
}

impl Default for ShowCue {
    fn default() -> Self {
        ShowCue {
            uuid: String::new(),
            comment: String::new(),
            start: 1,
            end: 2,
            hue: 0.0,
            entered: EventSpec { message: EventMessage::None, note: 1, channel: 1 },
            started_on_beat: EventSpec { message: EventMessage::None, note: 2, channel: 1 },
            started_late: EventSpec { message: EventMessage::Same, note: 3, channel: 1 },
            expressions: BTreeMap::new(),
            legacy_expressions: BTreeMap::new(),
        }
    }
}

/// Cue expression kinds.
pub const CUE_EXPRESSIONS: &[(&str, &str)] = &[
    ("entered", "Entered Expression"),
    ("started-on-beat", "Started On-Beat Expression"),
    ("started-late", "Started Late Expression"),
    ("beat", "Beat Expression"),
    ("tracked", "Tracked Update Expression"),
    ("ended", "Ended Expression"),
    ("exited", "Exited Expression"),
];

/// Track expression kinds.
pub const TRACK_EXPRESSIONS: &[(&str, &str)] = &[
    ("setup", "Setup Expression"),
    ("enabled", "Enabled Filter Expression"),
    ("loaded", "Loaded Expression"),
    ("playing", "Playing Expression"),
    ("beat", "Beat Expression"),
    ("tracked", "Tracked Update Expression"),
    ("stopped", "Stopped Expression"),
    ("unloaded", "Unloaded Expression"),
    ("shutdown", "Shutdown Expression"),
];

/// Summary metadata saved with a track so it can be shown without the player.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct ShowTrackMetadata {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub key: Option<String>,
    pub duration: u32,
    pub tempo: u32,
    pub comment: String,
}

/// A track in a show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct ShowTrack {
    pub signature: String,
    pub metadata: ShowTrackMetadata,
    pub enabled: ShowEnabled,
    pub midi_device: Option<String>,
    pub loaded: EventSpec,
    pub playing: EventSpec,
    pub cues: Vec<ShowCue>,
    pub expressions: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub legacy_expressions: BTreeMap<String, String>,
}

/// A show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Show {
    pub version: u32,
    pub enabled: ShowEnabled,
    pub tracks: BTreeMap<String, ShowTrack>,
    /// Show-level expressions: shared, setup, enabled (default), shutdown...
    pub expressions: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub legacy_expressions: BTreeMap<String, String>,
    /// Raw triggers that belong to the show.
    pub triggers: Vec<TriggerConfig>,
}

impl Default for Show {
    fn default() -> Self {
        Show { version: 3, enabled: ShowEnabled::Always, tracks: BTreeMap::new(), expressions: BTreeMap::new(), legacy_expressions: BTreeMap::new(), triggers: Vec::new() }
    }
}

fn stub(clojure: &str) -> String {
    let mut out = String::from("// Imported from the Java version; original Clojure below.\n");
    for line in clojure.lines() {
        out.push_str("// ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn edn_strings(e: Option<&Edn>) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    if let Some(Edn::Map(entries)) = e {
        for (k, v) in entries {
            if let (Edn::Keyword(k), Some(s)) = (k, v.as_str()) {
                if !s.trim().is_empty() {
                    m.insert(k.clone(), s.to_string());
                }
            }
        }
    }
    m
}

fn edn_event(e: Option<&Edn>, default: EventSpec) -> EventSpec {
    let Some(e) = e else { return default };
    EventSpec {
        message: e.get("message").and_then(Edn::as_str).map(EventMessage::from_label).unwrap_or(default.message),
        note: e.get("note").and_then(Edn::as_i64).map(|n| n.clamp(0, 127) as u8).unwrap_or(default.note),
        channel: e.get("channel").and_then(Edn::as_i64).map(|n| n.clamp(1, 16) as u8).unwrap_or(default.channel),
    }
}

fn edn_opt_string(e: Option<&Edn>) -> Option<String> {
    e.and_then(Edn::as_str).map(str::to_string)
}

impl Show {
    pub fn load(path: &Path) -> io::Result<Show> {
        if path.extension().map(|e| e == "bls").unwrap_or(false) {
            return import_bls(path);
        }
        let text = fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)
    }

    /// Add a track from data retrieved from a player.
    pub fn add_track(&mut self, data: &TrackData) -> Option<String> {
        let signature = data.signature.clone()?;
        let md = data.metadata.as_ref()?;
        let label = |i: &Option<beat_link::data::SearchableItem>| i.as_ref().map(|x| x.label.clone());
        self.tracks.entry(signature.clone()).or_insert_with(|| ShowTrack {
            signature: signature.clone(),
            metadata: ShowTrackMetadata {
                title: md.title.clone(),
                artist: label(&md.artist),
                album: label(&md.album),
                genre: label(&md.genre),
                key: label(&md.key),
                duration: md.duration,
                tempo: md.tempo,
                comment: md.comment.clone(),
            },
            enabled: ShowEnabled::Default,
            loaded: EventSpec { message: EventMessage::None, note: 126, channel: 1 },
            playing: EventSpec { message: EventMessage::None, note: 127, channel: 1 },
            ..Default::default()
        });
        Some(signature)
    }
}

/// Import a `.bls` show archive written by the Java version.
pub fn import_bls(path: &Path) -> io::Result<Show> {
    let file = fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file).map_err(io::Error::other)?;
    let mut read = |name: &str| -> io::Result<Option<String>> {
        match zip.by_name(name) {
            Ok(mut f) => {
                let mut s = String::new();
                f.read_to_string(&mut s)?;
                Ok(Some(s))
            }
            Err(zip::result::ZipError::FileNotFound) => Ok(None),
            Err(e) => Err(io::Error::other(e)),
        }
    };
    let contents = read("contents.edn")?.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing contents.edn"))?;
    let root = parse_edn(&contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut show = Show::default();
    show.enabled = root.get("enabled").and_then(Edn::as_str).and_then(ShowEnabled::from_label).unwrap_or(ShowEnabled::Always);
    show.legacy_expressions = edn_strings(root.get("expressions"));
    show.expressions = show.legacy_expressions.iter().map(|(k, v)| (k.clone(), stub(v))).collect();
    if let Some(triggers) = root.get("triggers").and_then(Edn::as_seq) {
        show.triggers = triggers.iter().map(crate::legacy::import_trigger).collect();
    }
    let names: Vec<String> = (0..zip.len()).filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string())).collect();
    let mut read = |name: &str| -> io::Result<Option<String>> {
        match zip.by_name(name) {
            Ok(mut f) => {
                let mut s = String::new();
                f.read_to_string(&mut s)?;
                Ok(Some(s))
            }
            Err(_) => Ok(None),
        }
    };
    for name in names {
        let Some(signature) = name.strip_prefix("tracks/").and_then(|r| r.strip_suffix("/contents.edn")) else { continue };
        let Some(text) = read(&name)? else { continue };
        let t = parse_edn(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let md_text = read(&format!("tracks/{signature}/metadata.edn"))?.unwrap_or_default();
        let md = parse_edn(&md_text).unwrap_or(Edn::Nil);
        let mut track = ShowTrack {
            signature: signature.to_string(),
            metadata: ShowTrackMetadata {
                title: md.get("title").and_then(Edn::as_str).unwrap_or("").to_string(),
                artist: edn_opt_string(md.get("artist")),
                album: edn_opt_string(md.get("album")),
                genre: edn_opt_string(md.get("genre")),
                key: edn_opt_string(md.get("key")),
                duration: md.get("duration").and_then(Edn::as_i64).unwrap_or(0) as u32,
                tempo: md.get("tempo").and_then(Edn::as_i64).unwrap_or(0) as u32,
                comment: md.get("comment").and_then(Edn::as_str).unwrap_or("").to_string(),
            },
            enabled: t.get("enabled").and_then(Edn::as_str).and_then(ShowEnabled::from_label).unwrap_or(ShowEnabled::Default),
            midi_device: t.get("midi-device").and_then(|d| d.get("full-name")).and_then(Edn::as_str).map(|s| s.trim_start_matches("CoreMIDI4J - ").to_string()),
            loaded: EventSpec {
                message: t.get("loaded-message").and_then(Edn::as_str).map(EventMessage::from_label).unwrap_or_default(),
                note: t.get("loaded-note").and_then(Edn::as_i64).unwrap_or(126) as u8,
                channel: t.get("loaded-channel").and_then(Edn::as_i64).unwrap_or(1) as u8,
            },
            playing: EventSpec {
                message: t.get("playing-message").and_then(Edn::as_str).map(EventMessage::from_label).unwrap_or_default(),
                note: t.get("playing-note").and_then(Edn::as_i64).unwrap_or(127) as u8,
                channel: t.get("playing-channel").and_then(Edn::as_i64).unwrap_or(1) as u8,
            },
            ..Default::default()
        };
        track.legacy_expressions = edn_strings(t.get("expressions"));
        track.expressions = track.legacy_expressions.iter().map(|(k, v)| (k.clone(), stub(v))).collect();
        if let Some(Edn::Map(cues)) = t.get("cues").and_then(|c| c.get("cues")) {
            for (_, c) in cues {
                let legacy = edn_strings(c.get("expressions"));
                let events = c.get("events");
                track.cues.push(ShowCue {
                    uuid: match c.get("uuid") {
                        Some(Edn::Tagged(_, inner)) => inner.as_str().unwrap_or("").to_string(),
                        Some(v) => v.as_str().unwrap_or("").to_string(),
                        None => String::new(),
                    },
                    comment: c.get("comment").and_then(Edn::as_str).unwrap_or("").to_string(),
                    start: c.get("start").and_then(Edn::as_i64).unwrap_or(1),
                    end: c.get("end").and_then(Edn::as_i64).unwrap_or(2),
                    hue: match c.get("hue") {
                        Some(Edn::Float(f)) => *f,
                        Some(Edn::Int(i)) => *i as f64,
                        _ => 0.0,
                    },
                    entered: edn_event(events.and_then(|e| e.get("entered")), ShowCue::default().entered),
                    started_on_beat: edn_event(events.and_then(|e| e.get("started-on-beat")), ShowCue::default().started_on_beat),
                    started_late: edn_event(events.and_then(|e| e.get("started-late")), ShowCue::default().started_late),
                    expressions: legacy.iter().map(|(k, v)| (k.clone(), stub(v))).collect(),
                    legacy_expressions: legacy,
                });
            }
        }
        track.cues.sort_by_key(|c| (c.start, c.end));
        show.tracks.insert(signature.to_string(), track);
    }
    Ok(show)
}

// -------------------------------------------------------------------------------------
// Runtime
// -------------------------------------------------------------------------------------

/// What a player is doing with a show track.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerTrackState {
    pub signature: String,
    pub playing: bool,
    pub enabled: bool,
    pub beat: Option<i64>,
    pub status: Option<Arc<CdjStatus>>,
}

#[derive(Default)]
struct TrackRuntime {
    loaded_players: BTreeSet<u8>,
    playing_players: BTreeSet<u8>,
    /// Per cue uuid: players inside the cue, and players playing inside it.
    cue_entered: HashMap<String, BTreeSet<u8>>,
    cue_playing: HashMap<String, BTreeSet<u8>>,
    /// Which started event (on-beat or late) each cue last sent, for the matching ended.
    cue_started_with: HashMap<String, EventSpec>,
    locals: Store,
    compiled: HashMap<String, Compiled>,
}

/// An event a show produced (used by tests and the UI activity log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShowEvent {
    Loaded(String),
    Unloaded(String),
    Playing(String),
    Stopped(String),
    CueEntered(String, String),
    CueExited(String, String),
    CueStartedOnBeat(String, String),
    CueStartedLate(String, String),
    CueEnded(String, String),
}

/// Runs a show.
pub struct ShowRunner {
    pub path: Option<PathBuf>,
    pub show: RwLock<Show>,
    expressions: Arc<ExpressionEngine>,
    outputs: Arc<Outputs>,
    runtime: Mutex<HashMap<String, TrackRuntime>>,
    players: Mutex<HashMap<u8, PlayerTrackState>>,
    pub show_locals: Store,
    events: Mutex<Vec<ShowEvent>>,
}

impl ShowRunner {
    pub fn new(show: Show, path: Option<PathBuf>, expressions: Arc<ExpressionEngine>, outputs: Arc<Outputs>) -> Arc<ShowRunner> {
        let runner = Arc::new(ShowRunner {
            path,
            show: RwLock::new(show),
            expressions,
            outputs,
            runtime: Mutex::new(HashMap::new()),
            players: Mutex::new(HashMap::new()),
            show_locals: Store::new(),
            events: Mutex::new(Vec::new()),
        });
        runner.compile_all();
        let signatures: Vec<String> = runner.show.read().tracks.keys().cloned().collect();
        for sig in signatures {
            runner.run_track_expression(&sig, "setup", None, None);
        }
        runner
    }

    pub fn title(&self) -> String {
        self.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Untitled Show".into())
    }

    /// Recompile all track and cue expressions (after editing).
    pub fn compile_all(&self) {
        let show = self.show.read().clone();
        let mut runtime = self.runtime.lock();
        for (sig, track) in &show.tracks {
            let rt = runtime.entry(sig.clone()).or_default();
            rt.compiled.clear();
            for (kind, title) in TRACK_EXPRESSIONS {
                if let Ok(Some(c)) = self.expressions.compile(track.expressions.get(*kind).map(String::as_str).unwrap_or(""), &format!("{} {}", track.metadata.title, title)) {
                    rt.compiled.insert(kind.to_string(), c);
                }
            }
            for cue in &track.cues {
                for (kind, title) in CUE_EXPRESSIONS {
                    let src = cue.expressions.get(*kind).map(String::as_str).unwrap_or("");
                    match self.expressions.compile(src, &format!("Cue \"{}\" {}", cue.comment, title)) {
                        Ok(Some(c)) => {
                            rt.compiled.insert(format!("cue:{}:{}", cue.uuid, kind), c);
                        }
                        Ok(None) => {}
                        Err(e) => log::error!("{e}"),
                    }
                }
            }
        }
    }

    /// Events produced since the last call.
    pub fn take_events(&self) -> Vec<ShowEvent> {
        std::mem::take(&mut *self.events.lock())
    }

    fn context(&self, sig: &str, cue: Option<&ShowCue>, status: Option<Arc<CdjStatus>>, locals: Store) -> Context {
        let show = self.show.read();
        let mut extra = BTreeMap::new();
        if let Some(track) = show.tracks.get(sig) {
            let mut t = Map::new();
            t.insert("signature".into(), sig.to_string().into());
            t.insert("title".into(), track.metadata.title.clone().into());
            t.insert("artist".into(), track.metadata.artist.clone().unwrap_or_default().into());
            extra.insert("track".into(), Dynamic::from_map(t));
        }
        if let Some(cue) = cue {
            let mut c = Map::new();
            c.insert("uuid".into(), cue.uuid.clone().into());
            c.insert("comment".into(), cue.comment.clone().into());
            c.insert("start".into(), cue.start.into());
            c.insert("end".into(), cue.end.into());
            extra.insert("cue".into(), Dynamic::from_map(c));
        }
        extra.insert("show_locals".into(), Dynamic::from(self.show_locals.clone()));
        Context { status: status.map(StatusValue::Cdj).unwrap_or_default(), track: None, position_ms: None, locals: Some(locals), extra }
    }

    fn run_track_expression(&self, sig: &str, kind: &str, cue: Option<&ShowCue>, status: Option<Arc<CdjStatus>>) {
        let key = match cue {
            Some(c) => format!("cue:{}:{}", c.uuid, kind),
            None => kind.to_string(),
        };
        let (compiled, locals) = {
            let mut rt = self.runtime.lock();
            let r = rt.entry(sig.to_string()).or_default();
            (r.compiled.get(&key).cloned(), r.locals.clone())
        };
        if let Some(c) = compiled {
            let ctx = self.context(sig, cue, status, locals);
            if let Err(e) = self.expressions.run(&c, &ctx) {
                log::error!("{e}");
            }
        }
    }

    fn send_event(&self, device: &Option<String>, spec: EventSpec, on: bool) {
        let Some(device) = device else { return };
        let r = match (spec.message, on) {
            (EventMessage::Note, true) => self.outputs.midi.note_on(device, spec.note, 127, spec.channel),
            (EventMessage::Note, false) => self.outputs.midi.note_off(device, spec.note, spec.channel),
            (EventMessage::Cc, true) => self.outputs.midi.control(device, spec.note, 127, spec.channel),
            (EventMessage::Cc, false) => self.outputs.midi.control(device, spec.note, 0, spec.channel),
            _ => Ok(()),
        };
        if let Err(e) = r {
            log::error!("Problem sending show MIDI: {e}");
        }
    }

    /// Is a track enabled for a player, given the show and track settings?
    fn track_enabled(&self, sig: &str, status: &CdjStatus) -> bool {
        let show = self.show.read();
        let Some(track) = show.tracks.get(sig) else { return false };
        let mode = if track.enabled == ShowEnabled::Default { show.enabled } else { track.enabled };
        match mode {
            ShowEnabled::Always => true,
            ShowEnabled::Never | ShowEnabled::Default => false,
            ShowEnabled::OnAir => status.is_on_air(),
            ShowEnabled::Master => status.is_tempo_master(),
            ShowEnabled::Custom => {
                let (compiled, locals) = {
                    let mut rt = self.runtime.lock();
                    let r = rt.entry(sig.to_string()).or_default();
                    (r.compiled.get("enabled").cloned(), r.locals.clone())
                };
                drop(show);
                compiled
                    .map(|c| self.expressions.run(&c, &self.context(sig, None, Some(Arc::new(status.clone())), locals)).map(|v| crate::expressions::truthy(&v)).unwrap_or(false))
                    .unwrap_or(false)
            }
        }
    }

    /// Update a player's state: what track it has (by signature), whether it plays,
    /// and its beat position. Fires the resulting events.
    pub fn update_player(&self, player: u8, data: Option<&Arc<TrackData>>, status: Option<&Arc<CdjStatus>>, beat: Option<i64>) {
        let signature = data.and_then(|d| d.signature.clone()).filter(|s| self.show.read().tracks.contains_key(s));
        let new_state = match (&signature, status) {
            (Some(sig), Some(st)) => Some(PlayerTrackState {
                signature: sig.clone(),
                playing: st.is_playing(),
                enabled: self.track_enabled(sig, st),
                beat: beat.or_else(|| st.beat_number().map(i64::from)),
                status: Some(st.clone()),
            }),
            (Some(sig), None) => Some(PlayerTrackState { signature: sig.clone(), playing: false, enabled: false, beat, status: None }),
            _ => None,
        };
        let old_state = {
            let mut players = self.players.lock();
            match &new_state {
                Some(s) => players.insert(player, s.clone()),
                None => players.remove(&player),
            }
        };
        let mut affected: BTreeSet<String> = BTreeSet::new();
        if let Some(o) = &old_state {
            affected.insert(o.signature.clone());
        }
        if let Some(n) = &new_state {
            affected.insert(n.signature.clone());
        }
        for sig in affected {
            self.reconcile_track(&sig, status.cloned());
        }
        if let (Some(n), Some(st)) = (&new_state, status) {
            self.run_track_expression(&n.signature, "tracked", None, Some(st.clone()));
        }
    }

    fn reconcile_track(&self, sig: &str, status: Option<Arc<CdjStatus>>) {
        let Some(track) = self.show.read().tracks.get(sig).cloned() else { return };
        let players: Vec<(u8, PlayerTrackState)> =
            self.players.lock().iter().filter(|(_, s)| s.signature == sig).map(|(p, s)| (*p, s.clone())).collect();
        let loaded: BTreeSet<u8> = players.iter().map(|(p, _)| *p).collect();
        let playing: BTreeSet<u8> = players.iter().filter(|(_, s)| s.playing && s.enabled).map(|(p, _)| *p).collect();

        let (was_loaded, was_playing) = {
            let mut rt = self.runtime.lock();
            let r = rt.entry(sig.to_string()).or_default();
            let prev = (!r.loaded_players.is_empty(), !r.playing_players.is_empty());
            r.loaded_players = loaded.clone();
            r.playing_players = playing.clone();
            prev
        };
        let now_loaded = !loaded.is_empty();
        let now_playing = !playing.is_empty();
        if now_loaded && !was_loaded {
            self.send_event(&track.midi_device, track.loaded, true);
            self.events.lock().push(ShowEvent::Loaded(sig.to_string()));
            self.run_track_expression(sig, "loaded", None, status.clone());
        }
        if now_playing && !was_playing {
            self.send_event(&track.midi_device, track.playing, true);
            self.events.lock().push(ShowEvent::Playing(sig.to_string()));
            self.run_track_expression(sig, "playing", None, status.clone());
        }

        for cue in &track.cues {
            let inside: BTreeSet<u8> = players.iter().filter(|(_, s)| s.beat.map(|b| b >= cue.start && b < cue.end).unwrap_or(false)).map(|(p, _)| *p).collect();
            let inside_playing: BTreeSet<u8> = inside.intersection(&playing).copied().collect();
            let (was_inside, was_inside_playing) = {
                let mut rt = self.runtime.lock();
                let r = rt.entry(sig.to_string()).or_default();
                let a = r.cue_entered.insert(cue.uuid.clone(), inside.clone()).map(|s| !s.is_empty()).unwrap_or(false);
                let b = r.cue_playing.insert(cue.uuid.clone(), inside_playing.clone()).map(|s| !s.is_empty()).unwrap_or(false);
                (a, b)
            };
            if !inside.is_empty() && !was_inside {
                self.send_event(&track.midi_device, cue.entered, true);
                self.events.lock().push(ShowEvent::CueEntered(sig.to_string(), cue.uuid.clone()));
                self.run_track_expression(sig, "entered", Some(cue), status.clone());
            }
            if !inside_playing.is_empty() && !was_inside_playing {
                let on_beat = players.iter().any(|(p, s)| inside_playing.contains(p) && s.beat == Some(cue.start));
                let spec = if on_beat || cue.started_late.message == EventMessage::Same { cue.started_on_beat } else { cue.started_late };
                self.send_event(&track.midi_device, spec, true);
                self.runtime.lock().entry(sig.to_string()).or_default().cue_started_with.insert(cue.uuid.clone(), spec);
                if on_beat {
                    self.events.lock().push(ShowEvent::CueStartedOnBeat(sig.to_string(), cue.uuid.clone()));
                    self.run_track_expression(sig, "started-on-beat", Some(cue), status.clone());
                } else {
                    self.events.lock().push(ShowEvent::CueStartedLate(sig.to_string(), cue.uuid.clone()));
                    let kind = if cue.expressions.contains_key("started-late") { "started-late" } else { "started-on-beat" };
                    self.run_track_expression(sig, kind, Some(cue), status.clone());
                }
            }
            if inside_playing.is_empty() && was_inside_playing {
                let spec = self.runtime.lock().entry(sig.to_string()).or_default().cue_started_with.remove(&cue.uuid).unwrap_or(cue.started_on_beat);
                self.send_event(&track.midi_device, spec, false);
                self.events.lock().push(ShowEvent::CueEnded(sig.to_string(), cue.uuid.clone()));
                self.run_track_expression(sig, "ended", Some(cue), status.clone());
            }
            if inside.is_empty() && was_inside {
                self.send_event(&track.midi_device, cue.entered, false);
                self.events.lock().push(ShowEvent::CueExited(sig.to_string(), cue.uuid.clone()));
                self.run_track_expression(sig, "exited", Some(cue), status.clone());
            }
        }

        if !now_playing && was_playing {
            self.send_event(&track.midi_device, track.playing, false);
            self.events.lock().push(ShowEvent::Stopped(sig.to_string()));
            self.run_track_expression(sig, "stopped", None, status.clone());
        }
        if !now_loaded && was_loaded {
            self.send_event(&track.midi_device, track.loaded, false);
            self.events.lock().push(ShowEvent::Unloaded(sig.to_string()));
            self.run_track_expression(sig, "unloaded", None, status);
        }
    }

    /// Players that currently have a show track loaded.
    pub fn player_states(&self) -> BTreeMap<u8, PlayerTrackState> {
        self.players.lock().iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Shut the show down: unload everything and run shutdown expressions.
    pub fn shutdown(&self) {
        let players: Vec<u8> = self.players.lock().keys().copied().collect();
        for p in players {
            self.update_player(p, None, None, None);
        }
        let sigs: Vec<String> = self.show.read().tracks.keys().cloned().collect();
        for sig in sigs {
            self.run_track_expression(&sig, "shutdown", None, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beat_link::metadata_finder::TrackData;
    use beat_link::packets::{CdjStatusBuilder, TrackType};

    fn data(sig: &str) -> Arc<TrackData> {
        Arc::new(TrackData { signature: Some(sig.into()), ..Default::default() })
    }

    fn status(playing: bool, beat: u32) -> Arc<CdjStatus> {
        Arc::new(CdjStatusBuilder { device_number: 1, playing, beat_number: Some(beat), track_type: TrackType::Rekordbox, rekordbox_id: 1, bpm: 12000, ..Default::default() }.build())
    }

    fn runner() -> Arc<ShowRunner> {
        let outputs = Arc::new(Outputs::new_for_tests());
        let mut show = Show::default();
        let mut track = ShowTrack {
            signature: "abc".into(),
            enabled: ShowEnabled::Default,
            midi_device: Some("Show".into()),
            loaded: EventSpec { message: EventMessage::Note, note: 126, channel: 1 },
            playing: EventSpec { message: EventMessage::Note, note: 127, channel: 1 },
            ..Default::default()
        };
        let mut cue = ShowCue { uuid: "c1".into(), comment: "drop".into(), start: 33, end: 41, ..Default::default() };
        cue.entered = EventSpec { message: EventMessage::Cc, note: 20, channel: 2 };
        cue.started_on_beat = EventSpec { message: EventMessage::Note, note: 40, channel: 2 };
        cue.expressions.insert("started-on-beat".into(), "locals.increment(\"starts\");".into());
        track.cues.push(cue);
        show.tracks.insert("abc".into(), track);
        ShowRunner::new(show, None, Arc::new(ExpressionEngine::new(outputs.clone())), outputs)
    }

    #[test]
    fn track_and_cue_lifecycle() {
        let r = runner();
        r.update_player(1, Some(&data("abc")), Some(&status(false, 1)), None);
        r.update_player(1, Some(&data("abc")), Some(&status(true, 1)), None);
        r.update_player(1, Some(&data("abc")), Some(&status(true, 33)), None);
        r.update_player(1, Some(&data("abc")), Some(&status(true, 41)), None);
        r.update_player(1, Some(&data("abc")), Some(&status(false, 42)), None);
        r.update_player(1, None, None, None);
        let ev = r.take_events();
        assert_eq!(
            ev,
            vec![
                ShowEvent::Loaded("abc".into()),
                ShowEvent::Playing("abc".into()),
                ShowEvent::CueEntered("abc".into(), "c1".into()),
                ShowEvent::CueStartedOnBeat("abc".into(), "c1".into()),
                ShowEvent::CueEnded("abc".into(), "c1".into()),
                ShowEvent::CueExited("abc".into(), "c1".into()),
                ShowEvent::Stopped("abc".into()),
                ShowEvent::Unloaded("abc".into()),
            ]
        );
        let starts = r.runtime.lock().get("abc").unwrap().locals.get("starts").as_int().unwrap();
        assert_eq!(starts, 1);
        let midi: Vec<Vec<u8>> = r.outputs.midi.history().into_iter().map(|(_, b)| b).collect();
        assert_eq!(midi[0], vec![0x90, 126, 127]);
        assert_eq!(midi[1], vec![0x90, 127, 127]);
        assert_eq!(midi[2], vec![0xb1, 20, 127]);
        assert_eq!(midi[3], vec![0x91, 40, 127]);
    }

    #[test]
    fn late_start_and_unknown_tracks() {
        let r = runner();
        r.update_player(1, Some(&data("zzz")), Some(&status(true, 35)), None);
        assert!(r.take_events().is_empty());
        r.update_player(1, Some(&data("abc")), Some(&status(true, 35)), None);
        let ev = r.take_events();
        assert!(ev.contains(&ShowEvent::CueStartedLate("abc".into(), "c1".into())));
    }

    #[test]
    fn imports_java_show() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../doc/modules/ROOT/attachments/BreakBuddy.bls");
        if !path.exists() {
            return;
        }
        let show = Show::load(&path).unwrap();
        let track = show.tracks.get("6c8139bd531a417999317a9d024d6d8ea6993507").unwrap();
        assert_eq!(track.metadata.title, "Concrete Angel (Original Mix)");
        assert_eq!(track.cues.len(), 3);
        assert_eq!(track.cues[0].start, 593);
        assert_eq!(track.cues[0].entered.message, EventMessage::Custom);
        assert!(track.cues[0].legacy_expressions["entered"].contains("bb-load-track-on-other-player"));
        assert!(show.legacy_expressions.contains_key("shared"));
        let json = serde_json::to_string(&show).unwrap();
        let back: Show = serde_json::from_str(&json).unwrap();
        assert_eq!(back, show);
    }
}
