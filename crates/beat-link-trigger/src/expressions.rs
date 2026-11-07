//! User expressions.
//!
//! The Java/Clojure version compiles user-written Clojure for each trigger event.
//! This rewrite embeds the [Rhai](https://rhai.rs) scripting language instead. Each
//! expression runs with a scope containing the same convenience values the original
//! offered (with Clojure names converted to identifiers: `beat-number` becomes
//! `beat_number`, `playing?` becomes `playing`, `beat?` becomes `is_beat`), plus
//! `locals` (per trigger or show) and `globals` (shared) stores, and a library of
//! functions for MIDI, OSC, networking, Ableton Link and player control.

use crate::outputs::Outputs;
use beat_link::data::TrackMetadata;
use beat_link::packets::{Beat, CdjStatus, MixerStatus};
use beat_link::util;
use beat_link::TrackData;
use parking_lot::RwLock;
use rhai::{Array, Dynamic, Engine, EvalAltResult, ImmutableString, Map, Scope, AST};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A mutable map shared between expressions (`locals` and `globals`).
#[derive(Clone, Default)]
pub struct Store(pub Arc<RwLock<Map>>);

impl Store {
    pub fn new() -> Self {
        Store::default()
    }
    pub fn get(&self, key: &str) -> Dynamic {
        self.0.read().get(key).cloned().unwrap_or(Dynamic::UNIT)
    }
    pub fn set(&self, key: &str, value: Dynamic) {
        self.0.write().insert(key.into(), value);
    }
    pub fn remove(&self, key: &str) -> Dynamic {
        self.0.write().remove(key).unwrap_or(Dynamic::UNIT)
    }
    pub fn contains(&self, key: &str) -> bool {
        self.0.read().contains_key(key)
    }
    pub fn clear(&self) {
        self.0.write().clear();
    }
    pub fn keys(&self) -> Array {
        self.0.read().keys().map(|k| Dynamic::from(k.to_string())).collect()
    }
    pub fn to_map(&self) -> Map {
        self.0.read().clone()
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Store({} keys)", self.0.read().len())
    }
}

/// The kinds of expression a trigger can have, in the order the editor lists them.
pub const TRIGGER_EXPRESSIONS: &[(&str, &str, &str)] = &[
    ("setup", "Setup Expression", "Called once to set up any state your other expressions may need."),
    ("enabled", "Enabled Filter Expression", "Called to see if the trigger should be enabled (when Enabled is Custom). Return true to enable."),
    ("activation", "Activation Expression", "Called when the trigger becomes enabled and tripped."),
    ("beat", "Beat Expression", "Called on each beat from the watched devices."),
    ("tracked", "Tracked Update Expression", "Called for each update from the player a trigger is tracking."),
    ("deactivation", "Deactivation Expression", "Called when the trigger becomes disabled or idle. status may be ()."),
    ("shutdown", "Shutdown Expression", "Called once to release resources your trigger had been using."),
];

/// The global expressions of the Triggers window.
pub const GLOBAL_EXPRESSIONS: &[(&str, &str, &str)] = &[
    ("shared", "Shared Functions", "Functions (fn name(args) { ... }) available to every expression."),
    ("setup", "Global Setup Expression", "Called once to set up any state your trigger expressions may need."),
    ("online", "Came Online Expression", "Called when BLT has successfully joined a Pro DJ Link network."),
    ("offline", "Going Offline Expression", "Called when BLT is disconnecting from a Pro DJ Link network."),
    ("shutdown", "Global Shutdown Expression", "Called once to release global resources."),
];

/// The value an expression receives as `status`.
#[derive(Debug, Clone, Default)]
pub enum StatusValue {
    #[default]
    None,
    Cdj(Arc<CdjStatus>),
    Beat(Arc<Beat>),
    Mixer(Arc<MixerStatus>),
}

