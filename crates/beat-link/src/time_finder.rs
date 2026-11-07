//! Interpolates the playback position of each player within its track, using beat
//! grids, status packets, beat packets and (on CDJ-3000s) precise position packets.
//! Port of `org.deepsymmetry.beatlink.data.TimeFinder`.

use crate::beat_finder::BeatFinder;
use crate::clock::monotonic_nanos;
use crate::data::BeatGrid;
use crate::listeners::ListenerSet;
use crate::metadata_finder::MetadataFinder;
use crate::packets::{Beat, CdjStatus, DeviceUpdate, PrecisePosition};
use crate::util::pitch_to_multiplier;
use crate::virtual_cdj::VirtualCdj;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};

/// What we know about where a player is in its track.
#[derive(Debug, Clone)]
pub struct TrackPosition {
    /// Monotonic nanoseconds when this was determined.
    pub timestamp: u64,
    /// Position in the track, in milliseconds.
    pub milliseconds: i64,
    pub beat_number: i64,
    pub definitive: bool,
    pub playing: bool,
    pub pitch: f64,
    pub reverse: bool,
    pub beat_grid: Arc<BeatGrid>,
    pub precise: bool,
    pub from_beat: bool,
}

impl TrackPosition {
    /// Where the player should be now, extrapolating from this update.
    pub fn interpolate(&self, now: u64) -> i64 {
        if !self.playing {
            return self.milliseconds;
        }
        let elapsed = (now.saturating_sub(self.timestamp) / 1_000_000) as f64;
        let moved = (self.pitch * elapsed).round() as i64;
        if self.reverse {
            self.milliseconds - moved
        } else {
            self.milliseconds + moved
        }
    }
}

/// Reports a change in a player's position (including significant jumps).
#[derive(Debug, Clone)]
pub struct PositionUpdate {
    pub player: u8,
    pub position: Option<TrackPosition>,
    pub beat: Option<Arc<Beat>>,
}

/// Tracks player positions.
pub struct TimeFinder {
    me: Weak<TimeFinder>,
    metadata_finder: Arc<MetadataFinder>,
    running: AtomicBool,
    positions: RwLock<HashMap<u8, TrackPosition>>,
    /// Allowed drift (ms) before an update is considered a jump worth reporting.
    pub slack: AtomicI64,
    pub position_update: ListenerSet<PositionUpdate>,
}

