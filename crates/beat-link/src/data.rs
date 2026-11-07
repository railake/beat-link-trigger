//! Track data: metadata, beat grids, cue lists, waveforms, song structure (phrases),
//! artwork and track signatures. Ports of the classes in
//! `org.deepsymmetry.beatlink.data` that describe tracks, plus the subset of the
//! rekordbox ANLZ format (from crate-digger's `rekordbox_anlz.ksy`) that they use.

use crate::dbserver::{item, Message};
use crate::packets::{TrackSourceSlot, TrackType};
use crate::util::{bytes_to_number, bytes_to_number_le, half_frame_to_time, time_to_half_frame};
use serde::Serialize;

/// Identifies a slot on a particular player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct SlotReference {
    pub player: u8,
    pub slot: TrackSourceSlotKey,
}

/// A hashable, orderable wrapper for track source slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct TrackSourceSlotKey(pub u8);

impl From<TrackSourceSlot> for TrackSourceSlotKey {
    fn from(s: TrackSourceSlot) -> Self {
        TrackSourceSlotKey(s.protocol_value())
    }
}

impl TrackSourceSlotKey {
    pub fn slot(self) -> TrackSourceSlot {
        TrackSourceSlot::from_protocol(self.0)
    }
}

/// Uniquely identifies a track: where it came from, and its id there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct DataReference {
    pub player: u8,
    pub slot: TrackSourceSlotKey,
    pub rekordbox_id: u32,
    pub track_type: u8,
}

impl DataReference {
    pub fn new(player: u8, slot: TrackSourceSlot, rekordbox_id: u32, track_type: TrackType) -> Self {
        DataReference { player, slot: slot.into(), rekordbox_id, track_type: track_type.protocol_value() }
    }
    pub fn slot(&self) -> TrackSourceSlot {
        self.slot.slot()
    }
    pub fn track_type(&self) -> TrackType {
        TrackType::from_protocol(self.track_type)
    }
}

/// A metadata value that also has a database id (artist, album, genre...).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct SearchableItem {
    pub id: u32,
    pub label: String,
}

/// A track color label.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ColorItem {
    pub id: u32,
    pub label: String,
}

impl ColorItem {
    /// The RGB color rekordbox uses for a color id.
    pub fn color_for_id(id: u32) -> Option<(u8, u8, u8)> {
        match id {
            1 => Some((0xf8, 0x70, 0xf8)), // pink
            2 => Some((0xf8, 0x70, 0x70)), // red
            3 => Some((0xf8, 0xa0, 0x30)), // orange
            4 => Some((0xf8, 0xe3, 0x31)), // yellow
            5 => Some((0x10, 0xb1, 0x76)), // green
            6 => Some((0x48, 0xe7, 0xf8)), // aqua
            7 => Some((0x30, 0x90, 0xf8)), // blue
            8 => Some((0x90, 0x60, 0xf8)), // purple
            _ => None,
        }
    }
    /// The name of the color for an id.
    pub fn name_for_id(id: u32) -> &'static str {
        match id {
            1 => "Pink",
            2 => "Red",
            3 => "Orange",
            4 => "Yellow",
            5 => "Green",
            6 => "Aqua",
            7 => "Blue",
            8 => "Purple",
            _ => "No Color",
        }
    }
}

/// Metadata about a track, as served by a player's dbserver.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: Option<SearchableItem>,
    pub original_artist: Option<SearchableItem>,
    pub remixer: Option<SearchableItem>,
    pub album: Option<SearchableItem>,
    pub label: Option<SearchableItem>,
    pub genre: Option<SearchableItem>,
    pub key: Option<SearchableItem>,
    pub color: Option<ColorItem>,
    pub comment: String,
    pub date_added: String,
    /// Track length in seconds.
    pub duration: u32,
    /// Starting tempo times 100.
    pub tempo: u32,
    pub rating: u32,
    pub year: u32,
    pub bit_rate: u32,
    pub artwork_id: u32,
    #[serde(skip)]
    pub cue_list: Option<CueList>,
}

fn searchable(m: &Message) -> Option<SearchableItem> {
    Some(SearchableItem { id: m.number_arg(1)?, label: m.string_arg(3)?.to_string() })
}