/// Everything needed to build the scope an expression runs in.
#[derive(Default, Clone)]
pub struct Context {
    pub status: StatusValue,
    pub track: Option<Arc<TrackData>>,
    /// Interpolated playback position of the status's player, if known.
    pub position_ms: Option<i64>,
    pub locals: Option<Store>,
    /// Extra top-level variables (trigger settings, show information...).
    pub extra: BTreeMap<String, Dynamic>,
}

fn metadata_map(md: &TrackMetadata) -> Map {
    let mut m = Map::new();
    let label = |o: &Option<beat_link::data::SearchableItem>| Dynamic::from(o.as_ref().map(|i| i.label.clone()).unwrap_or_default());
    m.insert("title".into(), md.title.clone().into());
    m.insert("artist".into(), label(&md.artist));
    m.insert("album".into(), label(&md.album));
    m.insert("genre".into(), label(&md.genre));
    m.insert("key".into(), label(&md.key));
    m.insert("label".into(), label(&md.label));
    m.insert("remixer".into(), label(&md.remixer));
    m.insert("original_artist".into(), label(&md.original_artist));
    m.insert("comment".into(), md.comment.clone().into());
    m.insert("date_added".into(), md.date_added.clone().into());
    m.insert("duration".into(), (md.duration as i64).into());
    m.insert("tempo".into(), md.starting_tempo().into());
    m.insert("rating".into(), (md.rating as i64).into());
    m.insert("year".into(), (md.year as i64).into());
    m.insert("color".into(), md.color.as_ref().map(|c| c.label.clone()).unwrap_or_default().into());
    m
}

/// A Rhai map describing a CDJ status (also available as `status`).
pub fn cdj_status_map(s: &CdjStatus) -> Map {
    let mut m = Map::new();
    m.insert("kind".into(), "cdj".into());
    m.insert("device_number".into(), i64::from(s.device_number()).into());
    m.insert("device_name".into(), s.device_name().to_string().into());
    m.insert("address".into(), s.address().to_string().into());
    m.insert("beat_number".into(), s.beat_number_or_negative().into());
    m.insert("beat_within_bar".into(), i64::from(s.beat_within_bar()).into());
    m.insert("bpm".into(), i64::from(s.bpm).into());
    m.insert("pitch".into(), s.pitch.into());
    m.insert("effective_tempo".into(), s.effective_tempo().into());
    m.insert("playing".into(), s.is_playing().into());
    m.insert("on_air".into(), s.is_on_air().into());
    m.insert("tempo_master".into(), s.is_tempo_master().into());
    m.insert("synced".into(), s.is_synced().into());
    m.insert("rekordbox_id".into(), i64::from(s.rekordbox_id).into());
    m.insert("track_source_player".into(), i64::from(s.track_source_player).into());
    m.insert("track_source_slot".into(), s.track_source_slot.java_name().into());
    m.insert("track_type".into(), s.track_type.java_name().into());
    m.insert("track_number".into(), i64::from(s.track_number()).into());
    m.insert("firmware_version".into(), s.firmware_version.clone().into());
    m
}

