//! The trigger engine: the behavior behind each row of the Triggers window.
//! Port of `beat-link-trigger.triggers` (minus the Swing user interface).
//!
//! A trigger watches a player (a specific one, the tempo master, or the best match
//! among all players). When the watched player is playing and the trigger is
//! enabled, the trigger is *tripped*: it sends its activation message (MIDI note,
//! CC, clock start, or Ableton Link transport/tempo control) and runs its activation
//! expression; when it stops being tripped it sends the matching deactivation.

use crate::carabiner::Carabiner;
use crate::config::{ClockStart, EnabledMode, MessageKind, PlayerChoice, Settings, TriggerConfig};
use crate::expressions::{truthy, Compiled, Context, ExpressionEngine, StatusValue, Store, GLOBAL_EXPRESSIONS, TRIGGER_EXPRESSIONS};
use crate::outputs::Outputs;
use beat_link::metronome::{now_ms, Metronome};
use beat_link::packets::{Beat, CdjStatus};
use beat_link::util::pitch_to_multiplier;
use beat_link::TrackData;
use parking_lot::{Mutex, RwLock};
use rhai::Dynamic;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// What the trigger engine needs to know about the world.
pub trait Environment: Send + Sync {
    /// Whether a device is on the network (or simulated).
    fn device_present(&self, number: u8) -> bool;
    /// Track data for the track loaded on a player.
    fn track_data(&self, player: u8) -> Option<Arc<TrackData>>;
    /// Interpolated playback position of a player.
    fn position_ms(&self, player: u8) -> Option<i64>;
    /// Whether a (possibly simulated) player is the tempo master.
    fn is_master(&self, player: u8) -> bool;
}

/// An environment with nothing in it (used before going online, and in tests).
pub struct EmptyEnvironment;

impl Environment for EmptyEnvironment {
    fn device_present(&self, _: u8) -> bool {
        false
    }
    fn track_data(&self, _: u8) -> Option<Arc<TrackData>> {
        None
    }
    fn position_ms(&self, _: u8) -> Option<i64> {
        None
    }
    fn is_master(&self, _: u8) -> bool {
        false
    }
}

struct ClockThread {
    running: Arc<AtomicBool>,
    metronome: Arc<Metronome>,
    thread: thread::JoinHandle<()>,
}

/// The runtime state of a trigger.
#[derive(Default)]
pub struct TriggerState {
    pub playing: bool,
    pub on_air: bool,
    pub tripped: bool,
    /// Score and device number of the best match, for Any Player triggers.
    pub last_match: Option<(i32, u8)>,
    pub status: Option<Arc<CdjStatus>>,
    /// The result of the custom Enabled Filter expression.
    pub custom_enabled: Option<bool>,
    clock: Option<ClockThread>,
    compiled: HashMap<String, Compiled>,
    /// Compilation problems by expression kind.
    pub errors: BTreeMap<String, String>,
}

/// A trigger and its state.
pub struct Trigger {
    pub config: RwLock<TriggerConfig>,
    pub state: Mutex<TriggerState>,
    pub locals: Store,
}

/// A read-only view of a trigger for user interfaces.
#[derive(Debug, Clone)]
pub struct TriggerView {
    pub index: usize,
    pub config: TriggerConfig,
    pub playing: bool,
    pub on_air: bool,
    pub enabled: bool,
    pub tripped: bool,
    pub tracking: Option<u8>,
    pub status_summary: Option<String>,
    pub errors: BTreeMap<String, String>,
}

/// Runs all the triggers.
pub struct TriggerEngine {
    triggers: RwLock<Vec<Arc<Trigger>>>,
    pub expressions: Arc<ExpressionEngine>,
    pub outputs: Arc<Outputs>,
    carabiner: RwLock<Option<Arc<Carabiner>>>,
    environment: RwLock<Arc<dyn Environment>>,
    global_compiled: RwLock<HashMap<String, Compiled>>,
    /// Problems compiling global expressions.
    pub global_errors: RwLock<BTreeMap<String, String>>,
    /// How much the tempo must change (BPM) before a MIDI clock adjusts.
    pub midi_clock_sensitivity: Mutex<f64>,
}

fn would_enable(config: &TriggerConfig, on_air: bool, custom: Option<bool>) -> bool {
    match config.enabled {
        EnabledMode::Always => true,
        EnabledMode::OnAir => on_air,
        EnabledMode::Custom => custom.unwrap_or(false),
        EnabledMode::Never => false,
    }
}

