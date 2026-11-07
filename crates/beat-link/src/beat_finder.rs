//! Watches port 50001 for beats, precise positions, on-air reports, sync control,
//! fader start and tempo master handoff packets. Port of `BeatFinder`.

use crate::device_finder::DeviceFinder;
use crate::listeners::ListenerSet;
use crate::network;
use crate::packets::{Beat, PrecisePosition, XDJ_AZ_NAME};
use crate::util::{self, PacketType};
use parking_lot::{Mutex, RwLock};
use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

/// A command telling us to start and/or stop playing (from a mixer's fader start).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaderStart {
    pub start: BTreeSet<u8>,
    pub stop: BTreeSet<u8>,
}

/// A request from a player that we hand it the tempo master role, or its response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterHandoff {
    /// Device `n` is asking the current master to yield to it.
    YieldTo(u8),
    /// Device `n` responded to our request; `true` if it agreed to yield.
    Response(u8, bool),
}

/// Sync control commands: become master, or turn sync on/off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncCommand {
    BecomeMaster,
    SyncOn,
    SyncOff,
}

type BeatEnricher = Arc<dyn Fn(&mut Beat) + Send + Sync>;

/// Listens on the beat port.
pub struct BeatFinder {
    device_finder: Arc<DeviceFinder>,
    running: AtomicBool,
    socket: Mutex<Option<Arc<UdpSocket>>>,
    enricher: RwLock<Option<BeatEnricher>>,
    last_xdj_az_on_air: AtomicU64,
    /// Delivered before `beat`, for the virtual CDJ and time finder.
    pub beat_internal: ListenerSet<Arc<Beat>>,
    pub beat: ListenerSet<Arc<Beat>>,
    pub precise_position: ListenerSet<Arc<PrecisePosition>>,
    pub on_air: ListenerSet<BTreeSet<u8>>,
    pub sync: ListenerSet<SyncCommand>,
    pub master_handoff: ListenerSet<MasterHandoff>,
    pub fader_start: ListenerSet<FaderStart>,
    pub lifecycle: ListenerSet<bool>,
}

