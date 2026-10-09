//! Program status (OSC 7501, the Program Status Protocol): what the programs
//! in a session say they are doing, kept per session.
//!
//! A program reports `ESC ] 7501 ; key=value:key=value ESC \`. The terminal
//! keeps one record per id and applies the protocol's lifetime rules;
//! libghostty leaves both to the embedder, and Berth's scanner hands the
//! reports over unparsed (`berth_term::StatusEvent`). This module is the
//! parser, with every limit the protocol sets, and the store.
//!
//! Reports are untrusted: any program in the session can print one, and so
//! can a file that is `cat`ted. They are for display only. Nothing Berth does
//! (resuming, confirming, running anything) may follow from one (D53).
//!
//! Pure: no pty, no clock, so every rule is tested as written.

use std::collections::BTreeMap;

use berth_proto::v1;

/// Records kept per session. The protocol allows up to 256 and asks for at
/// least 64; past the cap the least recently updated goes.
pub const MAX_RECORDS: usize = 256;

const MAX_KEY: usize = 16;
const MAX_APP: usize = 32;
const MAX_ID: usize = 128;
const MAX_ID_SEGMENT: usize = 32;
const MAX_ID_DEPTH: usize = 8;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE: usize = 192;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG: usize = 2048;

/// One report, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// Remove the record at `id` and every record beneath it; with no id,
    /// every record.
    Clear { id: String },
    /// Replace the record at `id` entirely. The root record's id is empty.
    Set { id: String, record: Record },
}

/// What a program last said under one id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub state: v1::StatusState,
    pub kind: Option<v1::StatusKind>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

/// Parse a report's pairs. `None` discards the whole report, as the protocol
/// says to when the state is missing or unknown, the id is malformed, a limit
/// is broken, base64 is invalid or decoded text holds control characters. A
/// malformed pair on its own is skipped, an unknown key ignored, and of a
/// repeated key the last wins.
pub fn parse(pairs: &str) -> Option<Report> {
    let mut state = None;
    let mut id = None;
    let mut kind = None;
    let mut progress = None;
    let mut app = None;
    let mut title = None;
    let mut msg = None;
    for pair in pairs.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key.is_empty()
            || key.len() > MAX_KEY
            || !key.bytes().all(|b| b.is_ascii_lowercase())
            || !value.bytes().all(value_byte)
        {
            continue;
        }
        match key {
            "state" => state = Some(value),
            "id" => id = Some(value),
            "kind" => kind = Some(value),
            "progress" => progress = Some(value),
            "app" => app = Some(value),
            "title" => title = Some(value),
            "msg" => msg = Some(value),
            _ => {}
        }
    }
    let id = match id {
        Some(id) => valid_id(id)?.to_string(),
        None => String::new(),
    };
    let state = match state? {
        "clear" => return Some(Report::Clear { id }),
        "idle" => v1::StatusState::Idle,
        "working" => v1::StatusState::Working,
        "done" => v1::StatusState::Done,
        "blocked" => v1::StatusState::Blocked,
        "error" => v1::StatusState::Error,
        _ => return None,
    };
    if app.is_some_and(|app| app.len() > MAX_APP) {
        return None;
    }
    let title = match title {
        Some(title) => Some(text(title, MAX_TITLE_ENCODED, MAX_TITLE)?),
        None => None,
    };
    let msg = match msg {
        Some(msg) => Some(text(msg, MAX_MSG_ENCODED, MAX_MSG)?),
        None => None,
    };
    // `kind` means something only when blocked, `progress` only when working
    // or blocked; elsewhere, and when not understood, they are malformed
    // pairs and skipped.
    let kind = (state == v1::StatusState::Blocked)
        .then_some(kind)
        .flatten()
        .and_then(|kind| match kind {
            "permission" => Some(v1::StatusKind::Permission),
            "question" => Some(v1::StatusKind::Question),
            "auth" => Some(v1::StatusKind::Auth),
            _ => None,
        });
    let progress = matches!(state, v1::StatusState::Working | v1::StatusState::Blocked)
        .then_some(progress)
        .flatten()
        .filter(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|p| p.parse::<u8>().ok())
        .filter(|p| *p <= 100);
    Some(Report::Set {
        id,
        record: Record {
            state,
            kind,
            progress,
            app: app.filter(|app| !app.is_empty()).map(str::to_string),
            title: title.filter(|t| !t.is_empty()),
            msg: msg.filter(|m| !m.is_empty()),
        },
    })
}

/// The bytes a value may hold: `[A-Za-z0-9_.,+/=-]`.
fn value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b)
}

