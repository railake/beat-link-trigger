//! The OBS overlay web server: renders a template with the current player state,
//! and serves `params.json`, artwork and waveform images for live updating pages.
//! Port of `beat-link-trigger.overlay` (Selmer templates become MiniJinja).

use beat_link::data::{CueList, WaveformDetail, WaveformPreview};
use beat_link::packets::{CdjStatus, DeviceKind};
use beat_link::util::{pitch_to_multiplier, pitch_to_percentage, time_to_half_frame};
use beat_link::TrackData;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::{self, Cursor};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

/// The built-in template, converted from the Java version's Selmer template.
pub const DEFAULT_TEMPLATE: &str = include_str!("../assets/overlay.html");
/// The built-in stylesheet.
pub const DEFAULT_STYLES: &str = include_str!("../assets/styles.css");

/// Information about one device for the overlay.
#[derive(Clone, Default)]
pub struct DeviceInfo {
    pub number: u8,
    pub name: String,
    pub address: String,
    pub status: Option<Arc<CdjStatus>>,
    pub track: Option<Arc<TrackData>>,
    pub position_ms: Option<i64>,
}

/// Where the overlay gets its data.
pub trait OverlaySource: Send + Sync {
    fn devices(&self) -> Vec<DeviceInfo>;
    fn master(&self) -> Option<u8>;
}

/// Format a pitch the way players display it.
pub fn format_pitch(pitch: f64) -> String {
    let abs = pitch.abs();
    let formatted = if abs < 20.0 { format!("{abs:5.2}") } else { format!("{abs:5.1}") };
    let sign = if formatted == " 0.00" { " " } else if pitch < 0.0 { "-" } else { "+" };
    format!("{sign}{formatted}%")
}

/// Explode a track time into the pieces a CDJ displays.
pub fn format_time(ms: i64) -> Value {
    let ms = ms.max(0);
    let half_frames = time_to_half_frame(ms) % 150;
    let minutes = ms / 60000;
    let seconds = (ms % 60000) / 1000;
    let frames = half_frames / 2;
    let tenths = if half_frames % 2 == 0 { 0 } else { 5 };
    json!({
        "raw-milliseconds": ms,
        "minutes": minutes,
        "seconds": seconds,
        "frames": frames,
        "frame-tenths": tenths,
        "display": format!("{minutes:02}:{seconds:02}:{frames:02}.{tenths}"),
    })
}

fn track_params(data: &TrackData) -> Option<Value> {
    let md = data.metadata.as_ref()?;
    let label = |i: &Option<beat_link::data::SearchableItem>| i.as_ref().map(|x| Value::String(x.label.clone())).unwrap_or(Value::Null);
    Some(json!({
        "id": data.reference.map(|r| r.rekordbox_id),
        "slot": data.reference.map(|r| r.slot().java_name()),
        "source-player": data.reference.map(|r| r.player),
        "title": md.title,
        "album": label(&md.album),
        "artist": label(&md.artist),
        "color-name": md.color.as_ref().map(|c| beat_link::data::ColorItem::name_for_id(c.id)),
        "color": md.color.as_ref().and_then(|c| beat_link::data::ColorItem::color_for_id(c.id)).map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}")),
        "comment": md.comment,
        "added": md.date_added,
        "duration": md.duration,
        "genre": label(&md.genre),
        "key": label(&md.key),
        "label": label(&md.label),
        "original-artist": label(&md.original_artist),
        "rating": md.rating,
        "remixer": label(&md.remixer),
        "starting-tempo": md.starting_tempo(),
        "year": md.year,
    }))
}

