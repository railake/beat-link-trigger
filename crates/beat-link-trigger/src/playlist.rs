//! Writes a CSV playlist of the tracks played. Port of `playlist_writer.clj`.
//!
//! A track is recorded once it stops (or another track replaces it) if it played
//! for at least the configured number of seconds, optionally only counting time
//! the channel was on the air, or only the tempo master.

use beat_link::packets::{CdjStatus, TrackSourceSlot, TrackType};
use beat_link::TrackData;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The CSV header the Java version writes.
pub const HEADER: [&str; 8] = ["Title", "Artist", "Album", "Player", "Source", "Started", "Stopped", "Play Time"];

/// Quote a CSV field if needed.
pub fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_row(fields: &[String]) -> String {
    let mut line = fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",");
    line.push_str("\r\n");
    line
}

/// Describe where a track was loaded from.
pub fn format_source(status: &CdjStatus) -> String {
    if matches!(status.track_type, TrackType::Rekordbox | TrackType::Unanalyzed | TrackType::CdDigitalAudio) {
        let slot = match status.track_source_slot {
            TrackSourceSlot::SdSlot => " SD",
            TrackSourceSlot::UsbSlot | TrackSourceSlot::Usb2Slot => " USB",
            TrackSourceSlot::CdSlot => " CD",
            TrackSourceSlot::Collection => " rekordbox",
            _ => " ?",
        };
        format!("Player {}{}", status.track_source_player, slot)
    } else {
        String::new()
    }
}

/// Format a play time in seconds as mm:ss.
pub fn format_play_time(seconds: u64) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

struct Entry {
    status: Arc<CdjStatus>,
    data: Option<Arc<TrackData>>,
    started: chrono::DateTime<chrono::Local>,
}

fn same_track(a: &CdjStatus, b: &CdjStatus) -> bool {
    a.track_type == b.track_type
        && a.track_number() == b.track_number()
        && a.track_source_player == b.track_source_player
        && a.track_source_slot == b.track_source_slot
        && a.rekordbox_id == b.rekordbox_id
}

/// Settings for a playlist writer.
#[derive(Debug, Clone)]
pub struct PlaylistOptions {
    pub min_play_seconds: u64,
    pub on_air_only: bool,
    pub master_only: bool,
}

/// Records played tracks.
pub struct PlaylistWriter {
    file: PathBuf,
    options: PlaylistOptions,
    playing: Mutex<HashMap<u8, Entry>>,
}

impl PlaylistWriter {
    /// Open (or create) a playlist file. If `append` is false and the file exists, a
    /// new file with a timestamp suffix is created, like the Java version.
    pub fn open(file: &Path, append: bool, options: PlaylistOptions) -> io::Result<PlaylistWriter> {
        let mut file = file.to_path_buf();
        if file.exists() && !append {
            let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "playlist".into());
            let stamp = chrono::Local::now().format("%Y_%m_%dT%H_%M_%S");
            file = file.with_file_name(format!("{stem}_{stamp}.csv"));
        }
        if !file.exists() {
            if let Some(parent) = file.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            fs::write(&file, csv_row(&HEADER.map(String::from)))?;
        }
        Ok(PlaylistWriter { file, options, playing: Mutex::new(HashMap::new()) })
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    fn write_if_played_enough(&self, player: u8, entry: &Entry) {
        let now = chrono::Local::now();
        let played = (now - entry.started).num_seconds().max(0) as u64;
        if played < self.options.min_play_seconds {
            return;
        }
        let md = entry.data.as_ref().and_then(|d| d.metadata.clone());
        let (title, artist, album) = match (&md, entry.status.track_type) {
            (Some(md), _) => (
                md.title.clone(),
                md.artist.as_ref().map(|a| a.label.clone()).unwrap_or_default(),
                md.album.as_ref().map(|a| a.label.clone()).unwrap_or_default(),
            ),
            (None, TrackType::Rekordbox) => ("Unknown (no metadata found)".into(), String::new(), String::new()),
            (None, TrackType::Streaming) => ("Unknown (non-rekordbox)".into(), String::new(), String::new()),
            (None, _) => ("Unknown (unknown track type)".into(), String::new(), String::new()),
        };
        let row = csv_row(&[
            title,
            artist,
            album,
            player.to_string(),
            format_source(&entry.status),
            entry.started.to_rfc2822(),
            now.to_rfc2822(),
            format_play_time(played),
        ]);
        let result = OpenOptions::new().append(true).open(&self.file).and_then(|mut f| f.write_all(row.as_bytes()));
        if let Err(e) = result {
            log::error!("Problem adding entry to playlist file {:?}: {e}", self.file);
        }
    }

