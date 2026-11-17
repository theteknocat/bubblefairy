//! `BF_DEMO=1`: invented chats instead of a Mac, for screenshots and UI work. Nothing real is shown.

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::mac::{Cmd, Event};

fn ago(min: i64) -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 - min * 60;
    gtk::glib::DateTime::from_unix_utc(t).unwrap().format_iso8601().unwrap().to_string()
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
            m(7, false, 3, "Oh, can you send me the trail map when you get a sec?"),
        ],
        2 => vec![
            msg(2, 1, false, "+15550102", "Sam Rivera", 90, "Finished chapter 12 last night. Wow."),
            msg(2, 2, false, "+15550103", "Lena Park", 85, "No spoilers!! I'm two chapters behind 😅"),
            msg(2, 3, true, "", "", 80, "Same, saving it for the weekend"),
            msg(2, 4, false, "+15550104", "Omar Haddad", 40, "Next meetup at my place? I'll make pie 🥧"),
        ],
        3 => vec![msg(3, 1, false, "+15550199", "", 200, "Your table for 4 is ready. Please come to the host stand."), msg(3, 2, true, "", "", 199, "On our way, thanks!")],
        _ => vec![msg(id, 1, false, "x", "", 1500, "Call me when you're free"), msg(id, 2, true, "", "", 1490, "Will do 👍")],
    };
    if id == 1 {
        // files of each kind, for looking at their cards (none of these exist, so previews fall back to icons)
        let file = |n: i64, me: bool, min: i64, name: &str, mime: &str, bytes: i64, missing: bool| {
            let mut m = msg(1, n, me, "+15550101", "Maya Thompson", min, "\u{FFFC}");
            m["attachments"] = json!([{"filename": format!("~/Library/Messages/Attachments/xx/{name}"), "original_path": format!("~/Library/Messages/Attachments/xx/{name}"),
                "transfer_name": name, "mime_type": mime, "total_bytes": bytes, "missing": missing, "uti": "", "is_sticker": false}]);
            m
        };
        v.insert(3, file(20, false, 186, "Trail-map-Saturday.pdf", "application/pdf", 1_940_000, false));
        v.insert(4, file(21, true, 185, "IMG_1304.MOV", "video/quicktime", 38_400_000, false));
        v.insert(5, file(22, false, 184, "Sundre.m4a", "audio/x-m4a", 3_300_000, false));
        v.insert(6, file(23, false, 183, "Maya Thompson.vcf", "text/vcard", 4_000, false));
        v.insert(7, file(24, true, 182, "Packing list with a really quite long name.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", 12_000, true));
    }
    v
}

pub async fn run(mut cmds: mpsc::UnboundedReceiver<Cmd>, ev: mpsc::UnboundedSender<Event>) {
    let _ = ev.send(Event::Connected);
    let mut next = 50;
    while let Some(c) = cmds.recv().await {
        let _ = match c {
            Cmd::Chats => ev.send(Event::Chats(chats())),
            Cmd::History(id, _) => ev.send(Event::History(id, history(id))),
            Cmd::Preview(id) => ev.send(Event::Preview(id, history(id).pop().unwrap())),
            Cmd::Send { chat_id, text, token, .. } => {
                next += 1;
                let _ = ev.send(Event::Message(msg(chat_id, next, true, "", "", 0, &text)));
                ev.send(Event::Sent { text: String::new(), files: Vec::new(), err: None, token })
            }
            Cmd::Older { id, .. } => ev.send(Event::Older(id, Vec::new())),
            Cmd::Contacts => ev.send(Event::Contacts(Default::default())),
            Cmd::Link(id) => ev.send(Event::Link(id, None)),
            Cmd::FileInfo { path, .. } => ev.send(Event::FileInfo(path, None)),
            Cmd::Image { path, full } => {
                // BF_DEMO_FRAME=<picture file>: stands in for the first frame of the demo video
                let bytes = if path.ends_with(".MOV") { std::env::var("BF_DEMO_FRAME").ok().and_then(|f| std::fs::read(f).ok()) } else { None };
                ev.send(Event::Image { path, full, bytes })
            }
            Cmd::Recent(id) => ev.send(Event::Recent(id, Vec::new())),
            Cmd::Download { path, dest, .. } => ev.send(Event::Downloaded { path, dest, err: Some("demo".into()) }),
            Cmd::Bulk(path) => ev.send(Event::Image { path, full: false, bytes: None }),
            Cmd::Map(q) => ev.send(Event::Map(q, None)),
            Cmd::Media { chat_id, kind, offset } => {
                let it = |name: &str, size: i64, missing: bool| crate::mac::MediaItem { path: format!("~/demo/{name}"), name: name.into(), size, date: 1_792_000_000, missing, ..Default::default() };
                let items = match kind {
                    crate::mac::Kind::Files if chat_id == 1 => vec![
                        it("Trail-map-Saturday.pdf", 1_940_000, false),
                        it("IMG_1304.MOV", 38_400_000, false),
                        it("Sundre.m4a", 3_300_000, false),
                        it("Maya Thompson.vcf", 4_000, false),
                        it("Packing list with a really quite long name.docx", 12_000, true),
                        it("photos-2026.zip", 52_000_000, false),
                    ],
                    _ => Vec::new(),
                };
                ev.send(Event::Media { chat_id, kind, offset, items, next: offset, more: false })
            }
            Cmd::Configure(_) => Ok(()),
        };
    }
}
