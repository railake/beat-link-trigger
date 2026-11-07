//! Watches for device announcements on port 50000, keeping track of which devices
//! are on the network. Port of `org.deepsymmetry.beatlink.DeviceFinder`.

use crate::clock::current_time_millis;
use crate::listeners::ListenerSet;
use crate::network::{self, NetworkConfig};
use crate::packets::{DeviceAnnouncement, DeviceReference};
use crate::util::{self, PacketType};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

/// Devices that have not announced themselves for this long are considered gone.
pub const MAXIMUM_AGE_MS: u64 = 10_000;

/// Device models that can serve metadata to more than three clients.
pub const METADATA_FLEXIBLE_DEVICES: [&str; 3] = ["CDJ-3000", "CDJ-3000X", "XDJ-AZ"];

/// A non-keep-alive packet received on the announcement port (device number claims,
/// defenses and assignments), which the virtual CDJ needs to see.
#[derive(Debug, Clone)]
pub struct SpecialAnnouncement {
    pub kind: PacketType,
    pub data: Vec<u8>,
    pub address: Ipv4Addr,
}

/// Tracks devices present on the network.
pub struct DeviceFinder {
    config: NetworkConfig,
    running: AtomicBool,
    socket: Mutex<Option<Arc<UdpSocket>>>,
    devices: RwLock<HashMap<DeviceReference, DeviceAnnouncement>>,
    first_device_time: AtomicU64,
    start_time: AtomicU64,
    ignored_addresses: RwLock<HashSet<Ipv4Addr>>,
    ignored_names: RwLock<HashSet<String>>,
    limit3_seen: AtomicBool,
    /// Called when a new device appears.
    pub device_found: ListenerSet<DeviceAnnouncement>,
    /// Called when a device disappears.
    pub device_lost: ListenerSet<DeviceAnnouncement>,
    /// Called for every keep-alive (used to defend our device number).
    pub keep_alive: ListenerSet<DeviceAnnouncement>,
    /// Called for device-number claim related packets.
    pub special_announcement: ListenerSet<SpecialAnnouncement>,
    /// Called with `true` when started and `false` when stopped.
    pub lifecycle: ListenerSet<bool>,
}

impl DeviceFinder {
    pub fn new(config: NetworkConfig) -> Arc<Self> {
        Arc::new(DeviceFinder {
            config,
            running: AtomicBool::new(false),
            socket: Mutex::new(None),
            devices: RwLock::new(HashMap::new()),
            first_device_time: AtomicU64::new(0),
            start_time: AtomicU64::new(0),
            ignored_addresses: RwLock::new(HashSet::new()),
            ignored_names: RwLock::new(HashSet::new()),
            limit3_seen: AtomicBool::new(false),
            device_found: ListenerSet::new(),
            device_lost: ListenerSet::new(),
            keep_alive: ListenerSet::new(),
            special_announcement: ListenerSet::new(),
            lifecycle: ListenerSet::new(),
        })
    }

