//! The bytes of every frozen message. If one of these fails, an edit changed a
//! released format: put the change in a new version instead, and leave this
//! file alone.
//!
//! Packed bodies are frozen as what they unpack to, not as compressed bytes,
//! which another zstd build may produce differently.

use berth_proto::v1::{
    Cell, Claude, ClaudeState, Color, Cursor, Event, Modes, Row, Screen, SessionInfo, ToApp,
    ToServer, style,
};
use berth_proto::{Hello, Packed, v1};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn screen() -> Screen {
    let cell = |text: &str, fg, bg, style, width| Cell {
        text: text.into(),
        fg,
        bg,
        style,
        width,
    };
    Screen {
        first_line: 300,
        cols: 4,
        rows: 2,
        cursor: Cursor {
            line: 1,
            column: 2,
            visible: true,
        },
        modes: Modes {
            alternate_screen: false,
            application_cursor: true,
            mouse_reporting: false,
            bracketed_paste: true,
            alternate_scroll: false,
        },
        lines: vec![
            Row {
                cells: vec![
                    cell("$", Color::Palette(2), Color::Default, style::BOLD, 1),
                    cell("\u{65e5}", Color::Rgb(1, 2, 3), Color::Default, 0, 2),
                    cell("", Color::Default, Color::Default, 0, 0),
                    cell(
                        "e\u{301}",
                        Color::Default,
                        Color::Palette(4),
                        style::UNDERLINE | style::INVERSE,
                        1,
                    ),
                ],
                wrapped: true,
            },
            Row {
                cells: vec![cell(" ", Color::Default, Color::Default, 0, 1); 4],
                wrapped: false,
            },
        ],
    }
}

/// One of every message to the server. The match is what makes a new variant
/// fail here: it must be added to a new version, not to v1.
fn to_server() -> Vec<(ToServer, &'static str)> {
    let all = vec![
        (ToServer::ListSessions, "00"),
        (
            ToServer::Create {
                name: Some("work".into()),
                cwd: Some("/home/u".into()),
                cols: 120,
                rows: 40,
            },
            "010104776f726b01072f686f6d652f757828",
        ),
        (
            ToServer::Attach {
                session: 7,
                cols: 80,
                rows: 24,
            },
            "02075018",
        ),
        (ToServer::Detach { session: 7 }, "0307"),
        (ToServer::Kill { session: 300 }, "04ac02"),
        (
            ToServer::Input {
                session: 7,
                bytes: b"ls\r".to_vec(),
            },
            "0507036c730d",
        ),
        (
            ToServer::Resize {
                session: 7,
                cols: 100,
                rows: 30,
            },
            "0607641e",
        ),
        (
            ToServer::Confirmed {
                session: 7,
                seq: 1 << 40,
            },
            "0707808080808020",
        ),
        (ToServer::Visible { visible: false }, "0800"),
    ];
    for (message, _) in &all {
        match message {
            ToServer::ListSessions
            | ToServer::Create { .. }
            | ToServer::Attach { .. }
            | ToServer::Detach { .. }
            | ToServer::Kill { .. }
            | ToServer::Input { .. }
            | ToServer::Resize { .. }
            | ToServer::Confirmed { .. }
            | ToServer::Visible { .. } => {}
        }
    }
    all
}

/// One of every message to the app, packed ones with their bodies.
fn to_app() -> Vec<(ToApp, &'static str)> {
    let claude = Claude {
        state: ClaudeState::Permission,
        session_id: Some("0b9c5f5e-7b39-4a55-8a53-2b8f0d1c9e11".into()),
        cwd: Some("/home/u/project".into()),
    };
    let all = vec![
        (
            ToApp::Sessions(vec![SessionInfo {
                id: 7,
                name: "work".into(),
                cwd: None,
                cols: 80,
                rows: 24,
                claude: Some(claude.clone()),
            }]),
            "00010704776f726b0050180101012430623963356635652d376233392d346135352d386135332d326238663064316339653131010f2f686f6d652f752f70726f6a656374",
        ),
        (ToApp::Created { session: 8 }, "0108"),
        (
            ToApp::Output {
                session: 7,
                seq: 1234,
                bytes: b"\x1b[31mred\x1b[0m".to_vec(),
            },
            "0207d2090c1b5b33316d7265641b5b306d",
        ),
        (
            ToApp::Screen {
                session: 7,
                seq: 1234,
                screen: Packed(Vec::new()),
            },
            "0307d20900",
        ),
        (
            ToApp::History {
                session: 7,
                newest: 99,
                lines: Packed(Vec::new()),
            },
            "04076300",
        ),
        (
            ToApp::Event {
                session: 7,
                event: Event::Exited { code: Some(-1) },
            },
            "0507020101",
        ),
        (
            ToApp::Refused {
                what: "attach 9".into(),
                why: "no such session".into(),
            },
            "060861747461636820390f6e6f20737563682073657373696f6e",
        ),
    ];
    for (message, _) in &all {
        match message {
            ToApp::Sessions(_)
            | ToApp::Created { .. }
            | ToApp::Output { .. }
            | ToApp::Screen { .. }
            | ToApp::History { .. }
            | ToApp::Event { .. }
            | ToApp::Refused { .. } => {}
        }
    }
    all
}

