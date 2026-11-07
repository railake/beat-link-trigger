//! Parsed representations of the UDP packets exchanged on a Pro DJ Link network.
//!
//! Each type keeps the raw packet bytes (so nothing is lost, and expressions can dig
//! into undocumented fields) along with the sender address and a monotonic timestamp.
//! Byte offsets follow `CdjStatus`, `MixerStatus`, `Beat`, `PrecisePosition` and
//! `DeviceAnnouncement` in the Java beat-link library and the dysentery analysis.

use crate::clock::{current_time_millis, monotonic_nanos};
use crate::util::{self, bytes_to_number, read_name, PacketType, NO_BPM};
use std::fmt;
use std::net::Ipv4Addr;
use std::sync::Arc;

/// Errors that can occur when interpreting a packet.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PacketError {
    #[error("{kind} packet must be at least {needed} bytes, got {got}")]
    TooShort { kind: &'static str, needed: usize, got: usize },
    #[error("{kind} packet must be exactly {needed} bytes, got {got}")]
    WrongLength { kind: &'static str, needed: usize, got: usize },
    #[error("packet does not have a valid Pro DJ Link header")]
    BadHeader,
}

/// Name reported by the Opus Quad, which needs special device-number handling.
pub const OPUS_NAME: &str = "OPUS-QUAD";
/// Name reported by the XDJ-AZ.
pub const XDJ_AZ_NAME: &str = "XDJ-AZ";
/// Name reported by the CDJ-3000X.
pub const CDJ_3000X_NAME: &str = "CDJ-3000X";

/// Offset of the device number in most device update packets.
pub const DEVICE_NUMBER_OFFSET: usize = 0x21;

// ---------------------------------------------------------------------------------
// Device announcements
// ---------------------------------------------------------------------------------

/// A keep-alive packet announcing the presence of a device on the network.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceAnnouncement {
    pub address: Ipv4Addr,
    /// Wall-clock milliseconds when the announcement was received.
    pub timestamp: u64,
    pub name: String,
    pub number: u8,
    pub bytes: Vec<u8>,
}

impl DeviceAnnouncement {
    /// Size of a keep-alive packet.
    pub const LENGTH: usize = 0x36;

    /// Parse a keep-alive packet.
    pub fn parse(data: &[u8], address: Ipv4Addr) -> Result<Self, PacketError> {
        Self::parse_as(data, address, None)
    }

    /// Parse a keep-alive packet, optionally overriding the device number (used to fan
    /// one Opus Quad announcement out into four virtual players).
    pub fn parse_as(data: &[u8], address: Ipv4Addr, number: Option<u8>) -> Result<Self, PacketError> {
        if data.len() < Self::LENGTH {
            return Err(PacketError::TooShort { kind: "Device announcement", needed: Self::LENGTH, got: data.len() });
        }
        let bytes = data[..Self::LENGTH].to_vec();
        Ok(DeviceAnnouncement {
            address,
            timestamp: current_time_millis(),
            name: read_name(&bytes, 0x0c, 20),
            number: number.unwrap_or(bytes[0x24]),
            bytes,
        })
    }

    /// The MAC address reported in the announcement.
    pub fn hardware_address(&self) -> [u8; 6] {
        let mut result = [0u8; 6];
        result.copy_from_slice(&self.bytes[0x26..0x2c]);
        result
    }

    /// The number of peers the device reports seeing.
    pub fn peer_count(&self) -> u8 {
        self.bytes[0x30]
    }

    pub fn is_opus_quad(&self) -> bool {
        self.name == OPUS_NAME
    }
    pub fn is_xdj_az(&self) -> bool {
        self.name == XDJ_AZ_NAME
    }
    pub fn is_cdj_3000x(&self) -> bool {
        self.name == CDJ_3000X_NAME
    }
    /// Devices which use Device Library Plus (SQLite) exports.
    pub fn is_using_device_library_plus(&self) -> bool {
        self.is_opus_quad() || self.is_xdj_az() || self.is_cdj_3000x()
    }

    /// What kind of device this is, based on its number.
    pub fn kind(&self) -> DeviceKind {
        DeviceKind::for_number(self.number)
    }
}

impl fmt::Debug for DeviceAnnouncement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DeviceAnnouncement[device:{}, name:{}, address:{}, peers:{}]",
            self.number,
            self.name,
            self.address,
            self.peer_count()
        )
    }
}

/// Broad classification of devices by their number, as used by the overlay templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceKind {
    Players,
    Collections,
    Mixers,
}

impl DeviceKind {
    pub fn for_number(number: u8) -> DeviceKind {
        if number < 0x10 {
            DeviceKind::Players
        } else if !(0x20..=0x28).contains(&number) {
            DeviceKind::Collections
        } else {
            DeviceKind::Mixers
        }
    }
}

// ---------------------------------------------------------------------------------
// Shared header for device updates
// ---------------------------------------------------------------------------------

/// Fields common to every device update packet.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateHeader {
    pub address: Ipv4Addr,
    /// Monotonic nanoseconds when the packet was received.
    pub timestamp: u64,
    pub device_name: String,
    pub device_number: u8,
    pub bytes: Vec<u8>,
}

impl UpdateHeader {
    fn new(data: &[u8], address: Ipv4Addr, device_number_offset: usize) -> Self {
        let device_name = read_name(data, 0x0b, 20);
        let raw_number = data[device_number_offset];
        let device_number =
            if device_name == OPUS_NAME { util::translate_opus_player_numbers(raw_number) } else { raw_number };
        UpdateHeader { address, timestamp: monotonic_nanos(), device_name, device_number, bytes: data.to_vec() }
    }