impl TriggerEngine {
    pub fn new(expressions: Arc<ExpressionEngine>, outputs: Arc<Outputs>) -> Arc<Self> {
        Arc::new(TriggerEngine {
            triggers: RwLock::new(Vec::new()),
            expressions,
            outputs,
            carabiner: RwLock::new(None),
            environment: RwLock::new(Arc::new(EmptyEnvironment)),
            global_compiled: RwLock::new(HashMap::new()),
            global_errors: RwLock::new(BTreeMap::new()),
            midi_clock_sensitivity: Mutex::new(0.01),
        })
    }

    pub fn set_environment(&self, env: Arc<dyn Environment>) {
        *self.environment.write() = env;
    }

    pub fn set_carabiner(&self, carabiner: Option<Arc<Carabiner>>) {
        *self.carabiner.write() = carabiner;
    }

    fn env(&self) -> Arc<dyn Environment> {
        self.environment.read().clone()
    }

    fn sync_triggers(&self) -> Option<Arc<Carabiner>> {
        self.carabiner.read().clone().filter(|c| c.sync_triggers())
    }

    // ------------------------------------------------------------------ loading

    /// Replace all triggers and global expressions with those in `settings`,
    /// running shutdown expressions for the old ones and setup for the new ones.
    pub fn load(&self, settings: &Settings) {
        self.shutdown_all();
        *self.global_errors.write() = BTreeMap::new();
        if let Err(e) = self.expressions.set_shared_functions(settings.global_expression("shared")) {
            log::error!("{e}");
            self.global_errors.write().insert("shared".into(), e.message);
        }
        let mut compiled = HashMap::new();
        for (kind, title, _) in GLOBAL_EXPRESSIONS {
            if *kind == "shared" {
                continue;
            }
            match self.expressions.compile(settings.global_expression(kind), title) {
                Ok(Some(c)) => {
                    compiled.insert(kind.to_string(), c);
                }
                Ok(None) => {}
                Err(e) => {
                    log::error!("{e}");
                    self.global_errors.write().insert(kind.to_string(), e.message);
                }
            }
        }
        *self.global_compiled.write() = compiled;
        self.run_global("setup");
        let triggers: Vec<Arc<Trigger>> = settings.triggers.iter().map(|c| self.build_trigger(c.clone())).collect();
        *self.triggers.write() = triggers.clone();
        for (i, t) in triggers.iter().enumerate() {
            self.run_trigger_expression(i, t, "setup", StatusValue::None, false);
        }
    }

    fn build_trigger(&self, config: TriggerConfig) -> Arc<Trigger> {
        let trigger = Arc::new(Trigger { config: RwLock::new(config), state: Mutex::new(TriggerState::default()), locals: Store::new() });
        self.compile_trigger(&trigger);
        trigger
    }

    fn compile_trigger(&self, trigger: &Trigger) {
        let config = trigger.config.read().clone();
        let mut compiled = HashMap::new();
        let mut errors = BTreeMap::new();
        for (kind, title, _) in TRIGGER_EXPRESSIONS {
            let description = format!("Trigger \"{}\" {}", config.comment, title);
            match self.expressions.compile(config.expression(kind), &description) {
                Ok(Some(c)) => {
                    compiled.insert(kind.to_string(), c);
                }
                Ok(None) => {}
                Err(e) => {
                    log::error!("{e}");
                    errors.insert(kind.to_string(), e.message);
                }
            }
        }
        let mut state = trigger.state.lock();
        state.compiled = compiled;
        state.errors = errors;
    }

    /// Run every trigger's shutdown expression and the global shutdown.
    pub fn shutdown_all(&self) {
        let old: Vec<Arc<Trigger>> = std::mem::take(&mut *self.triggers.write());
        for (i, t) in old.iter().enumerate() {
            self.cleanup_trigger(i, t);
        }
        if !self.global_compiled.read().is_empty() {
            self.run_global("shutdown");
        }
        self.global_compiled.write().clear();
    }

    fn cleanup_trigger(&self, index: usize, t: &Arc<Trigger>) {
        let was_tripped = t.state.lock().tripped;
        if was_tripped {
            let status = t.state.lock().status.clone();
            self.report_deactivation(index, t, status, true);
        }
        self.stop_clock(t);
        self.run_trigger_expression(index, t, "shutdown", StatusValue::None, false);
    }

