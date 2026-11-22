//! `BF_DEMO=1`: invented chats instead of a Mac, for screenshots and UI work. Nothing real is shown.

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::mac::{Cmd, Event};

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn ago(min: i64) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - min * 60;
    gtk::glib::DateTime::from_unix_utc(t)
        .unwrap()
        .format_iso8601()
        .unwrap()
        .to_string()
}

fn chats() -> Vec<Value> {
    vec![
        json!({"id": 1, "contact_name": "Maya Thompson", "participants": ["+15550101"], "service": "iMessage", "is_group": false, "unread_count": 2, "last_message_at": ago(3)}),
        json!({"id": 2, "display_name": "Book Club 📚", "participants": ["+15550102", "+15550103", "+15550104"], "service": "iMessage", "is_group": true, "unread_count": 0, "last_message_at": ago(40)}),
        json!({"id": 3, "participants": ["+15550199"], "service": "SMS", "is_group": false, "unread_count": 0, "last_message_at": ago(200)}),
        json!({"id": 4, "contact_name": "Dad", "participants": ["dad@example.com"], "service": "iMessage", "is_group": false, "unread_count": 0, "last_message_at": ago(1500)}),
        json!({"id": 5, "contact_name": "Priya Shah", "participants": ["+15550105"], "service": "iMessage", "is_group": false, "unread_count": 0, "last_message_at": ago(4000)}),
        json!({"id": 6, "display_name": "Hiking Crew", "participants": ["+15550102", "+15550103", "+15550104", "+15550105", "+15550106", "+15550107", "+15550108"], "service": "iMessage", "is_group": true, "unread_count": 0, "last_message_at": ago(2500)}),
        json!({"id": 7, "display_name": "Dinner Four", "participants": ["+15550102", "+15550103", "+15550104", "+15550105"], "service": "iMessage", "is_group": true, "unread_count": 0, "last_message_at": ago(3000)}),
    ]
}

fn msg(chat: i64, n: i64, me: bool, sender: &str, name: &str, min: i64, text: &str) -> Value {
    json!({"id": chat * 100 + n, "guid": format!("g{chat}-{n}"), "chat_id": chat, "is_from_me": me, "sender": sender,
           "sender_name": name, "text": text, "created_at": ago(min), "attachments": []})
}