    /// Whether this came from an older CDJ-900 or CDJ-2000 (pre-nexus) player.
    pub fn is_pre_nexus_cdj(&self) -> bool {
        self.device_name.starts_with("CDJ") && (self.device_name.ends_with("900") || self.device_name.ends_with("2000"))
    }

    pub fn is_from_opus_quad(&self) -> bool {
        self.device_name == OPUS_NAME
    }
}

// ---------------------------------------------------------------------------------
// CDJ status
// ---------------------------------------------------------------------------------

/// Where a player loaded a track from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TrackSourceSlot {
    NoTrack,
    CdSlot,
    SdSlot,
    UsbSlot,
    Collection,
    Usb2Slot,
    BeatportLinkStreaming,
    BeatportLinkStreamingAz,
    Unknown,
}

impl TrackSourceSlot {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            0 => TrackSourceSlot::NoTrack,
            1 => TrackSourceSlot::CdSlot,
            2 => TrackSourceSlot::SdSlot,
            3 => TrackSourceSlot::UsbSlot,
            4 => TrackSourceSlot::Collection,
            7 => TrackSourceSlot::Usb2Slot,
            9 => TrackSourceSlot::BeatportLinkStreaming,
            14 => TrackSourceSlot::BeatportLinkStreamingAz,
            _ => TrackSourceSlot::Unknown,
        }
    }
    pub fn protocol_value(self) -> u8 {
        match self {
            TrackSourceSlot::NoTrack => 0,
            TrackSourceSlot::CdSlot => 1,
            TrackSourceSlot::SdSlot => 2,
            TrackSourceSlot::UsbSlot => 3,
            TrackSourceSlot::Collection => 4,
            TrackSourceSlot::Usb2Slot => 7,
            TrackSourceSlot::BeatportLinkStreaming => 9,
            TrackSourceSlot::BeatportLinkStreamingAz => 14,
            TrackSourceSlot::Unknown => 0xff,
        }
    }
    /// The Java enum constant name, which expressions and saved files use.
    pub fn java_name(self) -> &'static str {
        match self {
            TrackSourceSlot::NoTrack => "NO_TRACK",
            TrackSourceSlot::CdSlot => "CD_SLOT",
            TrackSourceSlot::SdSlot => "SD_SLOT",
            TrackSourceSlot::UsbSlot => "USB_SLOT",
            TrackSourceSlot::Collection => "COLLECTION",
            TrackSourceSlot::Usb2Slot => "USB_2_SLOT",
            TrackSourceSlot::BeatportLinkStreaming => "BEATPORT_LINK_STREAMING",
            TrackSourceSlot::BeatportLinkStreamingAz => "BEATPORT_LINK_STREAMING_AZ",
            TrackSourceSlot::Unknown => "UNKNOWN",
        }
    }
}

/// The kind of track a player has loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TrackType {
    NoTrack,
    Rekordbox,
    Unanalyzed,
    CdDigitalAudio,
    Streaming,
    Unknown,
}

impl TrackType {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            0 => TrackType::NoTrack,
            1 => TrackType::Rekordbox,
            2 => TrackType::Unanalyzed,
            5 => TrackType::CdDigitalAudio,
            6 => TrackType::Streaming,
            _ => TrackType::Unknown,
        }
    }
    pub fn protocol_value(self) -> u8 {
        match self {
            TrackType::NoTrack => 0,
            TrackType::Rekordbox => 1,
            TrackType::Unanalyzed => 2,
            TrackType::CdDigitalAudio => 5,
            TrackType::Streaming => 6,
            TrackType::Unknown => 0xff,
        }
    }
    pub fn java_name(self) -> &'static str {
        match self {
            TrackType::NoTrack => "NO_TRACK",
            TrackType::Rekordbox => "REKORDBOX",
            TrackType::Unanalyzed => "UNANALYZED",
            TrackType::CdDigitalAudio => "CD_DIGITAL_AUDIO",
            TrackType::Streaming => "STREAMING",
            TrackType::Unknown => "UNKNOWN",
        }
    }
}

/// The primary play state reported at offset 0x7b.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlayState1 {
    NoTrack,
    Loading,
    Playing,
    Looping,
    Paused,
    Cued,
    CuePlaying,
    CueScratching,
    Searching,
    Ended,
    EmergencyLoop,
    Unknown,
}

impl PlayState1 {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            0x00 => PlayState1::NoTrack,
            0x02 => PlayState1::Loading,
            0x03 => PlayState1::Playing,
            0x04 => PlayState1::Looping,
            0x05 => PlayState1::Paused,
            0x06 => PlayState1::Cued,
            0x07 => PlayState1::CuePlaying,
            0x08 => PlayState1::CueScratching,
            0x09 => PlayState1::Searching,
            0x11 => PlayState1::Ended,
            0x12 => PlayState1::EmergencyLoop,
            _ => PlayState1::Unknown,
        }
    }
    pub fn protocol_value(self) -> u8 {
        match self {
            PlayState1::NoTrack => 0x00,
            PlayState1::Loading => 0x02,
            PlayState1::Playing => 0x03,
            PlayState1::Looping => 0x04,
            PlayState1::Paused => 0x05,
            PlayState1::Cued => 0x06,
            PlayState1::CuePlaying => 0x07,
            PlayState1::CueScratching => 0x08,
            PlayState1::Searching => 0x09,
            PlayState1::Ended => 0x11,
            PlayState1::EmergencyLoop => 0x12,
            PlayState1::Unknown => 0xff,
        }
    }
}

/// Whether the platter/playhead is moving (offset 0x8b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlayState2 {
    Moving,
    Stopped,
    Unknown,
}

