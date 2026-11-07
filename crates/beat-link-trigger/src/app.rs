//! The running application: owns the protocol stack, simulator, trigger engine,
//! shows, Carabiner connection, overlay server and playlist writer, and routes
//! events between them. Replaces the global state spread across `core.clj`,
//! `triggers.clj` and friends.

use crate::carabiner::Carabiner;
use crate::config::{Settings, SyncMode};
use crate::expressions::ExpressionEngine;
use crate::outputs::{Outputs, PlayerCommand};
use crate::overlay::{DeviceInfo, OverlayServer, OverlaySource};
use crate::playlist::{PlaylistOptions, PlaylistWriter};
use crate::shows::{Show, ShowRunner};
use crate::simulator::{SimEvent, Simulator};
use crate::triggers::{Environment, TriggerEngine};
use beat_link::packets::{Beat, CdjStatus, DeviceUpdate};
use beat_link::{NetworkConfig, ProDjLink, TrackData, VirtualCdjSettings};
use parking_lot::{Mutex, RwLock};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;

/// Whether we are connected to a DJ Link network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnlineState {
    Offline,
    /// Looking for devices / claiming a device number.
    Connecting,
    Online,
}

/// Options for constructing the application.
#[derive(Debug, Clone, Default)]
pub struct AppOptions {
    /// Network configuration (ports, forced interface) for the protocol stack.
    pub network: NetworkConfig,
    /// Device number to claim (0 for automatic).
    pub device_number: u8,
}

/// The application.
pub struct App {
    me: Weak<App>,
    pub settings: RwLock<Settings>,
    pub settings_path: RwLock<Option<PathBuf>>,
    pub link: Arc<ProDjLink>,
    pub outputs: Arc<Outputs>,
    pub expressions: Arc<ExpressionEngine>,
    pub triggers: Arc<TriggerEngine>,
    pub carabiner: Arc<Carabiner>,
    pub simulator: Arc<Simulator>,
    pub shows: RwLock<Vec<Arc<ShowRunner>>>,
    overlay: Mutex<Option<OverlayServer>>,
    playlist: Mutex<Option<PlaylistWriter>>,
    state: Mutex<OnlineState>,
    want_online: AtomicBool,
    options: AppOptions,
}

struct AppEnvironment(Weak<App>);

impl Environment for AppEnvironment {
    fn device_present(&self, n: u8) -> bool {
        self.0.upgrade().map(|a| a.device_present(n)).unwrap_or(false)
    }
    fn track_data(&self, p: u8) -> Option<Arc<TrackData>> {
        self.0.upgrade().and_then(|a| a.track_data(p))
    }
    fn position_ms(&self, p: u8) -> Option<i64> {
        self.0.upgrade().and_then(|a| a.position_ms(p))
    }
    fn is_master(&self, p: u8) -> bool {
        self.0.upgrade().and_then(|a| a.simulator.player(p)).map(|s| s.master).unwrap_or(false)
    }
}

impl App {
    /// Build the application with settings (does not go online).
    pub fn new(settings: Settings, options: AppOptions) -> Arc<App> {
        let outputs = Arc::new(Outputs::new());
        let expressions = Arc::new(ExpressionEngine::new(outputs.clone()));
        let triggers = TriggerEngine::new(expressions.clone(), outputs.clone());
        let link = ProDjLink::new(options.network.clone());
        let carabiner = Carabiner::new(link.virtual_cdj.clone(), &link.beat_finder);
        let app = Arc::new_cyclic(|me: &Weak<App>| {
            let sink_app = me.clone();
            let simulator = Simulator::start(Arc::new(move |e: SimEvent| {
                if let Some(app) = sink_app.upgrade() {
                    app.handle_sim_event(e);
                }
            }));
            App {
                me: me.clone(),
                settings: RwLock::new(settings.clone()),
                settings_path: RwLock::new(None),
                link,
                outputs,
                expressions,
                triggers,
                carabiner,
                simulator,
                shows: RwLock::new(Vec::new()),
                overlay: Mutex::new(None),
                playlist: Mutex::new(None),
                state: Mutex::new(OnlineState::Offline),
                want_online: AtomicBool::new(false),
                options,
            }
        });
        app.wire();
        app.triggers.set_environment(Arc::new(AppEnvironment(app.me.clone())));
        app.triggers.set_carabiner(Some(app.carabiner.clone()));
        app.carabiner.configure(&settings.carabiner);
        app.triggers.load(&settings);
        for path in settings.shows.clone() {
            if let Err(e) = app.open_show(&path) {
                log::error!("Unable to open show {path:?}: {e}");
            }
        }
        app.apply_services();
        app
    }

