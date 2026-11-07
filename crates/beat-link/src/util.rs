//! Low-level helpers shared by the whole protocol implementation: the packet header,
//! packet type identification, number encoding, and pitch/time conversions.
//!
//! Ported from `org.deepsymmetry.beatlink.Util`.

use std::net::Ipv4Addr;

/// The ten bytes that start every Pro DJ Link UDP packet ("Qspt1WmJOL").
pub const MAGIC_HEADER: [u8; 10] = [0x51, 0x73, 0x70, 0x74, 0x31, 0x57, 0x6d, 0x4a, 0x4f, 0x4c];

/// Where the packet type byte lives in every Pro DJ Link packet.
pub const PACKET_TYPE_OFFSET: usize = 0x0a;

/// The port on which devices announce their presence (and claim device numbers).
pub const ANNOUNCEMENT_PORT: u16 = 50000;
/// The port on which beats, sync commands and master handoffs are broadcast.
pub const BEAT_PORT: u16 = 50001;
/// The port on which status updates are sent to (and by) the virtual CDJ.
pub const UPDATE_PORT: u16 = 50002;

/// The pitch value which represents playing at normal speed (0%).
pub const NEUTRAL_PITCH: i64 = 1_048_576;

/// The value a player reports for BPM when no track is loaded.
pub const NO_BPM: u32 = 0xffff;

/// Identifies the kinds of packet the protocol uses. Packet type bytes are reused on
/// different ports, so identification always needs the port too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketType {
    FaderStartCommand,
    ChannelsOnAir,
    MediaQuery,
    MediaResponse,
    DeviceHello,
    DeviceRekordboxLightingHelloBytes,
    DeviceNumberStage1,
    DeviceNumberWillAssign,
    DeviceNumberStage2,
    DeviceNumberAssign,
    DeviceNumberStage3,
    DeviceNumberAssignmentFinished,
    DeviceKeepAlive,
    DeviceNumberInUse,
    CdjStatus,
    OpusMetadata,
    LoadTrackCommand,
    LoadTrackAck,
    MasterHandoffRequest,
    MasterHandoffResponse,
    Beat,
    PrecisePosition,
    MixerStatus,
    SyncControl,
    LoadSettingsCommand,
}

impl PacketType {
    /// All packet types, used to build lookup tables.
    pub const ALL: [PacketType; 25] = [
        PacketType::FaderStartCommand,
        PacketType::ChannelsOnAir,
        PacketType::MediaQuery,
        PacketType::MediaResponse,
        PacketType::DeviceHello,
        PacketType::DeviceRekordboxLightingHelloBytes,
        PacketType::DeviceNumberStage1,
        PacketType::DeviceNumberWillAssign,
        PacketType::DeviceNumberStage2,
        PacketType::DeviceNumberAssign,
        PacketType::DeviceNumberStage3,
        PacketType::DeviceNumberAssignmentFinished,
        PacketType::DeviceKeepAlive,
        PacketType::DeviceNumberInUse,
        PacketType::CdjStatus,
        PacketType::OpusMetadata,
        PacketType::LoadTrackCommand,
        PacketType::LoadTrackAck,
        PacketType::MasterHandoffRequest,
        PacketType::MasterHandoffResponse,
        PacketType::Beat,
        PacketType::PrecisePosition,
        PacketType::MixerStatus,
        PacketType::SyncControl,
        PacketType::LoadSettingsCommand,
    ];

