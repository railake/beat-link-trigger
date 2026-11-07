//! A musical timeline: the Rust equivalent of the `electro` library's `Metronome` and
//! `Snapshot` classes, which beat-link uses to act as a tempo master and which Beat
//! Link Trigger uses to generate MIDI clock.
//!
//! Times are milliseconds on the monotonic clock (see [`crate::clock`]), kept as `f64`
//! so tempo changes do not accumulate rounding error.

use crate::clock::monotonic_nanos;
use parking_lot::Mutex;

/// Current time in (fractional) milliseconds on the monotonic clock.
pub fn now_ms() -> f64 {
    monotonic_nanos() as f64 / 1_000_000.0
}

/// How many milliseconds a number of beats takes at a tempo.
pub fn beats_to_milliseconds(beats: f64, tempo: f64) -> f64 {
    beats * 60_000.0 / tempo
}

/// Normalize a phase difference so it falls in the range [-0.5, 0.5], i.e. the
/// smallest adjustment that reaches the same phase.
pub fn find_closest_delta(delta: f64) -> f64 {
    let mut d = delta - delta.floor();
    if d > 0.5 {
        d -= 1.0;
    }
    d
}

#[derive(Debug, Clone, Copy)]
struct Timeline {
    start: f64,
    tempo: f64,
    beats_per_bar: u32,
    bars_per_phrase: u32,
}

/// A metronome keeps track of a musical timeline: a start time and a tempo.
#[derive(Debug)]
pub struct Metronome {
    timeline: Mutex<Timeline>,
}

impl Default for Metronome {
    fn default() -> Self {
        Self::new()
    }
}

impl Metronome {
    /// Create a metronome at 120 BPM in 4/4 time, starting now.
    pub fn new() -> Self {
        Self {
            timeline: Mutex::new(Timeline { start: now_ms(), tempo: 120.0, beats_per_bar: 4, bars_per_phrase: 8 }),
        }
    }

    /// The current tempo in beats per minute.
    pub fn tempo(&self) -> f64 {
        self.timeline.lock().tempo
    }

    /// When beat one began.
    pub fn start_time(&self) -> f64 {
        self.timeline.lock().start
    }

    /// Beats per bar (normally 4).
    pub fn beats_per_bar(&self) -> u32 {
        self.timeline.lock().beats_per_bar
    }

    /// Change the tempo, keeping the current position in the timeline (in beats) constant.
    pub fn set_tempo(&self, tempo: f64) {
        if tempo <= 0.0 || !tempo.is_finite() {
            return;
        }
        let mut t = self.timeline.lock();
        let now = now_ms();
        let beats = (now - t.start) / beats_to_milliseconds(1.0, t.tempo);
        t.tempo = tempo;
        t.start = now - beats * beats_to_milliseconds(1.0, tempo);
    }

    /// Shift the start of the timeline by a number of milliseconds.
    pub fn adjust_start(&self, ms: f64) {
        self.timeline.lock().start += ms;
    }

    /// Set the start time of the timeline directly.
    pub fn set_start_time(&self, start: f64) {
        self.timeline.lock().start = start;
    }

    /// Move the timeline so that `beat` is happening right now, preserving the beat phase.
    pub fn jump_to_beat(&self, beat: i64) {
        let now = now_ms();
        let snapshot = self.snapshot_at(now);
        let phase = snapshot.beat_phase();
        let mut t = self.timeline.lock();
        t.start = now - ((beat - 1) as f64 + phase) * beats_to_milliseconds(1.0, t.tempo);
    }

    /// Shift the timeline (by the smallest amount) so the current beat phase becomes `phase`.
    pub fn set_beat_phase(&self, phase: f64) {
        let snapshot = self.snapshot();
        let delta = find_closest_delta(phase - snapshot.beat_phase());
        let interval = snapshot.beat_interval();
        self.adjust_start(-delta * interval);
    }

    /// Shift the timeline (by the smallest amount) so the current bar phase becomes `phase`.
    pub fn set_bar_phase(&self, phase: f64) {
        let snapshot = self.snapshot();
        let delta = find_closest_delta(phase - snapshot.bar_phase());
        let interval = snapshot.bar_interval();
        self.adjust_start(-delta * interval);
    }

    /// Capture the state of the timeline right now.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_at(now_ms())
    }

    /// Capture the state of the timeline at a particular instant.
    pub fn snapshot_at(&self, instant: f64) -> Snapshot {
        let t = *self.timeline.lock();
        Snapshot {
            instant,
            start: t.start,
            tempo: t.tempo,
            beats_per_bar: t.beats_per_bar,
            bars_per_phrase: t.bars_per_phrase,
        }
    }

    /// When the specified beat occurs on the current timeline.
    pub fn time_of_beat(&self, beat: i64) -> f64 {
        self.snapshot().time_of_beat(beat)
    }
}