/// Build the parameter map for one device.
pub fn describe_device(d: &DeviceInfo) -> Value {
    let mut m = Map::new();
    m.insert("number".into(), json!(d.number));
    m.insert("name".into(), json!(d.name));
    m.insert("address".into(), json!(d.address));
    let kind = match DeviceKind::for_number(d.number) {
        DeviceKind::Players => "players",
        DeviceKind::Collections => "collections",
        DeviceKind::Mixers => "mixers",
    };
    m.insert("kind".into(), json!(kind));
    if let Some(t) = d.track.as_ref().and_then(|t| track_params(t)) {
        m.insert("track".into(), t);
    }
    if let Some(s) = &d.status {
        let bpm_valid = s.has_bpm();
        let pitch = pitch_to_percentage(s.pitch);
        for (k, v) in [
            ("beat-within-bar", if s.is_beat_within_bar_meaningful() { json!(s.beat_within_bar()) } else { Value::Null }),
            ("track-bpm", if bpm_valid { json!(f64::from(s.bpm) / 100.0) } else { Value::Null }),
            ("tempo", if bpm_valid { json!(s.effective_tempo()) } else { Value::Null }),
            ("pitch", json!(pitch)),
            ("pitch-display", json!(format_pitch(pitch))),
            ("pitch-multiplier", json!(pitch_to_multiplier(s.pitch))),
            ("is-synced", json!(s.is_synced())),
            ("is-tempo-master", json!(s.is_tempo_master())),
            ("beat-number", json!(s.beat_number_or_negative())),
            ("cue-countdown", json!(s.cue_countdown())),
            ("cue-countdown-display", json!(s.format_cue_countdown())),
            ("firmware-version", json!(s.firmware_version)),
            ("track-number", json!(s.track_number())),
            ("track-source-player", json!(s.track_source_player)),
            ("is-at-end", json!(s.is_at_end())),
            ("is-bpm-only-synced", json!(s.is_bpm_only_synced())),
            ("is-busy", json!(s.is_busy())),
            ("is-cued", json!(s.is_cued())),
            ("is-looping", json!(s.is_looping())),
            ("is-on-air", json!(s.is_on_air())),
            ("is-paused", json!(s.is_paused())),
            ("is-playing", json!(s.is_playing())),
            ("is-playing-backwards", json!(s.is_playing_backwards())),
            ("is-playing-cdj-mode", json!(s.is_playing_cdj_mode())),
            ("is-playing-forwards", json!(s.is_playing_forwards())),
            ("is-playing-vinyl-mode", json!(s.is_playing_vinyl_mode())),
            ("is-searching", json!(s.is_searching())),
            ("is-track-loaded", json!(s.is_track_loaded())),
        ] {
            m.insert(k.into(), v);
        }
    }
    if let Some(played) = d.position_ms {
        m.insert("time-played".into(), format_time(played));
        let total = d.track.as_ref().and_then(|t| t.detail.as_ref().map(|w| w.total_time()).or_else(|| t.metadata.as_ref().map(|md| i64::from(md.duration) * 1000)));
        if let Some(total) = total {
            m.insert("time-remaining".into(), format_time((total - played).max(0)));
        }
    }
    Value::Object(m)
}

/// Build the full template parameters.
pub fn build_params(source: &dyn OverlaySource) -> Value {
    let mut groups: BTreeMap<&str, Map<String, Value>> = BTreeMap::new();
    groups.insert("players", Map::new());
    let devices = source.devices();
    for d in &devices {
        let v = describe_device(d);
        let kind = v["kind"].as_str().unwrap_or("players").to_string();
        let kind: &str = match kind.as_str() {
            "mixers" => "mixers",
            "collections" => "collections",
            _ => "players",
        };
        groups.entry(kind).or_default().insert(d.number.to_string(), v);
    }
    let mut root = Map::new();
    for (k, v) in groups {
        root.insert(k.into(), Value::Object(v));
    }
    if let Some(master) = source.master() {
        if let Some(d) = devices.iter().find(|d| d.number == master) {
            root.insert("master".into(), describe_device(d));
        }
    }
    Value::Object(root)
}

fn number_format(value: minijinja::Value, format: String) -> String {
    let n = i64::try_from(value.clone()).ok();
    match (n, format.as_str()) {
        (Some(n), f) if f.starts_with("%0") && f.ends_with('d') => {
            let width: usize = f[2..f.len() - 1].parse().unwrap_or(0);
            format!("{n:0width$}")
        }
        (Some(n), _) => n.to_string(),
        (None, _) => value.to_string(),
    }
}

/// Render the overlay template with parameters.
pub fn render(template: &str, params: &Value) -> Result<String, minijinja::Error> {
    let mut env = minijinja::Environment::new();
    // Selmer (used by the Java version) quietly renders missing values as empty.
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Chainable);
    env.add_filter("number_format", number_format);
    env.add_template("overlay", template)?;
    env.get_template("overlay")?.render(params)
}