impl PlayState2 {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            0x6a | 0x7a | 0xfa => PlayState2::Moving,
            0x6e | 0x7e | 0xfe => PlayState2::Stopped,
            _ => PlayState2::Unknown,
        }
    }
}

/// Direction and jog mode (offset 0x9d).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlayState3 {
    NoTrack,
    PausedOrReverse,
    ForwardVinyl,
    Slipping,
    ForwardCdj,
    Unknown,
}

impl PlayState3 {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            0x00 => PlayState3::NoTrack,
            0x01 => PlayState3::PausedOrReverse,
            0x09 => PlayState3::ForwardVinyl,
            0x0b => PlayState3::Slipping,
            0x0d => PlayState3::ForwardCdj,
            _ => PlayState3::Unknown,
        }
    }
    pub fn protocol_value(self) -> u8 {
        match self {
            PlayState3::NoTrack => 0x00,
            PlayState3::PausedOrReverse => 0x01,
            PlayState3::ForwardVinyl => 0x09,
            PlayState3::Slipping => 0x0b,
            PlayState3::ForwardCdj => 0x0d,
            PlayState3::Unknown => 0xff,
        }
    }
}

/// Status flag bits found at offset 0x89 of CDJ status packets (and 0x27 of mixer status).
pub mod flags {
    pub const BPM_SYNC: u8 = 0x02;
    pub const ON_AIR: u8 = 0x08;
    pub const SYNCED: u8 = 0x10;
    pub const MASTER: u8 = 0x20;
    pub const PLAYING: u8 = 0x40;
}

/// A status update from a CDJ (or XDJ, or other player).
#[derive(Debug, Clone, PartialEq)]
pub struct CdjStatus {
    pub header: UpdateHeader,
    pub track_source_player: u8,
    pub track_source_slot: TrackSourceSlot,
    pub track_type: TrackType,
    pub rekordbox_id: u32,
    pub pitch: i64,
    pub bpm: u32,
    pub play_state_1: PlayState1,
    pub play_state_2: PlayState2,
    pub play_state_3: PlayState3,
    pub firmware_version: String,
    pub handing_master_to_device: u8,
}

impl CdjStatus {
    pub const STATUS_FLAGS: usize = 0x89;
    pub const MASTER_HAND_OFF: usize = 0x9f;
    pub const LOCAL_CD_STATE: usize = 0x37;
    pub const LOCAL_USB_STATE: usize = 0x6f;
    pub const LOCAL_SD_STATE: usize = 0x73;
    pub const MINIMUM_PACKET_SIZE: usize = 0xcc;

    /// Parse a CDJ status packet received from `address`.
    pub fn parse(data: &[u8], address: Ipv4Addr) -> Result<Self, PacketError> {
        if data.len() < Self::MINIMUM_PACKET_SIZE {
            return Err(PacketError::TooShort { kind: "CDJ status", needed: Self::MINIMUM_PACKET_SIZE, got: data.len() });
        }
        let reported = bytes_to_number(data, 0x22, 2) as usize + 0x24;
        if reported != data.len() {
            log::trace!("CDJ status reported length {} but was {}", reported, data.len());
        }
        let mut header = UpdateHeader::new(data, address, DEVICE_NUMBER_OFFSET);
        let source_byte = data[0x28];
        let (track_source_player, track_source_slot) = if header.is_from_opus_quad() && source_byte < 16 {
            // The Opus Quad always reports USB, and we pretend the archive is loaded.
            header.bytes[Self::LOCAL_USB_STATE] = 0;
            (util::translate_opus_player_numbers(source_byte), TrackSourceSlot::UsbSlot)
        } else {
            (source_byte, TrackSourceSlot::from_protocol(data[0x29]))
        };
        Ok(CdjStatus {
            track_source_player,
            track_source_slot,
            track_type: TrackType::from_protocol(data[0x2a]),
            rekordbox_id: bytes_to_number(data, 0x2c, 4) as u32,
            pitch: bytes_to_number(data, 0x8d, 3) as i64,
            bpm: bytes_to_number(data, 0x92, 2) as u32,
            play_state_1: PlayState1::from_protocol(data[0x7b]),
            play_state_2: PlayState2::from_protocol(data[0x8b]),
            play_state_3: PlayState3::from_protocol(data[0x9d]),
            firmware_version: read_name(data, 0x7c, 4),
            handing_master_to_device: data[Self::MASTER_HAND_OFF],
            header,
        })
    }

    fn bytes(&self) -> &[u8] {
        &self.header.bytes
    }
    fn flag(&self, mask: u8) -> bool {
        self.bytes()[Self::STATUS_FLAGS] & mask != 0
    }

    pub fn device_number(&self) -> u8 {
        self.header.device_number
    }
    pub fn device_name(&self) -> &str {
        &self.header.device_name
    }
    pub fn address(&self) -> Ipv4Addr {
        self.header.address
    }
    pub fn timestamp(&self) -> u64 {
        self.header.timestamp
    }