/// Builds the scope variables for a context.
pub fn build_scope(ctx: &Context, globals: &Store) -> Scope<'static> {
    let mut scope = Scope::new();
    scope.push("globals", globals.clone());
    if let Some(locals) = &ctx.locals {
        scope.push("locals", locals.clone());
    }
    let metadata = ctx.track.as_ref().and_then(|t| t.metadata.clone());
    let cue_list = ctx.track.as_ref().and_then(|t| t.cue_list.clone());

    // Fields common to every device update.
    let (device_number, device_name, address, timestamp, pitch, bpm, beat_within_bar, bar_meaningful, master, synced) =
        match &ctx.status {
            StatusValue::Cdj(s) => (
                Some(s.device_number()),
                s.device_name().to_string(),
                s.address().to_string(),
                s.timestamp(),
                s.pitch,
                s.bpm,
                s.beat_within_bar(),
                s.is_beat_within_bar_meaningful(),
                s.is_tempo_master(),
                s.is_synced(),
            ),
            StatusValue::Beat(b) => (
                Some(b.device_number()),
                b.device_name().to_string(),
                b.header.address.to_string(),
                b.timestamp(),
                b.pitch,
                b.bpm,
                b.beat_within_bar(),
                b.bar_meaningful,
                b.tempo_master,
                b.synced,
            ),
            StatusValue::Mixer(m) => (
                Some(m.device_number()),
                m.header.device_name.clone(),
                m.header.address.to_string(),
                m.header.timestamp,
                m.pitch,
                m.bpm,
                m.beat_within_bar(),
                false,
                m.is_tempo_master(),
                m.is_synced(),
            ),
            StatusValue::None => (None, String::new(), String::new(), 0, util::NEUTRAL_PITCH, util::NO_BPM, 0, false, false, false),
        };

    let status_dynamic = match &ctx.status {
        StatusValue::Cdj(s) => Dynamic::from_map(cdj_status_map(s)),
        StatusValue::Beat(b) => {
            let mut m = Map::new();
            m.insert("kind".into(), "beat".into());
            m.insert("device_number".into(), i64::from(b.device_number()).into());
            m.insert("beat_within_bar".into(), i64::from(b.beat_within_bar()).into());
            m.insert("bpm".into(), i64::from(b.bpm).into());
            m.insert("effective_tempo".into(), b.effective_tempo().into());
            m.insert("next_beat".into(), i64::from(b.next_beat()).into());
            m.insert("next_bar".into(), i64::from(b.next_bar()).into());
            Dynamic::from_map(m)
        }
        StatusValue::Mixer(m) => {
            let mut map = Map::new();
            map.insert("kind".into(), "mixer".into());
            map.insert("device_number".into(), i64::from(m.device_number()).into());
            map.insert("bpm".into(), i64::from(m.bpm).into());
            map.insert("effective_tempo".into(), m.effective_tempo().into());
            Dynamic::from_map(map)
        }
        StatusValue::None => Dynamic::UNIT,
    };
    scope.push_dynamic("status", status_dynamic);

    if let Some(n) = device_number {
        let multiplier = util::pitch_to_multiplier(pitch);
        scope.push("device_number", i64::from(n));
        scope.push("device_name", device_name);
        scope.push("address", address);
        scope.push("timestamp", timestamp as i64);
        scope.push("raw_pitch", pitch);
        scope.push("pitch_multiplier", multiplier);
        scope.push("pitch_percent", util::pitch_to_percentage(pitch));
        scope.push("raw_bpm", i64::from(bpm));
        scope.push("track_bpm", if bpm == util::NO_BPM { Dynamic::UNIT } else { Dynamic::from(f64::from(bpm) / 100.0) });
        scope.push("effective_tempo", if bpm == util::NO_BPM { 0.0 } else { f64::from(bpm) * multiplier / 100.0 });
        scope.push("beat_within_bar", i64::from(beat_within_bar));
        scope.push("bar_meaningful", bar_meaningful);
        scope.push("tempo_master", master);
        scope.push("synced", synced);
        scope.push("is_beat", matches!(ctx.status, StatusValue::Beat(_)));
        scope.push("cdj", n < 17 && !matches!(ctx.status, StatusValue::Mixer(_)));
        scope.push("mixer", matches!(ctx.status, StatusValue::Mixer(_)));
    }

    if let StatusValue::Cdj(s) = &ctx.status {
        scope.push("beat_number", s.beat_number_or_negative());
        scope.push("busy", s.is_busy());
        scope.push("cue_countdown", i64::from(s.cue_countdown()));
        scope.push("cue_countdown_text", s.format_cue_countdown());
        scope.push("cued", s.is_cued());
        scope.push("looping", s.is_looping());
        scope.push("on_air", s.is_on_air());
        scope.push("paused", s.is_paused());
        scope.push("playing", s.is_playing());
        scope.push("rekordbox_id", i64::from(s.rekordbox_id));
        scope.push("track_number", i64::from(s.track_number()));
        scope.push("track_source_player", i64::from(s.track_source_player));
        scope.push("track_source_slot", s.track_source_slot.java_name().to_string());
        scope.push("track_type", s.track_type.java_name().to_string());
        if let (Some(grid), Some(beat)) = (ctx.track.as_ref().and_then(|t| t.beat_grid.clone()), s.beat_number()) {
            scope.push("bar_number", grid.bar_number(i64::from(beat)));
        }
    }

    // Track metadata bindings.
    match &metadata {
        Some(md) => {
            scope.push("track_metadata", metadata_map(md));
            scope.push("track_title", md.title.clone());
            scope.push("track_artist", md.artist_label().to_string());
            scope.push("track_comment", md.comment.clone());
            scope.push("track_genre", md.genre.as_ref().map(|g| g.label.clone()).unwrap_or_default());
            scope.push("track_key", md.key.as_ref().map(|g| g.label.clone()).unwrap_or_default());
            scope.push("track_label", md.label.as_ref().map(|g| g.label.clone()).unwrap_or_default());
            scope.push("track_length", i64::from(md.duration));
        }
        None => {
            for name in ["track_metadata", "track_title", "track_artist", "track_comment", "track_genre", "track_key", "track_label", "track_length"] {
                scope.push_dynamic(name, Dynamic::UNIT);
            }
        }
    }
    match ctx.position_ms {
        Some(ms) => {
            scope.push("track_time_reached", ms);
            let next = cue_list.as_ref().and_then(|c| c.next_cue_after(ms)).map(|c| c.description());
            let prev = cue_list.as_ref().and_then(|c| c.previous_cue_before(ms)).map(|c| c.description());
            scope.push_dynamic("next_cue", next.map(Dynamic::from).unwrap_or(Dynamic::UNIT));
            scope.push_dynamic("previous_cue", prev.map(Dynamic::from).unwrap_or(Dynamic::UNIT));
        }
        None => {
            scope.push_dynamic("track_time_reached", Dynamic::UNIT);
            scope.push_dynamic("next_cue", Dynamic::UNIT);
            scope.push_dynamic("previous_cue", Dynamic::UNIT);
        }
    }
    for (k, v) in &ctx.extra {
        scope.push_dynamic(k.clone(), v.clone());
    }
    scope
}

