//! Importing files written by the Java/Clojure version of Beat Link Trigger.
//!
//! Trigger files (`.blt`) are EDN. Everything except the Clojure expressions maps
//! directly onto the new configuration; the Clojure source is preserved in
//! `legacy-expressions` so it can be consulted while porting it to Rhai.

use crate::config::{ClockStart, EnabledMode, MessageKind, PlayerChoice, Settings, TriggerConfig};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

/// A parsed EDN value.
#[derive(Debug, Clone, PartialEq)]
pub enum Edn {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Char(char),
    Keyword(String),
    Symbol(String),
    List(Vec<Edn>),
    Vector(Vec<Edn>),
    Set(Vec<Edn>),
    Map(Vec<(Edn, Edn)>),
    Tagged(String, Box<Edn>),
}

impl Edn {
    /// Look up a keyword in a map (tagged values are looked through).
    pub fn get(&self, key: &str) -> Option<&Edn> {
        match self {
            Edn::Map(entries) => entries.iter().find(|(k, _)| matches!(k, Edn::Keyword(s) if s == key)).map(|(_, v)| v),
            Edn::Tagged(_, inner) => inner.get(key),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Edn::Str(s) => Some(s),
            Edn::Tagged(_, inner) => inner.as_str(),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Edn::Int(i) => Some(*i),
            Edn::Float(f) => Some(*f as i64),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Edn::Bool(b) => Some(*b),
            Edn::Nil => Some(false),
            _ => None,
        }
    }
    pub fn as_seq(&self) -> Option<&[Edn]> {
        match self {
            Edn::List(v) | Edn::Vector(v) | Edn::Set(v) => Some(v),
            _ => None,
        }
    }
    /// Map entries keyed by keyword name.
    pub fn keyword_map(&self) -> BTreeMap<String, &Edn> {
        let mut m = BTreeMap::new();
        if let Edn::Map(entries) = self {
            for (k, v) in entries {
                if let Edn::Keyword(name) = k {
                    m.insert(name.clone(), v);
                }
            }
        }
        m
    }
}

/// Errors reading EDN.
#[derive(Debug, thiserror::Error)]
#[error("EDN parse error at byte {position}: {message}")]
pub struct EdnError {
    pub position: usize,
    pub message: String,
}