impl TimeFinder {
    pub fn new(virtual_cdj: &Arc<VirtualCdj>, beat_finder: &Arc<BeatFinder>, metadata_finder: Arc<MetadataFinder>) -> Arc<Self> {
        let finder = Arc::new_cyclic(|me| TimeFinder {
            me: me.clone(),
            metadata_finder,
            running: AtomicBool::new(false),
            positions: RwLock::new(HashMap::new()),
            slack: AtomicI64::new(50),
            position_update: ListenerSet::new(),
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
        beat_finder.beat_internal.add(move |b: &Arc<Beat>| {
            if let Some(f) = w.upgrade() {
                if f.is_running() {
                    f.handle_beat(b);
                }
            }
        });
        let w = finder.me.clone();
        beat_finder.precise_position.add(move |p: &Arc<PrecisePosition>| {
            if let Some(f) = w.upgrade() {
                if f.is_running() {
                    f.handle_precise(p);
                }
            }
        });
        let w = finder.me.clone();
        finder.metadata_finder.track_update.add(move |u| {
            if let Some(f) = w.upgrade() {
                if u.data.as_ref().and_then(|d| d.beat_grid.clone()).is_none() {
                    f.positions.write().remove(&u.player);
                }
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
        self.positions.write().clear();
    }

    /// The interpolated playback position for a player, in milliseconds.
    pub fn time_for(&self, player: u8) -> Option<i64> {
        self.positions.read().get(&player).map(|p| p.interpolate(monotonic_nanos()))
    }

    /// The latest position update for a player.
    pub fn latest_position_for(&self, player: u8) -> Option<TrackPosition> {
        self.positions.read().get(&player).cloned()
    }

    fn grid_for(&self, player: u8) -> Option<Arc<BeatGrid>> {
        self.metadata_finder.latest_track_data(player).and_then(|d| d.beat_grid.clone())
    }

    fn time_of_beat(grid: &BeatGrid, beat: i64) -> i64 {
        if beat <= grid.beat_count() as i64 {
            return grid.time_within_track(beat) as i64;
        }
        if grid.beat_count() < 2 {
            return grid.time_within_track(1) as i64;
        }
        let last = grid.beat_count() as i64;
        let interval = grid.time_within_track(last) as i64 - grid.time_within_track(last - 1) as i64;
        grid.time_within_track(last) as i64 + interval * (beat - last)
    }

    fn adjacent_cue(&self, status: &CdjStatus, grid: &BeatGrid) -> Option<i64> {
        let data = self.metadata_finder.latest_track_data(status.device_number())?;
        let cues = data.cue_list.as_ref()?;
        let new_beat = status.beat_number_or_negative();
        for entry in &cues.entries {
            let entry_beat = grid.find_beat_at_time(entry.cue_time.max(0) as u64);
            if (new_beat - entry_beat).abs() < 2 {
                return Some(entry.cue_time);
            }
            if entry_beat > new_beat {
                break;
            }
        }
        None
    }

    fn interpolate_from_update(&self, last: &TrackPosition, status: &CdjStatus, grid: &BeatGrid) -> i64 {
        let beat_number = status.beat_number_or_negative();
        let no_longer_playing = !status.is_playing();
        if last.playing && no_longer_playing {
            if let Some(t) = self.adjacent_cue(status, grid) {
                return t;
            }
        }
        if !last.playing {
            if last.beat_number == beat_number && no_longer_playing {
                return last.milliseconds;
            } else if no_longer_playing {
                if beat_number < 0 {
                    return -1;
                }
                return Self::time_of_beat(grid, beat_number);
            }
        }
        let elapsed = (status.timestamp().saturating_sub(last.timestamp) / 1_000_000) as f64;
        let moved = (last.pitch * elapsed).round() as i64;
        let interpolated = if last.reverse { (last.milliseconds - moved).max(0) } else { last.milliseconds + moved };
        if (grid.find_beat_at_time(interpolated.max(0) as u64) - beat_number).abs() < 2 {
            return interpolated;
        }
        if status.is_playing_forwards() {
            Self::time_of_beat(grid, beat_number)
        } else {
            grid.time_within_track((beat_number + 1).min(grid.beat_count() as i64)) as i64
        }
    }

    fn publish(&self, player: u8, new: Option<TrackPosition>, beat: Option<Arc<Beat>>) {
        let old = match &new {
            Some(p) => self.positions.write().insert(player, p.clone()),
            None => self.positions.write().remove(&player),
        };
        let significant = match (&old, &new) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(o), Some(n)) => {
                let now = monotonic_nanos();
                let skew = (o.interpolate(now) - n.interpolate(now)).abs();
                let tolerance = if o.playing { self.slack.load(Ordering::Relaxed) } else { 0 };
                o.playing != n.playing || o.reverse != n.reverse || skew > tolerance || beat.is_some()
            }
        };
        if significant {
            self.position_update.deliver(&PositionUpdate { player, position: new, beat });
        }
    }

    fn handle_status(&self, status: &CdjStatus) {
        let player = status.device_number();
        let last = self.positions.read().get(&player).cloned();
        if last.as_ref().map(|l| l.precise).unwrap_or(false) {
            return;
        }
        let Some(grid) = self.grid_for(player) else {
            if last.is_some() {
                self.publish(player, None, None);
            }
            return;
        };
        let beat_number = status.beat_number_or_negative();
        if beat_number < 0 {
            self.publish(player, None, None);
            return;
        }
        let new = match &last {
            Some(l) if Arc::ptr_eq(&l.beat_grid, &grid) => {
                if l.timestamp >= status.timestamp() {
                    return;
                }
                let t = self.interpolate_from_update(l, status, &grid);
                let reverse = status.is_playing_backwards();
                TrackPosition {
                    timestamp: status.timestamp(),
                    milliseconds: t,
                    beat_number,
                    definitive: false,
                    playing: status.is_playing() && (!reverse || t > 0),
                    pitch: pitch_to_multiplier(status.pitch),
                    reverse,
                    beat_grid: grid,
                    precise: false,
                    from_beat: false,
                }
            }
            _ => {
                let t = self.adjacent_cue(status, &grid).unwrap_or_else(|| Self::time_of_beat(&grid, beat_number));
                TrackPosition {
                    timestamp: status.timestamp(),
                    milliseconds: t,
                    beat_number,
                    definitive: false,
                    playing: status.is_playing(),
                    pitch: pitch_to_multiplier(status.pitch),
                    reverse: status.is_playing_backwards(),
                    beat_grid: grid,
                    precise: false,
                    from_beat: false,
                }
            }
        };
        self.publish(player, Some(new), None);
    }

    fn handle_beat(&self, beat: &Arc<Beat>) {
        let player = beat.device_number();
        if player >= 16 {
            return;
        }
        let last = self.positions.read().get(&player).cloned();
        let Some(grid) = self.grid_for(player) else {
            if last.as_ref().map(|l| !l.precise).unwrap_or(false) {
                self.publish(player, None, Some(beat.clone()));
            }
            return;
        };
        let Some(last) = last.filter(|l| Arc::ptr_eq(&l.beat_grid, &grid)) else { return };
        let distance_into_beat = last.milliseconds - grid.time_within_track(last.beat_number) as i64;
        let far_enough = if beat.bpm > 0 { 6_000_000 / i64::from(beat.bpm) / 5 } else { 0 };
        let beat_number = if distance_into_beat >= far_enough {
            (last.beat_number + 1).min(grid.beat_count() as i64)
        } else {
            last.beat_number
        };
        let new = TrackPosition {
            timestamp: beat.timestamp(),
            milliseconds: Self::time_of_beat(&grid, beat_number),
            beat_number,
            definitive: true,
            playing: true,
            pitch: pitch_to_multiplier(beat.pitch),
            reverse: false,
            beat_grid: grid,
            precise: last.precise,
            from_beat: true,
        };
        self.publish(player, Some(new), Some(beat.clone()));
    }

    fn handle_precise(&self, p: &Arc<PrecisePosition>) {
        let player = p.device_number();
        let Some(grid) = self.grid_for(player) else { return };
        let ms = i64::from(p.playback_position);
        let last = self.positions.read().get(&player).cloned();
        let playing = last.as_ref().map(|l| l.playing).unwrap_or(true) || p.pitch != crate::util::NEUTRAL_PITCH;
        let new = TrackPosition {
            timestamp: p.header.timestamp,
            milliseconds: ms,
            beat_number: grid.find_beat_at_time(ms.max(0) as u64),
            definitive: true,
            playing,
            pitch: pitch_to_multiplier(p.pitch),
            reverse: last.as_ref().map(|l| l.reverse).unwrap_or(false),
            beat_grid: grid,
            precise: true,
            from_beat: false,
        };
        self.publish(player, Some(new), None);
    }

    /// Clear the precise flag so status packets drive the position again (used when
    /// a CDJ-3000 stops sending precise positions).
    pub fn forget(&self, player: u8) {
        self.positions.write().remove(&player);
    }

    #[doc(hidden)]
    pub fn me(&self) -> Option<Arc<TimeFinder>> {
        self.me.upgrade()
    }
}
