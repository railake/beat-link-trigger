//! Ableton Link integration through the Carabiner daemon. Port of the
//! `beat-carabiner` library and BLT's `carabiner` namespace.
//!
//! Carabiner speaks a line-oriented text protocol on TCP port 17000; responses are
//! a symbol followed by an EDN map, e.g. `status { :peers 1 :bpm 120.0 ... }`.

use crate::config::{CarabinerConfig, SyncMode};
use crate::legacy::{parse_edn_all, Edn};
use beat_link::beat_finder::BeatFinder;
use beat_link::clock::monotonic_micros;
use beat_link::listeners::{ListenerId, ListenerSet};
use beat_link::metronome::find_closest_delta;
use beat_link::packets::{Beat, DeviceUpdate};
use beat_link::virtual_cdj::VirtualCdj;
use parking_lot::Mutex;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;

/// How close the Link tempo must be to our target before we leave it alone.
pub const BPM_TOLERANCE: f64 = 0.00001;
/// How far off a beat (as a fraction of a beat) Link may drift before we nudge it.
pub const SKEW_TOLERANCE: f64 = 0.0166;

/// A snapshot of the Carabiner connection state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CarabinerState {
    pub connected: bool,
    pub link_bpm: f64,
    pub link_peers: i64,
    pub target_bpm: Option<f64>,
    pub sync_mode: SyncMode,
    pub bars: bool,
    pub latency_ms: i64,
    pub version: Option<String>,
}

struct Inner {
    stream: Option<TcpStream>,
    state: CarabinerState,
    /// The last beat probe: (Link time we asked about, beat within bar to align to).
    beat_probe: Option<(i64, Option<u8>)>,
    /// The last phase probe: (Link time, our snapshot then).
    phase_probe: Option<(i64, beat_link::metronome::Snapshot)>,
    running: Option<Arc<AtomicBool>>,
    master_listeners: Vec<ListenerId>,
}

/// Connects BLT to Ableton Link.
pub struct Carabiner {
    me: Weak<Carabiner>,
    inner: Mutex<Inner>,
    virtual_cdj: Arc<VirtualCdj>,
    /// Delivered whenever the state changes.
    pub state_changed: ListenerSet<CarabinerState>,
    /// Delivered with `true` if the connection was lost unexpectedly.
    pub disconnected: ListenerSet<bool>,
}

fn parse_float(e: Option<&Edn>) -> Option<f64> {
    match e? {
        Edn::Float(f) => Some(*f),
        Edn::Int(i) => Some(*i as f64),
        _ => None,
    }
}

impl Carabiner {
    pub fn new(virtual_cdj: Arc<VirtualCdj>, beat_finder: &Arc<BeatFinder>) -> Arc<Self> {
        let c = Arc::new_cyclic(|me| Carabiner {
            me: me.clone(),
            inner: Mutex::new(Inner {
                stream: None,
                state: CarabinerState { latency_ms: 1, ..Default::default() },
                beat_probe: None,
                phase_probe: None,
                running: None,
                master_listeners: Vec::new(),
            }),
            virtual_cdj,
            state_changed: ListenerSet::new(),
            disconnected: ListenerSet::new(),
        });
        let _ = beat_finder;
        // Phase alignment daemon for Full sync mode when we are tempo master.
        let w = c.me.clone();
        thread::Builder::new()
            .name("Beat Carabiner Phase Alignment".into())
            .spawn(move || loop {
                let Some(c) = w.upgrade() else { break };
                let full = c.state().sync_mode == SyncMode::Full;
                if full && c.is_connected() && c.virtual_cdj.is_tempo_master() {
                    c.align_pioneer_phase_to_ableton();
                }
                drop(c);
                thread::sleep(Duration::from_millis(200));
            })
            .ok();
        c
    }

    pub fn state(&self) -> CarabinerState {
        self.inner.lock().state.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.inner.lock().state.connected
    }

    fn publish(&self) {
        let s = self.state();
        self.state_changed.deliver(&s);
    }

    /// Apply the latency and bar alignment settings.
    pub fn configure(&self, config: &CarabinerConfig) {
        let mut inner = self.inner.lock();
        inner.state.latency_ms = config.latency_ms;
        inner.state.bars = config.bars;
        drop(inner);
        self.publish();
    }