impl TrackMetadata {
    /// Interpret the rendered menu items returned for a metadata request.
    pub fn from_menu_items(items: &[Message]) -> TrackMetadata {
        let mut md = TrackMetadata::default();
        for m in items {
            let Some(kind) = m.menu_item_type() else { continue };
            match kind {
                item::TRACK_TITLE => {
                    md.title = m.string_arg(3).unwrap_or_default().to_string();
                    md.artwork_id = m.number_arg(8).unwrap_or(0);
                }
                item::ARTIST => md.artist = searchable(m),
                item::ORIGINAL_ARTIST => md.original_artist = searchable(m),
                // beat-link falls through from REMIXER to ALBUM_TITLE (a bug); we do not.
                item::REMIXER => md.remixer = searchable(m),
                item::ALBUM_TITLE => md.album = searchable(m),
                item::LABEL => md.label = searchable(m),
                item::DURATION => md.duration = m.number_arg(1).unwrap_or(0),
                item::TEMPO => md.tempo = m.number_arg(1).unwrap_or(0),
                item::COMMENT => md.comment = m.string_arg(3).unwrap_or_default().to_string(),
                item::KEY => md.key = searchable(m),
                item::RATING => md.rating = m.number_arg(1).unwrap_or(0),
                item::COLOR_NONE..=item::COLOR_PURPLE => {
                    md.color = Some(ColorItem {
                        id: m.number_arg(1).unwrap_or(0),
                        label: m.string_arg(3).unwrap_or_default().to_string(),
                    })
                }
                item::GENRE => md.genre = searchable(m),
                item::DATE_ADDED => md.date_added = m.string_arg(3).unwrap_or_default().to_string(),
                item::YEAR => md.year = m.number_arg(1).unwrap_or(0),
                item::BIT_RATE => md.bit_rate = m.number_arg(1).unwrap_or(0),
                other => log::debug!("Ignoring track metadata item with unknown type 0x{other:04x}"),
            }
        }
        md
    }

    pub fn artist_label(&self) -> &str {
        self.artist.as_ref().map(|a| a.label.as_str()).unwrap_or("")
    }
    pub fn starting_tempo(&self) -> f64 {
        f64::from(self.tempo) / 100.0
    }
}

// ---------------------------------------------------------------------------------
// Beat grids
// ---------------------------------------------------------------------------------

/// The beat grid of a track: where each beat falls and its position within the bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BeatGrid {
    beat_within_bar: Vec<u16>,
    bpm: Vec<u16>,
    time: Vec<u32>,
}

impl BeatGrid {
    /// Build from parallel arrays.
    pub fn new(beat_within_bar: Vec<u16>, bpm: Vec<u16>, time: Vec<u32>) -> Self {
        assert_eq!(beat_within_bar.len(), time.len());
        assert_eq!(bpm.len(), time.len());
        BeatGrid { beat_within_bar, bpm, time }
    }

    /// Parse the little-endian beat grid blob returned by a dbserver BEAT_GRID response.
    pub fn from_dbserver(bytes: &[u8]) -> Self {
        let count = bytes.len().saturating_sub(20) / 16;
        let mut g = BeatGrid { beat_within_bar: Vec::with_capacity(count), bpm: Vec::with_capacity(count), time: Vec::with_capacity(count) };
        for i in 0..count {
            let base = 20 + i * 16;
            g.beat_within_bar.push(bytes_to_number_le(bytes, base, 2) as u16);
            g.bpm.push(bytes_to_number_le(bytes, base + 2, 2) as u16);
            g.time.push(bytes_to_number_le(bytes, base + 4, 4) as u32);
        }
        g
    }

    /// Parse the body of an ANLZ PQTZ tag.
    pub fn from_anlz(body: &[u8]) -> Option<Self> {
        if body.len() < 12 {
            return None;
        }
        let n = bytes_to_number(body, 8, 4) as usize;
        let mut g = BeatGrid { beat_within_bar: Vec::with_capacity(n), bpm: Vec::with_capacity(n), time: Vec::with_capacity(n) };
        for i in 0..n {
            let base = 12 + i * 8;
            if base + 8 > body.len() {
                break;
            }
            g.beat_within_bar.push(bytes_to_number(body, base, 2) as u16);
            g.bpm.push(bytes_to_number(body, base + 2, 2) as u16);
            g.time.push(bytes_to_number(body, base + 4, 4) as u32);
        }
        Some(g)
    }

    pub fn beat_count(&self) -> usize {
        self.time.len()
    }

    fn offset(&self, beat: i64) -> usize {
        if beat < 1 {
            0
        } else if beat as usize > self.time.len() {
            self.time.len().saturating_sub(1)
        } else {
            beat as usize - 1
        }
    }

    /// Milliseconds into the track at which a beat (numbered from 1) occurs.
    pub fn time_within_track(&self, beat: i64) -> u64 {
        if beat == 0 || self.time.is_empty() {
            return 0;
        }
        u64::from(self.time[self.offset(beat)])
    }

    /// Position of a beat within its bar (1-4).
    pub fn beat_within_bar(&self, beat: i64) -> u16 {
        if self.time.is_empty() {
            return 0;
        }
        self.beat_within_bar[self.offset(beat)]
    }

    /// Tempo at a beat, times 100.
    pub fn bpm(&self, beat: i64) -> u16 {
        if self.time.is_empty() {
            return 0;
        }
        self.bpm[self.offset(beat)]
    }

    /// The bar number containing a beat (with -1 for a partial first bar).
    pub fn bar_number(&self, beat: i64) -> i64 {
        if self.time.is_empty() {
            return 0;
        }
        let offset = i64::from(self.beat_within_bar(1)) - 1;
        let bar = (offset + self.offset(beat) as i64) / 4;
        if offset == 0 {
            bar + 1
        } else if bar == 0 {
            -1
        } else {
            bar
        }
    }

