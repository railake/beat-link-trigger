//! Watches for tracks being loaded and retrieves everything we can learn about them
//! from the dbserver of the player they were loaded from. This merges beat-link's
//! `MetadataFinder`, `BeatGridFinder`, `WaveformFinder`, `ArtFinder`,
//! `AnalysisTagFinder` and `SignatureFinder` into one component that produces a
//! single [`TrackData`] per loaded track.

use crate::data::{
    compute_track_signature, fourcc_protocol_number, AnlzSection, BeatGrid, CueList, DataReference, SongStructure,
    TrackMetadata, WaveformDetail, WaveformPreview, WaveformStyle,
};
use crate::dbserver::{kind, query_db_server_port, Client, Field, MenuIdentifier};
use crate::device_finder::DeviceFinder;
use crate::listeners::ListenerSet;
use crate::packets::{CdjStatus, DeviceAnnouncement, DeviceUpdate, TrackSourceSlot, TrackType};
use crate::virtual_cdj::VirtualCdj;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;

/// Everything we have learned about a track.
#[derive(Debug, Clone, Default)]
pub struct TrackData {
    pub reference: Option<DataReference>,
    pub metadata: Option<TrackMetadata>,
    pub beat_grid: Option<Arc<BeatGrid>>,
    pub cue_list: Option<CueList>,
    pub preview: Option<WaveformPreview>,
    pub detail: Option<WaveformDetail>,
    /// RGB waveform detail entries (PWV5), used for signatures.
    pub color_detail: Option<Vec<u8>>,
    pub song_structure: Option<SongStructure>,
    /// Album art (normally JPEG bytes).
    pub artwork: Option<Vec<u8>>,
    pub signature: Option<String>,
}

impl TrackData {
    /// Compute the signature if we have everything required.
    pub fn compute_signature(&self) -> Option<String> {
        let md = self.metadata.as_ref()?;
        let grid = self.beat_grid.as_ref()?;
        let detail = self.color_detail.as_ref()?;
        Some(compute_track_signature(&md.title, md.artist.as_ref().map(|a| a.label.as_str()), md.duration, detail, grid))
    }
}

/// Reports that the track loaded on a player changed (or new data arrived for it).
#[derive(Debug, Clone)]
pub struct TrackUpdate {
    pub player: u8,
    pub data: Option<Arc<TrackData>>,
}

/// Retrieves and caches track data.
pub struct MetadataFinder {
    me: Weak<MetadataFinder>,
    device_finder: Arc<DeviceFinder>,
    virtual_cdj: Arc<VirtualCdj>,
    running: AtomicBool,
    passive: AtomicBool,
    loaded: RwLock<HashMap<u8, DataReference>>,
    cache: RwLock<HashMap<DataReference, Arc<TrackData>>>,
    in_flight: Mutex<HashSet<DataReference>>,
    ports: Mutex<HashMap<Ipv4Addr, u16>>,
    player_locks: Mutex<HashMap<u8, Arc<Mutex<()>>>>,
    /// Socket timeout for dbserver connections.
    pub timeout: Mutex<Duration>,
    /// Delivered when a player's loaded track (or what we know about it) changes.
    pub track_update: ListenerSet<TrackUpdate>,
}