    /// Process a status update (with the player's current track data).
    pub fn handle_status(&self, status: &Arc<CdjStatus>, data: Option<Arc<TrackData>>) {
        let player = status.device_number();
        let counts = status.is_playing()
            && status.is_track_loaded()
            && (!self.options.on_air_only || status.is_on_air())
            && (!self.options.master_only || status.is_tempo_master());
        let mut playing = self.playing.lock();
        let replace = match playing.get(&player) {
            Some(entry) => !same_track(&entry.status, status) || !counts,
            None => false,
        };
        if replace {
            if let Some(entry) = playing.remove(&player) {
                self.write_if_played_enough(player, &entry);
            }
        }
        if counts {
            match playing.get_mut(&player) {
                Some(entry) => {
                    if entry.data.is_none() {
                        entry.data = data;
                    }
                }
                None => {
                    playing.insert(player, Entry { status: status.clone(), data, started: chrono::Local::now() });
                }
            }
        }
    }

    /// Write out everything still playing (when closing the playlist).
    pub fn close(&self) {
        let entries: Vec<(u8, Entry)> = self.playing.lock().drain().collect();
        for (player, entry) in entries {
            self.write_if_played_enough(player, &entry);
        }
    }

    #[cfg(test)]
    fn backdate(&self, player: u8, seconds: i64) {
        if let Some(e) = self.playing.lock().get_mut(&player) {
            e.started -= chrono::Duration::seconds(seconds);
        }
    }
}

impl Drop for PlaylistWriter {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beat_link::data::{SearchableItem, TrackMetadata};
    use beat_link::packets::CdjStatusBuilder;

    fn status(id: u32, playing: bool) -> Arc<CdjStatus> {
        Arc::new(
            CdjStatusBuilder {
                device_number: 2,
                track_source_player: 3,
                track_source_slot: TrackSourceSlot::UsbSlot,
                track_type: TrackType::Rekordbox,
                rekordbox_id: id,
                playing,
                on_air: true,
                ..Default::default()
            }
            .build(),
        )
    }

    #[test]
    fn writes_played_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("set.csv");
        let w = PlaylistWriter::open(&path, false, PlaylistOptions { min_play_seconds: 10, on_air_only: true, master_only: false }).unwrap();
        let data = Arc::new(TrackData {
            metadata: Some(TrackMetadata { title: "One, Two".into(), artist: Some(SearchableItem { id: 1, label: "Band".into() }), ..Default::default() }),
            ..Default::default()
        });
        w.handle_status(&status(1, true), Some(data));
        w.backdate(2, 75);
        w.handle_status(&status(2, true), None); // new track replaces old: old written
        w.handle_status(&status(2, false), None); // stopped too soon: not written
        let text = fs::read_to_string(w.file()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Title,Artist,Album,Player,Source,Started,Stopped,Play Time");
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("\"One, Two\",Band,,2,Player 3 USB,"));
        assert!(lines[1].ends_with(",01:15"));
        // Opening again without append creates a new timestamped file.
        let w2 = PlaylistWriter::open(&path, false, PlaylistOptions { min_play_seconds: 0, on_air_only: false, master_only: false }).unwrap();
        assert_ne!(w2.file(), path.as_path());
    }
}