    /// The beat number at or immediately before a time, with beat-link's conventions:
    /// -1 if before the first beat.
    pub fn find_beat_at_time(&self, ms: u64) -> i64 {
        match self.time.binary_search(&(ms as u32)) {
            Ok(i) => i as i64 + 1,
            Err(0) => -1,
            Err(i) => i as i64,
        }
    }
}

// ---------------------------------------------------------------------------------
// Cue lists
// ---------------------------------------------------------------------------------

/// A memory point, loop or hot cue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CueEntry {
    /// 0 for memory points, otherwise the hot cue number (1 = A).
    pub hot_cue_number: u8,
    pub is_loop: bool,
    /// Position in half-frames.
    pub cue_position: i64,
    /// Time in milliseconds.
    pub cue_time: i64,
    pub loop_position: i64,
    pub loop_time: i64,
    pub comment: String,
    /// Memory point color id (0 = none).
    pub color_id: u8,
    pub embedded_color: Option<(u8, u8, u8)>,
    pub rekordbox_color: Option<(u8, u8, u8)>,
}

impl CueEntry {
    /// The label rekordbox would show for this cue.
    pub fn description(&self) -> String {
        let kind = if self.hot_cue_number > 0 {
            format!("Hot {} {}", if self.is_loop { "Loop" } else { "Cue" }, (b'@' + self.hot_cue_number) as char)
        } else if self.is_loop {
            "Loop".into()
        } else {
            "Memory".into()
        };
        if self.comment.is_empty() {
            kind
        } else {
            format!("{kind}: {}", self.comment)
        }
    }

    /// The display color for the cue.
    pub fn color(&self) -> (u8, u8, u8) {
        if self.hot_cue_number == 0 {
            if let Some(c) = ColorItem::color_for_id(u32::from(self.color_id)) {
                return c;
            }
        } else if let Some(c) = self.rekordbox_color.or(self.embedded_color) {
            return c;
        }
        if self.hot_cue_number > 0 {
            (0, 255, 0)
        } else if self.is_loop {
            (255, 200, 0)
        } else {
            (255, 0, 0)
        }
    }
}

/// The cues of a track, sorted by position.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct CueList {
    pub entries: Vec<CueEntry>,
}

/// The rekordbox hot cue color palette (color code → RGB).
pub fn rekordbox_color(code: u8) -> Option<(u8, u8, u8)> {
    const TABLE: [(u8, u8, u8); 0x3e] = [
        (0x30, 0x5a, 0xff), (0x50, 0x73, 0xff), (0x50, 0x8c, 0xff), (0x50, 0xa0, 0xff), (0x50, 0xb4, 0xff),
        (0x50, 0xb0, 0xf2), (0x50, 0xae, 0xe8), (0x45, 0xac, 0xdb), (0x00, 0xe0, 0xff), (0x19, 0xda, 0xf0),
        (0x32, 0xd2, 0xe6), (0x21, 0xb4, 0xb9), (0x20, 0xaa, 0xa0), (0x1f, 0xa3, 0x92), (0x19, 0xa0, 0x8c),
        (0x14, 0xa5, 0x84), (0x14, 0xaa, 0x7d), (0x10, 0xb1, 0x76), (0x30, 0xd2, 0x6e), (0x37, 0xde, 0x5a),
        (0x3c, 0xeb, 0x50), (0x28, 0xe2, 0x14), (0x7d, 0xc1, 0x3d), (0x8c, 0xc8, 0x32), (0x9b, 0xd7, 0x23),
        (0xa5, 0xe1, 0x16), (0xa5, 0xdc, 0x0a), (0xaa, 0xd2, 0x08), (0xb4, 0xc8, 0x05), (0xb4, 0xbe, 0x04),
        (0xba, 0xb4, 0x04), (0xc3, 0xaf, 0x04), (0xe1, 0xaa, 0x00), (0xff, 0xa0, 0x00), (0xff, 0x96, 0x00),
        (0xff, 0x8c, 0x00), (0xff, 0x75, 0x00), (0xe0, 0x64, 0x1b), (0xe0, 0x46, 0x1e), (0xe0, 0x30, 0x1e),
        (0xe0, 0x28, 0x23), (0xe6, 0x28, 0x28), (0xff, 0x37, 0x6f), (0xff, 0x2d, 0x6f), (0xff, 0x12, 0x7b),
        (0xf5, 0x1e, 0x8c), (0xeb, 0x2d, 0xa0), (0xe6, 0x37, 0xb4), (0xde, 0x44, 0xcf), (0xde, 0x44, 0x8d),
        (0xe6, 0x30, 0xb4), (0xe6, 0x19, 0xdc), (0xe6, 0x00, 0xff), (0xdc, 0x00, 0xff), (0xcc, 0x00, 0xff),
        (0xb4, 0x32, 0xff), (0xb9, 0x3c, 0xff), (0xc5, 0x42, 0xff), (0xaa, 0x5a, 0xff), (0xaa, 0x72, 0xff),
        (0x82, 0x72, 0xff), (0x64, 0x73, 0xff),
    ];
    if code == 0 || code as usize > TABLE.len() {
        None
    } else {
        Some(TABLE[code as usize - 1])
    }
}