/// Errors compiling or running an expression.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{description}: {message}")]
pub struct ExpressionError {
    pub description: String,
    pub message: String,
}

/// A compiled expression.
#[derive(Clone)]
pub struct Compiled {
    pub ast: Arc<AST>,
    pub description: String,
}

/// Compiles and runs expressions with the shared function library and outputs.
pub struct ExpressionEngine {
    engine: Engine,
    shared: RwLock<Option<Arc<AST>>>,
    pub globals: Store,
}

impl ExpressionEngine {
    pub fn new(outputs: Arc<Outputs>) -> Self {
        let mut engine = Engine::new();
        engine.set_max_expr_depths(256, 256);
        engine.on_print(|s| log::info!(target: "expression", "{s}"));
        engine.on_debug(|s, src, pos| log::debug!(target: "expression", "{} {:?} {}", src.unwrap_or(""), pos, s));
        register_store(&mut engine);
        register_library(&mut engine, outputs);
        ExpressionEngine { engine, shared: RwLock::new(None), globals: Store::new() }
    }

    /// The underlying Rhai engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compile the shared function library. Returns an error describing problems.
    pub fn set_shared_functions(&self, source: &str) -> Result<(), ExpressionError> {
        if source.trim().is_empty() {
            *self.shared.write() = None;
            return Ok(());
        }
        let ast = self
            .engine
            .compile(source)
            .map_err(|e| ExpressionError { description: "Shared Functions".into(), message: e.to_string() })?;
        *self.shared.write() = Some(Arc::new(ast.clone_functions_only()));
        Ok(())
    }

