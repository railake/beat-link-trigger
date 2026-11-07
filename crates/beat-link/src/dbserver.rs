//! The TCP "dbserver" protocol players use to serve track metadata, artwork, beat
//! grids, cue lists and waveforms. Port of `org.deepsymmetry.beatlink.dbserver`.

use crate::packets::{TrackSourceSlot, TrackType};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::Duration;

/// The port on which players answer "where is your dbserver?" queries.
pub const DB_SERVER_QUERY_PORT: u16 = 12523;
/// The value that starts every message.
pub const MESSAGE_START: u32 = 0x8723_49ae;
/// Reported as the item count when a menu has no results.
pub const NO_MENU_RESULTS_AVAILABLE: u32 = 0xffff_ffff;

/// A single field in a dbserver message.
#[derive(Clone, PartialEq, Eq)]
pub enum Field {
    /// A number of 1, 2 or 4 bytes.
    Number { value: u32, size: u8 },
    /// A binary blob.
    Binary(Vec<u8>),
    /// A UTF-16 string.
    String(String),
}

impl Field {
    pub fn number(value: u32) -> Field {
        Field::Number { value, size: 4 }
    }
    pub fn number_sized(value: u32, size: u8) -> Field {
        Field::Number { value, size }
    }

    /// The tag used in a message's argument-type list.
    pub fn argument_tag(&self) -> u8 {
        match self {
            Field::Number { .. } => 0x06,
            Field::Binary(_) => 0x03,
            Field::String(_) => 0x02,
        }
    }

    /// Encode the field for transmission.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Field::Number { value, size } => {
                let mut b = Vec::with_capacity(5);
                match size {
                    1 => {
                        b.push(0x0f);
                        b.push(*value as u8);
                    }
                    2 => {
                        b.push(0x10);
                        b.extend_from_slice(&(*value as u16).to_be_bytes());
                    }
                    _ => {
                        b.push(0x11);
                        b.extend_from_slice(&value.to_be_bytes());
                    }
                }
                b
            }
            Field::Binary(bytes) => {
                if bytes.is_empty() {
                    return Vec::new();
                }
                let mut b = Vec::with_capacity(bytes.len() + 5);
                b.push(0x14);
                b.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                b.extend_from_slice(bytes);
                b
            }
            Field::String(text) => {
                let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let mut b = Vec::with_capacity(units.len() * 2 + 5);
                b.push(0x26);
                b.extend_from_slice(&(units.len() as u32).to_be_bytes());
                for u in units {
                    b.extend_from_slice(&u.to_be_bytes());
                }
                b
            }
        }
    }

    /// Read a field from a stream.
    pub fn read<R: Read>(r: &mut R) -> io::Result<Field> {
        let mut tag = [0u8; 1];
        r.read_exact(&mut tag)?;
        match tag[0] {
            0x0f => {
                let mut b = [0u8; 1];
                r.read_exact(&mut b)?;
                Ok(Field::Number { value: u32::from(b[0]), size: 1 })
            }
            0x10 => {
                let mut b = [0u8; 2];
                r.read_exact(&mut b)?;
                Ok(Field::Number { value: u32::from(u16::from_be_bytes(b)), size: 2 })
            }
            0x11 => {
                let mut b = [0u8; 4];
                r.read_exact(&mut b)?;
                Ok(Field::Number { value: u32::from_be_bytes(b), size: 4 })
            }
            0x14 => {
                let mut size = [0u8; 4];
                r.read_exact(&mut size)?;
                let size = u32::from_be_bytes(size) as usize;
                if size > 64 * 1024 * 1024 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "binary field too large"));
                }
                let mut buf = vec![0u8; size];
                r.read_exact(&mut buf)?;
                Ok(Field::Binary(buf))
            }
            0x26 => {
                let mut size = [0u8; 4];
                r.read_exact(&mut size)?;
                let chars = u32::from_be_bytes(size) as usize;
                if chars > 1024 * 1024 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "string field too large"));
                }
                let mut buf = vec![0u8; chars * 2];
                r.read_exact(&mut buf)?;
                let mut units: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                if units.last() == Some(&0) {
                    units.pop();
                }
                Ok(Field::String(String::from_utf16_lossy(&units)))
            }
            other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unable to read a field with type tag {other}"))),
        }
    }

    pub fn as_number(&self) -> Option<u32> {
        match self {
            Field::Number { value, .. } => Some(*value),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Field::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_binary(&self) -> Option<&[u8]> {
        match self {
            Field::Binary(b) => Some(b),
            _ => None,
        }
    }
}

impl fmt::Debug for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Field::Number { value, size } => write!(f, "Number({value}/{size})"),
            Field::Binary(b) => write!(f, "Binary({} bytes)", b.len()),
            Field::String(s) => write!(f, "String({s:?})"),
        }
    }
}