impl CueList {
    fn sorted(mut entries: Vec<CueEntry>) -> CueList {
        entries.sort_by(|a, b| a.cue_position.cmp(&b.cue_position).then(a.hot_cue_number.cmp(&b.hot_cue_number)));
        CueList { entries }
    }

    /// Parse the extended (nxs2) cue list returned by a CUE_LIST_EXT response.
    pub fn from_nxs2_message(m: &Message) -> CueList {
        let bytes = m.binary_arg(3).unwrap_or_default();
        let count = m.number_arg(4).unwrap_or(0) as usize;
        let mut entries = Vec::new();
        let mut offset = 0usize;
        let byte = |i: usize| bytes.get(i).copied().unwrap_or(0);
        for _ in 0..count {
            if offset + 0x14 > bytes.len() {
                break;
            }
            let size = bytes_to_number_le(bytes, offset, 4) as usize;
            if size == 0 {
                break;
            }
            let cue_flag = byte(offset + 6);
            let hot = byte(offset + 4);
            if cue_flag != 0 || hot != 0 {
                let position = time_to_half_frame(bytes_to_number_le(bytes, offset + 12, 4) as i64);
                let mut comment = String::new();
                let mut comment_size = 0usize;
                if size > 0x49 && offset + 0x4a <= bytes.len() {
                    comment_size = bytes_to_number_le(bytes, offset + 0x48, 2) as usize;
                }
                if comment_size > 2 && offset + 0x4a + comment_size - 2 <= bytes.len() {
                    let raw = &bytes[offset + 0x4a..offset + 0x4a + comment_size - 2];
                    let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    comment = String::from_utf16_lossy(&units).trim().to_string();
                }
                let end = if cue_flag == 2 { Some(time_to_half_frame(bytes_to_number_le(bytes, offset + 16, 4) as i64)) } else { None };
                let (rekordbox_color, embedded_color, color_id) = if hot == 0 {
                    (None, None, byte(offset + 0x22))
                } else {
                    let base = offset + comment_size + 0x4e;
                    let (r, g, b) = (byte(base + 1), byte(base + 2), byte(base + 3));
                    (rekordbox_color(byte(base)), if r == 0 && g == 0 && b == 0 { None } else { Some((r, g, b)) }, 0)
                };
                entries.push(CueEntry {
                    hot_cue_number: hot,
                    is_loop: end.is_some(),
                    cue_position: position,
                    cue_time: half_frame_to_time(position),
                    loop_position: end.unwrap_or(0),
                    loop_time: end.map(half_frame_to_time).unwrap_or(0),
                    comment,
                    color_id,
                    embedded_color,
                    rekordbox_color,
                });
            }
            offset += size;
        }
        CueList::sorted(entries)
    }

    /// Parse the older (nexus) cue list returned by a CUE_LIST response.
    pub fn from_nexus_message(m: &Message) -> CueList {
        let bytes = m.binary_arg(3).unwrap_or_default();
        let mut entries = Vec::new();
        for chunk in bytes.chunks_exact(36) {
            if chunk[0] != 0 {
                let is_loop = chunk[0] == 2;
                let hot = chunk[1];
                let position = bytes_to_number_le(chunk, 12, 4) as i64;
                let end = bytes_to_number_le(chunk, 16, 4) as i64;
                entries.push(CueEntry {
                    hot_cue_number: hot,
                    is_loop,
                    cue_position: position,
                    cue_time: half_frame_to_time(position),
                    loop_position: if is_loop { end } else { 0 },
                    loop_time: if is_loop { half_frame_to_time(end) } else { 0 },
                    comment: String::new(),
                    color_id: 0,
                    embedded_color: None,
                    rekordbox_color: None,
                });
            }
        }
        CueList::sorted(entries)
    }

    /// Parse the body of an ANLZ `PCOB` (cue list) tag (the bytes after the 12-byte
    /// section header); call once per tag and merge.
    pub fn from_anlz_pcob(body: &[u8]) -> CueList {
        let mut entries = Vec::new();
        if body.len() < 12 {
            return CueList::default();
        }
        let count = bytes_to_number(body, 6, 2) as usize;
        let mut pos = 12;
        for _ in 0..count {
            if pos + 0x24 > body.len() || &body[pos..pos + 4] != b"PCPT" {
                break;
            }
            let len_entry = bytes_to_number(body, pos + 8, 4) as usize;
            let hot = bytes_to_number(body, pos + 12, 4) as u8;
            let kind = body[pos + 28];
            let time = bytes_to_number(body, pos + 32, 4) as i64;
            let loop_time = bytes_to_number(body, pos + 36, 4) as i64;
            {
                let is_loop = kind == 2;
                entries.push(CueEntry {
                    hot_cue_number: hot,
                    is_loop,
                    cue_position: time_to_half_frame(time),
                    cue_time: time,
                    loop_position: if is_loop { time_to_half_frame(loop_time) } else { 0 },
                    loop_time: if is_loop { loop_time } else { 0 },
                    comment: String::new(),
                    color_id: 0,
                    embedded_color: None,
                    rekordbox_color: None,
                });
            }
            pos += len_entry.max(0x24);
        }
        CueList::sorted(entries)
    }