    /// One of the four pitch values reported in the packet (1 is the effective pitch).
    pub fn pitch_number(&self, number: u8) -> Option<i64> {
        let b = self.bytes();
        match number {
            1 => Some(self.pitch),
            2 => Some(bytes_to_number(b, 153, 3) as i64),
            3 => Some(bytes_to_number(b, 193, 3) as i64),
            4 => Some(bytes_to_number(b, 197, 3) as i64),
            _ => None,
        }
    }
    pub fn beat_within_bar(&self) -> u8 {
        self.bytes()[0xa6]
    }
    pub fn is_beat_within_bar_meaningful(&self) -> bool {
        self.beat_within_bar() > 0
    }
    pub fn is_tempo_master(&self) -> bool {
        self.flag(flags::MASTER)
    }
    pub fn device_master_is_being_yielded_to(&self) -> Option<u8> {
        if self.handing_master_to_device == 0xff {
            None
        } else {
            Some(self.handing_master_to_device)
        }
    }
    pub fn effective_tempo(&self) -> f64 {
        f64::from(self.bpm) * util::pitch_to_multiplier(self.pitch) / 100.0
    }
    pub fn is_playing(&self) -> bool {
        if self.bytes().len() >= 0xd4 {
            self.flag(flags::PLAYING)
        } else {
            matches!(self.play_state_1, PlayState1::Playing | PlayState1::Looping)
                || (self.play_state_1 == PlayState1::Searching && self.play_state_2 == PlayState2::Moving)
        }
    }
    pub fn is_bpm_only_synced(&self) -> bool {
        self.flag(flags::BPM_SYNC)
    }
    pub fn is_synced(&self) -> bool {
        self.flag(flags::SYNCED)
    }
    pub fn is_on_air(&self) -> bool {
        self.flag(flags::ON_AIR)
    }
    pub fn is_local_usb_loaded(&self) -> bool {
        self.bytes()[Self::LOCAL_USB_STATE] == 0
    }
    pub fn is_local_usb_unloading(&self) -> bool {
        self.bytes()[Self::LOCAL_USB_STATE] == 2
    }
    pub fn is_local_usb_empty(&self) -> bool {
        self.bytes()[Self::LOCAL_USB_STATE] == 4
    }
    pub fn is_local_sd_loaded(&self) -> bool {
        self.bytes()[Self::LOCAL_SD_STATE] == 0
    }
    pub fn is_local_sd_unloading(&self) -> bool {
        self.bytes()[Self::LOCAL_SD_STATE] == 2
    }
    pub fn is_local_sd_empty(&self) -> bool {
        self.bytes()[Self::LOCAL_SD_STATE] == 4
    }
    pub fn is_disc_slot_empty(&self) -> bool {
        let v = self.bytes()[Self::LOCAL_CD_STATE];
        v != 0x1e && v != 0x11
    }
    pub fn is_disc_slot_asleep(&self) -> bool {
        self.bytes()[Self::LOCAL_CD_STATE] == 1
    }
    pub fn disc_track_count(&self) -> u32 {
        bytes_to_number(self.bytes(), 0x46, 2) as u32
    }
    pub fn is_track_loaded(&self) -> bool {
        self.play_state_1 != PlayState1::NoTrack
    }
    pub fn is_looping(&self) -> bool {
        self.play_state_1 == PlayState1::Looping
    }
    pub fn is_paused(&self) -> bool {
        matches!(self.play_state_1, PlayState1::Paused | PlayState1::Cued)
    }
    pub fn is_cued(&self) -> bool {
        self.play_state_1 == PlayState1::Cued
    }
    pub fn is_searching(&self) -> bool {
        self.play_state_1 == PlayState1::Searching
    }
    pub fn is_at_end(&self) -> bool {
        self.play_state_1 == PlayState1::Ended
    }
    pub fn is_playing_forwards(&self) -> bool {
        self.play_state_1 == PlayState1::Playing && self.play_state_3 != PlayState3::PausedOrReverse
    }
    pub fn is_playing_backwards(&self) -> bool {
        self.play_state_1 == PlayState1::Playing && self.play_state_3 == PlayState3::PausedOrReverse
    }
    pub fn is_playing_vinyl_mode(&self) -> bool {
        self.play_state_3 == PlayState3::ForwardVinyl
    }
    pub fn is_playing_cdj_mode(&self) -> bool {
        self.play_state_3 == PlayState3::ForwardCdj
    }
    pub fn is_link_media_available(&self) -> bool {
        self.bytes()[0x75] != 0
    }
    pub fn is_busy(&self) -> bool {
        self.bytes()[0x27] != 0
    }
    pub fn track_number(&self) -> u32 {
        bytes_to_number(self.bytes(), 0x32, 2) as u32
    }
    pub fn sync_number(&self) -> u32 {
        bytes_to_number(self.bytes(), 0x84, 4) as u32
    }
    /// The beat number within the track, or `None` if unknown (e.g. no beat grid).
    pub fn beat_number(&self) -> Option<u32> {
        let raw = bytes_to_number(self.bytes(), 0xa0, 4);
        if raw == 0xffff_ffff {
            None
        } else {
            Some(raw as u32)
        }
    }
    /// Beat number as the Java API reports it (-1 when unknown).
    pub fn beat_number_or_negative(&self) -> i64 {
        self.beat_number().map(i64::from).unwrap_or(-1)
    }
    pub fn cue_countdown(&self) -> u32 {
        bytes_to_number(self.bytes(), 0xa4, 2) as u32
    }
    /// Format the cue countdown the way players display it.
    pub fn format_cue_countdown(&self) -> String {
        let count = self.cue_countdown();
        if count == 511 {
            return "--.-".into();
        }
        if (1..=256).contains(&count) {
            let bars = (count - 1) / 4;
            let beats = ((count - 1) % 4) + 1;
            return format!("{bars:02}.{beats}");
        }
        if count == 0 {
            return "00.0".into();
        }
        "??.?".into()
    }
    pub fn packet_number(&self) -> u32 {
        bytes_to_number(self.bytes(), 0xc8, 4) as u32
    }
    pub fn can_report_looping(&self) -> bool {
        self.bytes().len() >= 0x1ca
    }
    pub fn active_loop_beats(&self) -> u32 {
        if self.can_report_looping() {
            bytes_to_number(self.bytes(), 0x1c8, 2) as u32
        } else {
            0
        }
    }
    pub fn loop_start(&self) -> u64 {
        if self.can_report_looping() {
            bytes_to_number(self.bytes(), 0x1b6, 4) * 65536 / 1000
        } else {
            0
        }
    }
    pub fn loop_end(&self) -> u64 {
        if self.can_report_looping() {
            bytes_to_number(self.bytes(), 0x1be, 4) * 65536 / 1000
        } else {
            0
        }
    }
    /// Whether the track BPM is meaningful (a track with a beat grid is loaded).
    pub fn has_bpm(&self) -> bool {
        self.bpm != NO_BPM
    }
}