/// Message types we know about.
pub mod kind {
    pub const SETUP_REQ: u16 = 0x0000;
    pub const INVALID_DATA: u16 = 0x0001;
    pub const TEARDOWN_REQ: u16 = 0x0100;
    pub const ROOT_MENU_REQ: u16 = 0x1000;
    pub const GENRE_MENU_REQ: u16 = 0x1001;
    pub const ARTIST_MENU_REQ: u16 = 0x1002;
    pub const ALBUM_MENU_REQ: u16 = 0x1003;
    pub const TRACK_MENU_REQ: u16 = 0x1004;
    pub const PLAYLIST_REQ: u16 = 0x1105;
    pub const SEARCH_MENU: u16 = 0x1300;
    pub const REKORDBOX_METADATA_REQ: u16 = 0x2002;
    pub const ALBUM_ART_REQ: u16 = 0x2003;
    pub const WAVE_PREVIEW_REQ: u16 = 0x2004;
    pub const FOLDER_MENU_REQ: u16 = 0x2006;
    pub const CUE_LIST_REQ: u16 = 0x2104;
    pub const UNANALYZED_METADATA_REQ: u16 = 0x2202;
    pub const BEAT_GRID_REQ: u16 = 0x2204;
    pub const WAVE_DETAIL_REQ: u16 = 0x2904;
    pub const CUE_LIST_EXT_REQ: u16 = 0x2b04;
    pub const ANLZ_TAG_REQ: u16 = 0x2c04;
    pub const RENDER_MENU_REQ: u16 = 0x3000;
    pub const MENU_AVAILABLE: u16 = 0x4000;
    pub const MENU_HEADER: u16 = 0x4001;
    pub const ALBUM_ART: u16 = 0x4002;
    pub const UNAVAILABLE: u16 = 0x4003;
    pub const MENU_ITEM: u16 = 0x4101;
    pub const MENU_FOOTER: u16 = 0x4201;
    pub const WAVE_PREVIEW: u16 = 0x4402;
    pub const BEAT_GRID: u16 = 0x4602;
    pub const CUE_LIST: u16 = 0x4702;
    pub const WAVE_DETAIL: u16 = 0x4a02;
    pub const CUE_LIST_EXT: u16 = 0x4e02;
    pub const ANLZ_TAG: u16 = 0x4f02;
}

/// Kinds of rendered menu items (argument 6 of a MENU_ITEM).
pub mod item {
    pub const FOLDER: u32 = 0x0001;
    pub const ALBUM_TITLE: u32 = 0x0002;
    pub const DISC: u32 = 0x0003;
    pub const TRACK_TITLE: u32 = 0x0004;
    pub const GENRE: u32 = 0x0006;
    pub const ARTIST: u32 = 0x0007;
    pub const PLAYLIST: u32 = 0x0008;
    pub const RATING: u32 = 0x000a;
    pub const DURATION: u32 = 0x000b;
    pub const TEMPO: u32 = 0x000d;
    pub const LABEL: u32 = 0x000e;
    pub const KEY: u32 = 0x000f;
    pub const BIT_RATE: u32 = 0x0010;
    pub const YEAR: u32 = 0x0011;
    pub const COLOR_NONE: u32 = 0x0013;
    pub const COLOR_PURPLE: u32 = 0x001b;
    pub const COMMENT: u32 = 0x0023;
    pub const HISTORY_PLAYLIST: u32 = 0x0024;
    pub const ORIGINAL_ARTIST: u32 = 0x0028;
    pub const REMIXER: u32 = 0x0029;
    pub const DATE_ADDED: u32 = 0x002e;
}