    pub fn config(&self) -> &NetworkConfig {
        &self.config
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// When we started listening (wall-clock ms).
    pub fn start_time(&self) -> u64 {
        self.start_time.load(Ordering::SeqCst)
    }

    /// When the first device of the current session was seen, if any.
    pub fn first_device_time(&self) -> Option<u64> {
        match self.first_device_time.load(Ordering::SeqCst) {
            0 => None,
            t => Some(t),
        }
    }

    /// Start listening for announcements.
    pub fn start(self: &Arc<Self>) -> io::Result<()> {
        let mut guard = self.socket.lock();
        if guard.is_some() {
            return Ok(());
        }
        let socket = Arc::new(network::bind_udp(SocketAddrV4::new(
            self.config.bind_address,
            self.config.ports.announcement,
        ))?);
        *guard = Some(socket.clone());
        self.start_time.store(current_time_millis(), Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
        drop(guard);
        let me = Arc::downgrade(self);
        thread::Builder::new().name("beat-link DeviceFinder receiver".into()).spawn(move || {
            let mut buffer = [0u8; 512];
            loop {
                let Some(finder) = me.upgrade() else { break };
                if !finder.is_running() {
                    break;
                }
                match socket.recv_from(&mut buffer) {
                    Ok((len, from)) => {
                        if let Some(address) = network::ipv4_of(&from) {
                            if !finder.is_address_ignored(address) {
                                finder.handle_packet(&buffer[..len], address);
                            }
                        }
                    }
                    Err(e) if network::is_timeout(&e) => {}
                    Err(e) => {
                        if finder.is_running() {
                            log::warn!("Problem reading from DeviceAnnouncement socket, stopping: {e}");
                            finder.stop();
                        }
                        break;
                    }
                }
                finder.expire_devices();
            }
        })?;
        self.lifecycle.deliver(&true);
        Ok(())
    }

    /// Stop listening, and report all devices as lost.
    pub fn stop(&self) {
        let had = self.socket.lock().take().is_some();
        if had {
            self.running.store(false, Ordering::SeqCst);
            self.flush();
            self.lifecycle.deliver(&false);
        }
    }

    /// Forget all devices (reporting them lost), e.g. after a network change.
    pub fn flush(&self) {
        let last: Vec<DeviceAnnouncement> = self.devices.write().drain().map(|(_, v)| v).collect();
        self.first_device_time.store(0, Ordering::SeqCst);
        for a in last {
            self.device_lost.deliver(&a);
        }
    }

    fn handle_packet(&self, data: &[u8], address: Ipv4Addr) {
        match util::validate_header(data, util::ANNOUNCEMENT_PORT) {
            Some(PacketType::DeviceKeepAlive) => match DeviceAnnouncement::parse(data, address) {
                Ok(announcement) => {
                    if self.is_device_name_ignored(&announcement.name) {
                        return;
                    }
                    if announcement.is_opus_quad() {
                        for i in 1..=4 {
                            if let Ok(a) = DeviceAnnouncement::parse_as(data, address, Some(i)) {
                                self.process_announcement(a);
                            }
                        }
                    } else {
                        self.keep_alive.deliver(&announcement);
                        self.process_announcement(announcement);
                    }
                }
                Err(e) => log::warn!("Ignoring malformed keep-alive: {e}"),
            },
            Some(PacketType::DeviceHello) => {}
            Some(kind) => {
                let name = util::read_name(data, 0x0c, 20);
                if !self.is_device_name_ignored(&name) {
                    self.special_announcement.deliver(&SpecialAnnouncement { kind, data: data.to_vec(), address });
                }
            }
            None => {}
        }
    }

    /// Process an announcement as if it had been received (also used by tests and
    /// by simulated devices).
    pub fn process_announcement(&self, announcement: DeviceAnnouncement) {
        let key = DeviceReference::for_announcement(&announcement);
        let _ = self.first_device_time.compare_exchange(0, current_time_millis(), Ordering::SeqCst, Ordering::SeqCst);
        let is_new = self.devices.write().insert(key, announcement.clone()).is_none();
        if is_new {
            if self.is_device_metadata_limited(&announcement) {
                self.limit3_seen.store(true, Ordering::SeqCst);
            }
            log::info!("Found device: {announcement:?}");
            self.device_found.deliver(&announcement);
        }
    }

    fn expire_devices(&self) {
        let now = current_time_millis();
        let expired: Vec<DeviceAnnouncement> = {
            let mut devices = self.devices.write();
            let stale: Vec<DeviceReference> = devices
                .iter()
                .filter(|(_, a)| now.saturating_sub(a.timestamp) > MAXIMUM_AGE_MS)
                .map(|(k, _)| *k)
                .collect();
            stale.into_iter().filter_map(|k| devices.remove(&k)).collect()
        };
        if self.devices.read().is_empty() {
            self.first_device_time.store(0, Ordering::SeqCst);
        }
        for a in expired {
            if self.is_device_metadata_limited(&a) && !self.current_devices().iter().any(|d| self.is_device_metadata_limited(d)) {
                self.limit3_seen.store(false, Ordering::SeqCst);
            }
            log::info!("Lost device: {a:?}");
            self.device_lost.deliver(&a);
        }
    }

    /// All devices currently visible, sorted by device number.
    pub fn current_devices(&self) -> Vec<DeviceAnnouncement> {
        let mut result: Vec<DeviceAnnouncement> = self.devices.read().values().cloned().collect();
        result.sort_by_key(|a| (a.number, a.address));
        result
    }

    /// The most recent announcement from a device number, if it is present.
    pub fn latest_announcement_from(&self, number: u8) -> Option<DeviceAnnouncement> {
        self.devices.read().values().find(|a| a.number == number).cloned()
    }

    pub fn add_ignored_address(&self, address: Ipv4Addr) {
        self.ignored_addresses.write().insert(address);
    }
    pub fn remove_ignored_address(&self, address: Ipv4Addr) {
        self.ignored_addresses.write().remove(&address);
    }
    pub fn is_address_ignored(&self, address: Ipv4Addr) -> bool {
        self.ignored_addresses.read().contains(&address)
    }
    pub fn add_ignored_device_name(&self, name: &str) {
        self.ignored_names.write().insert(name.to_string());
    }
    pub fn remove_ignored_device_name(&self, name: &str) {
        self.ignored_names.write().remove(name);
    }
    pub fn is_device_name_ignored(&self, name: &str) -> bool {
        self.ignored_names.read().contains(name)
    }

    /// Older players can only serve metadata to three other players at a time.
    pub fn is_device_metadata_limited(&self, a: &DeviceAnnouncement) -> bool {
        a.number < 7 && !METADATA_FLEXIBLE_DEVICES.contains(&a.name.as_str())
    }

    pub fn is_any_device_limited_to_three_database_clients(&self) -> bool {
        self.limit3_seen.load(Ordering::SeqCst)
    }
}

impl Drop for DeviceFinder {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}