impl fmt::Display for CdjStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CdjStatus[device:{}, name:{}, address:{}, pitch:{:+.2}%, rekordboxId:{}, from player:{}, in slot:{:?}, \
             track type:{:?}, track:{}, track BPM:{:.1}, effective BPM:{:.1}, beat:{}, beatWithinBar:{}, cue:{}, \
             Playing? {}, Master? {}, Synced? {}, On-Air? {}]",
            self.device_number(),
            self.device_name(),
            self.address(),
            util::pitch_to_percentage(self.pitch),
            self.rekordbox_id,
            self.track_source_player,
            self.track_source_slot,
            self.track_type,
            self.track_number(),
            f64::from(self.bpm) / 100.0,
            self.effective_tempo(),
            self.beat_number_or_negative(),
            self.beat_within_bar(),
            self.format_cue_countdown(),
            self.is_playing(),
            self.is_tempo_master(),
            self.is_synced(),
            self.is_on_air()
        )
    }
}

/// Builds synthetic CDJ status packets. Used by the playback simulator, by tests,
/// and to describe what a real player would send.
#[derive(Debug, Clone)]
pub struct CdjStatusBuilder {
    pub device_number: u8,
    pub device_name: String,
    pub address: Ipv4Addr,
    pub track_source_player: u8,
    pub track_source_slot: TrackSourceSlot,
    pub track_type: TrackType,
    pub rekordbox_id: u32,
    pub track_number: u32,
    pub pitch: i64,
    pub bpm: u32,
    pub beat_number: Option<u32>,
    pub beat_within_bar: u8,
    pub playing: bool,
    pub master: bool,
    pub synced: bool,
    pub on_air: bool,
    pub cued: bool,
    pub reverse: bool,
    pub cue_countdown: u32,
    pub handing_master_to: u8,
    pub sync_number: u32,
    pub packet_number: u32,
}

impl Default for CdjStatusBuilder {
    fn default() -> Self {
        CdjStatusBuilder {
            device_number: 1,
            device_name: "CDJ-3000".into(),
            address: Ipv4Addr::LOCALHOST,
            track_source_player: 0,
            track_source_slot: TrackSourceSlot::NoTrack,
            track_type: TrackType::NoTrack,
            rekordbox_id: 0,
            track_number: 0,
            pitch: util::NEUTRAL_PITCH,
            bpm: NO_BPM,
            beat_number: None,
            beat_within_bar: 0,
            playing: false,
            master: false,
            synced: false,
            on_air: false,
            cued: false,
            reverse: false,
            cue_countdown: 511,
            handing_master_to: 0xff,
            sync_number: 0,
            packet_number: 0,
        }
    }
}

impl CdjStatusBuilder {
    /// Length of the packets we build (the CDJ-3000 status length).
    pub const LENGTH: usize = 0x200;

    /// Build the raw packet bytes.
    pub fn build_bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; Self::LENGTH];
        b[..10].copy_from_slice(&util::MAGIC_HEADER);
        b[0x0a] = PacketType::CdjStatus.protocol_value();
        let name = self.device_name.as_bytes();
        let n = name.len().min(20);
        b[0x0b..0x0b + n].copy_from_slice(&name[..n]);
        b[0x1f] = 0x01;
        b[0x20] = 0x06;
        b[0x21] = self.device_number;
        util::number_to_bytes((Self::LENGTH - 0x24) as u64, &mut b, 0x22, 2);
        b[0x24] = self.device_number;
        let loaded = self.track_type != TrackType::NoTrack;
        b[0x28] = self.track_source_player;
        b[0x29] = self.track_source_slot.protocol_value();
        b[0x2a] = self.track_type.protocol_value();
        util::number_to_bytes(u64::from(self.rekordbox_id), &mut b, 0x2c, 4);
        util::number_to_bytes(u64::from(self.track_number), &mut b, 0x32, 2);
        b[CdjStatus::LOCAL_CD_STATE] = 0;
        b[CdjStatus::LOCAL_USB_STATE] = if loaded { 0 } else { 4 };
        b[CdjStatus::LOCAL_SD_STATE] = 4;
        let state1 = if !loaded {
            PlayState1::NoTrack
        } else if self.playing {
            PlayState1::Playing
        } else if self.cued {
            PlayState1::Cued
        } else {
            PlayState1::Paused
        };
        b[0x7b] = state1.protocol_value();
        b[0x7c..0x80].copy_from_slice(b"1.85");
        util::number_to_bytes(u64::from(self.sync_number), &mut b, 0x84, 4);
        b[CdjStatus::STATUS_FLAGS] = 0x84
            | if self.playing { flags::PLAYING } else { 0 }
            | if self.master { flags::MASTER } else { 0 }
            | if self.synced { flags::SYNCED } else { 0 }
            | if self.on_air { flags::ON_AIR } else { 0 };
        b[0x8b] = if self.playing { 0xfa } else { 0xfe };
        util::number_to_bytes(self.pitch.max(0) as u64, &mut b, 0x8c, 4);
        util::number_to_bytes(self.bpm as u64, &mut b, 0x92, 2);
        util::number_to_bytes(self.pitch.max(0) as u64, &mut b, 0x98, 4);
        b[0x9d] = if !loaded {
            PlayState3::NoTrack
        } else if self.playing && self.reverse {
            PlayState3::PausedOrReverse
        } else if self.playing {
            PlayState3::ForwardCdj
        } else {
            PlayState3::PausedOrReverse
        }
        .protocol_value();
        b[0x9e] = if self.master { 1 } else { 0 };
        b[CdjStatus::MASTER_HAND_OFF] = self.handing_master_to;
        util::number_to_bytes(u64::from(self.beat_number.unwrap_or(u32::MAX)), &mut b, 0xa0, 4);
        util::number_to_bytes(u64::from(self.cue_countdown), &mut b, 0xa4, 2);
        b[0xa6] = self.beat_within_bar;
        util::number_to_bytes(self.pitch.max(0) as u64, &mut b, 0xc0, 4);
        util::number_to_bytes(self.pitch.max(0) as u64, &mut b, 0xc4, 4);
        util::number_to_bytes(u64::from(self.packet_number), &mut b, 0xc8, 4);
        b
    }

    /// Build and parse a status object.
    pub fn build(&self) -> CdjStatus {
        CdjStatus::parse(&self.build_bytes(), self.address).expect("builder produces valid packets")
    }
}