/// Menus that requests can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuIdentifier {
    MainMenu = 1,
    SubMenu = 2,
    TrackInfo = 3,
    SortMenu = 5,
    Data = 8,
}

/// A dbserver message.
#[derive(Clone, PartialEq, Eq)]
pub struct Message {
    pub transaction: u32,
    pub kind: u16,
    pub arguments: Vec<Field>,
}

impl Message {
    pub fn new(transaction: u32, kind: u16, arguments: Vec<Field>) -> Message {
        assert!(arguments.len() <= 12, "Messages cannot have more than 12 arguments");
        Message { transaction, kind, arguments }
    }

    /// Encode the whole message.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut tags = [0u8; 12];
        for (i, a) in self.arguments.iter().enumerate() {
            tags[i] = a.argument_tag();
        }
        let mut out = Vec::new();
        out.extend(Field::number(MESSAGE_START).to_bytes());
        out.extend(Field::number(self.transaction).to_bytes());
        out.extend(Field::number_sized(u32::from(self.kind), 2).to_bytes());
        out.extend(Field::number_sized(self.arguments.len() as u32, 1).to_bytes());
        out.extend(Field::Binary(tags.to_vec()).to_bytes());
        for a in &self.arguments {
            out.extend(a.to_bytes());
        }
        out
    }

    /// Read a message from a stream.
    pub fn read<R: Read>(r: &mut R) -> io::Result<Message> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        match Field::read(r)? {
            Field::Number { value: MESSAGE_START, size: 4 } => {}
            other => return Err(bad(&format!("Did not find message start, got {other:?}"))),
        }
        let transaction = match Field::read(r)? {
            Field::Number { value, size: 4 } => value,
            other => return Err(bad(&format!("Bad transaction field {other:?}"))),
        };
        let kind = match Field::read(r)? {
            Field::Number { value, size: 2 } => value as u16,
            other => return Err(bad(&format!("Bad message type field {other:?}"))),
        };
        let count = match Field::read(r)? {
            Field::Number { value, size: 1 } if value <= 12 => value as usize,
            other => return Err(bad(&format!("Bad argument count field {other:?}"))),
        };
        let tags = match Field::read(r)? {
            Field::Binary(b) => b,
            other => return Err(bad(&format!("Bad argument tags field {other:?}"))),
        };
        let mut arguments = Vec::with_capacity(count);
        for i in 0..count {
            let tag = tags.get(i).copied().unwrap_or(0);
            // A zero-length binary field is not sent at all; it follows a zero size number.
            let last_zero = matches!(arguments.last(), Some(Field::Number { value: 0, .. }));
            let field = if tag == 3 && last_zero { Field::Binary(Vec::new()) } else { Field::read(r)? };
            if field.argument_tag() != tag {
                return Err(bad(&format!("Argument {i} had wrong type: expected tag {tag}, got {field:?}")));
            }
            arguments.push(field);
        }
        Ok(Message { transaction, kind, arguments })
    }

    /// For MENU_AVAILABLE responses, the number of items available.
    pub fn menu_results_count(&self) -> Option<u32> {
        if self.kind == kind::MENU_AVAILABLE {
            self.arguments.get(1).and_then(Field::as_number)
        } else {
            None
        }
    }

    /// For MENU_ITEM responses, the item type.
    pub fn menu_item_type(&self) -> Option<u32> {
        if self.kind == kind::MENU_ITEM {
            self.arguments.get(6).and_then(Field::as_number).map(|v| v & 0xffff)
        } else {
            None
        }
    }

    pub fn number_arg(&self, i: usize) -> Option<u32> {
        self.arguments.get(i).and_then(Field::as_number)
    }
    pub fn string_arg(&self, i: usize) -> Option<&str> {
        self.arguments.get(i).and_then(Field::as_str)
    }
    pub fn binary_arg(&self, i: usize) -> Option<&[u8]> {
        self.arguments.get(i).and_then(Field::as_binary)
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Message[transaction:{}, type:0x{:04x}, args:{:?}]", self.transaction, self.kind, self.arguments)
    }
}