    /// Run a global expression (setup, online, offline, shutdown).
    pub fn run_global(&self, kind: &str) -> Option<Dynamic> {
        let compiled = self.global_compiled.read().get(kind).cloned()?;
        match self.expressions.run(&compiled, &Context::default()) {
            Ok(v) => Some(v),
            Err(e) => {
                log::error!("{e}");
                None
            }
        }
    }

    // ------------------------------------------------------------------ editing

    /// The number of triggers.
    pub fn len(&self) -> usize {
        self.triggers.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Configurations of all triggers (for saving).
    pub fn configs(&self) -> Vec<TriggerConfig> {
        self.triggers.read().iter().map(|t| t.config.read().clone()).collect()
    }

    /// Views of all triggers (for display).
    pub fn views(&self) -> Vec<TriggerView> {
        self.triggers
            .read()
            .iter()
            .enumerate()
            .map(|(index, t)| {
                let config = t.config.read().clone();
                let s = t.state.lock();
                TriggerView {
                    index,
                    enabled: would_enable(&config, s.on_air, s.custom_enabled),
                    config,
                    playing: s.playing,
                    on_air: s.on_air,
                    tripped: s.tripped,
                    tracking: s.status.as_ref().map(|st| st.device_number()),
                    status_summary: s.status.as_ref().map(|st| describe_status(st)),
                    errors: s.errors.clone(),
                }
            })
            .collect()
    }

    /// Add a trigger at the end (running its setup expression).
    pub fn add(&self, config: TriggerConfig) -> usize {
        let t = self.build_trigger(config);
        let index = {
            let mut triggers = self.triggers.write();
            triggers.push(t.clone());
            triggers.len() - 1
        };
        self.run_trigger_expression(index, &t, "setup", StatusValue::None, false);
        index
    }

    /// Remove a trigger, running its deactivation and shutdown as needed.
    pub fn remove(&self, index: usize) {
        let removed = {
            let mut triggers = self.triggers.write();
            if index < triggers.len() {
                Some(triggers.remove(index))
            } else {
                None
            }
        };
        if let Some(t) = removed {
            self.cleanup_trigger(index, &t);
        }
    }

    /// Replace a trigger's configuration. If expressions changed they are recompiled;
    /// a changed setup expression is re-run (after the shutdown expression).
    pub fn update(&self, index: usize, config: TriggerConfig) {
        let Some(t) = self.triggers.read().get(index).cloned() else { return };
        let old = t.config.read().clone();
        if old == config {
            return;
        }
        let setup_changed = old.expression("setup") != config.expression("setup");
        if setup_changed {
            self.run_trigger_expression(index, &t, "shutdown", StatusValue::None, false);
        }
        *t.config.write() = config.clone();
        if old.expressions != config.expressions {
            self.compile_trigger(&t);
        }
        if setup_changed {
            self.run_trigger_expression(index, &t, "setup", StatusValue::None, false);
        }
        // Changing what a trigger watches or sends deactivates it; re-evaluate.
        let behavior_changed = old.players != config.players
            || old.message != config.message
            || old.note != config.note
            || old.channel != config.channel
            || old.output != config.output;
        let (playing, on_air, status) = {
            let s = t.state.lock();
            (s.playing, s.on_air, s.status.clone())
        };
        if behavior_changed && t.state.lock().tripped {
            self.report_deactivation(index, &t, status.clone(), true);
            t.state.lock().tripped = false;
            self.stop_clock(&t);
            t.state.lock().last_match = None;
        }
        self.update_player_state(index, &t, playing, on_air, None);
    }

    // ------------------------------------------------------------------ running expressions

    fn context_for(&self, index: usize, t: &Trigger, status: StatusValue) -> Context {
        let config = t.config.read().clone();
        let tripped = t.state.lock().tripped;
        let player = match &status {
            StatusValue::Cdj(s) => Some(s.device_number()),
            StatusValue::Beat(b) => Some(b.device_number()),
            _ => None,
        };
        let env = self.env();
        let mut extra = BTreeMap::new();
        extra.insert("trigger_index".into(), Dynamic::from(index as i64 + 1));
        extra.insert("trigger_comment".into(), Dynamic::from(config.comment.clone()));
        extra.insert("trigger_output".into(), Dynamic::from(config.output.clone().unwrap_or_default()));
        extra.insert("trigger_message".into(), Dynamic::from(config.message.label().to_string()));
        extra.insert("trigger_note".into(), Dynamic::from(i64::from(config.note)));
        extra.insert("trigger_channel".into(), Dynamic::from(i64::from(config.channel)));
        extra.insert("trigger_enabled".into(), Dynamic::from(config.enabled.label().to_string()));
        extra.insert("trigger_active".into(), Dynamic::from(tripped));
        Context {
            track: player.and_then(|p| env.track_data(p)),
            position_ms: player.and_then(|p| env.position_ms(p)),
            status,
            locals: Some(t.locals.clone()),
            extra,
        }
    }

    fn run_trigger_expression(&self, index: usize, t: &Trigger, kind: &str, status: StatusValue, _alert: bool) -> Option<Dynamic> {
        let compiled = t.state.lock().compiled.get(kind).cloned()?;
        let ctx = self.context_for(index, t, status);
        match self.expressions.run(&compiled, &ctx) {
            Ok(v) => Some(v),
            Err(e) => {
                log::error!("Problem running {e}");
                None
            }
        }
    }

    /// Run an expression with a simulated status, as the editor's Simulate buttons do.
    /// Returns the expression's result, or an error message.
    pub fn simulate(&self, index: usize, kind: &str, status: StatusValue) -> Result<Dynamic, String> {
        let t = self.triggers.read().get(index).cloned().ok_or("No such trigger")?;
        let compiled = t.state.lock().compiled.get(kind).cloned().ok_or("No expression of that kind")?;
        let ctx = self.context_for(index, &t, status);
        self.expressions.run(&compiled, &ctx).map_err(|e| e.to_string())
    }

    /// Send the activation or deactivation message for a trigger without involving
    /// Carabiner or MIDI clock (the "Simulate" menu items).
    pub fn simulate_message(&self, index: usize, activate: bool) {
        let Some(t) = self.triggers.read().get(index).cloned() else { return };
        let status = t.state.lock().status.clone();
        if activate {
            self.report_activation(index, &t, status, false);
        } else {
            self.report_deactivation(index, &t, status, false);
        }
    }

    // ------------------------------------------------------------------ status handling

    fn is_better_match(&self, index: usize, t: &Trigger, status: &Arc<CdjStatus>) -> bool {
        let config = t.config.read().clone();
        let custom_result = if config.enabled == EnabledMode::Custom {
            self.run_trigger_expression(index, t, "enabled", StatusValue::Cdj(status.clone()), false).map(|v| truthy(&v))
        } else {
            None
        };
        let enable = would_enable(&config, status.is_on_air(), custom_result);
        let this_device = status.device_number();
        let score = if enable { 1024 } else { 0 } + if status.is_playing() { 512 } else { 0 } - i32::from(this_device);
        let env = self.env();
        let mut s = t.state.lock();
        let (existing_score, existing_device) = match s.last_match {
            Some((sc, d)) => (sc, Some(d)),
            None => (-256, None),
        };
        let best = existing_device.map(|d| !env.device_present(d)).unwrap_or(false) || score >= existing_score;
        if best || Some(this_device) == existing_device {
            s.last_match = Some((score, this_device));
        }
        if best && config.enabled == EnabledMode::Custom {
            s.custom_enabled = custom_result;
        }
        best
    }

    fn matches(&self, index: usize, t: &Trigger, status: &Arc<CdjStatus>) -> bool {
        let players = t.config.read().players;
        match players {
            PlayerChoice::Player(n) => n == status.device_number(),
            PlayerChoice::Master => status.is_tempo_master(),
            PlayerChoice::Any => self.is_better_match(index, t, status),
        }
    }

    /// Process a status packet from a player (the `status-listener`).
    pub fn handle_status(&self, status: &Arc<CdjStatus>) {
        let triggers = self.triggers.read().clone();
        for (index, t) in triggers.iter().enumerate() {
            if self.matches(index, t, status) {
                let players = t.config.read().players;
                if players != PlayerChoice::Any && t.config.read().enabled == EnabledMode::Custom {
                    let result = self
                        .run_trigger_expression(index, t, "enabled", StatusValue::Cdj(status.clone()), false)
                        .map(|v| truthy(&v));
                    t.state.lock().custom_enabled = result;
                }
                self.update_player_state(index, t, status.is_playing(), status.is_on_air(), Some(status.clone()));
            }
        }
    }

    /// Process a beat packet (the `beat-listener`).
    pub fn handle_beat(&self, beat: &Arc<Beat>) {
        let triggers = self.triggers.read().clone();
        let env = self.env();
        for (index, t) in triggers.iter().enumerate() {
            let config = t.config.read().clone();
            let (last_match, tripped) = {
                let s = t.state.lock();
                (s.last_match, s.tripped)
            };
            let matched = match config.players {
                PlayerChoice::Player(n) => n == beat.device_number(),
                PlayerChoice::Master => beat.tempo_master || env.is_master(beat.device_number()),
                PlayerChoice::Any => last_match.map(|(_, d)| d) == Some(beat.device_number()),
            };
            if matched {
                self.run_trigger_expression(index, t, "beat", StatusValue::Beat(beat.clone()), false);
                if tripped && config.message == MessageKind::Link {
                    if let Some(c) = self.sync_triggers() {
                        c.beat_at_time((beat.timestamp() / 1000) as i64, if config.bar { Some(beat.beat_within_bar()) } else { None });
                    }
                }
            }
        }
    }

    /// A device left the network: triggers tracking it stop playing.
    pub fn device_lost(&self, number: u8) {
        let triggers = self.triggers.read().clone();
        for (index, t) in triggers.iter().enumerate() {
            let tracking = t.state.lock().status.as_ref().map(|s| s.device_number()) == Some(number);
            if tracking {
                {
                    let mut s = t.state.lock();
                    s.status = None;
                    if s.last_match.map(|(_, d)| d) == Some(number) {
                        s.last_match = None;
                    }
                }
                self.update_player_state(index, t, false, false, None);
            }
        }
    }

    fn update_player_state(&self, index: usize, t: &Arc<Trigger>, playing: bool, on_air: bool, status: Option<Arc<CdjStatus>>) {
        let config = t.config.read().clone();
        let (old_tripped, tripped, current_status) = {
            let mut s = t.state.lock();
            let old = s.tripped;
            s.playing = playing;
            s.on_air = on_air;
            if let Some(st) = &status {
                s.status = Some(st.clone());
            }
            s.tripped = playing && would_enable(&config, on_air, s.custom_enabled);
            (old, s.tripped, s.status.clone())
        };
        if tripped != old_tripped {
            if tripped {
                self.report_activation(index, t, current_status.clone(), true);
            } else {
                self.report_deactivation(index, t, current_status.clone(), true);
            }
        }
        if tripped && config.message == MessageKind::Link {
            if let (Some(c), Some(st)) = (self.sync_triggers(), current_status.as_ref()) {
                let tempo = self.sync_tempo(st);
                if Carabiner::valid_tempo(tempo) {
                    let _ = c.lock_tempo(tempo);
                } else {
                    c.unlock_tempo();
                }
            }
        }
        if config.message == MessageKind::Clock && tripped {
            self.start_clock(t);
        } else {
            self.stop_clock(t);
        }
        if let Some(st) = status {
            self.run_trigger_expression(index, t, "tracked", StatusValue::Cdj(st), false);
        }
    }

    /// The tempo used for Link and MIDI clock, honoring the `use_fixed_sync_bpm` global.
    fn sync_tempo(&self, status: &CdjStatus) -> f64 {
        let fixed = self.expressions.globals.get("use_fixed_sync_bpm");
        let fixed = fixed.as_float().ok().or_else(|| fixed.as_int().ok().map(|i| i as f64));
        match fixed {
            Some(bpm) => pitch_to_multiplier(status.pitch) * bpm,
            None => status.effective_tempo(),
        }
    }

    fn report_activation(&self, index: usize, t: &Arc<Trigger>, status: Option<Arc<CdjStatus>>, real: bool) {
        let config = t.config.read().clone();
        log::debug!("Reporting activation: {:?} {} on channel {}", config.message, config.note, config.channel);
        if let Some(output) = &config.output {
            let midi = &self.outputs.midi;
            let result = match config.message {
                MessageKind::Note => midi.note_on(output, config.note, 127, config.channel),
                MessageKind::Cc => midi.control(output, config.note, 127, config.channel),
                MessageKind::Clock if config.send && real => match config.start {
                    ClockStart::Start => midi.start(output),
                    ClockStart::Continue => midi.continue_(output),
                },
                _ => Ok(()),
            };
            if let Err(e) = result {
                log::error!("Problem reporting player activation: {e}");
            }
        }
        if real && config.message == MessageKind::Link && config.start_stop {
            if let Some(c) = self.sync_triggers() {
                c.start_transport();
            }
        }
        let status = status.map(StatusValue::Cdj).unwrap_or_default();
        self.run_trigger_expression(index, t, "activation", status, !real);
    }

    fn report_deactivation(&self, index: usize, t: &Arc<Trigger>, status: Option<Arc<CdjStatus>>, real: bool) {
        let config = t.config.read().clone();
        log::debug!("Reporting deactivation: {:?} {} on channel {}", config.message, config.note, config.channel);
        if let Some(output) = &config.output {
            let midi = &self.outputs.midi;
            let result = match config.message {
                MessageKind::Note => midi.note_off(output, config.note, config.channel),
                MessageKind::Cc => midi.control(output, config.note, 0, config.channel),
                MessageKind::Clock if config.stop && real => midi.stop(output),
                _ => Ok(()),
            };
            if let Err(e) = result {
                log::error!("Problem reporting player deactivation: {e}");
            }
        }
        if real && config.message == MessageKind::Link {
            if let Some(c) = self.sync_triggers() {
                c.unlock_tempo();
                if config.start_stop {
                    c.stop_transport();
                }
            }
        }
        let status = status.map(StatusValue::Cdj).unwrap_or_default();
        self.run_trigger_expression(index, t, "deactivation", status, !real);
    }

    // ------------------------------------------------------------------ MIDI clock

    fn clock_tempo(&self, t: &Trigger) -> f64 {
        let status = t.state.lock().status.clone();
        let fixed = self.expressions.globals.get("use_fixed_sync_bpm");
        let fixed = fixed.as_float().ok().or_else(|| fixed.as_int().ok().map(|i| i as f64));
        let base = match status {
            Some(s) if s.has_bpm() => self.sync_tempo(&s),
            _ => fixed.unwrap_or(120.0),
        };
        base * 24.0
    }

    fn start_clock(&self, t: &Arc<Trigger>) {
        let tempo = self.clock_tempo(t);
        let sensitivity = *self.midi_clock_sensitivity.lock();
        let mut s = t.state.lock();
        if let Some(clock) = &s.clock {
            if !clock.thread.is_finished() {
                if (tempo - clock.metronome.tempo()).abs() > sensitivity {
                    clock.metronome.set_tempo(tempo);
                    clock.thread.thread().unpark();
                }
                return;
            }
        }
        let running = Arc::new(AtomicBool::new(true));
        let metronome = Arc::new(Metronome::new());
        metronome.set_tempo(tempo);
        let output = t.config.read().output.clone();
        let outputs = self.outputs.clone();
        let (r, m) = (running.clone(), metronome.clone());
        let trigger = Arc::downgrade(t);
        let thread = thread::Builder::new()
            .name("MIDI clock sender".into())
            .spawn(move || {
                log::info!("MIDI clock thread starting");
                let mut last_beat = None;
                while r.load(Ordering::SeqCst) {
                    let Some(t) = trigger.upgrade() else { break };
                    let output = t.config.read().output.clone().or_else(|| output.clone());
                    drop(t);
                    let snap = m.snapshot();
                    let beat = snap.beat();
                    if let Some(out) = &output {
                        if last_beat != Some(beat) {
                            if let Err(e) = outputs.midi.clock(out) {
                                log::debug!("MIDI clock send failed: {e}");
                            }
                            last_beat = Some(beat);
                        }
                    }
                    let sleep = snap.time_of_beat(beat + 1) - now_ms();
                    if sleep > 5.0 {
                        thread::park_timeout(Duration::from_secs_f64((sleep - 5.0) / 1000.0));
                    } else if sleep > 0.0 {
                        let target = Instant::now() + Duration::from_secs_f64(sleep / 1000.0);
                        while Instant::now() < target {
                            std::hint::spin_loop();
                        }
                    }
                }
                log::info!("MIDI clock thread ending");
            })
            .expect("spawn clock thread");
        s.clock = Some(ClockThread { running, metronome, thread });
    }

    fn stop_clock(&self, t: &Trigger) {
        if let Some(clock) = t.state.lock().clock.take() {
            clock.running.store(false, Ordering::SeqCst);
            clock.thread.thread().unpark();
        }
    }

    /// Whether a trigger is currently sending MIDI clock.
    pub fn clock_running(&self, index: usize) -> bool {
        self.triggers
            .read()
            .get(index)
            .map(|t| t.state.lock().clock.as_ref().map(|c| !c.thread.is_finished()).unwrap_or(false))
            .unwrap_or(false)
    }

    /// Locals of a trigger (for inspection).
    pub fn locals(&self, index: usize) -> Option<Store> {
        self.triggers.read().get(index).map(|t| t.locals.clone())
    }
}

/// A one-line description of a player's status, like the label on each trigger.
pub fn describe_status(s: &CdjStatus) -> String {
    let bpm = if s.has_bpm() { format!("{:.1}", s.effective_tempo()) } else { "--.-".into() };
    format!(
        "{}{}, Track #{}, {} BPM ({:+.2}%), beat {}",
        if s.is_playing() { "Playing" } else { "Stopped" },
        if s.is_tempo_master() { ", Master" } else { "" },
        s.track_number(),
        bpm,
        beat_link::util::pitch_to_percentage(s.pitch),
        s.beat_number_or_negative()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use beat_link::packets::{CdjStatusBuilder, TrackType};

    struct TestEnv;
    impl Environment for TestEnv {
        fn device_present(&self, n: u8) -> bool {
            (1..=4).contains(&n)
        }
        fn track_data(&self, _: u8) -> Option<Arc<TrackData>> {
            None
        }
        fn position_ms(&self, _: u8) -> Option<i64> {
            None
        }
        fn is_master(&self, _: u8) -> bool {
            false
        }
    }

    fn engine() -> Arc<TriggerEngine> {
        let outputs = Arc::new(Outputs::new_for_tests());
        let e = TriggerEngine::new(Arc::new(ExpressionEngine::new(outputs.clone())), outputs);
        e.set_environment(Arc::new(TestEnv));
        e
    }

    fn status(n: u8, playing: bool, on_air: bool, master: bool) -> Arc<CdjStatus> {
        Arc::new(
            CdjStatusBuilder {
                device_number: n,
                playing,
                on_air,
                master,
                bpm: 12000,
                track_type: TrackType::Rekordbox,
                rekordbox_id: 1,
                beat_number: Some(1),
                ..Default::default()
            }
            .build(),
        )
    }

    fn settings(triggers: Vec<TriggerConfig>) -> Settings {
        Settings { triggers, ..Default::default() }
    }

    #[test]
    fn note_trigger_activates_and_deactivates() {
        let e = engine();
        let t = TriggerConfig {
            players: PlayerChoice::Player(2),
            output: Some("Out".into()),
            note: 60,
            channel: 3,
            enabled: EnabledMode::Always,
            ..Default::default()
        };
        e.load(&settings(vec![t]));
        e.handle_status(&status(1, true, true, false));
        assert!(e.outputs.midi.history().is_empty(), "other players are ignored");
        e.handle_status(&status(2, true, true, false));
        e.handle_status(&status(2, true, true, false));
        e.handle_status(&status(2, false, true, false));
        let h: Vec<Vec<u8>> = e.outputs.midi.history().into_iter().map(|(_, b)| b).collect();
        assert_eq!(h, vec![vec![0x92, 60, 127], vec![0x82, 60, 0]]);
    }

    #[test]
    fn on_air_and_never_modes() {
        let e = engine();
        let on_air = TriggerConfig { players: PlayerChoice::Player(1), output: Some("A".into()), enabled: EnabledMode::OnAir, message: MessageKind::Cc, note: 7, ..Default::default() };
        let never = TriggerConfig { players: PlayerChoice::Player(1), output: Some("B".into()), enabled: EnabledMode::Never, ..Default::default() };
        e.load(&settings(vec![on_air, never]));
        e.handle_status(&status(1, true, false, false));
        assert!(e.outputs.midi.history().is_empty());
        e.handle_status(&status(1, true, true, false));
        let h = e.outputs.midi.history();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0], ("A".to_string(), vec![0xb0, 7, 127]));
        e.handle_status(&status(1, true, false, false));
        assert_eq!(e.outputs.midi.history()[1], ("A".to_string(), vec![0xb0, 7, 0]));
    }

