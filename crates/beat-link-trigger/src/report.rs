//! The Expression Report: an HTML page listing every expression in the Triggers
//! window and open shows, flagging imported Clojure that still needs porting.
//! Port of `expression_report.clj` (without the in-page edit buttons).

use crate::config::Settings;
use crate::expressions::{GLOBAL_EXPRESSIONS, TRIGGER_EXPRESSIONS};
use crate::shows::{Show, CUE_EXPRESSIONS, TRACK_EXPRESSIONS};
use std::fmt::Write;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn section(out: &mut String, title: &str, source: &str, legacy: Option<&String>) {
    let needs_port = legacy.is_some() && source.trim_start().starts_with("// Imported");
    let _ = write!(
        out,
        "<div class=\"expr{}\"><h4>{}{}</h4><pre>{}</pre>",
        if needs_port { " port" } else { "" },
        escape(title),
        if needs_port { " <span class=\"badge\">needs porting from Clojure</span>" } else { "" },
        escape(source)
    );
    if let Some(l) = legacy {
        let _ = write!(out, "<details><summary>Original Clojure</summary><pre>{}</pre></details>", escape(l));
    }
    out.push_str("</div>\n");
}

/// Build the report.
pub fn build(settings: &Settings, shows: &[(String, Show)]) -> String {
    let mut out = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Expression Report</title><style>\
         body{font-family:system-ui,sans-serif;margin:2em;background:#fafafa;color:#222}\
         pre{background:#272822;color:#f8f8f2;padding:.8em;border-radius:6px;overflow-x:auto}\
         .expr{margin-left:1em}.port h4{color:#a40}.badge{background:#fd8;border-radius:4px;padding:0 .4em;font-size:.8em}\
         h2{border-bottom:2px solid #ccc}@media(prefers-color-scheme:dark){body{background:#1e1e1e;color:#ddd}}\
         </style></head><body><h1>Expression Report</h1>\n",
    );
    out.push_str("<h2>Triggers Window</h2>\n");
    for (kind, title, _) in GLOBAL_EXPRESSIONS {
        let src = settings.global_expression(kind);
        if !src.trim().is_empty() {
            section(&mut out, title, src, settings.legacy_expressions.get(*kind));
        }
    }
    for (i, t) in settings.triggers.iter().enumerate() {
        let any = TRIGGER_EXPRESSIONS.iter().any(|(k, _, _)| !t.expression(k).trim().is_empty());
        if !any {
            continue;
        }
        let _ = writeln!(out, "<h3>Trigger {}: {}</h3>", i + 1, escape(&t.comment));
        for (kind, title, _) in TRIGGER_EXPRESSIONS {
            let src = t.expression(kind);
            if !src.trim().is_empty() {
                section(&mut out, title, src, t.legacy_expressions.get(*kind));
            }
        }
    }
    for (name, show) in shows {
        let _ = writeln!(out, "<h2>Show: {}</h2>", escape(name));
        for (kind, src) in &show.expressions {
            section(&mut out, &format!("Show {kind} expression"), src, show.legacy_expressions.get(kind));
        }
        for track in show.tracks.values() {
            let _ = writeln!(out, "<h3>Track: {}</h3>", escape(&track.metadata.title));
            for (kind, title) in TRACK_EXPRESSIONS {
                if let Some(src) = track.expressions.get(*kind) {
                    section(&mut out, title, src, track.legacy_expressions.get(*kind));
                }
            }
            for cue in &track.cues {
                for (kind, title) in CUE_EXPRESSIONS {
                    if let Some(src) = cue.expressions.get(*kind) {
                        section(&mut out, &format!("Cue \"{}\" {}", cue.comment, title), src, cue.legacy_expressions.get(*kind));
                    }
                }
            }
        }
    }
    out.push_str("</body></html>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TriggerConfig;

    #[test]
    fn lists_expressions() {
        let mut s = Settings::default();
        let mut t = TriggerConfig { comment: "<Lights>".into(), ..Default::default() };
        t.expressions.insert("activation".into(), "// Imported\n// (foo)".into());
        t.legacy_expressions.insert("activation".into(), "(foo)".into());
        s.triggers = vec![t];
        let html = build(&s, &[]);
        assert!(html.contains("Trigger 1: &lt;Lights&gt;"));
        assert!(html.contains("needs porting"));
        assert!(html.contains("Original Clojure"));
    }
}