    /// Merge two cue lists (e.g. memory points and hot cues).
    pub fn merged(mut self, other: CueList) -> CueList {
        self.entries.extend(other.entries);
        CueList::sorted(self.entries)
    }

    /// The first cue at or after `ms`.
    pub fn next_cue_after(&self, ms: i64) -> Option<&CueEntry> {
        self.entries.iter().find(|e| e.cue_time >= ms)
    }
    /// The last cue before `ms`.
    pub fn previous_cue_before(&self, ms: i64) -> Option<&CueEntry> {
        self.entries.iter().rev().find(|e| e.cue_time < ms)
    }
}

// ---------------------------------------------------------------------------------
// Waveforms
// ---------------------------------------------------------------------------------

/// How waveform data is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum WaveformStyle {
    Blue,
    Rgb,
    ThreeBand,
}

/// A compact whole-track waveform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaveformPreview {
    pub style: WaveformStyle,
    /// For Blue: pairs of (height 0-31, intensity 0-7). For Rgb: six bytes per segment.
    #[serde(skip)]
    pub data: Vec<u8>,
}

impl WaveformPreview {
    /// Leading junk the dbserver includes before color preview data.
    pub const LEADING_DBSERVER_COLOR_JUNK_BYTES: usize = 28;

    pub fn blue(data: Vec<u8>) -> Self {
        WaveformPreview { style: WaveformStyle::Blue, data }
    }

    pub fn segment_count(&self) -> usize {
        match self.style {
            WaveformStyle::Blue => self.data.len() / 2,
            WaveformStyle::Rgb => self.data.len() / 6,
            WaveformStyle::ThreeBand => self.data.len() / 3,
        }
    }

    /// Height of a segment (0-31 for blue, 0-127ish for color).
    pub fn segment_height(&self, segment: usize) -> u8 {
        match self.style {
            WaveformStyle::Blue => self.data.get(segment * 2).copied().unwrap_or(0) & 0x1f,
            WaveformStyle::Rgb => {
                let b = segment * 6;
                let g = |i| self.data.get(b + i).copied().unwrap_or(0);
                g(5).max(g(3)).max(g(4))
            }
            WaveformStyle::ThreeBand => {
                let b = segment * 3;
                let g = |i| f32::from(self.data.get(b + i).copied().unwrap_or(0));
                (g(2) * 0.49 + g(0) * 0.32 + g(1) * 0.25).round() as u8
            }
        }
    }

    /// Color of a segment.
    pub fn segment_color(&self, segment: usize) -> (u8, u8, u8) {
        match self.style {
            WaveformStyle::Blue => {
                let intensity = self.data.get(segment * 2 + 1).copied().unwrap_or(0) & 7;
                if intensity >= 5 {
                    (116, 246, 244)
                } else {
                    (43, 89, 255)
                }
            }
            WaveformStyle::Rgb => {
                let b = segment * 6;
                let height = u32::from(self.segment_height(segment));
                if height == 0 {
                    return (0, 0, 0);
                }
                let g = |i| u32::from(self.data.get(b + i).copied().unwrap_or(0));
                ((g(3) * 255 / height) as u8, (g(4) * 255 / height) as u8, (g(5) * 255 / height) as u8)
            }
            WaveformStyle::ThreeBand => (43, 89, 255),
        }
    }

    pub fn max_height(&self) -> u8 {
        (0..self.segment_count()).map(|i| self.segment_height(i)).max().unwrap_or(0)
    }
}

/// A scrolling waveform with 150 segments per second of audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaveformDetail {
    pub style: WaveformStyle,
    #[serde(skip)]
    pub data: Vec<u8>,
}

impl WaveformDetail {
    pub const LEADING_DBSERVER_JUNK_BYTES: usize = 19;

    pub fn frame_count(&self) -> usize {
        match self.style {
            WaveformStyle::Blue => self.data.len(),
            WaveformStyle::Rgb => self.data.len() / 2,
            WaveformStyle::ThreeBand => self.data.len() / 3,
        }
    }

    /// Track length implied by the waveform, in milliseconds.
    pub fn total_time(&self) -> i64 {
        half_frame_to_time(self.frame_count() as i64)
    }