    /// The byte value that identifies this packet type.
    pub const fn protocol_value(self) -> u8 {
        match self {
            PacketType::FaderStartCommand => 0x02,
            PacketType::ChannelsOnAir => 0x03,
            PacketType::MediaQuery => 0x05,
            PacketType::MediaResponse => 0x06,
            PacketType::DeviceHello => 0x0a,
            PacketType::DeviceRekordboxLightingHelloBytes => 0x10,
            PacketType::DeviceNumberStage1 => 0x00,
            PacketType::DeviceNumberWillAssign => 0x01,
            PacketType::DeviceNumberStage2 => 0x02,
            PacketType::DeviceNumberAssign => 0x03,
            PacketType::DeviceNumberStage3 => 0x04,
            PacketType::DeviceNumberAssignmentFinished => 0x05,
            PacketType::DeviceKeepAlive => 0x06,
            PacketType::DeviceNumberInUse => 0x08,
            PacketType::CdjStatus => 0x0a,
            PacketType::OpusMetadata => 0x56,
            PacketType::LoadTrackCommand => 0x19,
            PacketType::LoadTrackAck => 0x1a,
            PacketType::MasterHandoffRequest => 0x26,
            PacketType::MasterHandoffResponse => 0x27,
            PacketType::Beat => 0x28,
            PacketType::PrecisePosition => 0x0b,
            PacketType::MixerStatus => 0x29,
            PacketType::SyncControl => 0x2a,
            PacketType::LoadSettingsCommand => 0x34,
        }
    }

    /// The port on which this kind of packet is sent.
    pub const fn port(self) -> u16 {
        match self {
            PacketType::FaderStartCommand
            | PacketType::ChannelsOnAir
            | PacketType::MasterHandoffRequest
            | PacketType::MasterHandoffResponse
            | PacketType::Beat
            | PacketType::PrecisePosition
            | PacketType::SyncControl => BEAT_PORT,
            PacketType::DeviceHello
            | PacketType::DeviceNumberStage1
            | PacketType::DeviceNumberWillAssign
            | PacketType::DeviceNumberStage2
            | PacketType::DeviceNumberAssign
            | PacketType::DeviceNumberStage3
            | PacketType::DeviceNumberAssignmentFinished
            | PacketType::DeviceKeepAlive
            | PacketType::DeviceNumberInUse => ANNOUNCEMENT_PORT,
            _ => UPDATE_PORT,
        }
    }

    /// A human-readable name for the packet type.
    pub const fn name(self) -> &'static str {
        match self {
            PacketType::FaderStartCommand => "Fader Start",
            PacketType::ChannelsOnAir => "Channels On Air",
            PacketType::MediaQuery => "Media Query",
            PacketType::MediaResponse => "Media Response",
            PacketType::DeviceHello => "Device Hello",
            PacketType::DeviceRekordboxLightingHelloBytes => "Rekordbox Lighting Hello Bytes",
            PacketType::DeviceNumberStage1 => "Device Number Claim Stage 1",
            PacketType::DeviceNumberWillAssign => "Device Number Will Be Assigned",
            PacketType::DeviceNumberStage2 => "Device Number Claim Stage 2",
            PacketType::DeviceNumberAssign => "Device Number Assignment",
            PacketType::DeviceNumberStage3 => "Device Number Claim Stage 3",
            PacketType::DeviceNumberAssignmentFinished => "Device Number Assignment Finished",
            PacketType::DeviceKeepAlive => "Device Keep-Alive",
            PacketType::DeviceNumberInUse => "Device Number In Use",
            PacketType::CdjStatus => "CDJ Status",
            PacketType::OpusMetadata => "OPUS Metadata",
            PacketType::LoadTrackCommand => "Load Track Command",
            PacketType::LoadTrackAck => "Load Track Acknowledgment",
            PacketType::MasterHandoffRequest => "Master Handoff Request",
            PacketType::MasterHandoffResponse => "Master Handoff Response",
            PacketType::Beat => "Beat",
            PacketType::PrecisePosition => "Precise Position",
            PacketType::MixerStatus => "Mixer Status",
            PacketType::SyncControl => "Sync Control",
            PacketType::LoadSettingsCommand => "Load Settings Command",
        }
    }

    /// Look up the packet type for a type byte received on a particular port.
    pub fn lookup(port: u16, value: u8) -> Option<PacketType> {
        PacketType::ALL
            .iter()
            .copied()
            .find(|t| t.port() == port && t.protocol_value() == value)
    }
}