// ---------------------------------------------------------------------------------
// Mixer status
// ---------------------------------------------------------------------------------

/// A status update from a DJM mixer.
#[derive(Debug, Clone, PartialEq)]
pub struct MixerStatus {
    pub header: UpdateHeader,
    pub pitch: i64,
    pub bpm: u32,
    pub handing_master_to_device: u8,
}

impl MixerStatus {
    pub const STATUS_FLAGS: usize = 0x27;
    pub const MASTER_HAND_OFF: usize = 0x36;
    pub const MINIMUM_PACKET_SIZE: usize = 0x38;

    pub fn parse(data: &[u8], address: Ipv4Addr) -> Result<Self, PacketError> {
        if data.len() < Self::MINIMUM_PACKET_SIZE {
            return Err(PacketError::TooShort { kind: "Mixer status", needed: Self::MINIMUM_PACKET_SIZE, got: data.len() });
        }
        Ok(MixerStatus {
            header: UpdateHeader::new(data, address, DEVICE_NUMBER_OFFSET),
            pitch: bytes_to_number(data, 0x28, 4) as i64,
            bpm: bytes_to_number(data, 0x2e, 2) as u32,
            handing_master_to_device: data[Self::MASTER_HAND_OFF],
        })
    }
    pub fn device_number(&self) -> u8 {
        self.header.device_number
    }
    pub fn beat_within_bar(&self) -> u8 {
        self.header.bytes[55]
    }
    pub fn is_tempo_master(&self) -> bool {
        self.header.bytes[Self::STATUS_FLAGS] & flags::MASTER != 0
    }
    pub fn is_synced(&self) -> bool {
        self.header.bytes[Self::STATUS_FLAGS] & flags::SYNCED != 0
    }
    pub fn device_master_is_being_yielded_to(&self) -> Option<u8> {
        if self.handing_master_to_device == 0xff {
            None
        } else {
            Some(self.handing_master_to_device)
        }
    }
    pub fn effective_tempo(&self) -> f64 {
        f64::from(self.bpm) * util::pitch_to_multiplier(self.pitch) / 100.0
    }
}

// ---------------------------------------------------------------------------------
// Beats and precise positions
// ---------------------------------------------------------------------------------

/// A beat announcement broadcast by a player (or mixer) on port 50001.
#[derive(Debug, Clone, PartialEq)]
pub struct Beat {
    pub header: UpdateHeader,
    pub pitch: i64,
    pub bpm: u32,
    /// Filled in by the network layer: whether the sender is the tempo master.
    pub tempo_master: bool,
    /// Filled in by the network layer from the sender's latest status.
    pub synced: bool,
    /// Filled in by the network layer from the sender's latest status.
    pub bar_meaningful: bool,
}

impl Beat {
    pub const LENGTH: usize = 0x60;

    pub fn parse(data: &[u8], address: Ipv4Addr) -> Result<Self, PacketError> {
        if data.len() < Self::LENGTH {
            return Err(PacketError::TooShort { kind: "Beat", needed: Self::LENGTH, got: data.len() });
        }
        let header = UpdateHeader::new(&data[..Self::LENGTH], address, DEVICE_NUMBER_OFFSET);
        let number = header.device_number;
        Ok(Beat {
            pitch: bytes_to_number(data, 0x55, 3) as i64,
            bpm: bytes_to_number(data, 0x5a, 2) as u32,
            tempo_master: false,
            synced: false,
            bar_meaningful: number < 33,
            header,
        })
    }
    pub fn device_number(&self) -> u8 {
        self.header.device_number
    }
    pub fn device_name(&self) -> &str {
        &self.header.device_name
    }
    pub fn timestamp(&self) -> u64 {
        self.header.timestamp
    }
    pub fn beat_within_bar(&self) -> u8 {
        self.header.bytes[0x5c]
    }
    pub fn next_beat(&self) -> u32 {
        bytes_to_number(&self.header.bytes, 0x24, 4) as u32
    }
    pub fn second_beat(&self) -> u32 {
        bytes_to_number(&self.header.bytes, 0x28, 4) as u32
    }
    pub fn next_bar(&self) -> u32 {
        bytes_to_number(&self.header.bytes, 0x2c, 4) as u32
    }
    pub fn fourth_beat(&self) -> u32 {
        bytes_to_number(&self.header.bytes, 0x30, 4) as u32
    }
    pub fn second_bar(&self) -> u32 {
        bytes_to_number(&self.header.bytes, 0x34, 4) as u32
    }
    pub fn eighth_beat(&self) -> u32 {
        bytes_to_number(&self.header.bytes, 0x38, 4) as u32
    }
    pub fn effective_tempo(&self) -> f64 {
        f64::from(self.bpm) * util::pitch_to_multiplier(self.pitch) / 100.0
    }