// ---------------------------------------------------------------------------------
// Image rendering
// ---------------------------------------------------------------------------------

struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Canvas {
        Canvas { w, h, px: vec![0; (w * h * 4) as usize] }
    }
    fn set(&mut self, x: u32, y: u32, (r, g, b): (u8, u8, u8), a: u8) {
        if x < self.w && y < self.h {
            let i = ((y * self.w + x) * 4) as usize;
            self.px[i..i + 4].copy_from_slice(&[r, g, b, a]);
        }
    }
    fn vline(&mut self, x: u32, y0: u32, y1: u32, c: (u8, u8, u8)) {
        for y in y0.min(y1)..=y0.max(y1).min(self.h.saturating_sub(1)) {
            self.set(x, y, c, 255);
        }
    }
    fn png(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(Cursor::new(&mut out), self.w, self.h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().map_err(io::Error::other)?;
            writer.write_image_data(&self.px).map_err(io::Error::other)?;
        }
        Ok(out)
    }
}

/// Render a waveform preview (whole track) as PNG, with a playhead and cues.
pub fn render_preview(preview: &WaveformPreview, width: u32, height: u32, position_ms: Option<i64>, duration_ms: Option<i64>, cues: Option<&CueList>) -> io::Result<Vec<u8>> {
    let width = width.clamp(16, 4000);
    let height = height.clamp(8, 1000);
    let mut c = Canvas::new(width, height);
    let segments = preview.segment_count().max(1);
    let max = f32::from(preview.max_height().max(1));
    let mid = height - 1;
    for x in 0..width {
        let seg = (x as usize * segments) / width as usize;
        let h = (f32::from(preview.segment_height(seg)) / max * (height - 1) as f32) as u32;
        c.vline(x, mid, mid.saturating_sub(h), preview.segment_color(seg));
    }
    if let (Some(duration), Some(cues)) = (duration_ms, cues) {
        for cue in &cues.entries {
            if duration > 0 {
                let x = ((cue.cue_time.max(0) as f64 / duration as f64) * width as f64) as u32;
                for y in 0..height.min(4) {
                    c.set(x, y, cue.color(), 255);
                }
            }
        }
    }
    if let (Some(pos), Some(duration)) = (position_ms, duration_ms) {
        if duration > 0 {
            let x = ((pos.max(0) as f64 / duration as f64) * width as f64) as u32;
            c.vline(x.min(width - 1), 0, height - 1, (255, 0, 0));
        }
    }
    c.png()
}

/// Render a scrolling waveform detail around the playhead as PNG.
pub fn render_detail(detail: &WaveformDetail, width: u32, height: u32, scale: u32, position_ms: Option<i64>) -> io::Result<Vec<u8>> {
    let width = width.clamp(16, 4000);
    let height = height.clamp(8, 1000);
    let scale = scale.clamp(1, 256) as i64;
    let mut c = Canvas::new(width, height);
    let center = time_to_half_frame(position_ms.unwrap_or(0));
    let mid = height / 2;
    for x in 0..width {
        let seg = center + (i64::from(x) - i64::from(width / 2)) * scale;
        if seg < 0 || seg as usize >= detail.frame_count() {
            continue;
        }
        let h = u32::from(detail.segment_height(seg as usize)) * (height / 2) / 31;
        c.vline(x, mid.saturating_sub(h), (mid + h).min(height - 1), detail.segment_color(seg as usize));
    }
    c.vline(width / 2, 0, height - 1, (255, 255, 255));
    c.png()
}

// ---------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------

/// A running overlay server.
pub struct OverlayServer {
    running: Arc<AtomicBool>,
    pub port: u16,
    server: Arc<tiny_http::Server>,
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
}

fn respond(req: tiny_http::Request, status: u16, content_type: &str, body: Vec<u8>) {
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).expect("valid header");
    let resp = tiny_http::Response::from_data(body).with_status_code(status).with_header(header);
    if let Err(e) = req.respond(resp) {
        log::debug!("Overlay response failed: {e}");
    }
}