/// An immutable view of a metronome timeline at a particular instant.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    instant: f64,
    start: f64,
    tempo: f64,
    beats_per_bar: u32,
    bars_per_phrase: u32,
}

impl Snapshot {
    /// Build a snapshot from raw values.
    pub fn new(instant: f64, start: f64, tempo: f64) -> Self {
        Snapshot { instant, start, tempo, beats_per_bar: 4, bars_per_phrase: 8 }
    }

    /// The instant this snapshot describes.
    pub fn instant(&self) -> f64 {
        self.instant
    }
    /// The tempo of the timeline.
    pub fn tempo(&self) -> f64 {
        self.tempo
    }
    /// When beat one began.
    pub fn start_time(&self) -> f64 {
        self.start
    }
    /// Milliseconds per beat.
    pub fn beat_interval(&self) -> f64 {
        beats_to_milliseconds(1.0, self.tempo)
    }
    /// Milliseconds per bar.
    pub fn bar_interval(&self) -> f64 {
        beats_to_milliseconds(f64::from(self.beats_per_bar), self.tempo)
    }
    /// Milliseconds per phrase.
    pub fn phrase_interval(&self) -> f64 {
        self.bar_interval() * f64::from(self.bars_per_phrase)
    }
    fn beats_elapsed(&self) -> f64 {
        (self.instant - self.start) / self.beat_interval()
    }
    /// The beat number (starting at 1) at this instant.
    pub fn beat(&self) -> i64 {
        self.beats_elapsed().floor() as i64 + 1
    }
    /// How far into the current beat we are (0.0 up to but excluding 1.0).
    pub fn beat_phase(&self) -> f64 {
        let b = self.beats_elapsed();
        b - b.floor()
    }
    /// The bar number (starting at 1).
    pub fn bar(&self) -> i64 {
        (self.beats_elapsed() / f64::from(self.beats_per_bar)).floor() as i64 + 1
    }
    /// How far into the current bar we are.
    pub fn bar_phase(&self) -> f64 {
        let b = self.beats_elapsed() / f64::from(self.beats_per_bar);
        b - b.floor()
    }
    /// The phrase number (starting at 1).
    pub fn phrase(&self) -> i64 {
        (self.beats_elapsed() / f64::from(self.beats_per_bar * self.bars_per_phrase)).floor() as i64 + 1
    }
    /// How far into the current phrase we are.
    pub fn phrase_phase(&self) -> f64 {
        let b = self.beats_elapsed() / f64::from(self.beats_per_bar * self.bars_per_phrase);
        b - b.floor()
    }
    /// Which beat within the bar (1 through beats-per-bar).
    pub fn beat_within_bar(&self) -> u32 {
        let beat = self.beat() - 1;
        (beat.rem_euclid(i64::from(self.beats_per_bar)) + 1) as u32
    }
    /// Which bar within the phrase (1 through bars-per-phrase).
    pub fn bar_within_phrase(&self) -> u32 {
        let bar = self.bar() - 1;
        (bar.rem_euclid(i64::from(self.bars_per_phrase)) + 1) as u32
    }
    /// When a particular beat occurs on this timeline.
    pub fn time_of_beat(&self, beat: i64) -> f64 {
        self.start + (beat - 1) as f64 * self.beat_interval()
    }
    /// Distance in milliseconds from the closest beat: positive when just after a beat,
    /// negative when approaching the next beat.
    pub fn distance_from_beat(&self) -> f64 {
        let phase = self.beat_phase();
        if phase <= 0.5 {
            phase * self.beat_interval()
        } else {
            (phase - 1.0) * self.beat_interval()
        }
    }
    /// Format as `bar.beat` like electro's `getMarker`.
    pub fn marker(&self) -> String {
        format!("{}.{}.{}", self.phrase(), self.bar_within_phrase(), self.beat_within_bar())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_math() {
        let s = Snapshot::new(1000.0 + 500.0 * 4.5, 1000.0, 120.0);
        assert_eq!(s.beat(), 5);
        assert!((s.beat_phase() - 0.5).abs() < 1e-9);
        assert_eq!(s.beat_within_bar(), 1);
        assert_eq!(s.bar(), 2);
        assert_eq!(s.time_of_beat(5), 3000.0);
        assert!((s.distance_from_beat() - 250.0).abs() < 1e-9);
    }

    #[test]
    fn tempo_change_preserves_position() {
        let m = Metronome::new();
        m.jump_to_beat(17);
        let before = m.snapshot();
        m.set_tempo(140.0);
        let after = m.snapshot();
        assert_eq!(before.beat(), after.beat());
        assert_eq!(after.tempo(), 140.0);
    }

    #[test]
    fn closest_delta() {
        assert!((find_closest_delta(0.75) + 0.25).abs() < 1e-9);
        assert!((find_closest_delta(-0.75) - 0.25).abs() < 1e-9);
        assert!((find_closest_delta(0.25) - 0.25).abs() < 1e-9);
    }
}