    fn wire(&self) {
        let w = self.me.clone();
        self.link.virtual_cdj.update.add(move |u: &DeviceUpdate| {
            if let (Some(app), DeviceUpdate::Cdj(s)) = (w.upgrade(), u) {
                app.handle_status(s);
            }
        });
        let w = self.me.clone();
        self.link.beat_finder.beat.add(move |b: &Arc<Beat>| {
            if let Some(app) = w.upgrade() {
                app.handle_beat(b);
            }
        });
        let w = self.me.clone();
        self.link.device_finder.device_lost.add(move |a| {
            if let Some(app) = w.upgrade() {
                app.device_lost(a.number);
            }
        });
        let w = self.me.clone();
        self.link.metadata_finder.track_update.add(move |u| {
            if let Some(app) = w.upgrade() {
                let status = app.link.virtual_cdj.latest_cdj_status(u.player);
                for show in app.shows.read().iter() {
                    show.update_player(u.player, u.data.as_ref(), status.as_ref(), None);
                }
            }
        });
        let w = self.me.clone();
        self.outputs.set_command_handler(move |cmd| {
            let app = w.upgrade().ok_or("Application shutting down")?;
            app.player_command(cmd)
        });
        let w = self.me.clone();
        self.outputs.set_time_query(move |p| w.upgrade().and_then(|a| a.position_ms(p)));
        let w = self.me.clone();
        self.carabiner.disconnected.add(move |unexpected| {
            if *unexpected {
                log::warn!("Carabiner connection closed unexpectedly.");
            }
            if let Some(app) = w.upgrade() {
                app.settings.write().carabiner.sync_mode = SyncMode::Off;
            }
        });
    }

    // ------------------------------------------------------------------ event routing

    fn handle_status(&self, status: &Arc<CdjStatus>) {
        self.triggers.handle_status(status);
        let player = status.device_number();
        let data = self.track_data(player);
        for show in self.shows.read().iter() {
            show.update_player(player, data.as_ref(), Some(status), None);
        }
        if let Some(p) = self.playlist.lock().as_ref() {
            p.handle_status(status, data);
        }
    }

    fn handle_beat(&self, beat: &Arc<Beat>) {
        self.triggers.handle_beat(beat);
    }

    fn device_lost(&self, number: u8) {
        self.triggers.device_lost(number);
        for show in self.shows.read().iter() {
            show.update_player(number, None, None, None);
        }
        let lost_last = self.is_online() && self.link.device_finder.current_devices().is_empty();
        if lost_last {
            log::warn!("Lost the last DJ Link device; going offline and looking for the network again.");
            let w = self.me.clone();
            thread::spawn(move || {
                if let Some(app) = w.upgrade() {
                    app.go_offline_internal();
                    thread::sleep(Duration::from_millis(500));
                    if app.want_online.load(Ordering::SeqCst) {
                        app.go_online();
                    }
                }
            });
        }
    }

    fn handle_sim_event(&self, e: SimEvent) {
        match e {
            SimEvent::Status(s) => self.handle_status(&s),
            SimEvent::Beat(b) => self.handle_beat(&b),
            SimEvent::Removed(n) => self.device_lost(n),
        }
    }

    fn player_command(&self, cmd: PlayerCommand) -> Result<(), String> {
        let vcdj = &self.link.virtual_cdj;
        match cmd {
            PlayerCommand::SyncMode(p, s) => vcdj.send_sync_mode_command(p, s).map_err(|e| e.to_string()),
            PlayerCommand::AppointMaster(p) => vcdj.appoint_tempo_master(p).map_err(|e| e.to_string()),
            PlayerCommand::FaderStart(start, stop) => {
                // Simulated players respond to fader start too.
                for n in &start {
                    self.simulator.update(*n, |p| p.playing = true);
                }
                for n in &stop {
                    self.simulator.update(*n, |p| p.playing = false);
                }
                if vcdj.is_running() {
                    vcdj.send_fader_start_command(&start, &stop).map_err(|e| e.to_string())
                } else {
                    Ok(())
                }
            }
            PlayerCommand::OnAir(set) => vcdj.send_on_air_command(&set).map_err(|e| e.to_string()),
            PlayerCommand::LinkTempo(bpm) => self.carabiner.set_link_tempo(bpm),
        }
    }

