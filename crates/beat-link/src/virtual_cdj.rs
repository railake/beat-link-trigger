//! Presents a virtual player on the network so the real players will send us status
//! updates, and (optionally) behaves like a real player: sending status, beats,
//! becoming tempo master, and responding to sync commands.
//! Port of `org.deepsymmetry.beatlink.VirtualCdj` (minus Opus Quad lighting mode).

use crate::beat_finder::{BeatFinder, MasterHandoff, SyncCommand};
use crate::device_finder::{DeviceFinder, SpecialAnnouncement};
use crate::listeners::ListenerSet;
use crate::metronome::{self, Metronome, Snapshot};
use crate::network::{self, InterfaceAddress};
use crate::packets::{
    Beat, CdjStatus, DeviceAnnouncement, DeviceReference, DeviceUpdate, MixerStatus, TrackSourceSlot, TrackType,
    BEAT_PAYLOAD,
};
use crate::player_settings::PlayerSettings;
use crate::util::{self, PacketType};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DEVICE_NAME_OFFSET: usize = 0x0c;
const DEVICE_NAME_LENGTH: usize = 0x14;
const DEVICE_NUMBER_OFFSET: usize = 0x24;
const MAC_ADDRESS_OFFSET: usize = 0x26;
const IP_ADDRESS_OFFSET: usize = 0x2c;

const KEEP_ALIVE_TEMPLATE: [u8; 0x36] = [
    0x51, 0x73, 0x70, 0x74, 0x31, 0x57, 0x6d, 0x4a, 0x4f, 0x4c, 0x06, 0x00, 0x62, 0x65, 0x61, 0x74, 0x2d, 0x6c, 0x69,
    0x6e, 0x6b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x36, 0x00, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x64,
];

fn announcement_packet(kind: PacketType, name: &str, body: &[u8]) -> Vec<u8> {
    // Packets on the announcement port carry an extra byte before the device name.
    let mut p = Vec::with_capacity(0x20 + body.len());
    p.extend_from_slice(&util::MAGIC_HEADER);
    p.push(kind.protocol_value());
    p.push(0x00);
    let mut n = [0u8; DEVICE_NAME_LENGTH];
    let bytes = name.as_bytes();
    let len = bytes.len().min(DEVICE_NAME_LENGTH);
    n[..len].copy_from_slice(&bytes[..len]);
    p.extend_from_slice(&n);
    p.extend_from_slice(body);
    p
}

/// The status payload template (bytes after the device name) for a CDJ status packet.
const STATUS_PAYLOAD: [u8; 0xfd] = [
    0x01, //
    0x04, 0x00, 0x00, 0xf8, 0x00, 0x00, 0x01, 0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, //
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa0, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x04, 0x04, 0x00, 0x00, 0x00, 0x04, //
    0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x31, 0x2e, 0x34, 0x33, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x10, 0x00, 0x00, //
    0x80, 0x00, 0x00, 0x00, 0x7f, 0xff, 0xff, 0xff, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x01, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x10, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0f, 0x01, 0x00, 0x00, //
    0x12, 0x34, 0x56, 0x78, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x15, 0x00, 0x00, 0x07, 0x61, 0x00, 0x00, 0x06, 0x2f,
];