/// Build the "r:m:s:t" argument that starts most requests.
pub fn build_rmst(requesting_player: u8, menu: MenuIdentifier, slot: TrackSourceSlot, track_type: TrackType) -> Field {
    Field::number(
        (u32::from(requesting_player) << 24)
            | ((menu as u32 & 0xff) << 16)
            | (u32::from(slot.protocol_value()) << 8)
            | u32::from(track_type.protocol_value()),
    )
}

/// Ask a player which port its dbserver is listening on.
pub fn query_db_server_port(address: Ipv4Addr, timeout: Duration) -> io::Result<u16> {
    const QUERY: [u8; 19] = [
        0x00, 0x00, 0x00, 0x0f, 0x52, 0x65, 0x6d, 0x6f, 0x74, 0x65, 0x44, 0x42, 0x53, 0x65, 0x72, 0x76, 0x65, 0x72, 0x00,
    ];
    let mut s = TcpStream::connect_timeout(&SocketAddr::V4(SocketAddrV4::new(address, DB_SERVER_QUERY_PORT)), timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.write_all(&QUERY)?;
    let mut buf = [0u8; 2];
    s.read_exact(&mut buf)?;
    let port = u16::from_be_bytes(buf);
    if port == 0xffff {
        return Err(io::Error::new(io::ErrorKind::NotConnected, "dbserver not yet ready"));
    }
    Ok(port)
}

/// An open connection to a player's dbserver.
pub struct Client {
    stream: TcpStream,
    pub target_player: u8,
    pub posing_as_player: u8,
    transaction_counter: u32,
    menu_batch_size: u32,
}

impl Client {
    /// Connect and perform the greeting and setup exchange.
    pub fn connect(address: Ipv4Addr, port: u16, target_player: u8, posing_as: u8, timeout: Duration) -> io::Result<Client> {
        let stream = TcpStream::connect_timeout(&SocketAddr::V4(SocketAddrV4::new(address, port)), timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        let mut client = Client { stream, target_player, posing_as_player: posing_as, transaction_counter: 0, menu_batch_size: 64 };
        client.stream.write_all(&Field::number(1).to_bytes())?;
        match Field::read(&mut client.stream)? {
            Field::Number { value: 1, size: 4 } => {}
            other => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unexpected dbserver greeting response {other:?}")))
            }
        }
        let setup = Message::new(0xffff_fffe, kind::SETUP_REQ, vec![Field::number(u32::from(posing_as))]);
        client.stream.write_all(&setup.to_bytes())?;
        let response = Message::read(&mut client.stream)?;
        if response.kind != kind::MENU_AVAILABLE || response.arguments.len() != 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unexpected setup response {response:?}")));
        }
        if response.number_arg(1) != Some(u32::from(target_player)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected to connect to player {target_player}, but it identified itself as {:?}", response.number_arg(1)),
            ));
        }
        Ok(client)
    }

    fn next_transaction(&mut self) -> u32 {
        self.transaction_counter += 1;
        self.transaction_counter
    }

    /// The r:m:s:t field identifying us as the requester.
    pub fn rmst(&self, menu: MenuIdentifier, slot: TrackSourceSlot, track_type: TrackType) -> Field {
        build_rmst(self.posing_as_player, menu, slot, track_type)
    }

    /// Send a request and read its single response, checking the type if given.
    pub fn simple_request(&mut self, request: u16, expected: Option<u16>, args: Vec<Field>) -> io::Result<Message> {
        let transaction = self.next_transaction();
        let message = Message::new(transaction, request, args);
        self.stream.write_all(&message.to_bytes())?;
        let response = Message::read(&mut self.stream)?;
        if response.transaction != transaction {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Wrong transaction in response {response:?}")));
        }
        if let Some(expected) = expected {
            if response.kind != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Expected response type 0x{expected:04x}, got {response:?}"),
                ));
            }
        }
        Ok(response)
    }

    /// Issue a menu request, returning the MENU_AVAILABLE response.
    pub fn menu_request(
        &mut self,
        request: u16,
        menu: MenuIdentifier,
        slot: TrackSourceSlot,
        track_type: TrackType,
        mut args: Vec<Field>,
    ) -> io::Result<Message> {
        args.insert(0, self.rmst(menu, slot, track_type));
        let response = self.simple_request(request, Some(kind::MENU_AVAILABLE), args)?;
        if response.number_arg(0) != Some(u32::from(request)) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Menu response for wrong request: {response:?}")));
        }
        Ok(response)
    }

    /// Render the items of the most recently requested menu.
    pub fn render_menu_items(
        &mut self,
        menu: MenuIdentifier,
        slot: TrackSourceSlot,
        track_type: TrackType,
        mut offset: u32,
        count: u32,
    ) -> io::Result<Vec<Message>> {
        let mut results = Vec::with_capacity(count as usize);
        let mut gathered = 0;
        while gathered < count {
            let batch = (count - gathered).min(self.menu_batch_size);
            let transaction = self.next_transaction();
            let request = Message::new(
                transaction,
                kind::RENDER_MENU_REQ,
                vec![
                    self.rmst(menu, slot, track_type),
                    Field::number(offset),
                    Field::number(batch),
                    Field::number(0),
                    Field::number(count),
                    Field::number(0),
                ],
            );
            self.stream.write_all(&request.to_bytes())?;
            let header = Message::read(&mut self.stream)?;
            if header.kind != kind::MENU_HEADER {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Expected MENU_HEADER, got {header:?}")));
            }
            loop {
                let m = Message::read(&mut self.stream)?;
                match m.kind {
                    kind::MENU_ITEM => results.push(m),
                    kind::MENU_FOOTER => break,
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Expected MENU_ITEM or FOOTER, got {m:?}"))),
                }
            }
            offset += batch;
            gathered += batch;
        }
        Ok(results)
    }

    /// Render all the items of a MENU_AVAILABLE response.
    pub fn render_all(&mut self, menu: MenuIdentifier, slot: TrackSourceSlot, track_type: TrackType, available: &Message) -> io::Result<Vec<Message>> {
        match available.menu_results_count() {
            None | Some(0) | Some(NO_MENU_RESULTS_AVAILABLE) => Ok(Vec::new()),
            Some(n) => self.render_menu_items(menu, slot, track_type, 0, n),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let teardown = Message::new(0xffff_fffe, kind::TEARDOWN_REQ, vec![]);
        let _ = self.stream.write_all(&teardown.to_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_round_trips() {
        for f in [
            Field::number(0x1234_5678),
            Field::number_sized(0x42, 1),
            Field::number_sized(0x4243, 2),
            Field::String("Hello, Wörld".into()),
            Field::Binary(vec![1, 2, 3]),
        ] {
            let bytes = f.to_bytes();
            let back = Field::read(&mut bytes.as_slice()).unwrap();
            assert_eq!(f, back);
        }
    }

    #[test]
    fn message_round_trip() {
        let m = Message::new(7, kind::REKORDBOX_METADATA_REQ, vec![
            build_rmst(3, MenuIdentifier::MainMenu, TrackSourceSlot::UsbSlot, TrackType::Rekordbox),
            Field::number(42),
        ]);
        let bytes = m.to_bytes();
        let back = Message::read(&mut bytes.as_slice()).unwrap();
        assert_eq!(m, back);
        assert_eq!(back.number_arg(0), Some(0x0301_0301));
    }

    #[test]
    fn zero_length_binary_after_zero_number() {
        // A message whose blob length is zero omits the blob field entirely.
        let m = Message::new(1, kind::BEAT_GRID, vec![Field::number(0x2204), Field::number(0), Field::number(0), Field::Binary(vec![])]);
        let bytes = m.to_bytes();
        let back = Message::read(&mut bytes.as_slice()).unwrap();
        assert_eq!(back.binary_arg(3), Some(&[][..]));
    }
}
