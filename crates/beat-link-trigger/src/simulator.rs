//! The shallow playback simulator: pretend players that play the bundled sample
//! tracks, producing status and beat packets so triggers, shows and overlays can be
//! built and tested without hardware. Port of `simulator.clj`.

use crate::legacy::{parse_edn, Edn};
use beat_link::data::{BeatGrid, CueList, SongStructure, TrackMetadata, WaveformDetail, WaveformPreview, WaveformStyle};
use beat_link::packets::{Beat, CdjStatus, CdjStatusBuilder, TrackSourceSlot, TrackType};
use beat_link::util::{percentage_to_pitch, pitch_to_multiplier};
use beat_link::TrackData;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

struct SampleFiles {
    metadata: &'static str,
    grid: &'static str,
    signature: &'static str,
    art: &'static [u8],
    preview: &'static [u8],
    detail: &'static [u8],
    cues: &'static [&'static [u8]],
    structure: Option<&'static [u8]>,
}

const SAMPLES: [SampleFiles; 2] = [
    SampleFiles {
        metadata: include_str!("../../../resources/beat_link_trigger/sampleTracks/1/metadata.edn"),
        grid: include_str!("../../../resources/beat_link_trigger/sampleTracks/1/beat-grid.edn"),
        signature: include_str!("../../../resources/beat_link_trigger/sampleTracks/1/signature.edn"),
        art: include_bytes!("../../../resources/beat_link_trigger/sampleTracks/1/art.jpg"),
        preview: include_bytes!("../../../resources/beat_link_trigger/sampleTracks/1/preview-color.data"),
        detail: include_bytes!("../../../resources/beat_link_trigger/sampleTracks/1/detail-color.data"),
        cues: &[
            include_bytes!("../../../resources/beat_link_trigger/sampleTracks/1/cue-list-0.kaitai"),
            include_bytes!("../../../resources/beat_link_trigger/sampleTracks/1/cue-list-1.kaitai"),
        ],
        structure: None,
    },
    SampleFiles {
        metadata: include_str!("../../../resources/beat_link_trigger/sampleTracks/2/metadata.edn"),
        grid: include_str!("../../../resources/beat_link_trigger/sampleTracks/2/beat-grid.edn"),
        signature: include_str!("../../../resources/beat_link_trigger/sampleTracks/2/signature.edn"),
        art: include_bytes!("../../../resources/beat_link_trigger/sampleTracks/2/art.jpg"),
        preview: include_bytes!("../../../resources/beat_link_trigger/sampleTracks/2/preview-color.data"),
        detail: include_bytes!("../../../resources/beat_link_trigger/sampleTracks/2/detail-color.data"),
        cues: &[
            include_bytes!("../../../resources/beat_link_trigger/sampleTracks/2/cue-list-0.kaitai"),
            include_bytes!("../../../resources/beat_link_trigger/sampleTracks/2/cue-list-1.kaitai"),
        ],
        structure: Some(include_bytes!("../../../resources/beat_link_trigger/sampleTracks/2/song-structure.kaitai")),
    },
];

fn edn_label(e: Option<&Edn>) -> Option<beat_link::data::SearchableItem> {
    e.and_then(Edn::as_str).map(|s| beat_link::data::SearchableItem { id: 0, label: s.to_string() })
}

