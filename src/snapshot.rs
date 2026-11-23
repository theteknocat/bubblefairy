//! A local copy of the chat list, its row previews and the contacts, so the next start can show them at once while
//! the Mac is still being reached. Whatever the Mac sends afterwards replaces all of it, so a stale copy only ever
//! shows for a moment. Two files in the cache folder, each tagged with the account (`user@host`) and a format
//! version; one that doesn't match is ignored.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{Value, json};

use crate::mac::{Contacts, parse_contacts};

const VERSION: i64 = 1;

/// The saved chat list and the previews of its rows.
pub struct Chats {
    pub chats: Vec<Value>,
    pub previews: HashMap<i64, Value>,
}

/// Write through a temporary file, so a quit half way never leaves a torn file behind.
fn write_atomic(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn read(path: &Path, who: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    (v["v"].as_i64() == Some(VERSION) && v["who"].as_str() == Some(who)).then_some(v)
}

/// Save the chats and the previews of those chats (`keep` = every chat id a preview may be filed under).
pub fn save_chats(
    path: &Path,
    who: &str,
    chats: &[Value],
    previews: &HashMap<i64, Value>,
    keep: &HashSet<i64>,
) {
    let previews: serde_json::Map<String, Value> = previews
        .iter()
        .filter(|(id, _)| keep.contains(id))
        .map(|(id, m)| (id.to_string(), m.clone()))
        .collect();
    let doc = json!({"v": VERSION, "who": who, "chats": chats, "previews": previews});
    write_atomic(path, &doc.to_string());
}

/// Save the Mac's raw contacts text (what `parse_contacts` reads), photos included.
pub fn save_contacts(path: &Path, who: &str, raw: &str) {
    write_atomic(
        path,
        &json!({"v": VERSION, "who": who, "text": raw}).to_string(),
    );
}

/// Read both files; either may be missing or out of date, which just means a cold start for that part.
pub fn load(
    chats_path: &Path,
    contacts_path: &Path,
    who: &str,
) -> (Option<Chats>, Option<Contacts>) {
    let chats = read(chats_path, who).and_then(|v| {
        let list = v["chats"].as_array()?.clone();
        let previews = v["previews"]
            .as_object()?
            .iter()
            .filter_map(|(id, m)| Some((id.parse().ok()?, m.clone())))
            .collect();
        Some(Chats {
            chats: list,
            previews,
        })
    });
    let contacts = read(contacts_path, who)
        .and_then(|v| v["text"].as_str().map(parse_contacts))
        .filter(|c| c.person_count() > 0);
    (chats, contacts)
}

/// Delete the saved copy.
pub fn clear(paths: &[&Path]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(p.with_extension("tmp"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_copy_comes_back_for_the_same_account_only() {
        let dir = std::env::temp_dir().join(format!("bf-snapshot-test-{}", std::process::id()));
        let (chats_file, contacts_file) = (dir.join("chats.json"), dir.join("contacts.json"));
        let chat = json!({"id": 4, "identifier": "+15550101234", "participants": ["+15550101234"]});
        let previews = HashMap::from([
            (4, json!({"text": "hi"})),
            (99, json!({"text": "not kept"})),
        ]);
        save_chats(
            &chats_file,
            "me@mac.local",
            &[chat],
            &previews,
            &HashSet::from([4]),
        );
        save_contacts(
            &contacts_file,
            "me@mac.local",
            "N|1:1|Ann Smith|\nK|1:1|+15550101234|mobile",
        );

        let (chats, contacts) = load(&chats_file, &contacts_file, "me@mac.local");
        let chats = chats.expect("chats");
        assert_eq!(chats.chats.len(), 1);
        assert_eq!(chats.previews.len(), 1);
        assert_eq!(chats.previews[&4]["text"], "hi");
        let contacts = contacts.expect("contacts");
        assert_eq!(
            contacts.person("+15550101234").map(|p| p.name.as_str()),
            Some("Ann Smith")
        );

        let (chats, contacts) = load(&chats_file, &contacts_file, "me@other.local");
        assert!(chats.is_none() && contacts.is_none());

        clear(&[&chats_file, &contacts_file]);
        let (chats, contacts) = load(&chats_file, &contacts_file, "me@mac.local");
        assert!(chats.is_none() && contacts.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
