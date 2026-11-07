# Beat Link Trigger — Rust rewrite

This branch contains a ground-up rewrite of Beat Link Trigger (BLT) in Rust,
alongside the original Clojure sources (which remain the reference until the
rewrite reaches full parity). It replaces both BLT itself and the Java
[beat-link](https://github.com/Deep-Symmetry/beat-link) library it depends on, so
no JVM is needed.

```
Cargo.toml                       workspace
crates/beat-link/                Pro DJ Link protocol (port of the Java beat-link library)
crates/beat-link-trigger/        the application: triggers, expressions, shows, GUI, CLI
```

## Building and running

Requires Rust 1.80 or newer. On Linux the MIDI and GUI features need the ALSA
development headers at build time (`libasound2-dev`) and an X11 or Wayland
desktop at run time.

```bash
cargo build --release
```

```bash
./target/release/beat-link-trigger
```

Useful options (the first four match the Java version):

| Option | Meaning |
| --- | --- |
| `-o`, `--offline` | Start offline. |
| `-s`, `--show FILE` | Open a show at startup (repeatable). |
| `-c`, `--config FILE` | Use a configuration file (`.bltj`, or a Java-era `.blt`). |
| `--reset FILE` | Write the saved configuration to a file and forget it. |
| `--headless` | Run without a GUI (servers, Raspberry Pi). |
| `--simulate N` | Add N simulated players playing the bundled sample tracks. |
| `--convert FILE` | Convert a Java `.blt` trigger file or `.bls` show to the new JSON formats. |
| `--check` | Compile every expression and report problems. |
| `--report FILE` | Write the expression report as HTML. |

Build without the GUI or MIDI support with
`cargo build --release --no-default-features` (headless only).

Run the tests with `cargo test --workspace`.

## What changed for users

* **Expressions are written in [Rhai](https://rhai.rs), not Clojure.** Rhai is a
  small embedded scripting language; it cannot run Clojure. The same convenience
  values are available with identifier-style names (`beat-number` → `beat_number`,
  `playing?` → `playing`, `beat?` → `is_beat`, `trigger-active?` → `trigger_active`).
  `locals` and `globals` are shared maps (`locals["count"] = 1`, `globals.increment("n")`).
  Functions cover MIDI, OSC, UDP/TCP, player commands (sync, master, fader start,
  on-air) and Ableton Link tempo; the expression editor lists them all.
* **Files are JSON.** Triggers save to `.bltj`, shows to `.blsj`. Java-era `.blt`
  and `.bls` files open directly (or convert with `--convert`): every setting,
  track, cue and MIDI mapping is imported, and each Clojure expression is kept,
  shown read-only in the editor, and turned into a commented stub so you can port
  it. The expression report flags every expression still needing a port.
* **Show tracks are still recognized by signature.** The signature algorithm is a
  byte-exact port, so imported shows match the same tracks (verified against the
  signatures stored with the bundled sample tracks).
* **OBS overlay templates use MiniJinja** instead of Selmer. `params.json` keeps
  the same keys, so overlay pages that fetch it keep working. In templates,
  hyphenated keys need brackets (`player["time-played"]`) and the number format
  filter is written `|number_format("%02d")`.

## Parity ledger

Status: ✅ ported, ◐ partially ported, ✗ not ported.

### Protocol (`crates/beat-link`, replacing the Java beat-link library)

| Java class(es) | Rust | Status |
| --- | --- | --- |
| `Util`, packet types and header validation | `util.rs` | ✅ |
| `DeviceAnnouncement`, `CdjStatus`, `MixerStatus`, `Beat`, `PrecisePosition` | `packets.rs` | ✅ |
| `DeviceFinder` | `device_finder.rs` | ✅ (Opus Quad announcement fan-out included) |
| `BeatFinder` (beats, precise position, on-air, sync, master handoff, fader start) | `beat_finder.rs` | ✅ |
| `VirtualCdj`: device number claiming (incl. mixer assignment), keep-alives, status, defense | `virtual_cdj.rs` | ✅ |
| `VirtualCdj`: sending status, beats, tempo master handoff, sync, `BeatSender` | `virtual_cdj.rs` | ✅ |
| `VirtualCdj` commands: sync, appoint master, fader start, on-air, load track, load settings, media query | `virtual_cdj.rs` | ✅ |
| `PlayerSettings` | `player_settings.rs` | ✅ |
| `dbserver` (`Field`s, `Message`, `Client`, port discovery) | `dbserver.rs` | ✅ |
| `MetadataFinder`, `BeatGridFinder`, `WaveformFinder`, `ArtFinder`, `AnalysisTagFinder`, `SignatureFinder` | `metadata_finder.rs`, `data.rs` | ✅ merged into one finder producing `TrackData` |
| `TimeFinder` | `time_finder.rs` | ✅ |
| electro `Metronome`/`Snapshot` | `metronome.rs` | ✅ |
| `CueList`, `BeatGrid`, `WaveformPreview`/`Detail`, `TrackMetadata`, ANLZ song structure | `data.rs` | ✅ (three-band waveform rendering simplified) |
| `MenuLoader` (browsing player menus) | — | ✗ |
| `CrateDigger` (NFS + rekordbox `export.pdb`), `OpusProvider`, `VirtualRekordbox` (Opus Quad / XDJ-AZ DLP) | — | ✗ |

### Application (`crates/beat-link-trigger`)

| Clojure namespace | Rust | Status |
| --- | --- | --- |
| `BeatLinkTrigger`, `core` (startup, CLI) | `main.rs`, `app.rs` | ✅ |
| `triggers` (matching, enabled filters, activation, MIDI clock, Link, auto-reconnect) | `triggers.rs`, `app.rs`, `gui.rs` | ✅ |
| `expressions`, `expressions.triggers`, `expressions.show` | `expressions.rs` | ◐ Rhai replaces Clojure; most helpers ported |
| `editors` | `gui.rs` editor windows | ◐ plain code editor, no syntax highlighting |
| `carabiner` + beat-carabiner | `carabiner.rs`, `gui.rs` | ◐ all sync modes; the "tempo-if-close" follow mode is not ported (jumps only) |
| `players` (Player Status) | `gui.rs` | ◐ no artwork image or scrolling detail waveform in the GUI (the overlay serves both) |
| `show`, `show-cues`, `show-util` | `shows.rs`, `gui.rs` | ◐ runtime and an editor for tracks and cues; no graphical cue painting on the waveform |
| `show-phrases` (phrase triggers) | — | ✗ (song structure data is parsed and available) |
| `overlay` | `overlay.rs` | ✅ (custom fonts route not ported) |
| `playlist-writer` | `playlist.rs` | ✅ |
| `simulator` | `simulator.rs` | ✅ |
| `track-loader` | `gui.rs` Load Track | ◐ by rekordbox ID; no menu browsing |
| `settings-loader` | `gui.rs` Load Settings | ✅ |
| `expression-report` | `report.rs` | ✅ (no in-page edit buttons) |
| `logs` | `logs.rs` | ✅ |
| `prefs`, `settings` | `config.rs`, `legacy.rs` | ✅ JSON, plus Java `.blt`/`.bls` import |
| `menus`, `about`, `help` | `gui.rs` | ◐ the embedded user guide is not bundled |
| `nrepl` | — | ✗ not applicable without Clojure |

## How this rewrite was verified

Unit and integration tests (`cargo test --workspace`, 63 tests) cover packet
round-trips, the dbserver message format, beat grids, cue lists, song structure
unmasking, the metronome, MIDI byte output, OSC, Carabiner's protocol, EDN/`.blt`
import, `.bls` show import, trigger semantics (Any/Master/specific player
matching, On-Air/Custom/Always enabling, activation/deactivation, MIDI clock
start/clock/stop, device loss), show track and cue events, the overlay server and
templates, and the playlist CSV. Two tests are worth calling out:

* `crates/beat-link/tests/loopback.rs` runs the whole protocol stack on the
  loopback interface against a fake CDJ (keep-alives, status packets, and a fake
  dbserver over TCP): it claims a device number, receives status, recognizes the
  tempo master, fetches metadata and a beat grid, and interpolates the playback
  position.
* `simulator::tests::sample_tracks_load_and_signatures_match_java` computes the
  track signatures of the bundled sample tracks and checks them against the
  signatures the Java `SignatureFinder` stored with them.

The rewrite was also examined with [REA](https://github.com/morluto/rea)
(Reverse Engineer Anything); see [`docs/rust-rewrite/rea-evidence.md`](docs/rust-rewrite/rea-evidence.md)
for what was captured and what it shows.

Not verified here: behavior against real Pioneer/AlphaTheta hardware, real MIDI
devices, a real Carabiner/Ableton Link session, and macOS/Windows builds.