/// Checks that a packet received on `port` starts with the magic header and has a
/// recognized type, returning that type. `port` must be one of the three standard
/// protocol ports (the logical port, even when sockets are bound elsewhere for tests).
pub fn validate_header(data: &[u8], port: u16) -> Option<PacketType> {
    if data.len() <= PACKET_TYPE_OFFSET {
        log::debug!("Packet too short to be a Pro DJ Link packet ({} bytes).", data.len());
        return None;
    }
    if data[..MAGIC_HEADER.len()] != MAGIC_HEADER {
        log::debug!("Packet did not have correct ten-byte header for the Pro DJ Link protocol.");
        return None;
    }
    let result = PacketType::lookup(port, data[PACKET_TYPE_OFFSET]);
    if result.is_none() {
        log::debug!(
            "Unrecognized Pro DJ Link packet type 0x{:02x} on port {}",
            data[PACKET_TYPE_OFFSET],
            port
        );
    }
    result
}

/// Assemble a packet with the standard header, type, device name and payload.
/// The device name is padded or truncated to twenty bytes.
pub fn build_packet(kind: PacketType, device_name: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(0x1f + payload.len());
    result.extend_from_slice(&MAGIC_HEADER);
    result.push(kind.protocol_value());
    let mut name = [0u8; 20];
    let n = device_name.len().min(20);
    name[..n].copy_from_slice(&device_name[..n]);
    result.extend_from_slice(&name);
    result.extend_from_slice(payload);
    result
}

/// Set a byte in a payload, using the address it will have in the assembled packet.
pub fn set_payload_byte(payload: &mut [u8], address: usize, value: u8) {
    payload[address - 0x1f] = value;
}

/// Reconstruct a big-endian unsigned number from a run of bytes.
pub fn bytes_to_number(buffer: &[u8], start: usize, length: usize) -> u64 {
    buffer[start..start + length]
        .iter()
        .fold(0u64, |acc, b| (acc << 8) + u64::from(*b))
}

/// Reconstruct a little-endian unsigned number from a run of bytes.
pub fn bytes_to_number_le(buffer: &[u8], start: usize, length: usize) -> u64 {
    buffer[start..start + length]
        .iter()
        .rev()
        .fold(0u64, |acc, b| (acc << 8) + u64::from(*b))
}

/// Write a number into a buffer as big-endian bytes.
pub fn number_to_bytes(number: u64, buffer: &mut [u8], start: usize, length: usize) {
    let mut n = number;
    for index in (start..start + length).rev() {
        buffer[index] = (n & 0xff) as u8;
        n >>= 8;
    }
}

/// Read a fixed-width, zero-padded ASCII name field and trim it.
pub fn read_name(buffer: &[u8], start: usize, length: usize) -> String {
    let end = (start + length).min(buffer.len());
    if start >= end {
        return String::new();
    }
    let raw = &buffer[start..end];
    let raw = match raw.iter().position(|b| *b == 0) {
        Some(nul) => &raw[..nul],
        None => raw,
    };
    String::from_utf8_lossy(raw).trim().to_string()
}

/// Convert a raw pitch value to a percentage (0.0 means normal speed).
pub fn pitch_to_percentage(pitch: i64) -> f64 {
    (pitch - NEUTRAL_PITCH) as f64 / (NEUTRAL_PITCH as f64 / 100.0)
}

/// Convert a percentage to a raw pitch value.
pub fn percentage_to_pitch(percentage: f64) -> i64 {
    (percentage * NEUTRAL_PITCH as f64 / 100.0).round() as i64 + NEUTRAL_PITCH
}

/// Convert a raw pitch value to a speed multiplier (1.0 means normal speed).
pub fn pitch_to_multiplier(pitch: i64) -> f64 {
    pitch as f64 / NEUTRAL_PITCH as f64
}

/// Convert a CDJ half-frame position (1/150 second) to milliseconds.
pub fn half_frame_to_time(half_frame: i64) -> i64 {
    half_frame * 100 / 15
}