    #[test]
    fn master_player_choice() {
        let e = engine();
        let t = TriggerConfig { players: PlayerChoice::Master, output: Some("M".into()), enabled: EnabledMode::Always, ..Default::default() };
        e.load(&settings(vec![t]));
        e.handle_status(&status(1, true, true, false));
        assert!(e.outputs.midi.history().is_empty());
        e.handle_status(&status(3, true, true, true));
        assert_eq!(e.outputs.midi.history().len(), 1);
        assert_eq!(e.views()[0].tracking, Some(3));
    }

    #[test]
    fn any_player_prefers_enabled_and_playing() {
        let e = engine();
        let t = TriggerConfig { players: PlayerChoice::Any, output: Some("X".into()), enabled: EnabledMode::OnAir, ..Default::default() };
        e.load(&settings(vec![t]));
        // Player 1 is stopped and off air; player 2 is playing on air: 2 wins.
        e.handle_status(&status(1, false, false, false));
        e.handle_status(&status(2, true, true, false));
        assert_eq!(e.views()[0].tracking, Some(2));
        assert!(e.views()[0].tripped);
        // A worse match from player 1 does not steal tracking.
        e.handle_status(&status(1, false, false, false));
        assert_eq!(e.views()[0].tracking, Some(2));
        assert!(e.views()[0].tripped);
    }