#[test]
#[ignore = "prints the golden bytes; run by hand only when adding a version"]
fn print_goldens() {
    for (m, _) in to_server() {
        println!("{m:?}\n  {}", hex(&m.encode().unwrap()));
    }
    for (m, _) in to_app() {
        println!("{m:?}\n  {}", hex(&m.encode().unwrap()));
    }
    println!(
        "screen\n  {}",
        hex(&postcard::to_stdvec(&screen()).unwrap())
    );
    for e in events() {
        println!("{e:?}\n  {}", hex(&postcard::to_stdvec(&e).unwrap()));
    }
    println!(
        "hello\n  {}",
        hex(&Hello::ours("0.1.0", &["claude"]).encode().unwrap())
    );
}

fn events() -> Vec<Event> {
    vec![
        Event::Title("vim".into()),
        Event::Bell,
        Event::Exited { code: None },
        Event::Claude(Claude {
            state: ClaudeState::Interrupted,
            session_id: None,
            cwd: None,
        }),
    ]
}

#[test]
fn every_variant_is_covered() {
    assert_eq!(to_server().len() as u32, v1::TO_SERVER_VARIANTS);
    assert_eq!(to_app().len() as u32, v1::TO_APP_VARIANTS);
}

const SCREEN: &str = "ac020402010201000100010002040124010200010103e697a50201020300000200000000000365cc810001041401010401200000000101200000000101200000000101200000000100";
const EVENTS: [&str; 4] = ["000376696d", "01", "0200", "03050000"];
const HELLO: &str = "4252544801010106636c6175646505302e312e30";

#[test]
fn messages_to_the_server_are_frozen() {
    for (message, golden) in to_server() {
        assert_eq!(hex(&message.encode().unwrap()), golden, "{message:?}");
        assert_eq!(ToServer::decode(&unhex(golden)).unwrap(), message);
    }
}

#[test]
fn messages_to_the_app_are_frozen() {
    for (message, golden) in to_app() {
        assert_eq!(hex(&message.encode().unwrap()), golden, "{message:?}");
        assert_eq!(ToApp::decode(&unhex(golden)).unwrap(), message);
    }
}

#[test]
fn the_screen_body_is_frozen() {
    assert_eq!(hex(&postcard::to_stdvec(&screen()).unwrap()), SCREEN);
    assert_eq!(
        postcard::from_bytes::<Screen>(&unhex(SCREEN)).unwrap(),
        screen()
    );
}

/// Packed, it comes back as the frozen body, whatever zstd made of it.
#[test]
fn a_packed_screen_unpacks_to_the_frozen_body() {
    let message = ToApp::Screen {
        session: 7,
        seq: 1234,
        screen: Packed::pack(&screen()).unwrap(),
    };
    let ToApp::Screen { screen: packed, .. } = ToApp::decode(&message.encode().unwrap()).unwrap()
    else {
        panic!("not a screen");
    };
    assert_eq!(packed.unpack::<Screen>().unwrap(), screen());
}

#[test]
fn history_packs_rows() {
    let lines = screen().lines;
    let message = ToApp::History {
        session: 7,
        newest: 99,
        lines: Packed::pack(&lines).unwrap(),
    };
    let ToApp::History { lines: packed, .. } = ToApp::decode(&message.encode().unwrap()).unwrap()
    else {
        panic!("not history");
    };
    assert_eq!(packed.unpack::<Vec<Row>>().unwrap(), lines);
}

#[test]
fn events_are_frozen() {
    for (event, golden) in events().into_iter().zip(EVENTS) {
        assert_eq!(
            hex(&postcard::to_stdvec(&event).unwrap()),
            golden,
            "{event:?}"
        );
        assert_eq!(
            postcard::from_bytes::<Event>(&unhex(golden)).unwrap(),
            event
        );
    }
}

/// The one encoding no version may ever change.
#[test]
fn the_greeting_is_frozen() {
    let hello = Hello::ours("0.1.0", &["claude"]);
    assert_eq!(hex(&hello.encode().unwrap()), HELLO);
    assert_eq!(Hello::decode(&unhex(HELLO)).unwrap(), hello);
}

/// A whole exchange through frames, as a connection would carry it.
#[test]
fn a_conversation_survives_the_frame_reader() {
    use berth_proto::{FrameReader, frame, negotiate};
    let app = Hello::ours("app", &["claude"]);
    let server = Hello::ours("server", &["claude"]);
    let mut wire = frame(&app.encode().unwrap()).unwrap();
    for (message, _) in to_server() {
        wire.extend(frame(&message.encode().unwrap()).unwrap());
    }
    let mut reader = FrameReader::new();
    for chunk in wire.chunks(5) {
        reader.push(chunk);
    }
    let hello = Hello::decode(&reader.next_frame().unwrap().unwrap()).unwrap();
    assert_eq!(negotiate(&server, &hello).unwrap().version, v1::VERSION);
    let mut got = Vec::new();
    while let Some(payload) = reader.next_frame().unwrap() {
        got.push(ToServer::decode(&payload).unwrap());
    }
    let sent: Vec<ToServer> = to_server().into_iter().map(|(m, _)| m).collect();
    assert_eq!(got, sent);
}
