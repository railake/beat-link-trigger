//! Output channels available to triggers and expressions: MIDI, OSC, and commands
//! to the DJ Link players (routed back to the running application).

use parking_lot::{Mutex, RwLock};
use rhai::Dynamic;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::net::UdpSocket;

/// Commands expressions can send to the DJ Link network.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerCommand {
    SyncMode(u8, bool),
    AppointMaster(u8),
    FaderStart(BTreeSet<u8>, BTreeSet<u8>),
    OnAir(BTreeSet<u8>),
    LinkTempo(f64),
}

type CommandHandler = Box<dyn Fn(PlayerCommand) -> Result<(), String> + Send + Sync>;
type TimeQuery = Box<dyn Fn(u8) -> Option<i64> + Send + Sync>;

/// The hub through which triggers and expressions reach the outside world.
pub struct Outputs {
    pub midi: MidiOutputs,
    pub osc: OscSender,
    command_handler: RwLock<Option<CommandHandler>>,
    time_query: RwLock<Option<TimeQuery>>,
}

impl Outputs {
    pub fn new() -> Self {
        Outputs { midi: MidiOutputs::new(), osc: OscSender::new(), command_handler: RwLock::new(None), time_query: RwLock::new(None) }
    }

    /// An instance whose MIDI output only records messages (for tests).
    pub fn new_for_tests() -> Self {
        let o = Self::new();
        o.midi.set_record_only(true);
        o
    }

    pub fn set_command_handler<F>(&self, f: F)
    where
        F: Fn(PlayerCommand) -> Result<(), String> + Send + Sync + 'static,
    {
        *self.command_handler.write() = Some(Box::new(f));
    }

    pub fn set_time_query<F>(&self, f: F)
    where
        F: Fn(u8) -> Option<i64> + Send + Sync + 'static,
    {
        *self.time_query.write() = Some(Box::new(f));
    }

    pub fn player_command(&self, cmd: PlayerCommand) -> Result<(), String> {
        match self.command_handler.read().as_ref() {
            Some(h) => h(cmd),
            None => Err("Not connected to a DJ Link network".into()),
        }
    }

    pub fn query_time(&self, player: u8) -> Option<i64> {
        self.time_query.read().as_ref().and_then(|q| q(player))
    }
}

impl Default for Outputs {
    fn default() -> Self {
        Self::new()
    }
}

/// Status bytes for the MIDI messages we send.
pub mod midi_bytes {
    pub const NOTE_OFF: u8 = 0x80;
    pub const NOTE_ON: u8 = 0x90;
    pub const CONTROL: u8 = 0xb0;
    pub const CLOCK: u8 = 0xf8;
    pub const START: u8 = 0xfa;
    pub const CONTINUE: u8 = 0xfb;
    pub const STOP: u8 = 0xfc;
}

/// Manages MIDI output connections, opened on first use and reused afterwards
/// (like BLT's `opened-outputs`). Every message sent is also kept in a short history
/// for the MIDI monitor and tests.
pub struct MidiOutputs {
    #[cfg(feature = "midi")]
    connections: Mutex<HashMap<String, midir::MidiOutputConnection>>,
    #[cfg(not(feature = "midi"))]
    connections: Mutex<HashMap<String, ()>>,
    history: Mutex<VecDeque<(String, Vec<u8>)>>,
    record_only: std::sync::atomic::AtomicBool,
}

const HISTORY_LIMIT: usize = 512;