const SYNC_CONTROL_PAYLOAD: [u8; 13] = [0x01, 0x00, 0x0d, 0x00, 0x08, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00, 0x0f];
const FADER_START_PAYLOAD: [u8; 9] = [0x01, 0x00, 0x0d, 0x00, 0x04, 0x02, 0x02, 0x02, 0x02];
const CHANNELS_ON_AIR_PAYLOAD: [u8; 14] =
    [0x01, 0x00, 0x0d, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
const CHANNELS_ON_AIR_EXTENDED_PAYLOAD: [u8; 22] = [
    0x01, 0x03, 0x0d, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00,
];
const MEDIA_QUERY_PAYLOAD: [u8; 17] =
    [0x01, 0x00, 0x0d, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
const YIELD_ACK_PAYLOAD: [u8; 13] = [0x01, 0x00, 0x0d, 0x00, 0x08, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00, 0x01];
const MASTER_HANDOFF_REQUEST_PAYLOAD: [u8; 9] = [0x01, 0x00, 0x0d, 0x00, 0x04, 0x00, 0x00, 0x00, 0x0d];
const LOAD_TRACK_PAYLOAD: [u8; 0x39] = {
    let mut p = [0u8; 0x39];
    p[0] = 0x01;
    p[2] = 0x0d;
    p[4] = 0x34;
    p[5] = 0x0d;
    p[0x14] = 0x32;
    p
};

/// Tunable behavior of the virtual CDJ.
#[derive(Debug, Clone)]
pub struct VirtualCdjSettings {
    /// The name we announce (at most 20 bytes).
    pub device_name: String,
    /// The device number to claim; 0 means pick one automatically.
    pub device_number: u8,
    /// When choosing automatically, try for a real player number (1-4) so we can send status.
    pub use_standard_player_number: bool,
    /// Milliseconds between keep-alive announcements (200-2000).
    pub announce_interval_ms: u64,
    /// Milliseconds between status packets when sending status (20-2000).
    pub status_interval_ms: u64,
    /// Smallest tempo change worth reporting.
    pub tempo_epsilon: f64,
    /// How long to wait for any device to appear before giving up on `start`.
    pub device_wait: Duration,
    /// Delay between device-number claim packets (300ms on real networks).
    pub claim_interval: Duration,
    /// How long after seeing the first device to wait before self-assigning a number.
    pub self_assignment_watch_period: Duration,
}

impl Default for VirtualCdjSettings {
    fn default() -> Self {
        VirtualCdjSettings {
            device_name: "beat-link".into(),
            device_number: 0,
            use_standard_player_number: false,
            announce_interval_ms: 1500,
            status_interval_ms: 200,
            tempo_epsilon: 0.0001,
            device_wait: Duration::from_secs(100),
            claim_interval: Duration::from_millis(300),
            self_assignment_watch_period: Duration::from_millis(4000),
        }
    }
}

/// Errors starting the virtual CDJ or sending commands.
#[derive(Debug, thiserror::Error)]
pub enum VirtualCdjError {
    #[error("no DJ Link devices found")]
    NoDevices,
    #[error("unable to find a network interface on the same network as {0}")]
    NoMatchingInterface(Ipv4Addr),
    #[error("unable to claim a device number")]
    ClaimFailed,
    #[error("the virtual CDJ is not running")]
    NotRunning,
    #[error("device {0} not found on network")]
    DeviceNotFound(u8),
    #[error("{0}")]
    IllegalState(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

struct Runtime {
    socket: Arc<UdpSocket>,
    interface: InterfaceAddress,
    matching_interfaces: Vec<String>,
}

struct BeatSender {
    running: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl BeatSender {
    const BEAT_THRESHOLD: f64 = 10.0;
    const SLEEP_THRESHOLD: f64 = 5.0;

    fn start(vcdj: Weak<VirtualCdj>) -> BeatSender {
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let thread = thread::Builder::new()
            .name("beat-link VirtualCdj beat sender".into())
            .spawn(move || {
                let mut last_beat_sent: Option<i64> = None;
                while flag.load(Ordering::SeqCst) {
                    let Some(v) = vcdj.upgrade() else { break };
                    let snapshot = v.metronome.snapshot();
                    if let Some(last) = last_beat_sent {
                        if snapshot.beat_phase() > 0.5 || snapshot.beat() != last {
                            last_beat_sent = None;
                        }
                    }
                    let current_due = snapshot.time_of_beat(snapshot.beat());
                    let next_due = snapshot.time_of_beat(snapshot.beat() + 1);
                    let distance_into_current = snapshot.instant() - current_due;
                    if distance_into_current < Self::BEAT_THRESHOLD && last_beat_sent != Some(snapshot.beat()) {
                        last_beat_sent = Some(v.send_beat_snapshot(&snapshot));
                    }
                    drop(v);
                    let sleep_ms = next_due - metronome::now_ms();
                    if sleep_ms > Self::SLEEP_THRESHOLD {
                        thread::park_timeout(Duration::from_secs_f64((sleep_ms - Self::SLEEP_THRESHOLD) / 1000.0));
                    } else if sleep_ms > 0.0 {
                        let target = Instant::now() + Duration::from_secs_f64(sleep_ms / 1000.0);
                        while Instant::now() < target {
                            std::hint::spin_loop();
                        }
                    }
                }
            })
            .expect("spawn beat sender");
        BeatSender { running, thread }
    }

    fn timeline_changed(&self) {
        self.thread.thread().unpark();
    }

    fn shut_down(self) {
        self.running.store(false, Ordering::SeqCst);
        self.thread.thread().unpark();
    }
}

/// The virtual CDJ.
pub struct VirtualCdj {
    me: Weak<VirtualCdj>,
    device_finder: Arc<DeviceFinder>,
    beat_finder: Arc<BeatFinder>,
    settings: Mutex<VirtualCdjSettings>,
    runtime: RwLock<Option<Runtime>>,
    running: AtomicBool,
    transitioning: AtomicBool,
    keep_alive: Mutex<[u8; 0x36]>,
    updates: RwLock<HashMap<DeviceReference, DeviceUpdate>>,
    tempo_master: RwLock<Option<DeviceUpdate>>,
    master_tempo: Mutex<f64>,
    claiming_number: AtomicU8,
    claim_rejected: AtomicBool,
    mixer_assigned: AtomicU8,
    sending_status: Mutex<Option<Arc<AtomicBool>>>,
    metronome: Metronome,
    where_stopped: Mutex<Snapshot>,
    playing: AtomicBool,
    master: AtomicBool,
    synced: AtomicBool,
    on_air: AtomicBool,
    following_master: AtomicBool,
    sync_counter: AtomicU32,
    largest_sync_counter: AtomicU32,
    next_master: AtomicU8,
    master_yielded_from: AtomicU8,
    requesting_master_role_from: AtomicU8,
    packet_counter: AtomicU32,
    beat_sender: Mutex<Option<BeatSender>>,
    /// Every status update received from a device.
    pub update: ListenerSet<DeviceUpdate>,
    /// The tempo master changed (`None` when there is no master, or we became master).
    pub master_changed: ListenerSet<Option<DeviceUpdate>>,
    /// The master tempo changed.
    pub tempo_changed: ListenerSet<f64>,
    /// A beat was received from the tempo master.
    pub master_beat: ListenerSet<Arc<Beat>>,
    /// Lifecycle: `true` on start, `false` on stop.
    pub lifecycle: ListenerSet<bool>,
}

impl VirtualCdj {
    /// Create the virtual CDJ and wire it to the finders it depends on.
    pub fn new(device_finder: Arc<DeviceFinder>, beat_finder: Arc<BeatFinder>) -> Arc<Self> {
        let metronome = Metronome::new();
        let stopped = metronome.snapshot_at(metronome.start_time());
        let vcdj = Arc::new_cyclic(|me| VirtualCdj {
            me: me.clone(),
            device_finder: device_finder.clone(),
            beat_finder: beat_finder.clone(),
            settings: Mutex::new(VirtualCdjSettings::default()),
            runtime: RwLock::new(None),
            running: AtomicBool::new(false),
            transitioning: AtomicBool::new(false),
            keep_alive: Mutex::new(KEEP_ALIVE_TEMPLATE),
            updates: RwLock::new(HashMap::new()),
            tempo_master: RwLock::new(None),
            master_tempo: Mutex::new(0.0),
            claiming_number: AtomicU8::new(0),
            claim_rejected: AtomicBool::new(false),
            mixer_assigned: AtomicU8::new(0),
            sending_status: Mutex::new(None),
            metronome,
            where_stopped: Mutex::new(stopped),
            playing: AtomicBool::new(false),
            master: AtomicBool::new(false),
            synced: AtomicBool::new(false),
            on_air: AtomicBool::new(false),
            following_master: AtomicBool::new(false),
            sync_counter: AtomicU32::new(1),
            largest_sync_counter: AtomicU32::new(1),
            next_master: AtomicU8::new(0xff),
            master_yielded_from: AtomicU8::new(0),
            requesting_master_role_from: AtomicU8::new(0),
            packet_counter: AtomicU32::new(0),
            beat_sender: Mutex::new(None),
            update: ListenerSet::new(),
            master_changed: ListenerSet::new(),
            tempo_changed: ListenerSet::new(),
            master_beat: ListenerSet::new(),
            lifecycle: ListenerSet::new(),
        });
        vcdj.set_device_name("beat-link");
        vcdj.wire_listeners();
        vcdj
    }

    fn wire_listeners(&self) {
        let w = self.me.clone();
        self.device_finder.special_announcement.add(move |s: &SpecialAnnouncement| {
            if let Some(v) = w.upgrade() {
                v.handle_special_announcement(s);
            }
        });
        let w = self.me.clone();
        self.device_finder.keep_alive.add(move |a: &DeviceAnnouncement| {
            if let Some(v) = w.upgrade() {
                if v.is_running() && a.number == v.device_number() {
                    v.defend_device_number(a.address);
                }
            }
        });
        let w = self.me.clone();
        self.device_finder.lifecycle.add(move |started| {
            if !*started {
                if let Some(v) = w.upgrade() {
                    if v.is_running() {
                        log::info!("VirtualCdj stopping because DeviceFinder has stopped.");
                        v.stop();
                    }
                }
            }
        });
        let w = self.me.clone();
        self.beat_finder.set_enricher(move |beat: &mut Beat| {
            if let Some(v) = w.upgrade() {
                if v.is_running() {
                    beat.tempo_master = v
                        .tempo_master()
                        .map(|m| m.address() == beat.header.address && m.device_number() == beat.device_number())
                        .unwrap_or(false);
                    if let Some(status) = v.latest_status_for_number(beat.device_number()) {
                        beat.synced = status.is_synced();
                        beat.bar_meaningful = status.is_beat_within_bar_meaningful();
                    }
                }
            }
        });
        let w = self.me.clone();
        self.beat_finder.beat_internal.add(move |beat| {
            if let Some(v) = w.upgrade() {
                v.process_beat(beat);
            }
        });
        let w = self.me.clone();
        self.beat_finder.on_air.add(move |channels| {
            if let Some(v) = w.upgrade() {
                v.set_on_air(channels.contains(&v.device_number()));
            }
        });
        let w = self.me.clone();
        self.beat_finder.fader_start.add(move |cmd| {
            if let Some(v) = w.upgrade() {
                let n = v.device_number();
                if cmd.start.contains(&n) {
                    v.set_playing(true);
                } else if cmd.stop.contains(&n) {
                    v.set_playing(false);
                }
            }
        });
        let w = self.me.clone();
        self.beat_finder.sync.add(move |cmd| {
            if let Some(v) = w.upgrade() {
                match cmd {
                    SyncCommand::SyncOn => v.set_synced(true),
                    SyncCommand::SyncOff => v.set_synced(false),
                    SyncCommand::BecomeMaster => {
                        if v.is_sending_status() {
                            let w2 = v.me.clone();
                            thread::spawn(move || {
                                if let Some(v) = w2.upgrade() {
                                    if let Err(e) = v.become_tempo_master() {
                                        log::error!("Problem becoming tempo master in response to sync command: {e}");
                                    }
                                }
                            });
                        } else {
                            log::warn!("Ignoring sync command to become tempo master, since we are not sending status.");
                        }
                    }
                }
            }
        });
        let w = self.me.clone();
        self.beat_finder.master_handoff.add(move |h| {
            if let Some(v) = w.upgrade() {
                v.handle_master_handoff(*h);
            }
        });
    }

    // ------------------------------------------------------------------ configuration

    /// Replace the settings (only effective for the parts that matter before `start`).
    pub fn configure(&self, settings: VirtualCdjSettings) {
        self.set_device_name(&settings.device_name);
        if !self.is_running() {
            self.keep_alive.lock()[DEVICE_NUMBER_OFFSET] = settings.device_number;
        }
        *self.settings.lock() = settings;
    }

    pub fn settings(&self) -> VirtualCdjSettings {
        self.settings.lock().clone()
    }

    /// Set the name we announce ourselves with.
    pub fn set_device_name(&self, name: &str) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(DEVICE_NAME_LENGTH);
        let mut ka = self.keep_alive.lock();
        ka[DEVICE_NAME_OFFSET..DEVICE_NAME_OFFSET + DEVICE_NAME_LENGTH].fill(0);
        ka[DEVICE_NAME_OFFSET..DEVICE_NAME_OFFSET + len].copy_from_slice(&bytes[..len]);
        self.settings.lock().device_name = String::from_utf8_lossy(&bytes[..len]).into_owned();
    }

    pub fn device_name(&self) -> String {
        util::read_name(&self.keep_alive.lock()[..], DEVICE_NAME_OFFSET, DEVICE_NAME_LENGTH)
    }

    /// The device number we are using (0 when not yet assigned).
    pub fn device_number(&self) -> u8 {
        self.keep_alive.lock()[DEVICE_NUMBER_OFFSET]
    }

    /// Choose the device number to claim on the next start (0 for automatic).
    pub fn set_device_number(&self, number: u8) -> Result<(), VirtualCdjError> {
        if self.is_running() {
            return Err(VirtualCdjError::IllegalState("Can't change device number once started.".into()));
        }
        self.keep_alive.lock()[DEVICE_NUMBER_OFFSET] = number;
        self.settings.lock().device_number = number;
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst) && !self.transitioning.load(Ordering::SeqCst)
    }

    /// The local address we are using on the DJ Link network.
    pub fn local_address(&self) -> Option<Ipv4Addr> {
        self.runtime.read().as_ref().map(|r| r.interface.address)
    }

    /// The broadcast address of the DJ Link network.
    pub fn broadcast_address(&self) -> Option<Ipv4Addr> {
        self.runtime.read().as_ref().map(|r| r.interface.broadcast)
    }

    /// Names of the interfaces that are on the DJ Link network (more than one is trouble).
    pub fn matching_interfaces(&self) -> Vec<String> {
        self.runtime.read().as_ref().map(|r| r.matching_interfaces.clone()).unwrap_or_default()
    }

    // ------------------------------------------------------------------ lifecycle

    /// Join the network: wait for a device, find the right interface, claim a device
    /// number, and start announcing ourselves. Blocks while claiming.
    pub fn start(&self) -> Result<(), VirtualCdjError> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.transitioning.store(true, Ordering::SeqCst);
        let result = self.start_internal();
        self.transitioning.store(false, Ordering::SeqCst);
        if result.is_ok() {
            self.lifecycle.deliver(&true);
        }
        result
    }

    fn start_internal(&self) -> Result<(), VirtualCdjError> {
        let me = self.me.upgrade().ok_or(VirtualCdjError::NotRunning)?;
        self.device_finder.start()?;
        let settings = self.settings();
        let deadline = Instant::now() + settings.device_wait;
        while self.device_finder.current_devices().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        let devices = self.device_finder.current_devices();
        let Some(a_device) = devices.first() else {
            log::warn!("No DJ Link devices found, giving up");
            return Err(VirtualCdjError::NoDevices);
        };
        let (interface, matching) = match &self.device_finder.config().forced_interface {
            Some(forced) => (forced.clone(), vec![forced.interface_name.clone()]),
            None => {
                let matches = network::matching_addresses(a_device.address);
                let Some(first) = matches.first().cloned() else {
                    log::warn!("Unable to find network interface to communicate with {a_device:?}, giving up.");
                    for line in network::describe_interfaces() {
                        log::warn!("  {line}");
                    }
                    return Err(VirtualCdjError::NoMatchingInterface(a_device.address));
                };
                let names: Vec<String> = matches.iter().map(|m| m.interface_name.clone()).collect();
                let unique: HashSet<&String> = names.iter().collect();
                if unique.len() > 1 {
                    log::warn!(
                        "Multiple network interfaces see the DJ Link network ({names:?}); this will likely cause duplicate packets."
                    );
                }
                (first, names)
            }
        };
        log::info!("Using interface {} with address {}", interface.interface_name, interface.address);
        {
            let mut ka = self.keep_alive.lock();
            if let Some(mac) = interface.hardware_address {
                ka[MAC_ADDRESS_OFFSET..MAC_ADDRESS_OFFSET + 6].copy_from_slice(&mac);
            }
            ka[IP_ADDRESS_OFFSET..IP_ADDRESS_OFFSET + 4].copy_from_slice(&interface.address.octets());
        }
        let socket = Arc::new(network::bind_udp(SocketAddrV4::new(
            interface.address,
            self.device_finder.config().ports.update,
        ))?);
        self.device_finder.add_ignored_address(interface.address);
        *self.runtime.write() = Some(Runtime { socket: socket.clone(), interface: interface.clone(), matching_interfaces: matching });
        self.running.store(true, Ordering::SeqCst);

        if !self.claim_device_number() {
            log::warn!("Unable to allocate a device number for the Virtual CDJ, giving up.");
            self.device_finder.remove_ignored_address(interface.address);
            *self.runtime.write() = None;
            self.running.store(false, Ordering::SeqCst);
            return Err(VirtualCdjError::ClaimFailed);
        }
        log::info!("Virtual CDJ claimed device number {}", self.device_number());

        // Status receiver.
        let w = Arc::downgrade(&me);
        let rsock = socket.clone();
        thread::Builder::new().name("beat-link VirtualCdj status receiver".into()).spawn(move || {
            let mut buffer = [0u8; 1024];
            loop {
                let Some(v) = w.upgrade() else { break };
                if !v.running.load(Ordering::SeqCst) || !v.is_same_socket(&rsock) {
                    break;
                }
                match rsock.recv_from(&mut buffer) {
                    Ok((len, from)) => {
                        if let Some(addr) = network::ipv4_of(&from) {
                            if Some(addr) != v.local_address() {
                                if let Some(update) = v.build_update(&buffer[..len], addr) {
                                    v.process_update(update);
                                }
                            }
                        }
                    }
                    Err(e) if network::is_timeout(&e) => {}
                    Err(e) => {
                        if v.is_running() {
                            log::warn!("Problem reading status socket, flushing devices and stopping: {e}");
                            v.device_finder.flush();
                            v.stop();
                        }
                        break;
                    }
                }
            }
        })?;

        // Keep-alive announcer.
        let w = Arc::downgrade(&me);
        let asock = socket;
        thread::Builder::new().name("beat-link VirtualCdj announcement sender".into()).spawn(move || loop {
            let Some(v) = w.upgrade() else { break };
            if !v.running.load(Ordering::SeqCst) || !v.is_same_socket(&asock) {
                break;
            }
            let packet = *v.keep_alive.lock();
            let interval = v.settings.lock().announce_interval_ms;
            let dest = v.broadcast_address().unwrap_or(Ipv4Addr::BROADCAST);
            let port = v.device_finder.config().ports.announcement;
            drop(v);
            if let Err(e) = asock.send_to(&packet, SocketAddrV4::new(dest, port)) {
                log::warn!("Unable to send announcement packet: {e}");
            }
            thread::sleep(Duration::from_millis(interval));
        })?;
        Ok(())
    }

    fn is_same_socket(&self, s: &Arc<UdpSocket>) -> bool {
        self.runtime.read().as_ref().map(|r| Arc::ptr_eq(&r.socket, s)).unwrap_or(false)
    }

    /// Leave the network.
    pub fn stop(&self) {
        if !self.running.load(Ordering::SeqCst) {
            return;
        }
        self.transitioning.store(true, Ordering::SeqCst);
        let _ = self.set_sending_status(false);
        self.updates.write().clear();
        self.set_tempo_master(None);
        if let Some(rt) = self.runtime.write().take() {
            self.device_finder.remove_ignored_address(rt.interface.address);
        }
        self.running.store(false, Ordering::SeqCst);
        let configured = self.settings.lock().device_number;
        self.keep_alive.lock()[DEVICE_NUMBER_OFFSET] = configured;
        self.transitioning.store(false, Ordering::SeqCst);
        self.lifecycle.deliver(&false);
    }

    // ------------------------------------------------------------------ device number claiming

    fn send_announcement_port(&self, packet: &[u8], dest: Ipv4Addr) -> io::Result<()> {
        let rt = self.runtime.read();
        let rt = rt.as_ref().ok_or_else(|| io::Error::other("not running"))?;
        rt.socket.send_to(packet, SocketAddrV4::new(dest, self.device_finder.config().ports.announcement))?;
        Ok(())
    }

    fn self_assign_device_number(&self) -> bool {
        let settings = self.settings();
        let watch = settings.self_assignment_watch_period.as_millis() as u64;
        loop {
            let now = crate::clock::current_time_millis();
            let started = self.device_finder.first_device_time().unwrap_or(now);
            if now.saturating_sub(started) >= watch {
                break;
            }
            thread::sleep(Duration::from_millis(watch - (now - started)));
        }
        if self.claiming_number.load(Ordering::SeqCst) == 0 && !settings.use_standard_player_number {
            self.claiming_number.store(6, Ordering::SeqCst);
        }
        let used: HashSet<u8> = self.device_finder.current_devices().iter().map(|d| d.number).collect();
        let starting = self.claiming_number.load(Ordering::SeqCst) + 1;
        for candidate in starting..16 {
            if !used.contains(&candidate) {
                self.claiming_number.store(candidate, Ordering::SeqCst);
                if settings.use_standard_player_number && candidate > 4 {
                    log::warn!("Unable to self-assign a standard player number, all are in use. Trying {candidate}.");
                }
                return true;
            }
        }
        log::warn!("Found no unused device numbers between {starting} and 15, giving up.");
        false
    }

    fn claim_device_number(&self) -> bool {
        let settings = self.settings();
        let name = self.device_name();
        let Some((broadcast, local, mac)) = self.runtime.read().as_ref().map(|r| {
            (r.interface.broadcast, r.interface.address, r.interface.hardware_address.unwrap_or_default())
        }) else {
            return false;
        };
        self.claim_rejected.store(false, Ordering::SeqCst);
        self.mixer_assigned.store(0, Ordering::SeqCst);
        let hello = announcement_packet(PacketType::DeviceHello, &name, &[0x01, 0x04, 0x00, 0x26, 0x01, 0x40]);
        for _ in 0..3 {
            if let Err(e) = self.send_announcement_port(&hello, broadcast) {
                log::warn!("Unable to send hello packet: {e}");
                return false;
            }
            thread::sleep(settings.claim_interval);
        }
        let requested = self.device_number();
        self.claiming_number.store(requested, Ordering::SeqCst);
        'self_assign: loop {
            if requested == 0 && !self.self_assign_device_number() {
                self.claiming_number.store(0, Ordering::SeqCst);
                return false;
            }
            // Stage 1.
            let mut stage1 = announcement_packet(PacketType::DeviceNumberStage1, &name, &[0x01, 0x03, 0x00, 0x2c, 0x0d, 0x01, 0, 0, 0, 0, 0, 0]);
            stage1[0x26..0x2c].copy_from_slice(&mac);
            for i in 1..=3u8 {
                if self.mixer_assigned.load(Ordering::SeqCst) != 0 {
                    break;
                }
                stage1[0x24] = i;
                if self.send_announcement_port(&stage1, broadcast).is_err() {
                    self.claiming_number.store(0, Ordering::SeqCst);
                    return false;
                }
                thread::sleep(settings.claim_interval);
                if self.claim_rejected.swap(false, Ordering::SeqCst) {
                    if requested == 0 {
                        continue 'self_assign;
                    }
                    log::warn!("Unable to use device number {requested}, another device has it.");
                    self.claiming_number.store(0, Ordering::SeqCst);
                    return false;
                }
            }
            // Stage 2.
            let mut stage2 = announcement_packet(
                PacketType::DeviceNumberStage2,
                &name,
                &[0x01, 0x03, 0x00, 0x32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0d, 0x00, 0x01, 0x00],
            );
            stage2[0x24..0x28].copy_from_slice(&local.octets());
            stage2[0x28..0x2e].copy_from_slice(&mac);
            stage2[0x2e] = self.claiming_number.load(Ordering::SeqCst);
            stage2[0x31] = if requested == 0 { 1 } else { 2 };
            for i in 1..=3u8 {
                if self.mixer_assigned.load(Ordering::SeqCst) != 0 {
                    break;
                }
                stage2[0x2f] = i;
                if self.send_announcement_port(&stage2, broadcast).is_err() {
                    self.claiming_number.store(0, Ordering::SeqCst);
                    return false;
                }
                thread::sleep(settings.claim_interval);
                if self.claim_rejected.swap(false, Ordering::SeqCst) {
                    if requested == 0 {
                        continue 'self_assign;
                    }
                    log::warn!("Unable to use device number {requested}, another device has it.");
                    self.claiming_number.store(0, Ordering::SeqCst);
                    return false;
                }
            }
            let assigned = self.mixer_assigned.swap(0, Ordering::SeqCst);
            if assigned > 0 {
                self.claiming_number.store(assigned, Ordering::SeqCst);
            }
            // Stage 3.
            let mut stage3 = announcement_packet(PacketType::DeviceNumberStage3, &name, &[0x01, 0x03, 0x00, 0x26, 0x0d, 0x00]);
            stage3[0x24] = self.claiming_number.load(Ordering::SeqCst);
            for i in 1..=3u8 {
                if self.mixer_assigned.load(Ordering::SeqCst) != 0 {
                    break;
                }
                stage3[0x25] = i;
                if self.send_announcement_port(&stage3, broadcast).is_err() {
                    self.claiming_number.store(0, Ordering::SeqCst);
                    return false;
                }
                thread::sleep(settings.claim_interval);
                if self.claim_rejected.swap(false, Ordering::SeqCst) {
                    if requested == 0 {
                        continue 'self_assign;
                    }
                    log::warn!("Unable to use device number {requested}, another device has it.");
                    self.claiming_number.store(0, Ordering::SeqCst);
                    return false;
                }
            }
            break;
        }
        let number = self.claiming_number.swap(0, Ordering::SeqCst);
        self.keep_alive.lock()[DEVICE_NUMBER_OFFSET] = number;
        self.mixer_assigned.store(0, Ordering::SeqCst);
        number != 0
    }

    fn request_number_from_mixer(&self, mixer: Ipv4Addr) {
        let Some((local, mac)) =
            self.runtime.read().as_ref().map(|r| (r.interface.address, r.interface.hardware_address.unwrap_or_default()))
        else {
            return;
        };
        let mut packet = announcement_packet(PacketType::DeviceNumberStage2, &self.device_name(), &[0x01, 0x02, 0x00, 0x32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x00]);
        packet[0x0b] = 0x01;
        packet[0x24..0x28].copy_from_slice(&local.octets());
        packet[0x28..0x2e].copy_from_slice(&mac);
        packet[0x2f] = 1;
        packet[0x31] = if self.settings.lock().device_number == 0 { 1 } else { 2 };
        if let Err(e) = self.send_announcement_port(&packet, mixer) {
            log::warn!("Unable to send device number request to mixer: {e}");
        }
    }

    fn defend_device_number(&self, invader: Ipv4Addr) {
        let Some(local) = self.local_address() else { return };
        let mut packet = announcement_packet(PacketType::DeviceNumberInUse, &self.device_name(), &[0x01, 0x02, 0x00, 0x29, 0, 0, 0, 0, 0]);
        packet[0x24] = self.device_number();
        packet[0x25..0x29].copy_from_slice(&local.octets());
        log::info!("Defending our device number against {invader}");
        if let Err(e) = self.send_announcement_port(&packet, invader) {
            log::error!("Unable to send device defense packet: {e}");
        }
    }

    fn handle_special_announcement(&self, s: &SpecialAnnouncement) {
        let data = &s.data;
        match s.kind {
            PacketType::DeviceNumberStage1 => {}
            PacketType::DeviceNumberStage2 => {
                if data.len() > 0x2e && self.is_running() && data[0x2e] == self.device_number() {
                    self.defend_device_number(s.address);
                }
            }
            PacketType::DeviceNumberStage3 => {
                if data.len() > 0x24 && self.is_running() && data[0x24] == self.device_number() {
                    self.defend_device_number(s.address);
                }
            }
            PacketType::DeviceNumberWillAssign => {
                if self.claiming_number.load(Ordering::SeqCst) != 0 {
                    self.request_number_from_mixer(s.address);
                }
            }
            PacketType::DeviceNumberAssign => {
                if data.len() > 0x24 {
                    self.mixer_assigned.store(data[0x24], Ordering::SeqCst);
                }
            }
            PacketType::DeviceNumberAssignmentFinished => {
                self.mixer_assigned.store(self.claiming_number.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            PacketType::DeviceNumberInUse => {
                if data.len() <= 0x24 {
                    return;
                }
                let defended = data[0x24];
                if defended == 0 {
                    log::warn!("Ignoring unexplained attempt to defend device 0.");
                } else if defended == self.claiming_number.load(Ordering::SeqCst) {
                    log::warn!("Another device is defending device number {defended}, so we can't use it.");
                    self.claim_rejected.store(true, Ordering::SeqCst);
                } else if self.is_running() && defended == self.device_number() {
                    log::warn!("Another device has claimed it owns our device number, shutting down.");
                    let w = self.me.clone();
                    thread::spawn(move || {
                        if let Some(v) = w.upgrade() {
                            v.stop();
                        }
                    });
                }
            }
            other => log::debug!("Ignoring special announcement {}", other.name()),
        }
    }

    // ------------------------------------------------------------------ receiving status

    fn build_update(&self, data: &[u8], address: Ipv4Addr) -> Option<DeviceUpdate> {
        match util::validate_header(data, util::UPDATE_PORT)? {
            PacketType::MixerStatus => MixerStatus::parse(data, address).ok().map(|m| DeviceUpdate::Mixer(Arc::new(m))),
            PacketType::CdjStatus => match CdjStatus::parse(data, address) {
                Ok(s) => Some(DeviceUpdate::Cdj(Arc::new(s))),
                Err(e) => {
                    log::warn!("Ignoring CDJ status packet: {e}");
                    None
                }
            },
            PacketType::LoadTrackAck => {
                log::info!("Received track load acknowledgment from player {}", data.get(0x21).copied().unwrap_or(0));
                None
            }
            PacketType::MediaResponse => {
                log::debug!("Received media response from {address}");
                None
            }
            other => {
                log::debug!("Ignoring {} packet sent to update port.", other.name());
                None
            }
        }
    }

    /// Process a device update as if it had arrived from the network. Public so that
    /// tests and simulated devices can feed updates in.
    pub fn process_update(&self, update: DeviceUpdate) {
        self.updates.write().insert(DeviceReference::for_update(&update), update.clone());
        if let DeviceUpdate::Cdj(s) = &update {
            self.largest_sync_counter.fetch_max(s.sync_number(), Ordering::SeqCst);
        }
        if update.is_tempo_master() {
            match update.device_master_is_being_yielded_to() {
                None => {
                    if self.master.load(Ordering::SeqCst) {
                        let next = self.next_master.load(Ordering::SeqCst);
                        if next == update.device_number() {
                            self.sync_counter.store(self.largest_sync_counter.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        } else if next == 0xff {
                            log::warn!("Saw master asserted by player {} when we were not yielding it.", update.device_number());
                        } else {
                            log::warn!("Expected to yield master to {next} but saw master asserted by {}", update.device_number());
                        }
                    }
                    self.master.store(false, Ordering::SeqCst);
                    self.next_master.store(0xff, Ordering::SeqCst);
                    self.set_tempo_master(Some(update.clone()));
                    if update.bpm() != util::NO_BPM {
                        self.set_master_tempo(update.effective_tempo());
                    }
                }
                Some(yielding_to) => {
                    if yielding_to == self.device_number() && yielding_to != 0 {
                        let expected = self.master_yielded_from.load(Ordering::SeqCst);
                        if update.device_number() != expected {
                            if expected == 0 {
                                log::info!("Accepting unsolicited Master yield; we must be the only synced device playing.");
                            } else {
                                log::warn!("Expected player {expected} to yield master to us, but player {} did.", update.device_number());
                            }
                        }
                        self.master.store(true, Ordering::SeqCst);
                        self.master_yielded_from.store(0, Ordering::SeqCst);
                        self.set_tempo_master(None);
                        self.set_master_tempo(self.tempo());
                    }
                }
            }
        } else {
            let old = self.tempo_master.read().clone();
            if let Some(old) = old {
                if old.address() == update.address() && old.device_number() == update.device_number() {
                    self.set_tempo_master(None);
                }
            }
        }
        self.update.deliver(&update);
    }

    fn process_beat(&self, beat: &Arc<Beat>) {
        if self.is_running() && beat.tempo_master {
            self.set_master_tempo(beat.effective_tempo());
            if self.following_master.load(Ordering::SeqCst) && !self.is_tempo_master() {
                self.metronome.set_beat_phase(0.0);
            }
            self.master_beat.deliver(beat);
        }
    }

    fn set_tempo_master(&self, new_master: Option<DeviceUpdate>) {
        let changed = {
            let mut guard = self.tempo_master.write();
            let changed = match (&*guard, &new_master) {
                (None, None) => false,
                (Some(_), None) | (None, Some(_)) => true,
                (Some(o), Some(n)) => o.address() != n.address() || o.device_number() != n.device_number(),
            };
            *guard = new_master.clone();
            changed
        };
        if changed {
            self.master_changed.deliver(&new_master);
        }
    }

    fn set_master_tempo(&self, tempo: f64) {
        let old = std::mem::replace(&mut *self.master_tempo.lock(), tempo);
        let has_master = self.tempo_master.read().is_some();
        if has_master && (tempo - old).abs() > self.settings.lock().tempo_epsilon {
            if self.is_synced() {
                self.metronome.set_tempo(tempo);
                self.notify_beat_sender();
            }
            self.tempo_changed.deliver(&tempo);
        }
    }

    /// The current tempo master, if any (not including ourselves).
    pub fn tempo_master(&self) -> Option<DeviceUpdate> {
        self.tempo_master.read().clone()
    }

    /// The tempo of the master player (0.0 when unknown).
    pub fn master_tempo(&self) -> f64 {
        *self.master_tempo.lock()
    }

    /// The latest status from every device seen recently.
    pub fn latest_status(&self) -> Vec<DeviceUpdate> {
        let now = crate::clock::monotonic_nanos();
        let max_age = crate::device_finder::MAXIMUM_AGE_MS * 1_000_000;
        let mut result: Vec<DeviceUpdate> =
            self.updates.read().values().filter(|u| now.saturating_sub(u.timestamp()) <= max_age).cloned().collect();
        result.sort_by_key(|u| u.device_number());
        result
    }

    /// The latest status from a device number.
    pub fn latest_status_for_number(&self, number: u8) -> Option<DeviceUpdate> {
        self.updates.read().values().find(|u| u.device_number() == number).cloned()
    }

    /// The latest CDJ status from a player number.
    pub fn latest_cdj_status(&self, number: u8) -> Option<Arc<CdjStatus>> {
        self.latest_status_for_number(number).and_then(|u| u.as_cdj().cloned())
    }

    /// Players whose addresses are not on our network (they will not hear us).
    pub fn find_unreachable_players(&self) -> Vec<DeviceAnnouncement> {
        let Some(iface) = self.runtime.read().as_ref().map(|r| r.interface.clone()) else { return vec![] };
        self.device_finder
            .current_devices()
            .into_iter()
            .filter(|d| !util::same_network(iface.prefix_length, iface.address, d.address))
            .collect()
    }

    // ------------------------------------------------------------------ sending commands

    fn assemble_and_send(&self, kind: PacketType, payload: &[u8], dest: Ipv4Addr, port: u16) -> io::Result<()> {
        let name = self.keep_alive.lock()[DEVICE_NAME_OFFSET..DEVICE_NAME_OFFSET + DEVICE_NAME_LENGTH].to_vec();
        let packet = util::build_packet(kind, &name, payload);
        let rt = self.runtime.read();
        let rt = rt.as_ref().ok_or_else(|| io::Error::other("Virtual CDJ is not running"))?;
        rt.socket.send_to(&packet, SocketAddrV4::new(dest, port))?;
        Ok(())
    }

    fn ensure_running(&self) -> Result<(), VirtualCdjError> {
        if self.is_running() {
            Ok(())
        } else {
            Err(VirtualCdjError::NotRunning)
        }
    }

    fn status_target(&self, number: u8) -> Result<DeviceUpdate, VirtualCdjError> {
        self.latest_status_for_number(number).ok_or(VirtualCdjError::DeviceNotFound(number))
    }

    fn send_sync_control(&self, target: &DeviceUpdate, command: u8) -> Result<(), VirtualCdjError> {
        self.ensure_running()?;
        let mut payload = SYNC_CONTROL_PAYLOAD;
        payload[2] = self.device_number();
        payload[8] = self.device_number();
        payload[12] = command;
        self.assemble_and_send(PacketType::SyncControl, &payload, target.address(), self.device_finder.config().ports.beat)?;
        Ok(())
    }

    /// Tell a player to turn sync mode on or off.
    pub fn send_sync_mode_command(&self, device: u8, synced: bool) -> Result<(), VirtualCdjError> {
        let target = self.status_target(device)?;
        self.send_sync_control(&target, if synced { 0x10 } else { 0x20 })
    }

    /// Tell a player to become tempo master.
    pub fn appoint_tempo_master(&self, device: u8) -> Result<(), VirtualCdjError> {
        let target = self.status_target(device)?;
        self.send_sync_control(&target, 0x01)
    }

    /// Broadcast a fader start/stop command.
    pub fn send_fader_start_command(&self, start: &BTreeSet<u8>, stop: &BTreeSet<u8>) -> Result<(), VirtualCdjError> {
        self.ensure_running()?;
        let mut payload = FADER_START_PAYLOAD;
        payload[2] = self.device_number();
        for i in 1..=4u8 {
            if start.contains(&i) {
                payload[i as usize + 4] = 0;
            }
            if stop.contains(&i) {
                payload[i as usize + 4] = 1;
            }
        }
        let dest = self.broadcast_address().ok_or(VirtualCdjError::NotRunning)?;
        self.assemble_and_send(PacketType::FaderStartCommand, &payload, dest, self.device_finder.config().ports.beat)?;
        Ok(())
    }

    /// Broadcast which channels are on the air (four-channel form).
    pub fn send_on_air_command(&self, on_air: &BTreeSet<u8>) -> Result<(), VirtualCdjError> {
        self.ensure_running()?;
        let mut payload = CHANNELS_ON_AIR_PAYLOAD;
        payload[2] = self.device_number();
        for i in 1..=4u8 {
            if on_air.contains(&i) {
                payload[i as usize + 4] = 1;
            }
        }
        let dest = self.broadcast_address().ok_or(VirtualCdjError::NotRunning)?;
        self.assemble_and_send(PacketType::ChannelsOnAir, &payload, dest, self.device_finder.config().ports.beat)?;
        Ok(())
    }

    /// Broadcast which channels are on the air (six-channel form used by CDJ-3000s).
    pub fn send_on_air_extended_command(&self, on_air: &BTreeSet<u8>) -> Result<(), VirtualCdjError> {
        self.ensure_running()?;
        let mut payload = CHANNELS_ON_AIR_EXTENDED_PAYLOAD;
        payload[2] = self.device_number();
        for i in 1..=4u8 {
            if on_air.contains(&i) {
                payload[i as usize + 4] = 1;
            }
        }
        for i in 5..=6u8 {
            if on_air.contains(&i) {
                payload[i as usize + 9] = 1;
            }
        }
        let dest = self.broadcast_address().ok_or(VirtualCdjError::NotRunning)?;
        self.assemble_and_send(PacketType::ChannelsOnAir, &payload, dest, self.device_finder.config().ports.beat)?;
        Ok(())
    }

    /// Ask a player what media is in a slot (the answer is logged).
    pub fn send_media_query(&self, player: u8, slot: TrackSourceSlot) -> Result<(), VirtualCdjError> {
        self.ensure_running()?;
        let announcement = self.device_finder.latest_announcement_from(player).ok_or(VirtualCdjError::DeviceNotFound(player))?;
        let mut payload = MEDIA_QUERY_PAYLOAD;
        payload[2] = self.device_number();
        payload[5..9].copy_from_slice(&self.local_address().unwrap_or(Ipv4Addr::UNSPECIFIED).octets());
        payload[12] = player;
        payload[16] = slot.protocol_value();
        self.assemble_and_send(PacketType::MediaQuery, &payload, announcement.address, self.device_finder.config().ports.update)?;
        Ok(())
    }

    /// Tell a player to load a track.
    pub fn send_load_track_command(
        &self,
        target_player: u8,
        rekordbox_id: u32,
        source_player: u8,
        source_slot: TrackSourceSlot,
        source_type: TrackType,
    ) -> Result<(), VirtualCdjError> {
        self.ensure_running()?;
        let target = self.status_target(target_player)?;
        let mut payload = LOAD_TRACK_PAYLOAD;
        let us = self.device_number();
        payload[0x02] = us;
        payload[0x05] = us;
        payload[0x09] = source_player;
        payload[0x0a] = source_slot.protocol_value();
        payload[0x0b] = source_type.protocol_value();
        payload[0x21] = target.device_number().wrapping_sub(1);
        if target.device_name().starts_with("XDJ-XZ") {
            payload[0x01] = 1;
            payload[0x2c] = 0x32;
            let devices = self.device_finder.current_devices();
            if let Some(n) = devices.iter().map(|d| d.number).find(|n| (0x11..0x20).contains(n)) {
                payload[0x02] = n;
                payload[0x05] = n;
            } else if let Some(n) = devices.iter().map(|d| d.number).find(|n| (0x29..0x30).contains(n)) {
                payload[0x02] = n;
                payload[0x05] = n;
            }
        }
        util::number_to_bytes(u64::from(rekordbox_id), &mut payload, 0x0d, 4);
        self.assemble_and_send(PacketType::LoadTrackCommand, &payload, target.address(), self.device_finder.config().ports.update)?;
        Ok(())
    }

    /// Send a set of "My Settings" to a player.
    pub fn send_load_settings_command(&self, target_player: u8, settings: &PlayerSettings) -> Result<(), VirtualCdjError> {
        self.ensure_running()?;
        let target = self.status_target(target_player)?;
        let payload = settings.build_payload(self.device_number(), target.device_number());
        self.assemble_and_send(PacketType::LoadSettingsCommand, &payload, target.address(), self.device_finder.config().ports.update)?;
        Ok(())
    }

    // ------------------------------------------------------------------ acting as a player

    /// Start or stop sending status packets (which makes us visible as a real player
    /// that can become tempo master and receive metadata for CDs and unanalyzed tracks).
    pub fn set_sending_status(&self, send: bool) -> Result<(), VirtualCdjError> {
        if self.is_sending_status() == send {
            return Ok(());
        }
        if send {
            self.ensure_running()?;
            let n = self.device_number();
            if !(1..=4).contains(&n) {
                return Err(VirtualCdjError::IllegalState(
                    "Can only send status when using a standard player number, 1 through 4.".into(),
                ));
            }
            self.beat_finder.start()?;
            let still = Arc::new(AtomicBool::new(true));
            *self.sending_status.lock() = Some(still.clone());
            let w = self.me.clone();
            thread::Builder::new().name("beat-link VirtualCdj status sender".into()).spawn(move || {
                while still.load(Ordering::SeqCst) {
                    let Some(v) = w.upgrade() else { break };
                    v.send_status();
                    let interval = v.settings.lock().status_interval_ms;
                    drop(v);
                    thread::sleep(Duration::from_millis(interval));
                }
            })?;
            if self.is_synced() {
                self.following_master.store(true, Ordering::SeqCst);
            }
            if self.is_playing() {
                *self.beat_sender.lock() = Some(BeatSender::start(self.me.clone()));
            }
        } else {
            self.following_master.store(false, Ordering::SeqCst);
            if let Some(flag) = self.sending_status.lock().take() {
                flag.store(false, Ordering::SeqCst);
            }
            if let Some(sender) = self.beat_sender.lock().take() {
                sender.shut_down();
            }
        }
        Ok(())
    }

    pub fn is_sending_status(&self) -> bool {
        self.sending_status.lock().is_some()
    }

    fn notify_beat_sender(&self) {
        if let Some(s) = self.beat_sender.lock().as_ref() {
            s.timeline_changed();
        }
    }

    /// Start or stop our simulated playback.
    pub fn set_playing(&self, playing: bool) {
        if self.playing.swap(playing, Ordering::SeqCst) == playing {
            return;
        }
        if playing {
            let beat = self.where_stopped.lock().beat();
            self.metronome.jump_to_beat(beat);
            if self.is_sending_status() {
                *self.beat_sender.lock() = Some(BeatSender::start(self.me.clone()));
            }
        } else {
            if let Some(s) = self.beat_sender.lock().take() {
                s.shut_down();
            }
            *self.where_stopped.lock() = self.metronome.snapshot();
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::SeqCst)
    }

    /// Where we are in our (virtual) playback timeline.
    pub fn playback_position(&self) -> Snapshot {
        if self.is_playing() {
            self.metronome.snapshot()
        } else {
            *self.where_stopped.lock()
        }
    }

    /// Nudge our playback timeline by `ms` milliseconds.
    pub fn adjust_playback_position(&self, ms: f64) {
        if ms != 0.0 {
            self.metronome.adjust_start(-ms);
            if self.metronome.snapshot().beat() < 1 {
                self.metronome.adjust_start(metronome::beats_to_milliseconds(f64::from(self.metronome.beats_per_bar()), self.metronome.tempo()));
            }
            self.notify_beat_sender();
        }
    }

    /// Request the tempo master role from the current master (or just take it if none).
    pub fn become_tempo_master(&self) -> Result<(), VirtualCdjError> {
        if !self.is_sending_status() {
            return Err(VirtualCdjError::IllegalState("Must be sending status updates to become the tempo master.".into()));
        }
        match self.tempo_master() {
            Some(current) => {
                let mut payload = MASTER_HANDOFF_REQUEST_PAYLOAD;
                payload[2] = self.device_number();
                payload[8] = self.device_number();
                self.requesting_master_role_from.store(current.device_number(), Ordering::SeqCst);
                self.assemble_and_send(PacketType::MasterHandoffRequest, &payload, current.address(), self.device_finder.config().ports.beat)?;
            }
            None => {
                if !self.master.load(Ordering::SeqCst) {
                    self.requesting_master_role_from.store(0, Ordering::SeqCst);
                    self.set_master_tempo(self.tempo());
                    self.master.store(true, Ordering::SeqCst);
                }
            }
        }
        Ok(())
    }

    /// Whether we are the tempo master.
    pub fn is_tempo_master(&self) -> bool {
        self.master.load(Ordering::SeqCst)
    }

    fn handle_master_handoff(&self, h: MasterHandoff) {
        match h {
            MasterHandoff::YieldTo(device) => {
                if self.is_tempo_master() {
                    if self.is_sending_status() && self.device_number() != device {
                        self.next_master.store(device, Ordering::SeqCst);
                        match self.latest_status_for_number(device) {
                            None => log::warn!("Unable to send master yield response to device {device}: no status seen."),
                            Some(target) => {
                                let mut payload = YIELD_ACK_PAYLOAD;
                                payload[2] = self.device_number();
                                payload[8] = self.device_number();
                                if let Err(e) = self.assemble_and_send(
                                    PacketType::MasterHandoffResponse,
                                    &payload,
                                    target.address(),
                                    self.device_finder.config().ports.update,
                                ) {
                                    log::error!("Problem sending master yield acknowledgment to player {device}: {e}");
                                }
                            }
                        }
                    }
                } else {
                    log::warn!("Ignoring instruction to yield master to device {device}: we were not tempo master.");
                }
            }
            MasterHandoff::Response(device, yielded) => {
                if yielded && self.is_sending_status() {
                    if device == self.requesting_master_role_from.load(Ordering::SeqCst) {
                        self.requesting_master_role_from.store(0, Ordering::SeqCst);
                        self.master_yielded_from.store(device, Ordering::SeqCst);
                    } else {
                        log::warn!("Ignoring unexpected master yield response from player {device}.");
                    }
                } else if !yielded {
                    log::warn!("Ignoring master yield response with unexpected non-yielding value.");
                }
            }
        }
    }

    /// Turn our sync mode on or off.
    pub fn set_synced(&self, sync: bool) {
        if self.synced.load(Ordering::SeqCst) != sync {
            self.following_master.store(sync && self.is_sending_status(), Ordering::SeqCst);
            if !self.is_tempo_master() && self.tempo_master().is_some() {
                let t = self.master_tempo();
                if t > 0.0 {
                    self.set_tempo(t);
                }
            }
        }
        self.synced.store(sync, Ordering::SeqCst);
    }

    pub fn is_synced(&self) -> bool {
        self.synced.load(Ordering::SeqCst)
    }

    pub fn set_on_air(&self, audible: bool) {
        self.on_air.store(audible, Ordering::SeqCst);
    }

    pub fn is_on_air(&self) -> bool {
        self.on_air.load(Ordering::SeqCst)
    }

    /// Set the tempo of our virtual playback.
    pub fn set_tempo(&self, bpm: f64) {
        if bpm <= 0.0 {
            return;
        }
        let old = self.metronome.tempo();
        self.metronome.set_tempo(bpm);
        self.notify_beat_sender();
        if self.is_tempo_master() && (bpm - old).abs() > self.settings.lock().tempo_epsilon {
            self.tempo_changed.deliver(&bpm);
        }
    }

    pub fn tempo(&self) -> f64 {
        self.metronome.tempo()
    }

    /// Jump our virtual playback to a beat.
    pub fn jump_to_beat(&self, beat: i64) {
        const MAX_BEAT: i64 = 65536;
        let beat = if beat < 1 { 1 } else if beat <= MAX_BEAT { beat } else { ((beat - 1) % MAX_BEAT) + 1 };
        if self.is_playing() {
            self.metronome.jump_to_beat(beat);
        } else {
            let t = self.metronome.time_of_beat(beat);
            *self.where_stopped.lock() = self.metronome.snapshot_at(t);
        }
    }

    /// Send a beat packet describing a point in our timeline.
    fn send_beat_snapshot(&self, snapshot: &Snapshot) -> i64 {
        let mut payload = BEAT_PAYLOAD;
        let n = self.device_number();
        payload[0x02] = n;
        let interval = snapshot.beat_interval() as u64;
        util::number_to_bytes(interval, &mut payload, 0x05, 4);
        util::number_to_bytes(interval * 2, &mut payload, 0x09, 4);
        util::number_to_bytes(interval * 4, &mut payload, 0x11, 4);
        util::number_to_bytes(interval * 8, &mut payload, 0x19, 4);
        let beats_left = 5 - i64::from(snapshot.beat_within_bar());
        let next_bar = (snapshot.beat_interval() * beats_left as f64) as u64;
        util::number_to_bytes(next_bar, &mut payload, 0x0d, 4);
        util::number_to_bytes(next_bar + snapshot.bar_interval() as u64, &mut payload, 0x15, 4);
        util::number_to_bytes((snapshot.tempo() * 100.0).round() as u64, &mut payload, 0x3b, 2);
        payload[0x3d] = snapshot.beat_within_bar() as u8;
        payload[0x40] = n;
        if let Some(dest) = self.broadcast_address() {
            if let Err(e) = self.assemble_and_send(PacketType::Beat, &payload, dest, self.device_finder.config().ports.beat) {
                log::error!("VirtualCdj failed to send beat packet: {e}");
            }
        }
        snapshot.beat()
    }

    /// Send a beat packet for the current playback position.
    pub fn send_beat(&self) -> i64 {
        let s = self.playback_position();
        self.send_beat_snapshot(&s)
    }

    fn avoid_beat_packet(&self) -> Snapshot {
        let mut state = self.playback_position();
        let mut distance = state.distance_from_beat();
        while self.is_playing()
            && ((distance < 0.0 && distance.abs() <= BeatSender::SLEEP_THRESHOLD)
                || (0.0..=BeatSender::BEAT_THRESHOLD + 1.0).contains(&distance))
        {
            thread::sleep(Duration::from_millis(2));
            state = self.playback_position();
            distance = state.distance_from_beat();
        }
        state
    }

    /// Build the status payload we would send right now.
    pub fn build_status_payload(&self, state: &Snapshot) -> [u8; 0xfd] {
        let playing = self.is_playing();
        let master = self.is_tempo_master();
        let mut payload = STATUS_PAYLOAD;
        let n = self.device_number();
        payload[0x02] = n;
        payload[0x05] = n;
        payload[0x08] = u8::from(playing);
        payload[0x09] = n;
        payload[0x5c] = if playing { 3 } else { 5 };
        util::number_to_bytes(u64::from(self.sync_counter.load(Ordering::SeqCst)), &mut payload, 0x65, 4);
        payload[0x6a] = 0x84
            + if playing { 0x40 } else { 0 }
            + if master { 0x20 } else { 0 }
            + if self.is_synced() { 0x10 } else { 0 }
            + if self.is_on_air() { 0x08 } else { 0 };
        payload[0x6c] = if playing { 0x7a } else { 0x7e };
        util::number_to_bytes((self.tempo() * 100.0).round() as u64, &mut payload, 0x73, 2);
        payload[0x7e] = if playing { 9 } else { 1 };
        payload[0x7f] = u8::from(master);
        payload[0x80] = self.next_master.load(Ordering::SeqCst);
        util::number_to_bytes(state.beat().max(0) as u64, &mut payload, 0x81, 4);
        payload[0x87] = state.beat_within_bar() as u8;
        util::number_to_bytes(u64::from(self.packet_counter.fetch_add(1, Ordering::SeqCst) + 1), &mut payload, 0xa9, 4);
        payload
    }

    fn send_status(&self) {
        let state = self.avoid_beat_packet();
        let payload = self.build_status_payload(&state);
        let name = self.keep_alive.lock()[DEVICE_NAME_OFFSET..DEVICE_NAME_OFFSET + DEVICE_NAME_LENGTH].to_vec();
        let packet = util::build_packet(PacketType::CdjStatus, &name, &payload);
        let port = self.device_finder.config().ports.update;
        let rt = self.runtime.read();
        let Some(rt) = rt.as_ref() else { return };
        for device in self.device_finder.current_devices() {
            if let Err(e) = rt.socket.send_to(&packet, SocketAddrV4::new(device.address, port)) {
                log::warn!("Unable to send status packet to {device:?}: {e}");
            }
        }
    }
}

impl std::fmt::Debug for VirtualCdj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "VirtualCdj[number:{}, name:{}, active:{}, localAddress:{:?}, masterTempo:{}, sendingStatus:{}]",
            self.device_number(),
            self.device_name(),
            self.is_running(),
            self.local_address(),
            self.master_tempo(),
            self.is_sending_status()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_payload_is_well_formed() {
        assert_eq!(0x1f + STATUS_PAYLOAD.len(), 0x11c);
        assert_eq!(util::bytes_to_number(&STATUS_PAYLOAD, 3, 2) as usize, 0x11c - 0x24);
    }

    #[test]
    fn announcement_packets_have_name_at_0x0c() {
        let p = announcement_packet(PacketType::DeviceHello, "beat-link", &[0x01, 0x04, 0x00, 0x26, 0x01, 0x40]);
        assert_eq!(p.len(), 0x26);
        assert_eq!(util::read_name(&p, 0x0c, 20), "beat-link");
        assert_eq!(util::validate_header(&p, util::ANNOUNCEMENT_PORT), Some(PacketType::DeviceHello));
    }

    #[test]
    fn keep_alive_template_layout() {
        assert_eq!(KEEP_ALIVE_TEMPLATE.len(), 0x36);
        assert_eq!(util::read_name(&KEEP_ALIVE_TEMPLATE, DEVICE_NAME_OFFSET, DEVICE_NAME_LENGTH), "beat-link");
        assert_eq!(util::validate_header(&KEEP_ALIVE_TEMPLATE, util::ANNOUNCEMENT_PORT), Some(PacketType::DeviceKeepAlive));
    }
}