    /// Build a beat packet as a player would send it.
    pub fn build_bytes(device_number: u8, device_name: &str, tempo: f64, beat_within_bar: u8, pitch: i64) -> Vec<u8> {
        let beat_ms = (60_000.0 / tempo.max(1.0)) as u64;
        let mut payload = BEAT_PAYLOAD;
        payload[0x02] = device_number;
        util::number_to_bytes(beat_ms, &mut payload, 0x05, 4);
        util::number_to_bytes(beat_ms * 2, &mut payload, 0x09, 4);
        util::number_to_bytes(beat_ms * 4, &mut payload, 0x11, 4);
        util::number_to_bytes(beat_ms * 8, &mut payload, 0x19, 4);
        let beats_left = 5u64.saturating_sub(u64::from(beat_within_bar));
        let next_bar = beat_ms * beats_left;
        util::number_to_bytes(next_bar, &mut payload, 0x0d, 4);
        util::number_to_bytes(next_bar + beat_ms * 4, &mut payload, 0x15, 4);
        util::number_to_bytes(pitch.max(0) as u64, &mut payload, 0x35, 4);
        let bpm = (tempo / util::pitch_to_multiplier(pitch) * 100.0).round() as u64;
        util::number_to_bytes(bpm, &mut payload, 0x3b, 2);
        payload[0x3d] = beat_within_bar;
        payload[0x40] = device_number;
        util::build_packet(PacketType::Beat, device_name.as_bytes(), &payload)
    }
}

/// The template for beat payloads (bytes after the device name).
pub const BEAT_PAYLOAD: [u8; 0x41] = {
    let mut p = [0xffu8; 0x41];
    let head = [0x01, 0x00, 0x0d, 0x00, 0x3c];
    let mut i = 0;
    while i < 5 {
        p[i] = head[i];
        i += 1;
    }
    let groups = [0x01, 0x02, 0x10, 0x04, 0x20, 0x08];
    let mut g = 0;
    while g < 6 {
        let mut j = 0;
        while j < 4 {
            p[5 + g * 4 + j] = groups[g];
            j += 1;
        }
        g += 1;
    }
    let tail = [0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x0d];
    let mut t = 0;
    while t < 12 {
        p[0x41 - 12 + t] = tail[t];
        t += 1;
    }
    p
};

/// A precise position packet sent by CDJ-3000s about every 30 milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct PrecisePosition {
    pub header: UpdateHeader,
    pub track_length: u32,
    pub playback_position: u32,
    pub pitch: i64,
    /// Effective BPM times 100.
    pub effective_bpm: u32,
}

impl PrecisePosition {
    pub const LENGTH: usize = 0x3c;

    pub fn parse(data: &[u8], address: Ipv4Addr) -> Result<Self, PacketError> {
        if data.len() < Self::LENGTH {
            return Err(PacketError::TooShort { kind: "Precise position", needed: Self::LENGTH, got: data.len() });
        }
        let mut raw_pitch = bytes_to_number(data, 0x2c, 4) as i64;
        if raw_pitch > 0x8000_0000 {
            raw_pitch -= 0x1_0000_0000;
        }
        Ok(PrecisePosition {
            header: UpdateHeader::new(&data[..Self::LENGTH], address, DEVICE_NUMBER_OFFSET),
            track_length: bytes_to_number(data, 0x24, 4) as u32,
            playback_position: bytes_to_number(data, 0x28, 4) as u32,
            pitch: util::percentage_to_pitch(raw_pitch as f64 / 100.0),
            effective_bpm: (bytes_to_number(data, 0x38, 4) * 10) as u32,
        })
    }
    pub fn device_number(&self) -> u8 {
        self.header.device_number
    }
    /// The track BPM (times 100), backed out from the effective tempo and pitch.
    pub fn bpm(&self) -> u32 {
        (f64::from(self.effective_bpm) / util::pitch_to_multiplier(self.pitch)).round() as u32
    }
    pub fn effective_tempo(&self) -> f64 {
        f64::from(self.effective_bpm) / 100.0
    }
}

// ---------------------------------------------------------------------------------
// The union of update types
// ---------------------------------------------------------------------------------

/// Any of the update packets that describe a device's state.
#[derive(Debug, Clone)]
pub enum DeviceUpdate {
    Cdj(Arc<CdjStatus>),
    Mixer(Arc<MixerStatus>),
    Beat(Arc<Beat>),
    Position(Arc<PrecisePosition>),
}