    #[test]
    fn custom_enabled_and_expressions() {
        let e = engine();
        let mut t = TriggerConfig { players: PlayerChoice::Player(1), enabled: EnabledMode::Custom, ..Default::default() };
        t.expressions.insert("setup".into(), "locals[\"activations\"] = 0;".into());
        t.expressions.insert("enabled".into(), "effective_tempo > 110.0 && beat_number > 0".into());
        t.expressions.insert("activation".into(), "locals.increment(\"activations\"); globals[\"last\"] = trigger_comment;".into());
        t.expressions.insert("tracked".into(), "locals[\"active\"] = trigger_active;".into());
        t.comment = "custom".into();
        e.load(&settings(vec![t]));
        e.handle_status(&status(1, true, false, false));
        let locals = e.locals(0).unwrap();
        assert_eq!(locals.get("activations").as_int().unwrap(), 1);
        assert!(locals.get("active").as_bool().unwrap());
        assert_eq!(e.expressions.globals.get("last").to_string(), "custom");
    }

    #[test]
    fn device_lost_deactivates() {
        let e = engine();
        let t = TriggerConfig { players: PlayerChoice::Player(4), output: Some("O".into()), enabled: EnabledMode::Always, ..Default::default() };
        e.load(&settings(vec![t]));
        e.handle_status(&status(4, true, true, false));
        e.device_lost(4);
        let h = e.outputs.midi.history();
        assert_eq!(h.len(), 2);
        assert_eq!(h[1].1[0], 0x80);
        assert!(!e.views()[0].tripped);
    }