impl MetadataFinder {
    pub fn new(device_finder: Arc<DeviceFinder>, virtual_cdj: Arc<VirtualCdj>) -> Arc<Self> {
        let finder = Arc::new_cyclic(|me| MetadataFinder {
            me: me.clone(),
            device_finder: device_finder.clone(),
            virtual_cdj: virtual_cdj.clone(),
            running: AtomicBool::new(false),
            passive: AtomicBool::new(false),
            loaded: RwLock::new(HashMap::new()),
            cache: RwLock::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            ports: Mutex::new(HashMap::new()),
            player_locks: Mutex::new(HashMap::new()),
            timeout: Mutex::new(Duration::from_secs(10)),
            track_update: ListenerSet::new(),
        });
        let w = finder.me.clone();
        virtual_cdj.update.add(move |u: &DeviceUpdate| {
            if let (Some(f), DeviceUpdate::Cdj(s)) = (w.upgrade(), u) {
                if f.is_running() {
                    f.handle_status(s);
                }
            }
        });
        let w = finder.me.clone();
        device_finder.device_lost.add(move |a: &DeviceAnnouncement| {
            if let Some(f) = w.upgrade() {
                f.handle_device_lost(a);
            }
        });
        finder
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    pub fn start(&self) {
        self.running.store(true, Ordering::SeqCst);
    }
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        let players: Vec<u8> = self.loaded.write().drain().map(|(k, _)| k).collect();
        for p in players {
            self.track_update.deliver(&TrackUpdate { player: p, data: None });
        }
    }
    /// In passive mode we never query players; only cached or supplied data is used.
    pub fn set_passive(&self, passive: bool) {
        self.passive.store(passive, Ordering::SeqCst);
    }
    pub fn is_passive(&self) -> bool {
        self.passive.load(Ordering::SeqCst)
    }

    /// Everything known about the track loaded on a player.
    pub fn latest_track_data(&self, player: u8) -> Option<Arc<TrackData>> {
        let reference = *self.loaded.read().get(&player)?;
        self.cache.read().get(&reference).cloned()
    }

    /// The track currently loaded on a player.
    pub fn loaded_reference(&self, player: u8) -> Option<DataReference> {
        self.loaded.read().get(&player).copied()
    }

    /// Players and the tracks loaded on them.
    pub fn loaded_tracks(&self) -> HashMap<u8, DataReference> {
        self.loaded.read().clone()
    }

    /// Supply track data obtained elsewhere (e.g. a simulator or a show file).
    pub fn insert_track_data(&self, reference: DataReference, data: TrackData) {
        let data = Arc::new(data);
        self.cache.write().insert(reference, data.clone());
        self.notify_players_of(reference, data);
    }

    fn notify_players_of(&self, reference: DataReference, data: Arc<TrackData>) {
        let players: Vec<u8> = self.loaded.read().iter().filter(|(_, r)| **r == reference).map(|(p, _)| *p).collect();
        for player in players {
            self.track_update.deliver(&TrackUpdate { player, data: Some(data.clone()) });
        }
    }

    fn handle_device_lost(&self, a: &DeviceAnnouncement) {
        let removed = self.loaded.write().remove(&a.number).is_some();
        if removed {
            self.track_update.deliver(&TrackUpdate { player: a.number, data: None });
        }
        // Tracks loaded from the lost player's media are gone too.
        let orphaned: Vec<u8> = self.loaded.read().iter().filter(|(_, r)| r.player == a.number).map(|(p, _)| *p).collect();
        for p in orphaned {
            self.loaded.write().remove(&p);
            self.track_update.deliver(&TrackUpdate { player: p, data: None });
        }
        self.cache.write().retain(|r, _| r.player != a.number);
        self.ports.lock().remove(&a.address);
    }

    fn handle_status(&self, status: &CdjStatus) {
        let player = status.device_number();
        let has_track = status.rekordbox_id != 0
            && status.track_source_player != 0
            && matches!(status.track_type, TrackType::Rekordbox | TrackType::Unanalyzed | TrackType::Streaming | TrackType::CdDigitalAudio);
        if !has_track {
            if self.loaded.write().remove(&player).is_some() {
                self.track_update.deliver(&TrackUpdate { player, data: None });
            }
            return;
        }
        let reference = DataReference::new(status.track_source_player, status.track_source_slot, status.rekordbox_id, status.track_type);
        let previous = self.loaded.write().insert(player, reference);
        if previous == Some(reference) {
            return;
        }
        if let Some(data) = self.cache.read().get(&reference).cloned() {
            self.track_update.deliver(&TrackUpdate { player, data: Some(data) });
            return;
        }
        self.track_update.deliver(&TrackUpdate { player, data: None });
        if self.is_passive() {
            return;
        }
        if !self.in_flight.lock().insert(reference) {
            return;
        }
        let w = self.me.clone();
        thread::Builder::new()
            .name(format!("beat-link metadata fetch {}", reference.rekordbox_id))
            .spawn(move || {
                if let Some(f) = w.upgrade() {
                    let result = f.fetch(reference);
                    f.in_flight.lock().remove(&reference);
                    match result {
                        Ok(data) => {
                            let data = Arc::new(data);
                            f.cache.write().insert(reference, data.clone());
                            f.notify_players_of(reference, data);
                        }
                        Err(e) => log::warn!("Unable to retrieve metadata for {reference:?}: {e}"),
                    }
                }
            })
            .ok();
    }