fn history(id: i64) -> Vec<Value> {
    let m = |n, me, min, t| msg(id, n, me, "+15550101", "Maya Thompson", min, t);
    let mut v = match id {
        1 => vec![
            m(1, false, 190, "Are we still on for Saturday? 🌲"),
            m(2, true, 188, "Yes! Trailhead at 9?"),
            m(3, false, 187, "Perfect. I'll bring snacks"),
            m(4, false, 187, "and the good thermos ☕️"),
            m(5, true, 60, "Just checked the forecast, sunny all day ☀️"),
            m(6, false, 4, "Amazing!! Can't wait"),
            m(
                7,
                false,
                3,
                "Oh, can you send me the trail map when you get a sec?",
            ),
        ],
        2 => vec![
            msg(
                2,
                1,
                false,
                "+15550102",
                "Sam Rivera",
                90,
                "Finished chapter 12 last night. Wow.",
            ),
            msg(
                2,
                2,
                false,
                "+15550103",
                "Lena Park",
                85,
                "No spoilers!! I'm two chapters behind 😅",
            ),
            msg(2, 3, true, "", "", 80, "Same, saving it for the weekend"),
            msg(
                2,
                4,
                false,
                "+15550104",
                "Omar Haddad",
                40,
                "Next meetup at my place? I'll make pie 🥧",
            ),
        ],
        3 => vec![
            msg(
                3,
                1,
                false,
                "+15550199",
                "",
                200,
                "Your table for 4 is ready. Please come to the host stand.",
            ),
            msg(3, 2, true, "", "", 199, "On our way, thanks!"),
        ],
        _ => vec![
            msg(id, 1, false, "x", "", 1500, "Call me when you're free"),
            msg(id, 2, true, "", "", 1490, "Will do 👍"),
        ],
    };
    if id == 1 {
        // files of each kind, for looking at their cards (none of these exist, so previews fall back to icons)
        let file = |n: i64,
                    me: bool,
                    min: i64,
                    name: &str,
                    mime: &str,
                    bytes: i64,
                    missing: bool| {
            let mut m = msg(1, n, me, "+15550101", "Maya Thompson", min, "\u{FFFC}");
            m["attachments"] = json!([{"filename": format!("~/Library/Messages/Attachments/xx/{name}"), "original_path": format!("~/Library/Messages/Attachments/xx/{name}"),
                "transfer_name": name, "mime_type": mime, "total_bytes": bytes, "missing": missing, "uti": "", "is_sticker": false}]);
            m
        };
        v.insert(
            3,
            file(
                20,
                false,
                186,
                "Trail-map-Saturday.pdf",
                "application/pdf",
                1_940_000,
                false,
            ),
        );
        v.insert(
            4,
            file(
                21,
                true,
                185,
                "IMG_1304.MOV",
                "video/quicktime",
                38_400_000,
                false,
            ),
        );
        v.insert(
            5,
            file(
                22,
                false,
                184,
                "Sundre.m4a",
                "audio/x-m4a",
                3_300_000,
                false,
            ),
        );
        v.insert(
            6,
            file(
                23,
                false,
                183,
                "Maya Thompson.vcf",
                "text/vcard",
                4_000,
                false,
            ),
        );
        v.insert(
            7,
            file(
                24,
                true,
                182,
                "Packing list with a really quite long name.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                12_000,
                true,
            ),
        );
    }
    // BF_DEMO_FAIL=1: my newest message in chat 4 failed to send
    if id == 4
        && std::env::var_os("BF_DEMO_FAIL").is_some()
        && let Some(m) = v.iter_mut().rev().find(|m| m["is_from_me"] == true)
    {
        m["error"] = json!(35);
    }
    // BF_DEMO_WEBLINK=<url>: a received message in chat 1 that is just this link
    if id == 1
        && let Ok(url) = std::env::var("BF_DEMO_WEBLINK")
    {
        v.push(msg(
            1,
            90,
            false,
            "+15550101",
            "Maya Thompson",
            1,
            &format!("have you seen this {url}"),
        ));
    }
    // BF_DEMO_REACT=1: reactions on my newest and on the newest received message
    if std::env::var_os("BF_DEMO_REACT").is_some() {
        let r = |who: &str, me: bool, e: &str| json!({"sender": who, "sender_name": who, "emoji": e, "is_from_me": me, "type": "custom"});
        if let Some(m) = v.iter_mut().rev().find(|m| m["is_from_me"] == true) {
            m["reactions"] = json!([r("Maya Thompson", false, "❤️")]);
        }
        if let Some(m) = v.iter_mut().rev().find(|m| m["is_from_me"] == false) {
            m["reactions"] = json!([
                r("You", true, "😂"),
                r("Dad", false, "😂"),
                r("Sam", false, "👍")
            ]);
        }
    }
    // BF_DEMO_REPLY=1: my newest message replies to the newest received one
    if std::env::var_os("BF_DEMO_REPLY").is_some() {
        let g = v
            .iter()
            .rev()
            .find(|m| m["is_from_me"] == false)
            .map(|m| m["guid"].clone());
        if let Some(g) = g {
            for m in v
                .iter_mut()
                .rev()
                .filter(|m| m["is_from_me"] == true)
                .take(2)
            {
                m["reply_to_guid"] = g.clone();
            }
        }
    }
    v
}