    #[test]
    fn clock_trigger_sends_start_clock_and_stop() {
        let e = engine();
        let t = TriggerConfig { players: PlayerChoice::Player(1), output: Some("C".into()), enabled: EnabledMode::Always, message: MessageKind::Clock, ..Default::default() };
        e.load(&settings(vec![t]));
        e.handle_status(&status(1, true, true, false));
        assert!(e.clock_running(0));
        thread::sleep(Duration::from_millis(120));
        e.handle_status(&status(1, false, true, false));
        thread::sleep(Duration::from_millis(30));
        let h: Vec<u8> = e.outputs.midi.history().into_iter().map(|(_, b)| b[0]).collect();
        assert_eq!(h[0], 0xfa, "start first");
        assert_eq!(*h.last().unwrap(), 0xfc, "stop last");
        let clocks = h.iter().filter(|b| **b == 0xf8).count();
        // 120 BPM * 24 = 48 pulses per second, so ~5 in 120ms.
        assert!((3..=9).contains(&clocks), "got {clocks} clock pulses");
        assert!(!e.clock_running(0));
    }

    #[test]
    fn update_recompiles_and_reports_errors() {
        let e = engine();
        e.load(&settings(vec![TriggerConfig::default()]));
        let mut c = e.configs()[0].clone();
        c.expressions.insert("beat".into(), "let = broken".into());
        e.update(0, c);
        assert!(e.views()[0].errors.contains_key("beat"));
    }
}