    /// Height (0-31) of the waveform at a half-frame.
    pub fn segment_height(&self, segment: usize) -> u8 {
        match self.style {
            WaveformStyle::Blue => self.data.get(segment).copied().unwrap_or(0) & 0x1f,
            WaveformStyle::Rgb => {
                if segment * 2 + 1 >= self.data.len() {
                    return 0;
                }
                let v = bytes_to_number(&self.data, segment * 2, 2) as u16;
                ((v >> 2) & 0x1f) as u8
            }
            WaveformStyle::ThreeBand => {
                let b = segment * 3;
                self.data.get(b..b + 3).map(|s| s.iter().copied().max().unwrap_or(0) / 8).unwrap_or(0)
            }
        }
    }

    /// Color of the waveform at a half-frame.
    pub fn segment_color(&self, segment: usize) -> (u8, u8, u8) {
        match self.style {
            WaveformStyle::Blue => {
                let intensity = self.data.get(segment).copied().unwrap_or(0) >> 5;
                // beat-link uses a palette from dark to light blue.
                let level = 100 + u32::from(intensity) * 20;
                ((level / 3) as u8, (level / 2) as u8, level.min(255) as u8)
            }
            WaveformStyle::Rgb => {
                if segment * 2 + 1 >= self.data.len() {
                    return (0, 0, 0);
                }
                let v = bytes_to_number(&self.data, segment * 2, 2) as u16;
                let red = ((v >> 13) & 7) as u8;
                let green = ((v >> 10) & 7) as u8;
                let blue = ((v >> 7) & 7) as u8;
                (red * 36, green * 36, blue * 36)
            }
            WaveformStyle::ThreeBand => (43, 89, 255),
        }
    }
}

// ---------------------------------------------------------------------------------
// ANLZ tags and song structure
// ---------------------------------------------------------------------------------

/// A tagged section of a rekordbox analysis file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnlzSection {
    pub fourcc: String,
    pub len_header: u32,
    /// The bytes after the 12-byte common header.
    pub body: Vec<u8>,
}

impl AnlzSection {
    /// Parse a tagged section starting at the beginning of `bytes`.
    pub fn parse(bytes: &[u8]) -> Option<(AnlzSection, usize)> {
        if bytes.len() < 12 {
            return None;
        }
        let fourcc = String::from_utf8_lossy(&bytes[0..4]).into_owned();
        let len_header = bytes_to_number(bytes, 4, 4) as u32;
        let len_tag = bytes_to_number(bytes, 8, 4) as usize;
        if len_tag < 12 || len_tag > bytes.len() {
            return None;
        }
        Some((AnlzSection { fourcc, len_header, body: bytes[12..len_tag].to_vec() }, len_tag))
    }

    /// Parse every tagged section of a complete ANLZ file.
    pub fn parse_file(bytes: &[u8]) -> Vec<AnlzSection> {
        let mut result = Vec::new();
        if bytes.len() < 12 || &bytes[0..4] != b"PMAI" {
            return result;
        }
        let mut pos = bytes_to_number(bytes, 4, 4) as usize;
        while pos < bytes.len() {
            match AnlzSection::parse(&bytes[pos..]) {
                Some((s, len)) => {
                    result.push(s);
                    pos += len;
                }
                None => break,
            }
        }
        result
    }

    /// The entries of a waveform tag (PWV3/4/5/6/7) whose header has len_entry_bytes,
    /// len_entries, and one unknown u4 before the data.
    pub fn waveform_entries(&self) -> Option<Vec<u8>> {
        if self.body.len() < 12 {
            return None;
        }
        let entry_bytes = bytes_to_number(&self.body, 0, 4) as usize;
        let entries = bytes_to_number(&self.body, 4, 4) as usize;
        let start = if self.fourcc == "PWV6" { 8 } else { 12 };
        let end = start + entry_bytes * entries;
        self.body.get(start..end.min(self.body.len())).map(|s| s.to_vec())
    }
}

/// The overall mood of a track's phrase analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TrackMood {
    High,
    Mid,
    Low,
    Unknown,
}

/// One phrase in a song structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Phrase {
    pub index: u16,
    /// First beat of the phrase.
    pub beat: u16,
    /// Raw phrase kind id (interpretation depends on mood).
    pub kind: u16,
    pub k1: u8,
    pub k2: u8,
    pub k3: u8,
    pub b: u8,
    pub beat2: u16,
    pub beat3: u16,
    pub beat4: u16,
    pub fill: u8,
    pub beat_fill: u16,
}

/// Phrase analysis for a track (from the PSSI tag).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SongStructure {
    pub mood: TrackMood,
    pub end_beat: u16,
    pub bank: u8,
    pub phrases: Vec<Phrase>,
}