/// Convert milliseconds to a half-frame position (truncating).
pub fn time_to_half_frame(milliseconds: i64) -> i64 {
    milliseconds * 15 / 100
}

/// Convert milliseconds to a half-frame position (rounding).
pub fn time_to_half_frame_rounded(milliseconds: i64) -> i64 {
    (milliseconds as f64 * 0.15).round() as i64
}

/// Opus Quad reports player numbers with extra high bits that must be masked off.
pub fn translate_opus_player_numbers(reported: u8) -> u8 {
    reported & 7
}

/// Checks whether two IPv4 addresses are on the same network given a prefix length.
pub fn same_network(prefix_length: u8, a: Ipv4Addr, b: Ipv4Addr) -> bool {
    if prefix_length == 0 {
        return true;
    }
    let mask: u32 = if prefix_length >= 32 { u32::MAX } else { u32::MAX << (32 - prefix_length) };
    (u32::from(a) & mask) == (u32::from(b) & mask)
}

/// Rewrites an artwork path to request the high-resolution variant.
pub fn high_resolution_path(art_path: &str) -> String {
    match art_path.rfind('.') {
        Some(dot) if !art_path[dot + 1..].contains('/') => {
            format!("{}_m{}", &art_path[..dot], &art_path[dot..])
        }
        _ => art_path.to_string(),
    }
}

/// Format a millisecond track time the way a CDJ displays it: `MM:SS:FF.T`.
pub fn format_track_time(ms: i64) -> String {
    let ms = ms.max(0);
    let half_frames = time_to_half_frame(ms) % 150;
    let minutes = ms / 60000;
    let seconds = (ms % 60000) / 1000;
    let frames = half_frames / 2;
    let tenths = if half_frames % 2 == 0 { 0 } else { 5 };
    format!("{minutes:02}:{seconds:02}:{frames:02}.{tenths}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_round_trip() {
        let mut buf = [0u8; 8];
        number_to_bytes(0x1234_5678, &mut buf, 2, 4);
        assert_eq!(buf, [0, 0, 0x12, 0x34, 0x56, 0x78, 0, 0]);
        assert_eq!(bytes_to_number(&buf, 2, 4), 0x1234_5678);
        assert_eq!(bytes_to_number_le(&[0x78, 0x56, 0x34, 0x12], 0, 4), 0x1234_5678);
    }

    #[test]
    fn pitch_conversions() {
        assert_eq!(pitch_to_percentage(NEUTRAL_PITCH), 0.0);
        assert!((pitch_to_percentage(percentage_to_pitch(6.0)) - 6.0).abs() < 0.0001);
        assert_eq!(pitch_to_multiplier(NEUTRAL_PITCH * 2), 2.0);
    }

    #[test]
    fn packet_lookup_uses_port() {
        assert_eq!(PacketType::lookup(UPDATE_PORT, 0x0a), Some(PacketType::CdjStatus));
        assert_eq!(PacketType::lookup(ANNOUNCEMENT_PORT, 0x0a), Some(PacketType::DeviceHello));
        assert_eq!(PacketType::lookup(BEAT_PORT, 0x28), Some(PacketType::Beat));
        assert_eq!(PacketType::lookup(BEAT_PORT, 0x0a), None);
    }

    #[test]
    fn half_frames() {
        assert_eq!(half_frame_to_time(150), 1000);
        assert_eq!(time_to_half_frame(1000), 150);
        assert_eq!(format_track_time(61_500), "01:01:37.5");
    }

    #[test]
    fn network_matching() {
        let a = Ipv4Addr::new(192, 168, 1, 10);
        let b = Ipv4Addr::new(192, 168, 1, 200);
        let c = Ipv4Addr::new(192, 168, 2, 10);
        assert!(same_network(24, a, b));
        assert!(!same_network(24, a, c));
        assert!(same_network(16, a, c));
    }

    #[test]
    fn hi_res_art() {
        assert_eq!(high_resolution_path("/PIONEER/ART/a1.jpg"), "/PIONEER/ART/a1_m.jpg");
    }
}