impl BeatFinder {
    pub fn new(device_finder: Arc<DeviceFinder>) -> Arc<Self> {
        Arc::new(BeatFinder {
            device_finder,
            running: AtomicBool::new(false),
            socket: Mutex::new(None),
            enricher: RwLock::new(None),
            last_xdj_az_on_air: AtomicU64::new(0),
            beat_internal: ListenerSet::new(),
            beat: ListenerSet::new(),
            precise_position: ListenerSet::new(),
            on_air: ListenerSet::new(),
            sync: ListenerSet::new(),
            master_handoff: ListenerSet::new(),
            fader_start: ListenerSet::new(),
            lifecycle: ListenerSet::new(),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Install the function that fills in context-dependent beat fields.
    pub fn set_enricher<F>(&self, f: F)
    where
        F: Fn(&mut Beat) + Send + Sync + 'static,
    {
        *self.enricher.write() = Some(Arc::new(f));
    }

    pub fn start(self: &Arc<Self>) -> io::Result<()> {
        let mut guard = self.socket.lock();
        if guard.is_some() {
            return Ok(());
        }
        let config = self.device_finder.config().clone();
        let socket = Arc::new(network::bind_udp(SocketAddrV4::new(config.bind_address, config.ports.beat))?);
        *guard = Some(socket.clone());
        self.running.store(true, Ordering::SeqCst);
        drop(guard);
        let me = Arc::downgrade(self);
        thread::Builder::new().name("beat-link BeatFinder receiver".into()).spawn(move || {
            let mut buffer = [0u8; 512];
            loop {
                let Some(finder) = me.upgrade() else { break };
                if !finder.is_running() {
                    break;
                }
                match socket.recv_from(&mut buffer) {
                    Ok((len, from)) => {
                        if let Some(address) = network::ipv4_of(&from) {
                            if !finder.device_finder.is_address_ignored(address) {
                                finder.handle_packet(&buffer[..len], address);
                            }
                        }
                    }
                    Err(e) if network::is_timeout(&e) => {}
                    Err(e) => {
                        if finder.is_running() {
                            log::warn!("Problem reading from beat socket, stopping: {e}");
                            finder.stop();
                        }
                        break;
                    }
                }
            }
        })?;
        self.lifecycle.deliver(&true);
        Ok(())
    }

    pub fn stop(&self) {
        if self.socket.lock().take().is_some() {
            self.running.store(false, Ordering::SeqCst);
            self.lifecycle.deliver(&false);
        }
    }

    /// Whether an XDJ-AZ in Pro DJ Link mode has reported on-air channels recently.
    pub fn can_see_xdj_az_in_pro_dj_link_mode(&self) -> bool {
        let saw = self.last_xdj_az_on_air.load(Ordering::SeqCst);
        saw != 0 && crate::clock::monotonic_nanos().saturating_sub(saw) < 1_000_000_000
    }

    /// Interpret a packet received on the beat port. Public so that simulated
    /// devices and tests can inject packets.
    pub fn handle_packet(&self, data: &[u8], address: Ipv4Addr) {
        let Some(kind) = util::validate_header(data, util::BEAT_PORT) else { return };
        let name = util::read_name(data, 0x0b, 20);
        if self.device_finder.is_device_name_ignored(&name) {
            return;
        }
        match kind {
            PacketType::Beat => match Beat::parse(data, address) {
                Ok(mut beat) => {
                    if let Some(e) = self.enricher.read().clone() {
                        e(&mut beat);
                    }
                    let beat = Arc::new(beat);
                    self.beat_internal.deliver(&beat);
                    self.beat.deliver(&beat);
                }
                Err(e) => log::debug!("Ignoring beat packet: {e}"),
            },
            PacketType::PrecisePosition => match PrecisePosition::parse(data, address) {
                Ok(p) => self.precise_position.deliver(&Arc::new(p)),
                Err(e) => log::debug!("Ignoring precise position packet: {e}"),
            },
            PacketType::ChannelsOnAir => {
                if data.len() >= 0x2d {
                    let mut audible = BTreeSet::new();
                    for channel in 1..=4u8 {
                        if data[0x23 + channel as usize] != 0 {
                            audible.insert(channel);
                        }
                    }
                    if data.len() >= 0x35 {
                        for channel in 5..=6u8 {
                            if data[0x28 + channel as usize] != 0 {
                                audible.insert(channel);
                            }
                        }
                    }
                    if name == XDJ_AZ_NAME {
                        self.last_xdj_az_on_air.store(crate::clock::monotonic_nanos(), Ordering::SeqCst);
                    }
                    self.on_air.deliver(&audible);
                }
            }
            PacketType::SyncControl => {
                if data.len() >= 0x2c {
                    // As in beat-link, "become master" also turns sync on.
                    match data[0x2b] {
                        0x01 => {
                            self.sync.deliver(&SyncCommand::BecomeMaster);
                            self.sync.deliver(&SyncCommand::SyncOn);
                        }
                        0x10 => self.sync.deliver(&SyncCommand::SyncOn),
                        0x20 => self.sync.deliver(&SyncCommand::SyncOff),
                        other => log::warn!("Unrecognized sync command 0x{other:02x}"),
                    }
                }
            }
            PacketType::MasterHandoffRequest => {
                if data.len() >= 0x28 {
                    self.master_handoff.deliver(&MasterHandoff::YieldTo(data[0x21]));
                }
            }
            PacketType::MasterHandoffResponse => {
                if data.len() >= 0x2c {
                    self.master_handoff.deliver(&MasterHandoff::Response(data[0x21], data[0x2b] == 1));
                }
            }
            PacketType::FaderStartCommand => {
                if data.len() >= 0x28 {
                    let mut start = BTreeSet::new();
                    let mut stop = BTreeSet::new();
                    for channel in 1..=4u8 {
                        match data[0x23 + channel as usize] {
                            0 => {
                                start.insert(channel);
                            }
                            1 => {
                                stop.insert(channel);
                            }
                            2 => {}
                            other => log::warn!("Unrecognized fader start command {other} for channel {channel}"),
                        }
                    }
                    self.fader_start.deliver(&FaderStart { start, stop });
                }
            }
            other => log::debug!("Ignoring {} packet on beat port", other.name()),
        }
    }
}