fn load_sample(i: usize, s: &SampleFiles) -> Arc<TrackData> {
    let md = parse_edn(s.metadata).unwrap_or(Edn::Nil);
    let metadata = TrackMetadata {
        title: md.get("title").and_then(Edn::as_str).unwrap_or("").to_string(),
        artist: edn_label(md.get("artist")),
        album: edn_label(md.get("album")),
        genre: edn_label(md.get("genre")),
        key: edn_label(md.get("key")),
        comment: md.get("comment").and_then(Edn::as_str).unwrap_or("").to_string(),
        date_added: md.get("date-added").and_then(Edn::as_str).unwrap_or("").to_string(),
        duration: md.get("duration").and_then(Edn::as_i64).unwrap_or(0) as u32,
        tempo: md.get("tempo").and_then(Edn::as_i64).unwrap_or(0) as u32,
        rating: md.get("rating").and_then(Edn::as_i64).unwrap_or(0) as u32,
        artwork_id: i as u32 + 1,
        ..Default::default()
    };
    let grid = parse_edn(s.grid).ok().and_then(|g| {
        let parts = g.as_seq()?.to_vec();
        let ints = |e: &Edn| -> Vec<i64> { e.as_seq().map(|v| v.iter().filter_map(Edn::as_i64).collect()).unwrap_or_default() };
        let (bars, times, bpms) = (ints(&parts[0]), ints(&parts[1]), ints(&parts[2]));
        Some(BeatGrid::new(bars.iter().map(|b| *b as u16).collect(), bpms.iter().map(|b| *b as u16).collect(), times.iter().map(|t| *t as u32).collect()))
    });
    let mut cues = CueList::default();
    // The Java version saved these tags as bare bodies, without section headers.
    for raw in s.cues {
        cues = cues.merged(CueList::from_anlz_pcob(raw));
    }
    let structure = s.structure.and_then(SongStructure::from_pssi);
    let mut data = TrackData {
        reference: Some(beat_link::data::DataReference::new(0, TrackSourceSlot::Collection, i as u32 + 1, TrackType::Rekordbox)),
        metadata: Some(metadata),
        beat_grid: grid.map(Arc::new),
        cue_list: Some(cues),
        preview: Some(WaveformPreview { style: WaveformStyle::Rgb, data: s.preview.to_vec() }),
        detail: Some(WaveformDetail { style: WaveformStyle::Rgb, data: s.detail.to_vec() }),
        color_detail: Some(s.detail.to_vec()),
        song_structure: structure,
        artwork: Some(s.art.to_vec()),
        signature: None,
    };
    data.signature = data.compute_signature().or_else(|| parse_edn(s.signature).ok().and_then(|e| e.as_str().map(str::to_string)));
    Arc::new(data)
}

/// The bundled sample tracks.
pub fn sample_tracks() -> &'static [Arc<TrackData>] {
    static TRACKS: OnceLock<Vec<Arc<TrackData>>> = OnceLock::new();
    TRACKS.get_or_init(|| SAMPLES.iter().enumerate().map(|(i, s)| load_sample(i, s)).collect())
}

/// The signature stored alongside a sample track by the Java version.
pub fn stored_sample_signature(i: usize) -> Option<String> {
    parse_edn(SAMPLES.get(i)?.signature).ok().and_then(|e| e.as_str().map(str::to_string))
}

/// What a simulated player is doing.
#[derive(Debug, Clone)]
pub struct SimulatedPlayer {
    pub number: u8,
    /// Index into [`sample_tracks`], if a track is loaded.
    pub track: Option<usize>,
    pub playing: bool,
    pub on_air: bool,
    pub master: bool,
    pub synced: bool,
    pub pitch_percent: f64,
    pub position_ms: f64,
    last_beat: Option<i64>,
    packet_number: u32,
}

impl SimulatedPlayer {
    pub fn new(number: u8) -> Self {
        SimulatedPlayer { number, track: None, playing: false, on_air: true, master: false, synced: false, pitch_percent: 0.0, position_ms: 0.0, last_beat: None, packet_number: 0 }
    }

    pub fn track_data(&self) -> Option<Arc<TrackData>> {
        self.track.and_then(|i| sample_tracks().get(i).cloned())
    }

    fn grid(&self) -> Option<Arc<BeatGrid>> {
        self.track_data().and_then(|t| t.beat_grid.clone())
    }

    /// The beat at the current position.
    pub fn beat(&self) -> Option<i64> {
        let g = self.grid()?;
        let b = g.find_beat_at_time(self.position_ms.max(0.0) as u64);
        Some(b.max(1))
    }

