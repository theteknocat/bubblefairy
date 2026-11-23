# Ponytail audit, 2026-11-22 (round 2)

**Scope:** the parts of the app where content from other people reaches this computer or runs something here.
Read: in `src/ui.rs` the link and text rendering (`linkify`, `without_url`, `highlight`, `word_spans`, `snippet`, `fold`),
`open_link`, `chat_youtube`, `open_media`, `notify`, `deliver`, `show_notification`, `on_message`, `watch_for_preview`,
`watch_for_media`, `learn_names`, `fill_names`, the save code (`Save`, `save_name`, `save_items`, `queue_save`, `copy_to`,
`unique`, `open_file`, `file_dest`), the map buttons (`open_map`, the card's address row); `src/sound.rs`, `src/tray.rs`,
`src/main.rs` (config, cache pruning), and the YouTube and player parts of `src/viewer.rs` and `src/player.rs`.
Round 1 (`docs/ponytail-audit-2026-11-22.md`) covered `mac.rs`, `setup.rs`, `webpreview.rs`, `snapshot.rs` and the
link fetching.

**Load assumed:** one person on one desktop. What matters is what a message from someone else can make the app do.

**Checked and fine (no finding):**
- `linkify` escapes every piece of text it puts into Pango markup, and only turns `http://` and `https://` words into
  links, so a message can't inject markup or another kind of link.
- `highlight`, `word_spans` and `snippet` can't panic on odd letters: `fold` only changes a letter when the new one takes
  the same number of bytes, so every byte offset found is valid in the original text.
- The YouTube player only gets an id that `youtube_id` already checked (11 letters, digits, `-`, `_`), so nothing can be
  injected into the player's script. It runs in an ephemeral web session with no cookies kept.
- Map buttons URL-encode the address before building the `geo:` and web addresses.
- `sound.rs` turns a chosen file into a URI with `gio::File::uri`, so spaces and `#` in a path are fine; a missing file
  falls back to the default chime.
- `tray.rs` has no outside input. `prune_cache` only deletes files in the images folder.

## Findings

1. **Nice to have: a link preview payload could carry any kind of address** (`mac.rs parse_link`). Status: fixed.
   - Case: a preview's address comes from the sender's payload with no check, and a click on its card hands it to the
     desktop (`smb://...`, an app's own scheme). Tested by hand with Messages: an `smb://` link gets no card there
     (plain clickable text), and an `http://` link gets a card with the address kept as sent. So only a hand-built payload
     could do it. Messages' own links are not changed, and the plain-link cards only match `http(s)`.
   - Fixed: `parse_link` drops a payload address that isn't `http://` or `https://` (the vetting is where the card is
     made, not at click time). Test: an archive naming `smb://nas.local/share` gives no preview. Decided with Peter not
     to make other schemes clickable in text: they stay plain, copyable text.
2. **Should fix: the file name a sender gives is used as is when saving** (`ui.rs Save::new` to `unique`). Status: found.
   - Status note: fixed. One `safe_name(name, fallback)` helper replaces `/` and control characters and turns an empty,
     `.` or `..` name into `file`; `file_dest` (the cache copy, which had its own cleaning) and `Save::new` both use it.
     Test: `sender_file_names_stay_inside_the_folder`. Only a hand-built message could send such a name (normal senders
     can't), so this is cheap hardening, and the shared helper removes a duplicate.
   - Case: a name with `../` or a leading `/` would make `dir.join(name)` point outside the folder you chose, and the copy
     overwrites. In 15,005 real attachments on this Mac no name has a `/`, so Apple seems to clean them, but nothing
     here checks. `file_dest` already cleans the same name for the cache copy.
3. **Should fix: group events make a sound and a notification** (`ui.rs on_message`). Status: found.
   - Status note: fixed. `on_message` skips `notify` for a group event, through a small `is_group_event(m)` helper
     (`item_type` other than 0) that the preview check in the same function now uses too, so the rule is in one place.
     Test: `group_events_are_not_messages`.
   - Case: imsg's live feed delivers someone joining, leaving or renaming a group as a message with no text (checked on
     the Mac: rows 317703 to 317705 are such events). `on_message` calls `notify` for anything not sent by you, so each one
     plays the chime and shows an empty notification. Messages doesn't.
4. **Nice to have: opened files are never size-capped** (`main.rs prune_old_dirs`). Status: found.
   - Status note: fixed, as a 2 GB cap on top of the 30 days (Peter's choice: whichever limit comes first). `prune_old_dirs`
     now removes folders older than 30 days, then the oldest of the rest until the total is under 2 GB (`FILES_MAX`).
     500 MB like pictures was judged too low for videos. Test extended: an over-the-size case drops the older folder.
   - Case: videos and files copied for viewing are only removed after 30 days, with no size limit like the 500 MB cap on
     pictures.

## Result of round 2

Four findings: 3 should fix, 1 nice to have (finding 1 was downgraded after Peter tested what Messages does). All four fixed.
Tests: 30 pass (new: `sender_file_names_stay_inside_the_folder`, `group_events_are_not_messages`, a non-web preview case
in `link_preview_archive`, an over-the-size case in the files pruning test). Clippy clean. Not tested live: a real group
event notification, a multi-gigabyte files folder.

## Not read (round 3)

`src/ui.rs`: `build` and the window layout, `render_conv` and `message_parts` (drawing a conversation), the settings
dialog, the sidebar list, pins and tiles, the search code, the info panel, the contact card body, the new-message view,
`relayout`. `src/viewer.rs` apart from YouTube, `src/player.rs` apart from opening a file, `src/bubble.rs`,
`src/chatscroll.rs`, `src/tilegrid.rs`, `src/pilllabel.rs`, `src/demo.rs`, the rest of `src/setup.rs` (the wizard's
window code; its backend was read in round 1).