impl SongStructure {
    /// Parse a PSSI tag body, unmasking it if necessary.
    pub fn from_pssi(body: &[u8]) -> Option<SongStructure> {
        if body.len() < 6 + 14 {
            return None;
        }
        let entry_bytes = bytes_to_number(body, 0, 4) as usize;
        let count = bytes_to_number(body, 4, 2) as usize;
        let mut rest = body[6..].to_vec();
        let raw_mood = bytes_to_number(&rest, 0, 2);
        if raw_mood > 20 {
            let c = count as u8;
            let mask: Vec<u8> = [
                0xCB, 0xE1, 0xEE, 0xFA, 0xE5, 0xEE, 0xAD, 0xEE, 0xE9, 0xD2, 0xE9, 0xEB, 0xE1, 0xE9, 0xF3, 0xE8, 0xE9, 0xF4,
                0xE1u8,
            ]
            .iter()
            .map(|b| b.wrapping_add(c))
            .collect();
            for (i, b) in rest.iter_mut().enumerate() {
                *b ^= mask[i % mask.len()];
            }
        }
        let mood = match bytes_to_number(&rest, 0, 2) {
            1 => TrackMood::High,
            2 => TrackMood::Mid,
            3 => TrackMood::Low,
            _ => TrackMood::Unknown,
        };
        let end_beat = bytes_to_number(&rest, 8, 2) as u16;
        let bank = rest[12];
        let entry_bytes = if entry_bytes == 0 { 24 } else { entry_bytes };
        let mut phrases = Vec::with_capacity(count);
        for i in 0..count {
            let base = 14 + i * entry_bytes;
            if base + 24 > rest.len() {
                break;
            }
            let e = &rest[base..base + 24];
            phrases.push(Phrase {
                index: bytes_to_number(e, 0, 2) as u16,
                beat: bytes_to_number(e, 2, 2) as u16,
                kind: bytes_to_number(e, 4, 2) as u16,
                k1: e[7],
                k2: e[9],
                b: e[11],
                beat2: bytes_to_number(e, 12, 2) as u16,
                beat3: bytes_to_number(e, 14, 2) as u16,
                beat4: bytes_to_number(e, 16, 2) as u16,
                k3: e[19],
                fill: e[21],
                beat_fill: bytes_to_number(e, 22, 2) as u16,
            });
        }
        Some(SongStructure { mood, end_beat, bank, phrases })
    }

    /// Human-readable name of a phrase, following beat-link's `Util.phraseLabel`.
    pub fn phrase_label(&self, p: &Phrase) -> &'static str {
        match self.mood {
            TrackMood::Low => match p.kind {
                1 => "Intro",
                2..=4 => "Verse 1",
                5..=7 => "Verse 2",
                8 => "Bridge",
                9 => "Chorus",
                10 => "Outro",
                _ => "Unknown Low",
            },
            TrackMood::Mid => match p.kind {
                1 => "Intro",
                2 => "Verse 1",
                3 => "Verse 2",
                4 => "Verse 3",
                5 => "Verse 4",
                6 => "Verse 5",
                7 => "Verse 6",
                8 => "Bridge",
                9 => "Chorus",
                10 => "Outro",
                _ => "Unknown Mid",
            },
            TrackMood::High => match p.kind {
                1 => {
                    if p.k1 == 1 {
                        "Intro 1"
                    } else {
                        "Intro 2"
                    }
                }
                2 => {
                    if p.k2 == 0 {
                        if p.k3 == 0 {
                            "Up 1"
                        } else {
                            "Up 2"
                        }
                    } else {
                        "Up 3"
                    }
                }
                3 => "Down",
                5 => {
                    if p.k1 == 1 {
                        "Chorus 1"
                    } else {
                        "Chorus 2"
                    }
                }
                6 => {
                    if p.k1 == 1 {
                        "Outro 1"
                    } else {
                        "Outro 2"
                    }
                }
                _ => "Unknown High",
            },
            TrackMood::Unknown => "Unknown",
        }
    }

    /// The phrase containing a beat, if any.
    pub fn phrase_at_beat(&self, beat: i64) -> Option<(usize, &Phrase)> {
        let mut found = None;
        for (i, p) in self.phrases.iter().enumerate() {
            if i64::from(p.beat) <= beat {
                found = Some((i, p));
            } else {
                break;
            }
        }
        let (i, p) = found?;
        let end = self.phrases.get(i + 1).map(|n| i64::from(n.beat)).unwrap_or(i64::from(self.end_beat));
        if beat < end {
            Some((i, p))
        } else {
            None
        }
    }

    /// The beat at which phrase `i` ends (exclusive).
    pub fn phrase_end(&self, i: usize) -> u16 {
        self.phrases.get(i + 1).map(|n| n.beat).unwrap_or(self.end_beat)
    }
}

/// Encode a four-character tag as the byte-reversed integer the dbserver expects.
pub fn fourcc_protocol_number(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut v: u32 = 0;
    for i in (0..4).rev() {
        v = v.wrapping_mul(256);
        if i < b.len() {
            v = v.wrapping_add(u32::from(b[i]));
        }
    }
    v
}