    /// Connect to Carabiner.
    pub fn connect(&self, host: &str, port: u16) -> io::Result<()> {
        if self.is_connected() {
            return Ok(());
        }
        let addr: SocketAddr = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address"))?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        stream.set_nodelay(true)?;
        let reader = stream.try_clone()?;
        let running = Arc::new(AtomicBool::new(true));
        {
            let mut inner = self.inner.lock();
            inner.stream = Some(stream);
            inner.state.connected = true;
            inner.running = Some(running.clone());
        }
        let w = self.me.clone();
        thread::Builder::new().name("Carabiner response handler".into()).spawn(move || {
            let mut reader = reader;
            let mut buffer = [0u8; 4096];
            let mut pending = String::new();
            let mut unexpected = false;
            while running.load(Ordering::SeqCst) {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        unexpected = running.load(Ordering::SeqCst);
                        break;
                    }
                    Ok(n) => {
                        pending.push_str(&String::from_utf8_lossy(&buffer[..n]));
                        // Only parse up to the last newline; keep any partial message.
                        if let Some(end) = pending.rfind('\n') {
                            let complete: String = pending.drain(..=end).collect();
                            if let Some(c) = w.upgrade() {
                                c.handle_responses(&complete);
                            }
                        }
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    Err(e) => {
                        log::error!("Problem reading from Carabiner: {e}");
                        unexpected = running.load(Ordering::SeqCst);
                        break;
                    }
                }
            }
            if let Some(c) = w.upgrade() {
                c.cleanup_connection();
                c.disconnected.deliver(&unexpected);
                c.publish();
            }
        })?;
        self.send("version")?;
        self.send("enable-start-stop-sync")?;
        self.send("status")?;
        self.publish();
        Ok(())
    }

    fn cleanup_connection(&self) {
        let listeners = {
            let mut inner = self.inner.lock();
            inner.stream = None;
            inner.state.connected = false;
            inner.state.sync_mode = SyncMode::Off;
            inner.state.target_bpm = None;
            if let Some(r) = inner.running.take() {
                r.store(false, Ordering::SeqCst);
            }
            std::mem::take(&mut inner.master_listeners)
        };
        for id in listeners {
            self.virtual_cdj.master_changed.remove(id);
            self.virtual_cdj.tempo_changed.remove(id);
            self.virtual_cdj.master_beat.remove(id);
        }
    }

    /// Disconnect from Carabiner.
    pub fn disconnect(&self) {
        if let Some(s) = self.inner.lock().stream.as_ref() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        self.cleanup_connection();
        self.publish();
    }

    fn send(&self, message: &str) -> io::Result<()> {
        let mut inner = self.inner.lock();
        let stream = inner.stream.as_mut().ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Carabiner is not connected"))?;
        log::debug!("Sending to Carabiner: {message}");
        stream.write_all(format!("{message}\n").as_bytes())
    }

    /// Process a burst of responses.
    pub fn handle_responses(&self, text: &str) {
        let values = match parse_edn_all(text) {
            Ok(v) => v,
            Err(e) => {
                log::error!("Unable to parse Carabiner response {text:?}: {e}");
                return;
            }
        };
        let mut it = values.into_iter();
        while let Some(cmd) = it.next() {
            let Edn::Symbol(name) = cmd else { continue };
            let arg = it.next().unwrap_or(Edn::Nil);
            match name.as_str() {
                "status" => self.handle_status(&arg),
                "beat-at-time" => self.handle_beat_at_time(&arg),
                "phase-at-time" => self.handle_phase_at_time(&arg),
                "version" => {
                    let v = arg.as_str().unwrap_or("").to_string();
                    log::info!("Connected to Carabiner version {v}");
                    self.inner.lock().state.version = Some(v);
                    self.publish();
                }
                "unsupported" => log::error!("Carabiner does not support command {arg:?}; please upgrade Carabiner."),
                other => log::error!("Unrecognized message from Carabiner: {other}"),
            }
        }
    }

    fn handle_status(&self, status: &Edn) {
        let bpm = parse_float(status.get("bpm")).unwrap_or(0.0);
        let peers = status.get("peers").and_then(Edn::as_i64).unwrap_or(0);
        {
            let mut inner = self.inner.lock();
            inner.state.link_bpm = bpm;
            inner.state.link_peers = peers;
        }
        self.check_link_tempo();
        self.publish();
    }

    fn check_link_tempo(&self) {
        let state = self.state();
        match state.target_bpm {
            Some(target) => {
                if (state.link_bpm - target).abs() > BPM_TOLERANCE {
                    let _ = self.send(&format!("bpm {target}"));
                }
            }
            None => {
                if self.virtual_cdj.is_tempo_master() && state.link_bpm > 0.0 {
                    self.virtual_cdj.set_tempo(state.link_bpm);
                }
            }
        }
    }

    fn handle_beat_at_time(&self, info: &Edn) {
        let Some(beat) = parse_float(info.get("beat")) else { return };
        let when = info.get("when").and_then(Edn::as_i64).unwrap_or(0);
        let raw_beat = beat.round() as i64;
        let raw_skew = beat.rem_euclid(1.0);
        let beat_skew = if raw_skew > 0.5 { raw_skew - 1.0 } else { raw_skew };
        let probe = self.inner.lock().beat_probe;
        let candidate = match probe {
            Some((time, Some(beat_number))) if time == when => {
                let bar_skew = (i64::from(beat_number) - 1) - raw_beat.rem_euclid(4);
                let adjustment = if bar_skew <= -2 { bar_skew + 4 } else { bar_skew };
                raw_beat + adjustment
            }
            _ => raw_beat,
        };
        let target = if candidate < 0 { candidate + 4 } else { candidate };
        if beat_skew.abs() > SKEW_TOLERANCE || target != raw_beat {
            log::debug!("Jumping Link timeline: raw beat {raw_beat}, target {target}, skew {beat_skew:.5}");
            let _ = self.send(&format!("force-beat-at-time {target} {when} 4.0"));
        }
    }

    fn handle_phase_at_time(&self, info: &Edn) {
        let when = info.get("when").and_then(Edn::as_i64).unwrap_or(0);
        let Some(phase) = parse_float(info.get("phase")) else { return };
        let (probe, align_bar) = {
            let inner = self.inner.lock();
            (inner.phase_probe, inner.state.bars)
        };
        let Some((probe_time, snapshot)) = probe else { return };
        if probe_time != when {
            log::warn!("Ignoring phase-at-time response for time {when} since was expecting {probe_time}");
            return;
        }
        let desired = if align_bar { phase / 4.0 } else { phase - phase.trunc() };
        let actual = if align_bar { snapshot.bar_phase() } else { snapshot.beat_phase() };
        let delta = find_closest_delta(desired - actual);
        let interval = if align_bar { snapshot.bar_interval() } else { snapshot.beat_interval() };
        let ms = (delta * interval).trunc();
        if ms.abs() > 0.0 {
            let beat_phase = self.virtual_cdj.playback_position().beat_phase();
            let mut beat_delta = if align_bar { delta * 4.0 } else { delta };
            if beat_delta > 0.0 {
                beat_delta += 0.1;
            }
            if (beat_phase + beat_delta).floor() == 0.0 || beat_delta.abs() > 0.2 {
                log::debug!("Adjusting Pioneer timeline, delta-ms: {ms}");
                self.virtual_cdj.adjust_playback_position(ms);
            }
        }
    }

    /// Whether a tempo is usable by Link.
    pub fn valid_tempo(bpm: f64) -> bool {
        (20.0..=999.0).contains(&bpm)
    }

    /// Hold the Link tempo at a value.
    pub fn lock_tempo(&self, bpm: f64) -> Result<(), String> {
        if self.state().sync_mode == SyncMode::Off {
            return Err("Must be synchronizing to lock tempo.".into());
        }
        if !Self::valid_tempo(bpm) {
            return Err("Tempo must be between 20.0 and 999.0 BPM".into());
        }
        self.inner.lock().state.target_bpm = Some(bpm);
        self.publish();
        self.check_link_tempo();
        Ok(())
    }

    /// Let other Link participants control the tempo.
    pub fn unlock_tempo(&self) {
        self.inner.lock().state.target_bpm = None;
        self.publish();
    }

    /// Ask which Link beat falls at a time (Link microseconds), so we can align it.
    pub fn beat_at_time(&self, time_us: i64, beat_number: Option<u8>) {
        let latency = self.inner.lock().state.latency_ms;
        let adjusted = time_us - latency * 1000;
        self.inner.lock().beat_probe = Some((adjusted, beat_number));
        let _ = self.send(&format!("beat-at-time {adjusted} 4.0"));
    }

    pub fn start_transport(&self) {
        let _ = self.send(&format!("start-playing {}", monotonic_micros()));
    }

    pub fn stop_transport(&self) {
        let _ = self.send(&format!("stop-playing {}", monotonic_micros()));
    }

    /// Set the Link tempo directly (unless it is already close).
    pub fn set_link_tempo(&self, bpm: f64) -> Result<(), String> {
        if !Self::valid_tempo(bpm) {
            return Err("Tempo must be between 20.0 and 999.0 BPM".into());
        }
        if (bpm - self.state().link_bpm).abs() > 0.005 {
            self.send(&format!("bpm {bpm}")).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn align_pioneer_phase_to_ableton(&self) {
        let latency = self.inner.lock().state.latency_ms;
        let now = monotonic_micros() as i64 + latency * 1000;
        let snapshot = self.virtual_cdj.playback_position();
        self.inner.lock().phase_probe = Some((now, snapshot));
        let _ = self.send(&format!("phase-at-time {now} 4.0"));
    }

    fn tie_ableton_to_pioneer(&self) {
        let mut inner = self.inner.lock();
        if !inner.master_listeners.is_empty() {
            return;
        }
        let w = self.me.clone();
        let tempo_id = self.virtual_cdj.tempo_changed.add(move |tempo: &f64| {
            if let Some(c) = w.upgrade() {
                if Carabiner::valid_tempo(*tempo) {
                    let _ = c.lock_tempo(*tempo);
                } else {
                    c.unlock_tempo();
                }
            }
        });
        let w = self.me.clone();
        let beat_id = self.virtual_cdj.master_beat.add(move |beat: &Arc<Beat>| {
            if let Some(c) = w.upgrade() {
                let bars = c.state().bars;
                c.beat_at_time((beat.timestamp() / 1000) as i64, if bars { Some(beat.beat_within_bar()) } else { None });
            }
        });
        inner.master_listeners = vec![tempo_id, beat_id];
        drop(inner);
        let tempo = self.virtual_cdj.master_tempo();
        if Self::valid_tempo(tempo) {
            let _ = self.lock_tempo(tempo);
        }
    }

    fn free_ableton_from_pioneer(&self) {
        let ids = std::mem::take(&mut self.inner.lock().master_listeners);
        for id in ids {
            self.virtual_cdj.tempo_changed.remove(id);
            self.virtual_cdj.master_beat.remove(id);
        }
        self.unlock_tempo();
    }

    fn tie_pioneer_to_ableton(&self) {
        self.free_ableton_from_pioneer();
        self.align_pioneer_phase_to_ableton();
        let bpm = self.state().link_bpm;
        if bpm > 0.0 {
            self.virtual_cdj.set_tempo(bpm);
        }
        self.virtual_cdj.set_playing(true);
        if let Err(e) = self.virtual_cdj.become_tempo_master() {
            log::error!("Unable to become tempo master: {e}");
        }
        let _ = self.send("status");
    }

    fn free_pioneer_from_ableton(&self) {
        self.virtual_cdj.set_playing(false);
        let mode = self.state().sync_mode;
        if matches!(mode, SyncMode::Passive | SyncMode::Full) && self.virtual_cdj.is_synced() {
            self.tie_ableton_to_pioneer();
        }
    }

    /// Tie the Link session to the DJ Link tempo (or release it), reflecting that in
    /// the virtual CDJ's sync state.
    pub fn sync_link(&self, sync: bool) {
        if self.virtual_cdj.is_synced() != sync {
            self.virtual_cdj.set_synced(sync);
        }
        let mode = self.state().sync_mode;
        if matches!(mode, SyncMode::Passive | SyncMode::Full) && !self.virtual_cdj.is_tempo_master() {
            if sync {
                self.tie_ableton_to_pioneer();
            } else {
                self.free_ableton_from_pioneer();
            }
        }
    }

    /// Make Link the tempo master of the DJ Link network (Full mode only), or stop.
    pub fn link_master(&self, master: bool) {
        if master {
            if self.state().sync_mode == SyncMode::Full {
                self.tie_pioneer_to_ableton();
            }
        } else {
            self.free_pioneer_from_ableton();
        }
    }

    /// Change the synchronization mode.
    pub fn set_sync_mode(&self, mode: SyncMode) -> Result<(), String> {
        if mode != SyncMode::Off && !self.is_connected() {
            return Err("Cannot synchronize without an active Carabiner connection.".into());
        }
        if mode != SyncMode::Off && !self.virtual_cdj.is_running() {
            return Err("Cannot synchronize when the Virtual CDJ isn't running.".into());
        }
        if mode == SyncMode::Full && !self.virtual_cdj.is_sending_status() {
            return Err("Cannot use full sync mode when the Virtual CDJ isn't sending status packets.".into());
        }
        self.inner.lock().state.sync_mode = mode;
        if matches!(mode, SyncMode::Passive | SyncMode::Full) {
            self.sync_link(self.virtual_cdj.is_synced() || mode == SyncMode::Passive);
            if mode == SyncMode::Full && self.virtual_cdj.is_tempo_master() {
                self.tie_pioneer_to_ableton();
            }
        } else {
            self.free_ableton_from_pioneer();
            self.free_pioneer_from_ableton();
        }
        self.publish();
        Ok(())
    }

    /// Triggers drive Link only in Manual ("Triggers") mode.
    pub fn sync_triggers(&self) -> bool {
        self.state().sync_mode == SyncMode::Manual
    }

    /// Handle the master changing while we are tied to it (no-op hook for symmetry).
    pub fn master_changed(&self, _update: &Option<DeviceUpdate>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use beat_link::network::NetworkConfig;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn talks_to_a_fake_carabiner() {
        let link = beat_link::ProDjLink::new(NetworkConfig::default());
        let carabiner = Carabiner::new(link.virtual_cdj.clone(), &link.beat_finder);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"version \"1.2.0\"\nstatus { :peers 2 :bpm 121.500000 :start 1 :beat 2.0 }\n").unwrap();
            let mut buf = [0u8; 1024];
            let mut seen = String::new();
            s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                if seen.contains("start-playing") {
                    break;
                }
            }
            tx.send(seen).unwrap();
        });
        carabiner.connect("127.0.0.1", port).unwrap();
        for _ in 0..100 {
            if carabiner.state().link_peers == 2 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let state = carabiner.state();
        assert!(state.connected);
        assert_eq!(state.link_peers, 2);
        assert!((state.link_bpm - 121.5).abs() < 1e-9);
        assert_eq!(state.version.as_deref(), Some("1.2.0"));
        assert!(carabiner.lock_tempo(128.0).is_err(), "cannot lock tempo while sync is off");
        carabiner.start_transport();
        let seen = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(seen.contains("version\n"));
        assert!(seen.contains("enable-start-stop-sync\n"));
        assert!(seen.contains("start-playing "));
        carabiner.disconnect();
        assert!(!carabiner.is_connected());
    }

    #[test]
    fn beat_alignment_jumps_when_skewed() {
        let link = beat_link::ProDjLink::new(NetworkConfig::default());
        let carabiner = Carabiner::new(link.virtual_cdj.clone(), &link.beat_finder);
        // Not connected, so sends fail silently; this exercises the parsing path.
        carabiner.handle_responses("beat-at-time { :when 100 :quantum 4.0 :beat 7.4 }\n");
        carabiner.handle_responses("status { :peers 0 :bpm 99.0 :start 0 :beat 0.0 }\n");
        assert!((carabiner.state().link_bpm - 99.0).abs() < 1e-9);
    }
}