/// An id: up to eight segments of `[A-Za-z0-9_.+-]{1,32}`, joined by `/`.
fn valid_id(id: &str) -> Option<&str> {
    let segment = |s: &str| {
        (1..=MAX_ID_SEGMENT).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
    };
    (id.len() <= MAX_ID && id.split('/').count() <= MAX_ID_DEPTH && id.split('/').all(segment))
        .then_some(id)
}

/// Base64 text within its limits, fit to show: no control characters (the
/// report is discarded for one), and with what could reorder or hide text on
/// a line taken out.
fn text(encoded: &str, max_encoded: usize, max_decoded: usize) -> Option<String> {
    if encoded.len() > max_encoded {
        return None;
    }
    let bytes = base64(encoded)?;
    if bytes.len() > max_decoded {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(text.chars().filter(|c| !invisible(*c)).collect())
}

/// Bidirectional overrides, embeddings and isolates, and invisible formatting
/// characters: taken out of text before it is shown, so a report cannot make
/// a label read as something else.
fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// Standard base64, padding optional. `None` for anything else.
fn base64(encoded: &str) -> Option<Vec<u8>> {
    let data = encoded.trim_end_matches('=');
    if encoded.len() - data.len() > 2 || data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let (mut bits, mut held) = (0u32, 0u8);
    for b in data.bytes() {
        let six = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(six);
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((bits >> held) as u8);
            bits &= (1 << held) - 1;
        }
    }
    // Bits left over must be padding zeros.
    (bits == 0).then_some(out)
}

/// A session's records.
#[derive(Debug, Default)]
pub struct Records {
    /// By id; the root record's is empty. Each with when it was last set.
    records: BTreeMap<String, (Record, u64)>,
    clock: u64,
}

impl Records {
    /// Apply a report. Returns whether anything changed.
    pub fn apply(&mut self, report: Report) -> bool {
        match report {
            Report::Clear { id } => self.clear(&id),
            Report::Set { id, record } => {
                self.clock += 1;
                let same = self.records.get(&id).is_some_and(|(old, _)| *old == record);
                self.records.insert(id, (record, self.clock));
                while self.records.len() > MAX_RECORDS {
                    let oldest = self
                        .records
                        .iter()
                        .min_by_key(|(_, (_, at))| *at)
                        .map(|(id, _)| id.clone());
                    match oldest {
                        Some(id) => self.records.remove(&id),
                        None => break,
                    };
                }
                !same
            }
        }
    }

    /// Remove the record at `id` and all beneath it; every record if `id` is
    /// empty.
    fn clear(&mut self, id: &str) -> bool {
        let before = self.records.len();
        if id.is_empty() {
            self.records.clear();
        } else {
            let beneath = format!("{id}/");
            self.records
                .retain(|key, _| key != id && !key.starts_with(&beneath));
        }
        self.records.len() != before
    }

    /// The program that had the terminal exited, or a new shell prompt began:
    /// records of something under way (`working`, `blocked`) go, and `idle`
    /// ones with them. `done` and `error` stay to be seen.
    pub fn program_ended(&mut self) -> bool {
        let before = self.records.len();
        self.records.retain(|_, (record, _)| {
            matches!(record.state, v1::StatusState::Done | v1::StatusState::Error)
        });
        self.records.len() != before
    }