    /// Compile an expression. Empty source compiles to `None`.
    pub fn compile(&self, source: &str, description: &str) -> Result<Option<Compiled>, ExpressionError> {
        if source.trim().is_empty() {
            return Ok(None);
        }
        let ast = self
            .engine
            .compile(source)
            .map_err(|e| ExpressionError { description: description.into(), message: e.to_string() })?;
        let ast = match self.shared.read().as_ref() {
            Some(shared) => shared.as_ref().merge(&ast),
            None => ast,
        };
        Ok(Some(Compiled { ast: Arc::new(ast), description: description.into() }))
    }

    /// Run a compiled expression in a context, returning its value.
    pub fn run(&self, compiled: &Compiled, ctx: &Context) -> Result<Dynamic, ExpressionError> {
        let mut scope = build_scope(ctx, &self.globals);
        self.engine
            .eval_ast_with_scope::<Dynamic>(&mut scope, &compiled.ast)
            .map_err(|e| ExpressionError { description: compiled.description.clone(), message: e.to_string() })
    }

    /// Compile and run source in one step (used by the simulate buttons and tests).
    pub fn eval(&self, source: &str, ctx: &Context) -> Result<Dynamic, ExpressionError> {
        match self.compile(source, "expression")? {
            Some(c) => self.run(&c, ctx),
            None => Ok(Dynamic::UNIT),
        }
    }
}

/// Rhai's notion of truthiness for an expression's result (Clojure-like: only
/// `false` and `()` are false).
pub fn truthy(value: &Dynamic) -> bool {
    if value.is_unit() {
        return false;
    }
    if let Ok(b) = value.as_bool() {
        return b;
    }
    true
}

fn register_store(engine: &mut Engine) {
    engine.register_type_with_name::<Store>("Store");
    engine.register_indexer_get(|s: &mut Store, k: &str| s.get(k));
    engine.register_indexer_set(|s: &mut Store, k: &str, v: Dynamic| s.set(k, v));
    engine.register_fn("get", |s: &mut Store, k: &str| s.get(k));
    engine.register_fn("get", |s: &mut Store, k: &str, default: Dynamic| if s.contains(k) { s.get(k) } else { default });
    engine.register_fn("set", |s: &mut Store, k: &str, v: Dynamic| s.set(k, v));
    engine.register_fn("remove", |s: &mut Store, k: &str| s.remove(k));
    engine.register_fn("contains", |s: &mut Store, k: &str| s.contains(k));
    engine.register_fn("clear", |s: &mut Store| s.clear());
    engine.register_fn("keys", |s: &mut Store| s.keys());
    engine.register_fn("to_map", |s: &mut Store| s.to_map());
    engine.register_fn("to_string", |s: &mut Store| format!("{:?}", s.to_map()));
    // Atomic-ish counter helper: increments and returns the new value.
    engine.register_fn("increment", |s: &mut Store, k: &str| {
        let mut guard = s.0.write();
        let next = guard.get(k).and_then(|v| v.as_int().ok()).unwrap_or(0) + 1;
        guard.insert(k.into(), next.into());
        next
    });
}

fn to_u8(v: i64, what: &str) -> Result<u8, Box<EvalAltResult>> {
    u8::try_from(v).map_err(|_| format!("{what} out of range: {v}").into())
}