    /// Build the status packet a real player would send.
    pub fn status(&mut self) -> Arc<CdjStatus> {
        self.packet_number += 1;
        let pitch = percentage_to_pitch(self.pitch_percent);
        let (bpm, beat, bar) = match (self.grid(), self.beat()) {
            (Some(g), Some(b)) => (u32::from(g.bpm(b)), Some(b as u32), g.beat_within_bar(b) as u8),
            _ => (beat_link::util::NO_BPM, None, 0),
        };
        let loaded = self.track.is_some();
        Arc::new(
            CdjStatusBuilder {
                device_number: self.number,
                device_name: "Simulated CDJ".into(),
                address: Ipv4Addr::LOCALHOST,
                track_source_player: if loaded { self.number } else { 0 },
                track_source_slot: if loaded { TrackSourceSlot::Collection } else { TrackSourceSlot::NoTrack },
                track_type: if loaded { TrackType::Rekordbox } else { TrackType::NoTrack },
                rekordbox_id: self.track.map(|t| t as u32 + 1).unwrap_or(0),
                track_number: self.track.map(|t| t as u32 + 1).unwrap_or(0),
                pitch,
                bpm,
                beat_number: beat,
                beat_within_bar: bar,
                playing: self.playing && loaded,
                master: self.master,
                synced: self.synced,
                on_air: self.on_air,
                cued: !self.playing,
                packet_number: self.packet_number,
                ..Default::default()
            }
            .build(),
        )
    }

    fn beat_packet(&self) -> Option<Arc<Beat>> {
        let g = self.grid()?;
        let b = self.beat()?;
        let tempo = f64::from(g.bpm(b)) / 100.0 * pitch_to_multiplier(percentage_to_pitch(self.pitch_percent));
        let bytes = Beat::build_bytes(self.number, "Simulated CDJ", tempo, g.beat_within_bar(b) as u8, percentage_to_pitch(self.pitch_percent));
        let mut beat = Beat::parse(&bytes, Ipv4Addr::LOCALHOST).ok()?;
        beat.tempo_master = self.master;
        beat.synced = self.synced;
        beat.bar_meaningful = true;
        Some(Arc::new(beat))
    }
}

/// Events produced by the simulator.
#[derive(Debug, Clone)]
pub enum SimEvent {
    Status(Arc<CdjStatus>),
    Beat(Arc<Beat>),
    Removed(u8),
}

type Sink = Arc<dyn Fn(SimEvent) + Send + Sync>;

/// Runs the simulated players.
pub struct Simulator {
    players: Mutex<BTreeMap<u8, SimulatedPlayer>>,
    running: Arc<AtomicBool>,
    sink: Sink,
}