    fn db_port(&self, address: Ipv4Addr) -> io::Result<u16> {
        if let Some(p) = self.ports.lock().get(&address) {
            return Ok(*p);
        }
        let timeout = *self.timeout.lock();
        let mut last = io::Error::other("no attempt");
        for _ in 0..3 {
            match query_db_server_port(address, timeout) {
                Ok(port) => {
                    self.ports.lock().insert(address, port);
                    return Ok(port);
                }
                Err(e) => {
                    last = e;
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
        Err(last)
    }

    /// Pick a player number to pose as when querying `target`, following beat-link's
    /// `ConnectionManager.chooseAskingPlayerNumber`.
    pub fn choose_asking_player_number(&self, target: &DeviceAnnouncement) -> Option<u8> {
        let fake = self.virtual_cdj.device_number();
        if fake > 4 && !self.device_finder.is_device_metadata_limited(target) {
            return Some(target.number);
        }
        if target.number > 15 || (1..=4).contains(&fake) {
            return Some(fake);
        }
        for candidate in self.device_finder.current_devices() {
            let n = candidate.number;
            if n != target.number && (1..=4).contains(&n) {
                if let Some(status) = self.virtual_cdj.latest_cdj_status(n) {
                    if status.track_source_player != target.number {
                        return Some(n);
                    }
                }
            }
        }
        None
    }

    /// Open a dbserver connection to a player.
    pub fn connect(&self, player: u8) -> io::Result<Client> {
        let announcement = self
            .device_finder
            .latest_announcement_from(player)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("player {player} not found")))?;
        let port = self.db_port(announcement.address)?;
        let posing = self.choose_asking_player_number(&announcement).ok_or_else(|| {
            io::Error::other(format!("No player number available to query player {player}"))
        })?;
        Client::connect(announcement.address, port, player, posing, *self.timeout.lock())
    }

    fn player_lock(&self, player: u8) -> Arc<Mutex<()>> {
        self.player_locks.lock().entry(player).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    /// Fetch everything about a track from its source player.
    pub fn fetch(&self, r: DataReference) -> io::Result<TrackData> {
        let lock = self.player_lock(r.player);
        let _guard = lock.lock();
        let mut client = self.connect(r.player)?;
        let slot = r.slot();
        let track_type = r.track_type();
        let mut data = TrackData { reference: Some(r), ..Default::default() };

        let request = if matches!(track_type, TrackType::Rekordbox | TrackType::Streaming) {
            kind::REKORDBOX_METADATA_REQ
        } else {
            kind::UNANALYZED_METADATA_REQ
        };
        let available = client.menu_request(request, MenuIdentifier::MainMenu, slot, track_type, vec![Field::number(r.rekordbox_id)])?;
        let items = client.render_all(MenuIdentifier::MainMenu, slot, track_type, &available)?;
        let mut metadata = TrackMetadata::from_menu_items(&items);

        if track_type == TrackType::Rekordbox {
            data.cue_list = fetch_cue_list(&mut client, r.rekordbox_id, slot).unwrap_or_else(|e| {
                log::debug!("No cue list: {e}");
                None
            });
            metadata.cue_list = data.cue_list.clone();

            let rmst = client.rmst(MenuIdentifier::Data, slot, track_type);
            if let Ok(m) = client.simple_request(kind::BEAT_GRID_REQ, None, vec![rmst, Field::number(r.rekordbox_id)]) {
                if m.kind == kind::BEAT_GRID {
                    if let Some(b) = m.binary_arg(3) {
                        data.beat_grid = Some(Arc::new(BeatGrid::from_dbserver(b)));
                    }
                }
            }
            let rmst = client.rmst(MenuIdentifier::Data, slot, track_type);
            if let Ok(m) = client.simple_request(
                kind::WAVE_PREVIEW_REQ,
                None,
                vec![rmst, Field::number(1), Field::number(r.rekordbox_id), Field::number(0)],
            ) {
                if m.kind == kind::WAVE_PREVIEW {
                    if let Some(b) = m.binary_arg(3) {
                        data.preview = Some(WaveformPreview::blue(b.to_vec()));
                    }
                }
            }
            let rmst = client.rmst(MenuIdentifier::MainMenu, slot, track_type);
            if let Ok(m) = client.simple_request(kind::WAVE_DETAIL_REQ, None, vec![rmst, Field::number(r.rekordbox_id), Field::number(0)]) {
                if m.kind == kind::WAVE_DETAIL {
                    if let Some(b) = m.binary_arg(3) {
                        let skip = WaveformDetail::LEADING_DBSERVER_JUNK_BYTES.min(b.len());
                        data.detail = Some(WaveformDetail { style: WaveformStyle::Blue, data: b[skip..].to_vec() });
                    }
                }
            }
            if let Some(section) = fetch_anlz_tag(&mut client, r.rekordbox_id, slot, ".EXT", "PWV5") {
                data.color_detail = section.waveform_entries();
            }
            if let Some(section) = fetch_anlz_tag(&mut client, r.rekordbox_id, slot, ".EXT", "PSSI") {
                data.song_structure = SongStructure::from_pssi(&section.body);
            }
        }
        if metadata.artwork_id != 0 {
            let rmst = client.rmst(MenuIdentifier::Data, slot, track_type);
            if let Ok(m) = client.simple_request(kind::ALBUM_ART_REQ, None, vec![rmst, Field::number(metadata.artwork_id)]) {
                if m.kind == kind::ALBUM_ART {
                    data.artwork = m.binary_arg(3).map(|b| b.to_vec()).filter(|b| !b.is_empty());
                }
            }
        }
        data.metadata = Some(metadata);
        data.signature = data.compute_signature();
        Ok(data)
    }
}

fn fetch_cue_list(client: &mut Client, id: u32, slot: TrackSourceSlot) -> io::Result<Option<CueList>> {
    let rmst = client.rmst(MenuIdentifier::Data, slot, TrackType::Rekordbox);
    let m = client.simple_request(kind::CUE_LIST_EXT_REQ, None, vec![rmst, Field::number(id), Field::number(0)])?;
    if m.kind == kind::CUE_LIST_EXT {
        return Ok(Some(CueList::from_nxs2_message(&m)));
    }
    let rmst = client.rmst(MenuIdentifier::Data, slot, TrackType::Rekordbox);
    let m = client.simple_request(kind::CUE_LIST_REQ, None, vec![rmst, Field::number(id)])?;
    if m.kind == kind::CUE_LIST {
        return Ok(Some(CueList::from_nexus_message(&m)));
    }
    Ok(None)
}

fn fetch_anlz_tag(client: &mut Client, id: u32, slot: TrackSourceSlot, extension: &str, tag: &str) -> Option<AnlzSection> {
    let rmst = client.rmst(MenuIdentifier::MainMenu, slot, TrackType::Rekordbox);
    let ext = extension.trim_start_matches('.');
    let m = client
        .simple_request(
            kind::ANLZ_TAG_REQ,
            None,
            vec![rmst, Field::number(id), Field::number(fourcc_protocol_number(tag)), Field::number(fourcc_protocol_number(ext))],
        )
        .ok()?;
    if m.kind != kind::ANLZ_TAG {
        return None;
    }
    let bytes = m.binary_arg(3)?;
    if bytes.len() <= 4 {
        return None;
    }
    // Skip the length so we are at the four-character tag identifier.
    AnlzSection::parse(&bytes[4..]).map(|(s, _)| s)
}