    // ------------------------------------------------------------------ queries

    pub fn device_present(&self, n: u8) -> bool {
        self.simulator.player(n).is_some() || (self.is_online() && self.link.device_finder.latest_announcement_from(n).is_some())
    }

    pub fn track_data(&self, player: u8) -> Option<Arc<TrackData>> {
        match self.simulator.player(player) {
            Some(p) => p.track_data(),
            None => self.link.metadata_finder.latest_track_data(player),
        }
    }

    pub fn position_ms(&self, player: u8) -> Option<i64> {
        match self.simulator.player(player) {
            Some(p) if p.track.is_some() => Some(p.position_ms as i64),
            Some(_) => None,
            None => self.link.time_finder.time_for(player),
        }
    }

    /// The latest status of a player (real or simulated).
    pub fn latest_status(&self, player: u8) -> Option<Arc<CdjStatus>> {
        match self.simulator.player(player) {
            Some(mut p) => Some(p.status()),
            None => self.link.virtual_cdj.latest_cdj_status(player),
        }
    }

    // ------------------------------------------------------------------ online / offline

    pub fn online_state(&self) -> OnlineState {
        *self.state.lock()
    }

    pub fn is_online(&self) -> bool {
        self.online_state() == OnlineState::Online
    }

    /// Start trying to go online (in the background, retrying until it works or
    /// [`App::go_offline`] is called).
    pub fn go_online(&self) {
        self.want_online.store(true, Ordering::SeqCst);
        {
            let mut s = self.state.lock();
            if *s != OnlineState::Offline {
                return;
            }
            *s = OnlineState::Connecting;
        }
        let w = self.me.clone();
        thread::Builder::new()
            .name("Going online".into())
            .spawn(move || loop {
                let Some(app) = w.upgrade() else { break };
                if !app.want_online.load(Ordering::SeqCst) {
                    *app.state.lock() = OnlineState::Offline;
                    break;
                }
                let settings = app.settings.read().clone();
                let vsettings = VirtualCdjSettings {
                    device_name: settings.device_name.clone(),
                    device_number: app.options.device_number,
                    use_standard_player_number: settings.send_status,
                    device_wait: Duration::from_secs(2),
                    ..Default::default()
                };
                match app.link.go_online(vsettings) {
                    Ok(()) => {
                        if settings.send_status {
                            if let Err(e) = app.link.virtual_cdj.set_sending_status(true) {
                                log::warn!("Unable to send status packets: {e}");
                            }
                        }
                        *app.state.lock() = OnlineState::Online;
                        log::info!(
                            "Online as device {} ({}) at {:?}",
                            app.link.virtual_cdj.device_number(),
                            app.link.virtual_cdj.device_name(),
                            app.link.virtual_cdj.local_address()
                        );
                        app.triggers.run_global("online");
                        break;
                    }
                    Err(beat_link::VirtualCdjError::NoDevices) => {
                        drop(app);
                        thread::sleep(Duration::from_millis(500));
                    }
                    Err(e) => {
                        log::warn!("Unable to go online: {e}; will retry.");
                        app.link.go_offline();
                        drop(app);
                        thread::sleep(Duration::from_secs(2));
                    }
                }
            })
            .ok();
    }

    /// Go offline and stop trying to reconnect.
    pub fn go_offline(&self) {
        self.want_online.store(false, Ordering::SeqCst);
        self.go_offline_internal();
    }

    fn go_offline_internal(&self) {
        let was_online = self.is_online();
        if was_online {
            self.triggers.run_global("offline");
        }
        if self.carabiner.state().sync_mode != SyncMode::Off {
            let _ = self.carabiner.set_sync_mode(SyncMode::Off);
        }
        self.link.go_offline();
        *self.state.lock() = OnlineState::Offline;
    }

    // ------------------------------------------------------------------ settings and files

    /// Snapshot the current configuration (with the live trigger list).
    pub fn current_settings(&self) -> Settings {
        let mut s = self.settings.read().clone();
        s.triggers = self.triggers.configs();
        s.shows = self.shows.read().iter().filter_map(|r| r.path.clone()).collect();
        s
    }

    /// Replace the configuration: reloads triggers and restarts services.
    pub fn apply_settings(&self, settings: Settings) {
        *self.settings.write() = settings.clone();
        self.carabiner.configure(&settings.carabiner);
        self.triggers.load(&settings);
        self.apply_services();
    }