struct Reader<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn err<T>(&self, message: impl Into<String>) -> Result<T, EdnError> {
        Err(EdnError { position: self.pos, message: message.into() })
    }

    fn skip_ws(&mut self) {
        while self.pos < self.s.len() {
            match self.s[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' | b',' => self.pos += 1,
                b';' => {
                    while self.pos < self.s.len() && self.s[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn is_delim(b: u8) -> bool {
        matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b',' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'"' | b';')
    }

    fn token(&mut self) -> &'a str {
        let start = self.pos;
        while self.pos < self.s.len() && !Self::is_delim(self.s[self.pos]) {
            self.pos += 1;
        }
        std::str::from_utf8(&self.s[start..self.pos]).unwrap_or("")
    }

    fn read_seq(&mut self, close: u8) -> Result<Vec<Edn>, EdnError> {
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if self.pos >= self.s.len() {
                return self.err("unterminated collection");
            }
            if self.s[self.pos] == close {
                self.pos += 1;
                return Ok(items);
            }
            if let Some(v) = self.read_value()? {
                items.push(v);
            }
        }
    }

    fn read_string(&mut self) -> Result<Edn, EdnError> {
        self.pos += 1;
        let mut out = Vec::new();
        while self.pos < self.s.len() {
            let b = self.s[self.pos];
            self.pos += 1;
            match b {
                b'"' => return Ok(Edn::Str(String::from_utf8_lossy(&out).into_owned())),
                b'\\' => {
                    let Some(&e) = self.s.get(self.pos) else { break };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'u' => {
                            let hex = std::str::from_utf8(self.s.get(self.pos..self.pos + 4).unwrap_or(b"")).unwrap_or("");
                            if let Some(c) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                            }
                            self.pos += 4;
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
        self.err("unterminated string")
    }

    /// Read one value; `None` means a discarded (`#_`) form.
    fn read_value(&mut self) -> Result<Option<Edn>, EdnError> {
        self.skip_ws();
        let Some(&b) = self.s.get(self.pos) else { return self.err("unexpected end of input") };
        let v = match b {
            b'{' => {
                self.pos += 1;
                let items = self.read_seq(b'}')?;
                if items.len() % 2 != 0 {
                    return self.err("map with odd number of forms");
                }
                let mut entries = Vec::new();
                let mut it = items.into_iter();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    entries.push((k, v));
                }
                Edn::Map(entries)
            }
            b'[' => {
                self.pos += 1;
                Edn::Vector(self.read_seq(b']')?)
            }
            b'(' => {
                self.pos += 1;
                Edn::List(self.read_seq(b')')?)
            }
            b'"' => self.read_string()?,
            b':' => {
                self.pos += 1;
                Edn::Keyword(self.token().to_string())
            }
            b'\\' => {
                self.pos += 1;
                let t = self.token();
                let c = match t {
                    "newline" => '\n',
                    "space" => ' ',
                    "tab" => '\t',
                    "return" => '\r',
                    _ => t.chars().next().unwrap_or(' '),
                };
                Edn::Char(c)
            }
            b'#' => {
                self.pos += 1;
                match self.s.get(self.pos) {
                    Some(b'{') => {
                        self.pos += 1;
                        Edn::Set(self.read_seq(b'}')?)
                    }
                    Some(b'_') => {
                        self.pos += 1;
                        self.read_value()?;
                        return Ok(None);
                    }
                    _ => {
                        let tag = self.token().to_string();
                        let inner = loop {
                            if let Some(v) = self.read_value()? {
                                break v;
                            }
                        };
                        Edn::Tagged(tag, Box::new(inner))
                    }
                }
            }
            _ => {
                let t = self.token();
                if t.is_empty() {
                    return self.err(format!("unexpected character {:?}", b as char));
                }
                match t {
                    "nil" => Edn::Nil,
                    "true" => Edn::Bool(true),
                    "false" => Edn::Bool(false),
                    _ => {
                        let first = t.as_bytes()[0];
                        let numeric = first.is_ascii_digit() || ((first == b'-' || first == b'+') && t.len() > 1 && t.as_bytes()[1].is_ascii_digit());
                        if numeric {
                            let clean = t.trim_end_matches(['N', 'M']);
                            if let Ok(i) = clean.parse::<i64>() {
                                Edn::Int(i)
                            } else if let Ok(f) = clean.parse::<f64>() {
                                Edn::Float(f)
                            } else {
                                Edn::Symbol(t.to_string())
                            }
                        } else {
                            Edn::Symbol(t.to_string())
                        }
                    }
                }
            }
        };
        Ok(Some(v))
    }
}

/// Parse every top-level EDN value in a text (e.g. a burst of Carabiner responses).
pub fn parse_edn_all(text: &str) -> Result<Vec<Edn>, EdnError> {
    let mut r = Reader { s: text.as_bytes(), pos: 0 };
    let mut out = Vec::new();
    loop {
        r.skip_ws();
        if r.pos >= r.s.len() {
            return Ok(out);
        }
        if let Some(v) = r.read_value()? {
            out.push(v);
        }
    }
}

/// Parse a single EDN value from text.
pub fn parse_edn(text: &str) -> Result<Edn, EdnError> {
    let mut r = Reader { s: text.as_bytes(), pos: 0 };
    loop {
        if let Some(v) = r.read_value()? {
            return Ok(v);
        }
    }
}

fn string_map(v: Option<&Edn>) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    if let Some(Edn::Map(entries)) = v {
        for (k, v) in entries {
            if let (Edn::Keyword(k), Some(s)) = (k, v.as_str()) {
                if !s.trim().is_empty() {
                    m.insert(k.clone(), s.to_string());
                }
            }
        }
    }
    m
}

/// A placeholder Rhai expression that documents the Clojure it replaces.
fn porting_stub(clojure: &str) -> String {
    let mut out = String::from("// Imported from the Java version of Beat Link Trigger. The original Clojure\n// expression is below; rewrite it in Rhai to restore its behavior.\n");
    for line in clojure.lines() {
        out.push_str("// ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Convert one trigger map from a `.blt` file.
pub fn import_trigger(m: &Edn) -> TriggerConfig {
    let mut t = TriggerConfig::default();
    if let Some(s) = m.get("comment").and_then(Edn::as_str) {
        t.comment = s.trim().to_string();
    }
    if let Some(n) = m.get("players").and_then(|p| p.get("number")).and_then(Edn::as_i64) {
        t.players = PlayerChoice::from(n as i8);
    }
    if let Some(name) = m.get("outputs").and_then(|o| o.get("full-name")).and_then(Edn::as_str) {
        t.output = Some(name.trim_start_matches("CoreMIDI4J - ").to_string());
    }
    if let Some(msg) = m.get("message").and_then(Edn::as_str).and_then(MessageKind::from_label) {
        t.message = msg;
    }
    if let Some(n) = m.get("note").and_then(Edn::as_i64) {
        t.note = n.clamp(1, 127) as u8;
    }
    if let Some(n) = m.get("channel").and_then(Edn::as_i64) {
        t.channel = n.clamp(1, 16) as u8;
    }
    t.enabled = match m.get("enabled") {
        Some(Edn::Bool(true)) => EnabledMode::Always,
        Some(Edn::Bool(false)) => EnabledMode::Never,
        Some(v) => v.as_str().and_then(EnabledMode::from_label).unwrap_or_default(),
        None => EnabledMode::Never,
    };
    if let Some(b) = m.get("send").and_then(Edn::as_bool) {
        t.send = b;
    }
    if let Some(s) = m.get("start").and_then(Edn::as_str) {
        t.start = if s == "Continue" { ClockStart::Continue } else { ClockStart::Start };
    }
    if let Some(b) = m.get("stop").and_then(Edn::as_bool) {
        t.stop = b;
    }
    if let Some(b) = m.get("bar").and_then(Edn::as_bool) {
        t.bar = b;
    }
    if let Some(b) = m.get("start-stop").and_then(Edn::as_bool) {
        t.start_stop = b;
    }
    let mut legacy = string_map(m.get("expressions"));
    if let Some(s) = m.get("custom-enabled").and_then(Edn::as_str) {
        legacy.entry("enabled".into()).or_insert_with(|| s.to_string());
    }
    for (kind, src) in &legacy {
        t.expressions.insert(kind.clone(), porting_stub(src));
    }
    t.legacy_expressions = legacy;
    t
}

/// Convert a parsed `.blt` (or preferences) map into settings.
pub fn import_settings(root: &Edn) -> Settings {
    let mut s = Settings { triggers: Vec::new(), ..Settings::default() };
    if let Some(triggers) = root.get("triggers").and_then(Edn::as_seq) {
        s.triggers = triggers.iter().map(import_trigger).collect();
    }
    if s.triggers.is_empty() {
        s.triggers.push(TriggerConfig::default());
    }
    let legacy = string_map(root.get("expressions"));
    for (kind, src) in &legacy {
        s.expressions.insert(kind.clone(), porting_stub(src));
    }
    s.legacy_expressions = legacy;
    if let Some(b) = root.get("send-status?").and_then(Edn::as_bool) {
        s.send_status = b;
    }
    if let Some(c) = root.get("carabiner") {
        if let Some(p) = c.get("port").and_then(Edn::as_i64) {
            s.carabiner.port = p as u16;
        }
        if let Some(l) = c.get("latency").and_then(Edn::as_i64) {
            s.carabiner.latency_ms = l;
        }
        if let Some(b) = c.get("bars").and_then(Edn::as_bool) {
            s.carabiner.bars = b;
        }
    }
    if let Some(p) = root.get("overlay").and_then(|o| o.get("port")).and_then(Edn::as_i64) {
        s.overlay.port = p as u16;
    }
    s
}

/// Read a `.blt` trigger file from the Java version.
pub fn import_blt(path: &Path) -> io::Result<Settings> {
    let text = fs::read_to_string(path)?;
    let edn = parse_edn(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if edn.get("beat-link-trigger-version").is_none() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Not a Beat Link Trigger file"));
    }
    Ok(import_settings(&edn))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{:beat-link-trigger-version "0.5.5-SNAPSHOT",
 :triggers [{:bar true, :start "Start", :channel 1, :start-stop false, :note 17, :gear false, :stop false,
             :expressions {:tracked "(when trigger-active? (set-gm-tempo effective-tempo))"},
             :comment " Set chase speed", ; a comment
             :outputs #beat_link_trigger.util.MidiChoice{:full-name "CoreMIDI4J - Bus 1"},
             :send false, :players #beat_link_trigger.util.PlayerChoice{:number 0},
             :channel-label "Channel:", :enabled "Always", :message "Custom"}
            {:enabled true :message "CC" :note 300 :players #beat_link_trigger.core.PlayerChoice{:number 2}}],
 :expressions {:setup "(println \"hi\\n\")" :shutdown ""}
 :send-status? true
 #_ :ignored #_ 42
 :carabiner {:port 17001 :latency 5 :bars true}}"#;

    #[test]
    fn parses_edn_values() {
        let v = parse_edn("[1 -2 3.5 \"s\" :k sym nil true \\a #{1 2} (a b) #tag {:x 1}]").unwrap();
        let items = v.as_seq().unwrap();
        assert_eq!(items[0], Edn::Int(1));
        assert_eq!(items[1], Edn::Int(-2));
        assert_eq!(items[2], Edn::Float(3.5));
        assert_eq!(items[4], Edn::Keyword("k".into()));
        assert_eq!(items[6], Edn::Nil);
        assert_eq!(items[8], Edn::Char('a'));
        assert!(matches!(&items[11], Edn::Tagged(t, _) if t == "tag"));
    }

    #[test]
    fn imports_sample_blt() {
        let s = import_settings(&parse_edn(SAMPLE).unwrap());
        assert_eq!(s.triggers.len(), 2);
        let t = &s.triggers[0];
        assert_eq!(t.comment, "Set chase speed");
        assert_eq!(t.players, PlayerChoice::Master);
        assert_eq!(t.output.as_deref(), Some("Bus 1"));
        assert_eq!(t.message, MessageKind::Custom);
        assert_eq!(t.enabled, EnabledMode::Always);
        assert_eq!(t.note, 17);
        assert!(!t.send);
        assert!(t.legacy_expressions["tracked"].contains("set-gm-tempo"));
        assert!(t.expressions["tracked"].starts_with("// Imported"));
        let t2 = &s.triggers[1];
        assert_eq!(t2.enabled, EnabledMode::Always);
        assert_eq!(t2.note, 127);
        assert_eq!(t2.players, PlayerChoice::Player(2));
        assert_eq!(s.legacy_expressions["setup"], "(println \"hi\\n\")");
        assert!(!s.legacy_expressions.contains_key("shutdown"));
        assert!(s.send_status);
        assert_eq!(s.carabiner.port, 17001);
        assert!(s.carabiner.bars);
    }
}
