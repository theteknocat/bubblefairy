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
    match id {
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
    }
}

pub async fn run(mut cmds: mpsc::UnboundedReceiver<Cmd>, ev: mpsc::UnboundedSender<Event>) {
    let _ = ev.send(Event::Connected);
    let mut next = 50;
    while let Some(c) = cmds.recv().await {
        let _ = match c {
            Cmd::Chats => ev.send(Event::Chats(chats())),
            Cmd::History(id) => ev.send(Event::History(id, history(id))),
            Cmd::Preview(id) => ev.send(Event::Preview(id, history(id).pop().unwrap())),
            Cmd::Send { chat_id, text, .. } => {
                next += 1;
                let _ = ev.send(Event::Message(msg(chat_id, next, true, "", "", 0, &text)));
                ev.send(Event::Sent { text: String::new(), files: Vec::new(), err: None })
            }
            Cmd::Older { id, .. } => ev.send(Event::Older(id, Vec::new())),
            Cmd::Contacts => ev.send(Event::Contacts(Default::default())),
            Cmd::Link(id) => ev.send(Event::Link(id, None)),
            Cmd::Image { path, full } => ev.send(Event::Image { path, full, bytes: None }),
            Cmd::Configure(_) => Ok(()),
        };
    }
}