    /// Save to the current settings file (or the given one).
    pub fn save(&self, path: Option<&Path>) -> std::io::Result<PathBuf> {
        let path = path.map(Path::to_path_buf).or_else(|| self.settings_path.read().clone()).unwrap_or_else(crate::config::default_preferences_path);
        self.current_settings().save(&path)?;
        *self.settings_path.write() = Some(path.clone());
        Ok(path)
    }

    /// Load a trigger file (`.bltj`, or a `.blt` from the Java version).
    pub fn load_file(&self, path: &Path) -> std::io::Result<()> {
        let settings = Settings::load(path)?;
        self.apply_settings(settings);
        if path.extension().map(|e| e == "bltj").unwrap_or(false) {
            *self.settings_path.write() = Some(path.to_path_buf());
        }
        Ok(())
    }

    /// Open a show file (`.blsj`, or a `.bls` from the Java version).
    pub fn open_show(&self, path: &Path) -> std::io::Result<Arc<ShowRunner>> {
        if let Some(existing) = self.shows.read().iter().find(|s| s.path.as_deref() == Some(path)) {
            return Ok(existing.clone());
        }
        let show = Show::load(path)?;
        let runner = ShowRunner::new(show, Some(path.to_path_buf()), self.expressions.clone(), self.outputs.clone());
        self.shows.write().push(runner.clone());
        Ok(runner)
    }

    /// Create a new, empty show to be saved at `path`.
    pub fn new_show(&self, path: &Path) -> std::io::Result<Arc<ShowRunner>> {
        let show = Show::default();
        let path = path.with_extension(crate::shows::SHOW_FILE_EXTENSION);
        show.save(&path)?;
        self.open_show(&path)
    }

    pub fn close_show(&self, index: usize) {
        let removed = {
            let mut shows = self.shows.write();
            if index < shows.len() {
                Some(shows.remove(index))
            } else {
                None
            }
        };
        if let Some(r) = removed {
            r.shutdown();
        }
    }

    /// Start or stop the overlay server and playlist writer to match settings.
    pub fn apply_services(&self) {
        let settings = self.settings.read().clone();
        {
            let mut overlay = self.overlay.lock();
            let restart = match overlay.as_ref() {
                Some(o) => !settings.overlay.enabled || o.port != settings.overlay.port,
                None => settings.overlay.enabled,
            };
            if restart {
                if let Some(o) = overlay.take() {
                    o.stop();
                }
                if settings.overlay.enabled {
                    let source: Arc<dyn OverlaySource> = Arc::new(AppOverlay(self.me.clone()));
                    match OverlayServer::start(settings.overlay.port, settings.overlay.template.clone(), settings.overlay.public_folder.clone(), source) {
                        Ok(s) => *overlay = Some(s),
                        Err(e) => log::error!("Unable to start OBS overlay server on port {}: {e}", settings.overlay.port),
                    }
                }
            }
        }
        {
            let mut playlist = self.playlist.lock();
            let wanted = settings.playlist.enabled && settings.playlist.file.is_some();
            match (wanted, playlist.is_some()) {
                (true, false) => {
                    let opts = PlaylistOptions { min_play_seconds: settings.playlist.min_play_seconds, on_air_only: false, master_only: settings.playlist.master_only };
                    match PlaylistWriter::open(settings.playlist.file.as_ref().unwrap(), settings.playlist.append, opts) {
                        Ok(w) => *playlist = Some(w),
                        Err(e) => log::error!("Unable to open playlist file: {e}"),
                    }
                }
                (false, true) => {
                    if let Some(w) = playlist.take() {
                        w.close();
                    }
                }
                _ => {}
            }
        }
    }

    pub fn overlay_port(&self) -> Option<u16> {
        self.overlay.lock().as_ref().map(|o| o.port)
    }

    pub fn playlist_file(&self) -> Option<PathBuf> {
        self.playlist.lock().as_ref().map(|p| p.file().to_path_buf())
    }

    /// Generate the expression report HTML.
    pub fn expression_report(&self) -> String {
        let shows: Vec<(String, Show)> = self.shows.read().iter().map(|r| (r.title(), r.show.read().clone())).collect();
        crate::report::build(&self.current_settings(), &shows)
    }