impl OverlayServer {
    /// Start serving on `port` (0 picks a free port).
    pub fn start(port: u16, template: Option<PathBuf>, public: Option<PathBuf>, source: Arc<dyn OverlaySource>) -> io::Result<OverlayServer> {
        let server = Arc::new(tiny_http::Server::http(("0.0.0.0", port)).map_err(|e| io::Error::other(e.to_string()))?);
        let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(port);
        let running = Arc::new(AtomicBool::new(true));
        let template_cache = Arc::new(Mutex::new(None::<String>));
        for i in 0..4 {
            let server = server.clone();
            let running = running.clone();
            let source = source.clone();
            let template = template.clone();
            let public = public.clone();
            let cache = template_cache.clone();
            thread::Builder::new().name(format!("overlay-http-{i}")).spawn(move || {
                while running.load(Ordering::SeqCst) {
                    let req = match server.recv_timeout(std::time::Duration::from_millis(250)) {
                        Ok(Some(r)) => r,
                        Ok(None) => continue,
                        Err(_) => break,
                    };
                    let url = req.url().to_string();
                    let path = url.split('?').next().unwrap_or("/").to_string();
                    handle(req, &url, &path, &*source, template.as_ref(), public.as_ref(), &cache);
                }
            })?;
        }
        log::info!("OBS overlay server listening on port {port}");
        Ok(OverlayServer { running, port, server })
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.server.unblock();
    }
}