impl MidiOutputs {
    pub fn new() -> Self {
        MidiOutputs {
            connections: Mutex::new(HashMap::new()),
            history: Mutex::new(VecDeque::new()),
            record_only: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// When set, messages are recorded but not sent to real devices.
    pub fn set_record_only(&self, record_only: bool) {
        self.record_only.store(record_only, std::sync::atomic::Ordering::SeqCst);
    }

    /// Names of the MIDI outputs available on this system.
    pub fn list_outputs(&self) -> Vec<String> {
        #[cfg(feature = "midi")]
        {
            match midir::MidiOutput::new("Beat Link Trigger") {
                Ok(out) => out.ports().iter().filter_map(|p| out.port_name(p).ok()).collect(),
                Err(e) => {
                    log::debug!("MIDI unavailable: {e}");
                    Vec::new()
                }
            }
        }
        #[cfg(not(feature = "midi"))]
        {
            Vec::new()
        }
    }

    /// Recently sent messages (output name, bytes), oldest first.
    pub fn history(&self) -> Vec<(String, Vec<u8>)> {
        self.history.lock().iter().cloned().collect()
    }

    pub fn clear_history(&self) {
        self.history.lock().clear();
    }

    /// Forget open connections (e.g. after the MIDI environment changed).
    pub fn close_all(&self) {
        self.connections.lock().clear();
    }

    /// Whether an output is currently usable.
    pub fn is_available(&self, name: &str) -> bool {
        self.record_only.load(std::sync::atomic::Ordering::SeqCst)
            || self.connections.lock().contains_key(name)
            || self.list_outputs().iter().any(|n| n == name)
    }

    /// Send raw bytes to a named output.
    pub fn send_raw(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        {
            let mut h = self.history.lock();
            if h.len() >= HISTORY_LIMIT {
                h.pop_front();
            }
            h.push_back((name.to_string(), bytes.to_vec()));
        }
        if self.record_only.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        #[cfg(feature = "midi")]
        {
            let mut conns = self.connections.lock();
            if !conns.contains_key(name) {
                let out = midir::MidiOutput::new("Beat Link Trigger").map_err(|e| e.to_string())?;
                let port = out
                    .ports()
                    .into_iter()
                    .find(|p| out.port_name(p).map(|n| n == name).unwrap_or(false))
                    .ok_or_else(|| format!("MIDI output {name:?} not found"))?;
                let conn = out.connect(&port, "beat-link-trigger").map_err(|e| e.to_string())?;
                conns.insert(name.to_string(), conn);
            }
            let conn = conns.get_mut(name).expect("just inserted");
            if let Err(e) = conn.send(bytes) {
                conns.remove(name);
                return Err(format!("Problem sending to MIDI output {name:?}: {e}"));
            }
            Ok(())
        }
        #[cfg(not(feature = "midi"))]
        {
            let _ = bytes;
            Err(format!("MIDI support not compiled in; cannot send to {name:?}"))
        }
    }

    fn channel_byte(status: u8, channel: u8) -> Result<u8, String> {
        if !(1..=16).contains(&channel) {
            return Err(format!("MIDI channel must be 1-16, got {channel}"));
        }
        Ok(status | (channel - 1))
    }

    /// Note on; `channel` is 1-16 as users see it.
    pub fn note_on(&self, name: &str, note: u8, velocity: u8, channel: u8) -> Result<(), String> {
        self.send_raw(name, &[Self::channel_byte(midi_bytes::NOTE_ON, channel)?, note & 0x7f, velocity & 0x7f])
    }
    pub fn note_off(&self, name: &str, note: u8, channel: u8) -> Result<(), String> {
        self.send_raw(name, &[Self::channel_byte(midi_bytes::NOTE_OFF, channel)?, note & 0x7f, 0])
    }
    pub fn control(&self, name: &str, controller: u8, value: u8, channel: u8) -> Result<(), String> {
        self.send_raw(name, &[Self::channel_byte(midi_bytes::CONTROL, channel)?, controller & 0x7f, value & 0x7f])
    }
    pub fn clock(&self, name: &str) -> Result<(), String> {
        self.send_raw(name, &[midi_bytes::CLOCK])
    }
    pub fn start(&self, name: &str) -> Result<(), String> {
        self.send_raw(name, &[midi_bytes::START])
    }
    pub fn continue_(&self, name: &str) -> Result<(), String> {
        self.send_raw(name, &[midi_bytes::CONTINUE])
    }
    pub fn stop(&self, name: &str) -> Result<(), String> {
        self.send_raw(name, &[midi_bytes::STOP])
    }
}

impl Default for MidiOutputs {
    fn default() -> Self {
        Self::new()
    }
}

/// Sends OSC messages over UDP.
pub struct OscSender {
    socket: Mutex<Option<UdpSocket>>,
    history: Mutex<VecDeque<(String, String)>>,
}

impl OscSender {
    pub fn new() -> Self {
        OscSender { socket: Mutex::new(None), history: Mutex::new(VecDeque::new()) }
    }

    fn to_osc(v: &Dynamic) -> rosc::OscType {
        if let Ok(i) = v.as_int() {
            return rosc::OscType::Int(i as i32);
        }
        if let Ok(f) = v.as_float() {
            return rosc::OscType::Float(f as f32);
        }
        if let Ok(b) = v.as_bool() {
            return rosc::OscType::Bool(b);
        }
        if v.is_unit() {
            return rosc::OscType::Nil;
        }
        if v.is_array() {
            let items = v.clone().cast::<rhai::Array>();
            return rosc::OscType::Array(rosc::OscArray { content: items.iter().map(Self::to_osc).collect() });
        }
        rosc::OscType::String(v.to_string())
    }

    /// Encode an OSC message.
    pub fn encode(address: &str, args: &[Dynamic]) -> Result<Vec<u8>, String> {
        let msg = rosc::OscMessage { addr: address.to_string(), args: args.iter().map(Self::to_osc).collect() };
        rosc::encoder::encode(&rosc::OscPacket::Message(msg)).map_err(|e| e.to_string())
    }

    /// Send an OSC message to `host:port`.
    pub fn send(&self, host: &str, port: u16, address: &str, args: &[Dynamic]) -> Result<(), String> {
        let bytes = Self::encode(address, args)?;
        let mut guard = self.socket.lock();
        if guard.is_none() {
            *guard = Some(UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?);
        }
        guard.as_ref().unwrap().send_to(&bytes, (host, port)).map_err(|e| e.to_string())?;
        let mut h = self.history.lock();
        if h.len() >= HISTORY_LIMIT {
            h.pop_front();
        }
        h.push_back((format!("{host}:{port}"), address.to_string()));
        Ok(())
    }

    pub fn history(&self) -> Vec<(String, String)> {
        self.history.lock().iter().cloned().collect()
    }
}

impl Default for OscSender {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_bytes_and_history() {
        let m = MidiOutputs::new();
        m.set_record_only(true);
        m.note_on("Out", 60, 127, 1).unwrap();
        m.control("Out", 7, 0, 16).unwrap();
        m.clock("Out").unwrap();
        assert!(m.note_on("Out", 60, 127, 0).is_err());
        let h = m.history();
        assert_eq!(h[0].1, vec![0x90, 60, 127]);
        assert_eq!(h[1].1, vec![0xbf, 7, 0]);
        assert_eq!(h[2].1, vec![0xf8]);
    }

    #[test]
    fn osc_encodes_and_sends() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let port = receiver.local_addr().unwrap().port();
        let s = OscSender::new();
        s.send("127.0.0.1", port, "/beat", &[Dynamic::from(3_i64), Dynamic::from(128.5_f64), Dynamic::from("hi")]).unwrap();
        let mut buf = [0u8; 256];
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        let (_, packet) = rosc::decoder::decode_udp(&buf[..n]).unwrap();
        match packet {
            rosc::OscPacket::Message(m) => {
                assert_eq!(m.addr, "/beat");
                assert_eq!(m.args[0], rosc::OscType::Int(3));
                assert_eq!(m.args[2], rosc::OscType::String("hi".into()));
            }
            _ => panic!("expected message"),
        }
    }
}