pub async fn run(mut cmds: mpsc::UnboundedReceiver<Cmd>, ev: mpsc::UnboundedSender<Event>) {
    let _ = ev.send(Event::Connected);
    // BF_DEMO_INCOMING=<ms>: a message from chat 3 arrives after this long (to see the notification and hear the sound)
    if let Some(ms) = std::env::var("BF_DEMO_INCOMING")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        let ev2 = ev.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            let _ = ev2.send(Event::Message(msg(
                3,
                90,
                false,
                "+15550199",
                "",
                0,
                "Hello from the demo",
            )));
        });
    }
    let mut next = 50;
    while let Some(c) = cmds.recv().await {
        let _ = match c {
            Cmd::Chats(_) => ev.send(Event::Chats(chats())),
            Cmd::FindNamed { .. } | Cmd::FindChats { .. } => Ok(()),
            Cmd::FindChat(a) => ev.send(Event::FoundChat(
                a.first().cloned().unwrap_or_default(),
                None,
            )),
            Cmd::History(id, _) => ev.send(Event::History(id, history(id))),
            Cmd::Receipts(id) => {
                // the last message I sent in each demo chat: chat 1 has been read, the others only delivered
                let last = history(id)
                    .into_iter()
                    .rev()
                    .find(|m| m["is_from_me"] == true)
                    .and_then(|m| m["id"].as_i64());
                let r = last.map(|msg| crate::mac::Receipt {
                    msg,
                    delivered: true,
                    read_at: (id == 1).then(|| now_unix() - 600),
                    silenced: id == 4,
                });
                ev.send(Event::Receipt(id, r))
            }
            Cmd::Resend { token, .. } => {
                if std::env::var_os("BF_DEMO_SLOW").is_some() {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await; // dev aid: time to look at the sending state
                }
                ev.send(Event::Sent {
                    text: String::new(),
                    files: Vec::new(),
                    err: None,
                    token,
                })
            }
            Cmd::Preview(id) => ev.send(Event::Preview(id, history(id).pop().unwrap())),
            Cmd::Send {
                chat_id,
                text,
                token,
                ..
            } => {
                next += 1;
                let _ = ev.send(Event::Message(msg(chat_id, next, true, "", "", 0, &text)));
                ev.send(Event::Sent {
                    text: String::new(),
                    files: Vec::new(),
                    err: None,
                    token,
                })
            }
            Cmd::Older { id, .. } => ev.send(Event::Older(id, Vec::new())),
            Cmd::Search { q, seq } => {
                let q = q.to_lowercase();
                let mut hits: Vec<Value> = (1..=7)
                    .flat_map(history)
                    .filter(|m| {
                        m["text"]
                            .as_str()
                            .is_some_and(|t| t.to_lowercase().contains(&q))
                    })
                    .collect();
                hits.reverse();
                let mut counts = std::collections::HashMap::new();
                for h in &hits {
                    *counts
                        .entry(h["chat_id"].as_i64().unwrap_or(0))
                        .or_insert(0usize) += 1;
                }
                ev.send(Event::Found { seq, hits, counts })
            }
            Cmd::ChatHits { chat, q } => {
                let ql = q.to_lowercase();
                let mut hits: Vec<Value> = history(chat)
                    .into_iter()
                    .filter(|m| {
                        m["text"]
                            .as_str()
                            .is_some_and(|t| t.to_lowercase().contains(&ql))
                    })
                    .collect();
                hits.reverse();
                ev.send(Event::ChatFound { chat, q, hits })
            }
            Cmd::Window { id, .. } => ev.send(Event::Window {
                id,
                msgs: history(id),
                more: false,
            }),
            Cmd::Newer { id, .. } => ev.send(Event::Newer {
                id,
                msgs: Vec::new(),
                more: false,
            }),
            Cmd::ContactsCheck => Ok(()),
            Cmd::Contacts => ev.send(Event::Contacts(Default::default())),
            Cmd::Link(id) => ev.send(Event::Link(id, None)),
            Cmd::FileInfo { path, .. } => ev.send(Event::FileInfo(path, None)),
            Cmd::Image { path, full } => {
                // BF_DEMO_FRAME=<picture file>: stands in for the first frame of the demo video
                let bytes = if path.ends_with(".MOV") {
                    std::env::var("BF_DEMO_FRAME")
                        .ok()
                        .and_then(|f| std::fs::read(f).ok())
                } else {
                    None
                };
                ev.send(Event::Image { path, full, bytes })
            }
            Cmd::Recent(id) => ev.send(Event::Recent(id, Vec::new())),
            Cmd::Download { path, dest, .. } => ev.send(Event::Downloaded {
                path,
                dest,
                err: Some("demo".into()),
            }),
            Cmd::Bulk(path) => ev.send(Event::Image {
                path,
                full: false,
                bytes: None,
            }),
            Cmd::Map(q) => ev.send(Event::Map(q, None)),
            Cmd::Media {
                chat_id,
                kind,
                offset,
            } => {
                let it = |name: &str, size: i64, missing: bool| crate::mac::MediaItem {
                    path: format!("~/demo/{name}"),
                    name: name.into(),
                    size,
                    date: 1_792_000_000,
                    missing,
                    ..Default::default()
                };
                let items = match kind {
                    crate::mac::Kind::Files if chat_id == 1 => vec![
                        it("Trail-map-Saturday.pdf", 1_940_000, false),
                        it("IMG_1304.MOV", 38_400_000, false),
                        it("Sundre.m4a", 3_300_000, false),
                        it("Maya Thompson.vcf", 4_000, false),
                        it(
                            "Packing list with a really quite long name.docx",
                            12_000,
                            true,
                        ),
                        it("photos-2026.zip", 52_000_000, false),
                    ],
                    _ => Vec::new(),
                };
                ev.send(Event::Media {
                    chat_id,
                    kind,
                    offset,
                    items,
                    next: offset,
                    more: false,
                })
            }
            Cmd::Configure(_) | Cmd::Merge(_) => Ok(()),
        };
    }
}