    /// A full reset of the terminal: every record goes.
    pub fn reset(&mut self) -> bool {
        self.clear("")
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Every record as the wire carries it, by id, with the `app` a record
    /// without its own inherits from the nearest ancestor that has one.
    pub fn to_wire(&self) -> Vec<v1::Status> {
        self.records
            .iter()
            .map(|(id, (record, _))| v1::Status {
                id: id.clone(),
                state: record.state,
                kind: record.kind,
                progress: record.progress,
                app: record.app.clone().or_else(|| self.inherited_app(id)),
                title: record.title.clone(),
                msg: record.msg.clone(),
            })
            .collect()
    }

    fn inherited_app(&self, id: &str) -> Option<String> {
        let mut at = id;
        while let Some((parent, _)) = at.rsplit_once('/') {
            if let Some((record, _)) = self.records.get(parent)
                && record.app.is_some()
            {
                return record.app.clone();
            }
            at = parent;
        }
        // The root record is every other record's ancestor.
        (!id.is_empty())
            .then(|| self.records.get("").and_then(|(r, _)| r.app.clone()))
            .flatten()
    }
}

/// The one record a session is shown by, where it has several: what most
/// needs the user first. Blocked, then failed, then working, then finished,
/// then idle; among equals the first by id.
pub fn foremost(records: &[v1::Status]) -> Option<&v1::Status> {
    let rank = |state: v1::StatusState| match state {
        v1::StatusState::Blocked => 0,
        v1::StatusState::Error => 1,
        v1::StatusState::Working => 2,
        v1::StatusState::Done => 3,
        v1::StatusState::Idle => 4,
    };
    records.iter().min_by_key(|r| rank(r.state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use v1::{StatusKind, StatusState};

    fn set(pairs: &str) -> (String, Record) {
        match parse(pairs) {
            Some(Report::Set { id, record }) => (id, record),
            other => panic!("{pairs:?} parsed to {other:?}"),
        }
    }

    #[test]
    fn a_plain_report_parses() {
        // "Building" and "tests" in base64.
        let (id, record) =
            set("state=working:id=build/test:progress=40:app=make:title=dGVzdHM:msg=QnVpbGRpbmc=");
        assert_eq!(id, "build/test");
        assert_eq!(record.state, StatusState::Working);
        assert_eq!(record.progress, Some(40));
        assert_eq!(record.app.as_deref(), Some("make"));
        assert_eq!(record.title.as_deref(), Some("tests"));
        assert_eq!(record.msg.as_deref(), Some("Building"));
        assert_eq!(set("state=idle").0, "", "no id is the root record");
    }

    #[test]
    fn the_state_is_required_and_must_be_known() {
        assert_eq!(parse("app=x"), None);
        assert_eq!(parse("state=thinking"), None);
        assert_eq!(parse(""), None);
        assert_eq!(
            parse("state=clear:id=a"),
            Some(Report::Clear { id: "a".into() })
        );
    }

    #[test]
    fn malformed_pairs_are_skipped_and_the_rest_kept() {
        let (_, record) =
            set("nonsense:KEY=1:state=done:x y=1:app=ok:toolongkeyname1234=1:app!=no");
        assert_eq!(record.state, StatusState::Done);
        assert_eq!(record.app.as_deref(), Some("ok"));
        // A value outside the allowed bytes is a malformed pair.
        let (_, record) = set("state=done:app=has space");
        assert_eq!(record.app, None);
        // The last of a repeated key wins.
        assert_eq!(set("state=idle:state=error").1.state, StatusState::Error);
    }

    #[test]
    fn kind_and_progress_only_count_where_they_mean_something() {
        let (_, blocked) = set("state=blocked:kind=permission:progress=7");
        assert_eq!(blocked.kind, Some(StatusKind::Permission));
        assert_eq!(blocked.progress, Some(7));
        let (_, working) = set("state=working:kind=question:progress=100");
        assert_eq!((working.kind, working.progress), (None, Some(100)));
        let (_, done) = set("state=done:kind=auth:progress=50");
        assert_eq!((done.kind, done.progress), (None, None));
        for bad in ["101", "-1", "1.5", "", "0050", "abc"] {
            let (_, r) = set(&format!("state=working:progress={bad}"));
            assert_eq!(r.progress, None, "{bad:?}");
        }
        assert_eq!(set("state=blocked:kind=other").1.kind, None);
    }

    #[test]
    fn a_malformed_id_discards_the_report() {
        for bad in [
            "a//b",
            "/a",
            "a/",
            "a/b/c/d/e/f/g/h/i",
            &"x".repeat(33),
            "a,b",
            "a=b",
        ] {
            assert_eq!(parse(&format!("state=idle:id={bad}")), None, "{bad:?}");
        }
        let deep = "a/b/c/d/e/f/g/h";
        assert_eq!(set(&format!("state=idle:id={deep}")).0, deep);
        // 128 bytes in all: four segments of 32 is 131 with the slashes.
        let long = [&"x".repeat(32)[..]; 4].join("/");
        assert_eq!(parse(&format!("state=idle:id={long}")), None);
    }

    #[test]
    fn limits_and_bad_text_discard_the_report() {
        assert_eq!(parse(&format!("state=idle:app={}", "a".repeat(33))), None);
        assert!(parse(&format!("state=idle:app={}", "a".repeat(32))).is_some());
        // Not base64.
        assert_eq!(parse("state=idle:msg=a"), None);
        assert_eq!(parse("state=idle:msg=ab=c"), None);
        // A control character inside: "a\nb" and "a\x1b[31m".
        assert_eq!(parse("state=idle:msg=YQpi"), None);
        assert_eq!(parse("state=idle:title=YRtbMzFt"), None);
        // Not UTF-8: 0xff 0xfe.
        assert_eq!(parse("state=idle:msg=//4"), None);
        // Too long encoded, and too long decoded.
        assert_eq!(
            parse(&format!("state=idle:title={}", "QUFB".repeat(65))),
            None
        );
        assert_eq!(
            parse(&format!("state=idle:msg={}", "QUFB".repeat(684))),
            None
        );
        assert!(parse(&format!("state=idle:msg={}", "QUFB".repeat(682))).is_some());
    }

    #[test]
    fn text_that_could_mislead_is_neutralised() {
        // "a" RIGHT-TO-LEFT OVERRIDE "b" ZERO WIDTH SPACE "c".
        let (_, record) = set("state=idle:msg=YeKArmLigItj");
        assert_eq!(record.msg.as_deref(), Some("abc"));
    }

    #[test]
    fn base64_takes_padding_or_none() {
        assert_eq!(base64("QQ==").as_deref(), Some(&b"A"[..]));
        assert_eq!(base64("QQ").as_deref(), Some(&b"A"[..]));
        assert_eq!(base64("QUI=").as_deref(), Some(&b"AB"[..]));
        assert_eq!(base64("QUJD").as_deref(), Some(&b"ABC"[..]));
        assert_eq!(base64("").as_deref(), Some(&b""[..]));
        assert_eq!(base64("Q"), None);
        assert_eq!(base64("QQ==="), None);
        assert_eq!(base64("QR"), None, "leftover bits must be zero");
        assert_eq!(base64("Q-J_"), None, "not the URL alphabet");
    }

    fn records(reports: &[&str]) -> Records {
        let mut records = Records::default();
        for report in reports {
            records.apply(parse(report).expect(report));
        }
        records
    }

    fn ids(records: &Records) -> Vec<String> {
        records.to_wire().into_iter().map(|r| r.id).collect()
    }

    #[test]
    fn a_report_replaces_its_record_entirely() {
        let mut records = records(&["state=working:app=make:progress=10"]);
        assert!(records.apply(parse("state=working").unwrap()));
        let wire = records.to_wire();
        assert_eq!((wire[0].app.clone(), wire[0].progress), (None, None));
        assert!(!records.apply(parse("state=working").unwrap()), "no change");
    }

    #[test]
    fn clearing_takes_the_record_and_all_beneath() {
        let mut records = records(&[
            "state=working",
            "state=working:id=a",
            "state=working:id=a/b",
            "state=working:id=ab",
        ]);
        assert!(records.apply(parse("state=clear:id=a").unwrap()));
        assert_eq!(ids(&records), ["", "ab"]);
        assert!(!records.apply(parse("state=clear:id=a").unwrap()));
        assert!(records.apply(parse("state=clear").unwrap()));
        assert!(records.is_empty());
    }

    #[test]
    fn a_child_inherits_the_app_of_its_nearest_ancestor() {
        let records = records(&[
            "state=working:app=root",
            "state=working:id=a:app=mid",
            "state=working:id=a/b/c",
            "state=working:id=z",
            "state=working:id=a/own:app=own",
        ]);
        let app = |id: &str| {
            records
                .to_wire()
                .into_iter()
                .find(|r| r.id == id)
                .unwrap()
                .app
        };
        assert_eq!(
            app("a/b/c").as_deref(),
            Some("mid"),
            "a parent need not exist"
        );
        assert_eq!(app("z").as_deref(), Some("root"));
        assert_eq!(app("a/own").as_deref(), Some("own"));
    }

    #[test]
    fn a_program_ending_keeps_only_what_is_left_to_see() {
        let mut records = records(&[
            "state=working:id=w",
            "state=blocked:id=b",
            "state=idle:id=i",
            "state=done:id=d",
            "state=error:id=e",
        ]);
        assert!(records.program_ended());
        assert_eq!(ids(&records), ["d", "e"]);
        assert!(!records.program_ended());
        assert!(records.reset());
        assert!(records.is_empty());
    }

    #[test]
    fn past_the_cap_the_least_recently_updated_goes() {
        let mut records = Records::default();
        for n in 0..MAX_RECORDS {
            records.apply(parse(&format!("state=working:id=r{n}")).unwrap());
        }
        // Touch the oldest, then add one more: the second oldest goes.
        records.apply(parse("state=done:id=r0").unwrap());
        records.apply(parse("state=working:id=new").unwrap());
        let ids = ids(&records);
        assert_eq!(ids.len(), MAX_RECORDS);
        assert!(ids.contains(&"r0".to_string()));
        assert!(!ids.contains(&"r1".to_string()));
        assert!(ids.contains(&"new".to_string()));
    }

    #[test]
    fn the_foremost_record_is_the_one_that_needs_the_user() {
        let wire = records(&[
            "state=idle:id=a",
            "state=working:id=b",
            "state=blocked:id=c:kind=question",
            "state=done:id=d",
        ])
        .to_wire();
        assert_eq!(foremost(&wire).map(|r| r.id.as_str()), Some("c"));
        assert_eq!(foremost(&[]), None);
    }
}