fn register_library(engine: &mut Engine, outputs: Arc<Outputs>) {
    // Logging.
    engine.register_fn("log_info", |m: &str| log::info!(target: "expression", "{m}"));
    engine.register_fn("log_warn", |m: &str| log::warn!(target: "expression", "{m}"));
    engine.register_fn("log_error", |m: &str| log::error!(target: "expression", "{m}"));
    engine.register_fn("log_debug", |m: &str| log::debug!(target: "expression", "{m}"));

    // Time.
    engine.register_fn("now_ms", || beat_link::clock::current_time_millis() as i64);
    engine.register_fn("monotonic_us", || beat_link::clock::monotonic_micros() as i64);
    engine.register_fn("sleep_ms", |ms: i64| std::thread::sleep(std::time::Duration::from_millis(ms.max(0) as u64)));
    engine.register_fn("format_time", |ms: i64| util::format_track_time(ms));

    // MIDI output.
    let o = outputs.clone();
    engine.register_fn("midi_outputs", move || -> Array { o.midi.list_outputs().into_iter().map(Dynamic::from).collect() });
    let o = outputs.clone();
    engine.register_fn("midi_note_on", move |out: &str, note: i64, velocity: i64, channel: i64| -> Result<(), Box<EvalAltResult>> {
        o.midi.note_on(out, to_u8(note, "note")?, to_u8(velocity, "velocity")?, to_u8(channel, "channel")?).map_err(|e| e.into())
    });
    let o = outputs.clone();
    engine.register_fn("midi_note_off", move |out: &str, note: i64, channel: i64| -> Result<(), Box<EvalAltResult>> {
        o.midi.note_off(out, to_u8(note, "note")?, to_u8(channel, "channel")?).map_err(|e| e.into())
    });
    let o = outputs.clone();
    engine.register_fn("midi_cc", move |out: &str, controller: i64, value: i64, channel: i64| -> Result<(), Box<EvalAltResult>> {
        o.midi.control(out, to_u8(controller, "controller")?, to_u8(value, "value")?, to_u8(channel, "channel")?).map_err(|e| e.into())
    });
    let o = outputs.clone();
    engine.register_fn("midi_send", move |out: &str, bytes: Array| -> Result<(), Box<EvalAltResult>> {
        let raw: Result<Vec<u8>, _> = bytes.into_iter().map(|b| b.as_int().map_err(|_| "midi_send expects integers").and_then(|v| u8::try_from(v).map_err(|_| "byte out of range"))).collect();
        o.midi.send_raw(out, &raw.map_err(|e| e.to_string())?).map_err(|e| e.into())
    });

    // OSC.
    let o = outputs.clone();
    engine.register_fn("osc_send", move |host: &str, port: i64, address: &str, args: Array| -> Result<(), Box<EvalAltResult>> {
        o.osc.send(host, port as u16, address, &args).map_err(|e| e.into())
    });
    let o = outputs.clone();
    engine.register_fn("osc_send", move |host: &str, port: i64, address: &str| -> Result<(), Box<EvalAltResult>> {
        o.osc.send(host, port as u16, address, &[]).map_err(|e| e.into())
    });
    let o = outputs.clone();
    engine.register_fn("osc_send", move |host: &str, port: i64, address: &str, arg: Dynamic| -> Result<(), Box<EvalAltResult>> {
        let args = if arg.is_array() { arg.cast::<Array>() } else { vec![arg] };
        o.osc.send(host, port as u16, address, &args).map_err(|e| e.into())
    });

    // Raw networking for integrations (lighting desks, show controllers...).
    engine.register_fn("udp_send", |host: &str, port: i64, text: &str| -> Result<(), Box<EvalAltResult>> {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
        s.send_to(text.as_bytes(), (host, port as u16)).map_err(|e| e.to_string())?;
        Ok(())
    });
    engine.register_fn("tcp_send", |host: &str, port: i64, text: &str| -> Result<(), Box<EvalAltResult>> {
        use std::io::Write;
        let mut s = std::net::TcpStream::connect((host, port as u16)).map_err(|e| e.to_string())?;
        s.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        Ok(())
    });

    // Actions that need the running application are routed through the outputs hub.
    let o = outputs.clone();
    engine.register_fn("send_sync_mode", move |player: i64, synced: bool| -> Result<(), Box<EvalAltResult>> {
        o.player_command(crate::outputs::PlayerCommand::SyncMode(to_u8(player, "player")?, synced)).map_err(|e| e.into())
    });
    let o = outputs.clone();
    engine.register_fn("appoint_tempo_master", move |player: i64| -> Result<(), Box<EvalAltResult>> {
        o.player_command(crate::outputs::PlayerCommand::AppointMaster(to_u8(player, "player")?)).map_err(|e| e.into())
    });
    let o = outputs.clone();
    engine.register_fn("fader_start", move |start: Array, stop: Array| -> Result<(), Box<EvalAltResult>> {
        let conv = |a: Array| a.into_iter().filter_map(|v| v.as_int().ok()).filter_map(|v| u8::try_from(v).ok()).collect();
        o.player_command(crate::outputs::PlayerCommand::FaderStart(conv(start), conv(stop))).map_err(|e| e.into())
    });
    let o = outputs.clone();
    engine.register_fn("send_on_air", move |channels: Array| -> Result<(), Box<EvalAltResult>> {
        let set = channels.into_iter().filter_map(|v| v.as_int().ok()).filter_map(|v| u8::try_from(v).ok()).collect();
        o.player_command(crate::outputs::PlayerCommand::OnAir(set)).map_err(|e| e.into())
    });
    let o = outputs.clone();
    engine.register_fn("set_link_tempo", move |bpm: f64| -> Result<(), Box<EvalAltResult>> {
        o.player_command(crate::outputs::PlayerCommand::LinkTempo(bpm)).map_err(|e| e.into())
    });
    let o = outputs.clone();
    engine.register_fn("playback_time", move |player: i64| -> Dynamic {
        o.query_time(player as u8).map(Dynamic::from).unwrap_or(Dynamic::UNIT)
    });

    // String helpers commonly needed when talking to other systems.
    engine.register_fn("pad_left", |s: ImmutableString, width: i64, pad: &str| {
        let s = s.to_string();
        let w = width.max(0) as usize;
        if s.chars().count() >= w {
            s
        } else {
            pad.repeat(w - s.chars().count()) + s.as_str()
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use beat_link::packets::{CdjStatusBuilder, TrackType};

    fn engine() -> ExpressionEngine {
        ExpressionEngine::new(Arc::new(Outputs::new_for_tests()))
    }

    #[test]
    fn convenience_bindings() {
        let e = engine();
        let status = CdjStatusBuilder { device_number: 2, bpm: 12800, playing: true, beat_number: Some(9), track_type: TrackType::Rekordbox, rekordbox_id: 5, ..Default::default() }.build();
        let ctx = Context { status: StatusValue::Cdj(Arc::new(status)), ..Default::default() };
        let v = e.eval("if playing && device_number == 2 { beat_number * 2 } else { 0 }", &ctx).unwrap();
        assert_eq!(v.as_int().unwrap(), 18);
        let v = e.eval("effective_tempo", &ctx).unwrap();
        assert_eq!(v.as_float().unwrap(), 128.0);
        let v = e.eval("status.rekordbox_id", &ctx).unwrap();
        assert_eq!(v.as_int().unwrap(), 5);
    }

    #[test]
    fn locals_globals_and_shared_functions() {
        let e = engine();
        e.set_shared_functions("fn double(x) { x * 2 }").unwrap();
        let locals = Store::new();
        let ctx = Context { locals: Some(locals.clone()), ..Default::default() };
        let _ = e.eval("locals[\"count\"] = double(21); globals.set(\"seen\", true);", &ctx).unwrap();
        assert_eq!(locals.get("count").as_int().unwrap(), 42);
        assert!(e.globals.get("seen").as_bool().unwrap());
        assert_eq!(e.eval("locals.increment(\"n\"); locals.increment(\"n\")", &ctx).unwrap().as_int().unwrap(), 2);
    }

    #[test]
    fn errors_are_reported() {
        let e = engine();
        assert!(e.compile("let x = ;", "bad").is_err());
        let err = e.eval("undefined_function()", &Context::default()).unwrap_err();
        assert!(err.message.contains("undefined_function"));
    }

    #[test]
    fn truthiness() {
        assert!(!truthy(&Dynamic::UNIT));
        assert!(!truthy(&Dynamic::from(false)));
        assert!(truthy(&Dynamic::from(0_i64)));
        assert!(truthy(&Dynamic::from(true)));
    }
}