impl Simulator {
    /// Start a simulator that reports to `sink`.
    pub fn start(sink: Sink) -> Arc<Simulator> {
        let sim = Arc::new(Simulator { players: Mutex::new(BTreeMap::new()), running: Arc::new(AtomicBool::new(true)), sink });
        let w = Arc::downgrade(&sim);
        let running = sim.running.clone();
        thread::Builder::new()
            .name("Shallow playback simulator".into())
            .spawn(move || {
                let mut last = Instant::now();
                let mut last_status = Instant::now() - Duration::from_secs(1);
                while running.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(10));
                    let Some(sim) = w.upgrade() else { break };
                    let now = Instant::now();
                    let elapsed = now.duration_since(last).as_secs_f64() * 1000.0;
                    last = now;
                    let send_status = now.duration_since(last_status) >= Duration::from_millis(200);
                    if send_status {
                        last_status = now;
                    }
                    sim.tick(elapsed, send_status);
                }
            })
            .ok();
        sim
    }

    fn tick(&self, elapsed_ms: f64, send_status: bool) {
        let mut events = Vec::new();
        {
            let mut players = self.players.lock();
            for p in players.values_mut() {
                if p.playing && p.track.is_some() {
                    p.position_ms += elapsed_ms * pitch_to_multiplier(percentage_to_pitch(p.pitch_percent));
                    let duration = p.track_data().and_then(|t| t.metadata.as_ref().map(|m| f64::from(m.duration) * 1000.0)).unwrap_or(0.0);
                    if p.position_ms >= duration {
                        p.position_ms = duration;
                        p.playing = false;
                    }
                    let beat = p.beat();
                    if beat != p.last_beat {
                        p.last_beat = beat;
                        if let Some(b) = p.beat_packet() {
                            events.push(SimEvent::Beat(b));
                        }
                    }
                }
                if send_status {
                    events.push(SimEvent::Status(p.status()));
                }
            }
        }
        for e in events {
            (self.sink)(e);
        }
    }

    /// Add a simulated player (the lowest unused number from 1 to 6).
    pub fn add_player(&self) -> Option<u8> {
        let mut players = self.players.lock();
        let n = (1..=6).find(|n| !players.contains_key(n))?;
        let mut p = SimulatedPlayer::new(n);
        p.master = players.is_empty();
        players.insert(n, p);
        Some(n)
    }

    pub fn remove_player(&self, n: u8) {
        if self.players.lock().remove(&n).is_some() {
            (self.sink)(SimEvent::Removed(n));
        }
    }

    /// Change a player's state. Making a player master clears it on the others.
    pub fn update<F: FnOnce(&mut SimulatedPlayer)>(&self, n: u8, f: F) {
        let mut players = self.players.lock();
        let was_master = players.get(&n).map(|p| p.master).unwrap_or(false);
        if let Some(p) = players.get_mut(&n) {
            f(p);
            p.last_beat = if p.playing { p.last_beat } else { None };
        }
        if !was_master && players.get(&n).map(|p| p.master).unwrap_or(false) {
            for (k, p) in players.iter_mut() {
                if *k != n {
                    p.master = false;
                }
            }
        }
    }

    pub fn players(&self) -> Vec<SimulatedPlayer> {
        self.players.lock().values().cloned().collect()
    }

    pub fn player(&self, n: u8) -> Option<SimulatedPlayer> {
        self.players.lock().get(&n).cloned()
    }

    pub fn is_simulating(&self) -> bool {
        !self.players.lock().is_empty()
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        let numbers: Vec<u8> = self.players.lock().keys().copied().collect();
        for n in numbers {
            self.remove_player(n);
        }
    }
}

impl Drop for Simulator {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_tracks_load_and_signatures_match_java() {
        let tracks = sample_tracks();
        assert_eq!(tracks.len(), 2);
        let t = &tracks[0];
        let md = t.metadata.as_ref().unwrap();
        assert_eq!(md.title, "Concrete Angel (Original Mix)");
        assert_eq!(md.duration, 429);
        let grid = t.beat_grid.as_ref().unwrap();
        assert!(grid.beat_count() > 900);
        assert!(!t.cue_list.as_ref().unwrap().entries.is_empty());
        assert!(tracks[1].song_structure.is_some());
        // The Rust signature algorithm reproduces the Java SignatureFinder exactly.
        for i in 0..2 {
            let computed = sample_tracks()[i].compute_signature().unwrap();
            assert_eq!(Some(computed), stored_sample_signature(i), "signature mismatch for sample {}", i + 1);
        }
    }

    #[test]
    fn simulated_player_produces_status_and_beats() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let e = events.clone();
        let sim = Simulator::start(Arc::new(move |ev| e.lock().push(ev)));
        let n = sim.add_player().unwrap();
        sim.update(n, |p| {
            p.track = Some(1);
            p.position_ms = 30_000.0;
            p.playing = true;
            p.pitch_percent = 5.0;
        });
        thread::sleep(Duration::from_millis(1200));
        sim.stop();
        let ev = events.lock();
        let statuses: Vec<&Arc<CdjStatus>> = ev.iter().filter_map(|e| if let SimEvent::Status(s) = e { Some(s) } else { None }).collect();
        let beats = ev.iter().filter(|e| matches!(e, SimEvent::Beat(_))).count();
        assert!(statuses.len() >= 4);
        assert!(statuses.last().unwrap().is_playing());
        assert!(statuses.last().unwrap().is_tempo_master());
        assert!((statuses.last().unwrap().effective_tempo() - 127.0 * 1.05).abs() < 0.1);
        // 133 BPM for ~1.2 s is about 2-3 beats.
        assert!((1..=4).contains(&beats), "{beats} beats");
        assert!(matches!(ev.last().unwrap(), SimEvent::Removed(_)));
    }
}