impl Drop for OverlayServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle(
    req: tiny_http::Request,
    url: &str,
    path: &str,
    source: &dyn OverlaySource,
    template: Option<&PathBuf>,
    public: Option<&PathBuf>,
    _cache: &Mutex<Option<String>>,
) {
    let player = |prefix: &str| path.strip_prefix(prefix).and_then(|n| n.parse::<u8>().ok());
    if path == "/" || path == "/index.html" {
        let text = template.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_else(|| DEFAULT_TEMPLATE.to_string());
        match render(&text, &build_params(source)) {
            Ok(html) => respond(req, 200, "text/html; charset=utf-8", html.into_bytes()),
            Err(e) => {
                log::error!("Problem rendering OBS overlay: {e:#}");
                respond(req, 500, "text/plain; charset=utf-8", format!("Template error: {e:#}").into_bytes())
            }
        }
    } else if path == "/styles.css" {
        respond(req, 200, "text/css", DEFAULT_STYLES.as_bytes().to_vec());
    } else if path == "/params.json" {
        let body = serde_json::to_vec(&build_params(source)).unwrap_or_default();
        respond(req, 200, "application/json", body);
    } else if let Some(n) = player("/artwork/") {
        let art = source.devices().into_iter().find(|d| d.number == n).and_then(|d| d.track.and_then(|t| t.artwork.clone()));
        match art {
            Some(bytes) => respond(req, 200, "image/jpeg", bytes),
            None => {
                let c = Canvas::new(1, 1);
                respond(req, 200, "image/png", c.png().unwrap_or_default())
            }
        }
    } else if let Some(n) = player("/wave-preview/") {
        let device = source.devices().into_iter().find(|d| d.number == n);
        let width = query_param(url, "width").and_then(|v| v.parse().ok()).unwrap_or(408);
        let height = query_param(url, "height").and_then(|v| v.parse().ok()).unwrap_or(56);
        let image = device.as_ref().and_then(|d| {
            let t = d.track.as_ref()?;
            let duration = t.detail.as_ref().map(|w| w.total_time()).or_else(|| t.metadata.as_ref().map(|m| i64::from(m.duration) * 1000));
            render_preview(t.preview.as_ref()?, width, height, d.position_ms, duration, t.cue_list.as_ref()).ok()
        });
        respond(req, 200, "image/png", image.unwrap_or_else(|| Canvas::new(1, 1).png().unwrap_or_default()));
    } else if let Some(n) = player("/wave-detail/") {
        let device = source.devices().into_iter().find(|d| d.number == n);
        let width = query_param(url, "width").and_then(|v| v.parse().ok()).unwrap_or(500);
        let height = query_param(url, "height").and_then(|v| v.parse().ok()).unwrap_or(92);
        let scale = query_param(url, "scale").and_then(|v| v.parse().ok()).unwrap_or(4);
        let image = device.as_ref().and_then(|d| render_detail(d.track.as_ref()?.detail.as_ref()?, width, height, scale, d.position_ms).ok());
        respond(req, 200, "image/png", image.unwrap_or_else(|| Canvas::new(1, 1).png().unwrap_or_default()));
    } else if let (Some(rest), Some(root)) = (path.strip_prefix("/public/"), public) {
        let candidate = root.join(rest);
        let safe = candidate.canonicalize().ok().filter(|c| root.canonicalize().map(|r| c.starts_with(r)).unwrap_or(false));
        match safe.and_then(|p| std::fs::read(&p).ok().map(|b| (p, b))) {
            Some((p, bytes)) => {
                let ct = match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
                    "html" => "text/html",
                    "css" => "text/css",
                    "js" => "application/javascript",
                    "png" => "image/png",
                    "jpg" | "jpeg" => "image/jpeg",
                    "svg" => "image/svg+xml",
                    "woff2" => "font/woff2",
                    "ttf" => "font/ttf",
                    _ => "application/octet-stream",
                };
                respond(req, 200, ct, bytes)
            }
            None => respond(req, 404, "text/plain", b"Not found".to_vec()),
        }
    } else {
        respond(req, 404, "text/plain", b"Not found".to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beat_link::data::TrackMetadata;
    use beat_link::packets::{CdjStatusBuilder, TrackType};
    use std::io::{Read, Write};

    struct Fake;
    impl OverlaySource for Fake {
        fn devices(&self) -> Vec<DeviceInfo> {
            let status = CdjStatusBuilder { device_number: 1, playing: true, bpm: 12800, track_type: TrackType::Rekordbox, rekordbox_id: 3, beat_number: Some(5), beat_within_bar: 1, ..Default::default() }.build();
            let track = TrackData {
                metadata: Some(TrackMetadata { title: "Test Title".into(), duration: 200, tempo: 12800, ..Default::default() }),
                preview: Some(WaveformPreview::blue(vec![10, 1, 20, 6, 31, 3, 5, 0])),
                ..Default::default()
            };
            vec![DeviceInfo { number: 1, name: "CDJ-3000".into(), address: "10.0.0.2".into(), status: Some(Arc::new(status)), track: Some(Arc::new(track)), position_ms: Some(61_500) }]
        }
        fn master(&self) -> Option<u8> {
            Some(1)
        }
    }

    #[test]
    fn params_and_template() {
        let p = build_params(&Fake);
        assert_eq!(p["players"]["1"]["track"]["title"], "Test Title");
        assert_eq!(p["players"]["1"]["is-playing"], true);
        assert_eq!(p["players"]["1"]["time-played"]["display"], "01:01:37.5");
        assert_eq!(p["players"]["1"]["time-remaining"]["minutes"], 2);
        assert_eq!(p["master"]["number"], 1);
        let html = render(DEFAULT_TEMPLATE, &p).unwrap();
        assert!(html.contains("<span id=\"title-1\">Test Title</span>"));
        assert!(html.contains("time-played-minutes-1\" class=\"time\">01</span>"));
    }

    #[test]
    fn pitch_display() {
        assert_eq!(format_pitch(0.0), "  0.00%");
        assert_eq!(format_pitch(-3.5), "- 3.50%");
        assert_eq!(format_pitch(25.0), "+ 25.0%");
    }

    #[test]
    fn serves_http() {
        let server = OverlayServer::start(0, None, None, Arc::new(Fake)).unwrap();
        let get = |path: &str| -> (String, Vec<u8>) {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
            write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).unwrap();
            let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            (String::from_utf8_lossy(&buf[..split]).into_owned(), buf[split + 4..].to_vec())
        };
        let (head, body) = get("/params.json");
        assert!(head.contains("200"));
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["players"]["1"]["track"]["title"], "Test Title");
        let (head, body) = get("/wave-preview/1?width=100&height=20");
        assert!(head.contains("image/png"));
        assert_eq!(&body[1..4], b"PNG");
        let (head, _) = get("/");
        assert!(head.contains("text/html"));
        let (head, _) = get("/nope");
        assert!(head.contains("404"));
        server.stop();
    }
}