/// Compute a track signature exactly as beat-link's `SignatureFinder` does, so shows
/// created by the Java version still recognize their tracks. `color_detail` must be
/// the RGB (PWV5) waveform detail entries.
pub fn compute_track_signature(title: &str, artist: Option<&str>, duration: u32, color_detail: &[u8], grid: &BeatGrid) -> String {
    let mut h = sha1_smol::Sha1::new();
    h.update(title.as_bytes());
    h.update(&[0]);
    h.update(artist.unwrap_or("[no artist]").as_bytes());
    h.update(&[0]);
    h.update(&duration.to_be_bytes());
    h.update(color_detail);
    for beat in 1..=grid.beat_count() as i64 {
        h.update(&u32::from(grid.beat_within_bar(beat)).to_be_bytes());
        h.update(&(grid.time_within_track(beat) as u32).to_be_bytes());
    }
    h.digest().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbserver::{kind, Field};

    fn grid() -> BeatGrid {
        BeatGrid::new(vec![1, 2, 3, 4, 1, 2], vec![12000; 6], vec![100, 600, 1100, 1600, 2100, 2600])
    }

    #[test]
    fn beat_grid_lookup() {
        let g = grid();
        assert_eq!(g.find_beat_at_time(0), -1);
        assert_eq!(g.find_beat_at_time(100), 1);
        assert_eq!(g.find_beat_at_time(700), 2);
        assert_eq!(g.find_beat_at_time(99999), 6);
        assert_eq!(g.time_within_track(3), 1100);
        assert_eq!(g.time_within_track(99), 2600);
        assert_eq!(g.bar_number(5), 2);
    }

    #[test]
    fn dbserver_grid_parse() {
        let mut bytes = vec![0u8; 20];
        for (i, t) in [100u32, 600].iter().enumerate() {
            let mut e = [0u8; 16];
            e[0..2].copy_from_slice(&((i + 1) as u16).to_le_bytes());
            e[2..4].copy_from_slice(&12000u16.to_le_bytes());
            e[4..8].copy_from_slice(&t.to_le_bytes());
            bytes.extend_from_slice(&e);
        }
        let g = BeatGrid::from_dbserver(&bytes);
        assert_eq!(g.beat_count(), 2);
        assert_eq!(g.time_within_track(2), 600);
        assert_eq!(g.beat_within_bar(2), 2);
    }

    #[test]
    fn metadata_items() {
        let item = |t: u32, id: u32, label: &str| {
            Message::new(1, kind::MENU_ITEM, vec![
                Field::number(0),
                Field::number(id),
                Field::number(0),
                Field::String(label.into()),
                Field::number(0),
                Field::String(String::new()),
                Field::number(t),
                Field::number(0),
                Field::number(77),
            ])
        };
        let md = TrackMetadata::from_menu_items(&[
            item(item::TRACK_TITLE, 1, "Song"),
            item(item::ARTIST, 5, "Artist"),
            item(item::REMIXER, 6, "Remixer"),
            item(item::DURATION, 245, ""),
            item(item::TEMPO, 12800, ""),
        ]);
        assert_eq!(md.title, "Song");
        assert_eq!(md.artwork_id, 77);
        assert_eq!(md.artist_label(), "Artist");
        assert_eq!(md.remixer.as_ref().unwrap().label, "Remixer");
        assert!(md.album.is_none());
        assert_eq!(md.duration, 245);
        assert_eq!(md.starting_tempo(), 128.0);
    }

    #[test]
    fn signature_is_stable() {
        let s = compute_track_signature("Title", Some("Artist"), 300, &[1, 2, 3, 4], &grid());
        assert_eq!(s.len(), 40);
        assert_eq!(s, compute_track_signature("Title", Some("Artist"), 300, &[1, 2, 3, 4], &grid()));
        assert_ne!(s, compute_track_signature("Title", None, 300, &[1, 2, 3, 4], &grid()));
    }

    #[test]
    fn fourcc_encoding() {
        assert_eq!(fourcc_protocol_number("PSSI"), u32::from_le_bytes(*b"PSSI"));
        assert_eq!(fourcc_protocol_number("EXT"), u32::from_le_bytes([b'E', b'X', b'T', 0]));
    }

    #[test]
    fn song_structure_unmasked() {
        let mut body = Vec::new();
        body.extend_from_slice(&24u32.to_be_bytes());
        body.extend_from_slice(&2u16.to_be_bytes());
        let mut rest = vec![0u8; 14];
        rest[0..2].copy_from_slice(&1u16.to_be_bytes()); // high mood
        rest[8..10].copy_from_slice(&65u16.to_be_bytes());
        for (i, (beat, kind)) in [(1u16, 1u16), (33, 2)].iter().enumerate() {
            let mut e = [0u8; 24];
            e[0..2].copy_from_slice(&((i + 1) as u16).to_be_bytes());
            e[2..4].copy_from_slice(&beat.to_be_bytes());
            e[4..6].copy_from_slice(&kind.to_be_bytes());
            e[7] = 1;
            rest.extend_from_slice(&e);
        }
        body.extend(rest);
        let s = SongStructure::from_pssi(&body).unwrap();
        assert_eq!(s.mood, TrackMood::High);
        assert_eq!(s.phrases.len(), 2);
        assert_eq!(s.phrase_label(&s.phrases[0]), "Intro 1");
        assert_eq!(s.phrase_at_beat(40).unwrap().0, 1);
        assert!(s.phrase_at_beat(65).is_none());
    }
}