impl DeviceUpdate {
    pub fn header(&self) -> &UpdateHeader {
        match self {
            DeviceUpdate::Cdj(s) => &s.header,
            DeviceUpdate::Mixer(s) => &s.header,
            DeviceUpdate::Beat(s) => &s.header,
            DeviceUpdate::Position(s) => &s.header,
        }
    }
    pub fn device_number(&self) -> u8 {
        self.header().device_number
    }
    pub fn device_name(&self) -> &str {
        &self.header().device_name
    }
    pub fn address(&self) -> Ipv4Addr {
        self.header().address
    }
    pub fn timestamp(&self) -> u64 {
        self.header().timestamp
    }
    pub fn pitch(&self) -> i64 {
        match self {
            DeviceUpdate::Cdj(s) => s.pitch,
            DeviceUpdate::Mixer(s) => s.pitch,
            DeviceUpdate::Beat(s) => s.pitch,
            DeviceUpdate::Position(s) => s.pitch,
        }
    }
    pub fn bpm(&self) -> u32 {
        match self {
            DeviceUpdate::Cdj(s) => s.bpm,
            DeviceUpdate::Mixer(s) => s.bpm,
            DeviceUpdate::Beat(s) => s.bpm,
            DeviceUpdate::Position(s) => s.bpm(),
        }
    }
    pub fn effective_tempo(&self) -> f64 {
        match self {
            DeviceUpdate::Cdj(s) => s.effective_tempo(),
            DeviceUpdate::Mixer(s) => s.effective_tempo(),
            DeviceUpdate::Beat(s) => s.effective_tempo(),
            DeviceUpdate::Position(s) => s.effective_tempo(),
        }
    }
    pub fn is_tempo_master(&self) -> bool {
        match self {
            DeviceUpdate::Cdj(s) => s.is_tempo_master(),
            DeviceUpdate::Mixer(s) => s.is_tempo_master(),
            DeviceUpdate::Beat(s) => s.tempo_master,
            DeviceUpdate::Position(_) => false,
        }
    }
    pub fn is_synced(&self) -> bool {
        match self {
            DeviceUpdate::Cdj(s) => s.is_synced(),
            DeviceUpdate::Mixer(s) => s.is_synced(),
            DeviceUpdate::Beat(s) => s.synced,
            DeviceUpdate::Position(_) => false,
        }
    }
    pub fn device_master_is_being_yielded_to(&self) -> Option<u8> {
        match self {
            DeviceUpdate::Cdj(s) => s.device_master_is_being_yielded_to(),
            DeviceUpdate::Mixer(s) => s.device_master_is_being_yielded_to(),
            _ => None,
        }
    }
    pub fn beat_within_bar(&self) -> u8 {
        match self {
            DeviceUpdate::Cdj(s) => s.beat_within_bar(),
            DeviceUpdate::Mixer(s) => s.beat_within_bar(),
            DeviceUpdate::Beat(s) => s.beat_within_bar(),
            DeviceUpdate::Position(_) => 0,
        }
    }
    pub fn is_beat_within_bar_meaningful(&self) -> bool {
        match self {
            DeviceUpdate::Cdj(s) => s.is_beat_within_bar_meaningful(),
            DeviceUpdate::Mixer(_) => false,
            DeviceUpdate::Beat(s) => s.bar_meaningful,
            DeviceUpdate::Position(_) => false,
        }
    }
    pub fn as_cdj(&self) -> Option<&Arc<CdjStatus>> {
        match self {
            DeviceUpdate::Cdj(s) => Some(s),
            _ => None,
        }
    }
}

/// Identifies a device uniquely by number and address (so a device that changes
/// address is treated as new, as in beat-link's `DeviceReference`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceReference {
    pub number: u8,
    pub address: Ipv4Addr,
}

impl DeviceReference {
    pub fn for_announcement(a: &DeviceAnnouncement) -> Self {
        DeviceReference { number: a.number, address: a.address }
    }
    pub fn for_update(u: &DeviceUpdate) -> Self {
        DeviceReference { number: u.device_number(), address: u.address() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_builder_round_trips() {
        let status = CdjStatusBuilder {
            device_number: 3,
            track_source_player: 2,
            track_source_slot: TrackSourceSlot::UsbSlot,
            track_type: TrackType::Rekordbox,
            rekordbox_id: 4242,
            bpm: 12800,
            pitch: util::percentage_to_pitch(5.0),
            beat_number: Some(17),
            beat_within_bar: 1,
            playing: true,
            master: true,
            on_air: true,
            ..Default::default()
        }
        .build();
        assert_eq!(status.device_number(), 3);
        assert_eq!(status.track_source_player, 2);
        assert_eq!(status.track_source_slot, TrackSourceSlot::UsbSlot);
        assert_eq!(status.track_type, TrackType::Rekordbox);
        assert_eq!(status.rekordbox_id, 4242);
        assert!(status.is_playing());
        assert!(status.is_tempo_master());
        assert!(status.is_on_air());
        assert!(!status.is_synced());
        assert_eq!(status.beat_number(), Some(17));
        assert!((status.effective_tempo() - 134.4).abs() < 0.01);
        assert!(status.is_playing_forwards());
        assert_eq!(status.device_master_is_being_yielded_to(), None);
    }

    #[test]
    fn beat_round_trip() {
        let bytes = Beat::build_bytes(2, "CDJ-2000NXS2", 128.0, 3, util::NEUTRAL_PITCH);
        assert_eq!(bytes.len(), Beat::LENGTH);
        assert_eq!(util::validate_header(&bytes, util::BEAT_PORT), Some(PacketType::Beat));
        let beat = Beat::parse(&bytes, Ipv4Addr::LOCALHOST).unwrap();
        assert_eq!(beat.device_number(), 2);
        assert_eq!(beat.beat_within_bar(), 3);
        assert_eq!(beat.bpm, 12800);
        assert_eq!(beat.device_name(), "CDJ-2000NXS2");
        assert_eq!(beat.next_beat(), 468);
    }

    #[test]
    fn cue_countdown_format() {
        let mut b = CdjStatusBuilder { cue_countdown: 5, ..Default::default() };
        assert_eq!(b.build().format_cue_countdown(), "01.1");
        b.cue_countdown = 511;
        assert_eq!(b.build().format_cue_countdown(), "--.-");
        b.cue_countdown = 0;
        assert_eq!(b.build().format_cue_countdown(), "00.0");
    }

    #[test]
    fn device_kinds() {
        assert_eq!(DeviceKind::for_number(3), DeviceKind::Players);
        assert_eq!(DeviceKind::for_number(0x11), DeviceKind::Collections);
        assert_eq!(DeviceKind::for_number(0x21), DeviceKind::Mixers);
    }
}