    /// Shut everything down (running shutdown expressions).
    pub fn shutdown(&self) {
        self.go_offline();
        self.simulator.stop();
        let shows: Vec<Arc<ShowRunner>> = std::mem::take(&mut *self.shows.write());
        for s in shows {
            s.shutdown();
        }
        self.triggers.shutdown_all();
        if let Some(o) = self.overlay.lock().take() {
            o.stop();
        }
        if let Some(p) = self.playlist.lock().take() {
            p.close();
        }
        self.carabiner.disconnect();
    }

    /// Information about every device (real or simulated) for status displays.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let mut result = Vec::new();
        for p in self.simulator.players() {
            let mut sp = p.clone();
            result.push(DeviceInfo {
                number: p.number,
                name: "Simulated CDJ".into(),
                address: "127.0.0.1".into(),
                status: Some(sp.status()),
                track: p.track_data(),
                position_ms: if p.track.is_some() { Some(p.position_ms as i64) } else { None },
            });
        }
        if self.is_online() {
            for a in self.link.device_finder.current_devices() {
                if result.iter().any(|d| d.number == a.number) {
                    continue;
                }
                result.push(DeviceInfo {
                    number: a.number,
                    name: a.name.clone(),
                    address: a.address.to_string(),
                    status: self.link.virtual_cdj.latest_cdj_status(a.number),
                    track: self.link.metadata_finder.latest_track_data(a.number),
                    position_ms: self.link.time_finder.time_for(a.number),
                });
            }
        }
        result.sort_by_key(|d| d.number);
        result
    }

    /// The current tempo master's device number, if any.
    pub fn master(&self) -> Option<u8> {
        self.simulator
            .players()
            .iter()
            .find(|p| p.master)
            .map(|p| p.number)
            .or_else(|| if self.is_online() { self.link.virtual_cdj.tempo_master().map(|m| m.device_number()) } else { None })
    }
}

struct AppOverlay(Weak<App>);

impl OverlaySource for AppOverlay {
    fn devices(&self) -> Vec<DeviceInfo> {
        self.0.upgrade().map(|a| a.devices()).unwrap_or_default()
    }
    fn master(&self) -> Option<u8> {
        self.0.upgrade().and_then(|a| a.master())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EnabledMode, PlayerChoice, TriggerConfig};

    #[test]
    fn simulator_drives_triggers_and_overlay_params() {
        let mut settings = Settings::default();
        settings.triggers = vec![TriggerConfig {
            players: PlayerChoice::Master,
            enabled: EnabledMode::Always,
            output: Some("Test".into()),
            expressions: [("beat".to_string(), "globals.increment(\"beats\");".to_string())].into_iter().collect(),
            ..Default::default()
        }];
        let app = App::new(settings, AppOptions::default());
        app.outputs.midi.set_record_only(true);
        let n = app.simulator.add_player().unwrap();
        app.simulator.update(n, |p| {
            p.track = Some(0);
            p.position_ms = 10_000.0;
            p.playing = true;
        });
        thread::sleep(Duration::from_millis(1500));
        let views = app.triggers.views();
        assert!(views[0].tripped, "master trigger tripped by simulated player");
        assert!(app.expressions.globals.get("beats").as_int().unwrap_or(0) >= 1);
        let params = crate::overlay::build_params(&AppOverlay(Arc::downgrade(&app)));
        assert_eq!(params["players"][n.to_string()]["track"]["title"], "Concrete Angel (Original Mix)");
        assert_eq!(params["master"]["number"], n);
        app.simulator.remove_player(n);
        assert!(!app.triggers.views()[0].tripped);
        let midi = app.outputs.midi.history();
        assert_eq!(midi.first().unwrap().1[0], 0x90);
        assert_eq!(midi.last().unwrap().1[0], 0x80);
        app.shutdown();
    }

    #[test]
    fn imported_show_responds_to_simulated_sample_track() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../doc/modules/ROOT/attachments/BreakBuddy.bls");
        if !path.exists() {
            return;
        }
        let app = App::new(Settings::default(), AppOptions::default());
        let runner = app.open_show(&path).unwrap();
        let n = app.simulator.add_player().unwrap();
        // Sample track 1 has the signature of the BreakBuddy show's track.
        app.simulator.update(n, |p| {
            p.track = Some(0);
            p.position_ms = 0.0;
        });
        thread::sleep(Duration::from_millis(500));
        let events = runner.take_events();
        assert!(events.contains(&crate::shows::ShowEvent::Loaded("6c8139bd531a417999317a9d024d6d8ea6993507".into())), "{events:?}");
        app.shutdown();
    }
}
