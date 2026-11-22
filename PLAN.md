# BubbleFairy plan

BubbleFairy is an open-source (MIT) Linux desktop app for iMessage. The Mac does all of the iMessage work. BubbleFairy is the window on Linux that ferries messages to and from it over the local network.

Status (2026-11-21): Phases 1, 1.1, 1.2, the contacts integration, the setup wizard, the app icon, the tray icon and the release workflow (v0.2.1 is out) are built and tested against the test Mac. What's left is a few small gaps (see the end of each section's "Not done" notes) and the Flathub submission. Features built beyond the original plan (details in `CLAUDE.md`'s Layout notes):

- Viewer: video files play in it (copied from the Mac with progress, GStreamer player), YouTube links play in an embedded WebKitGTK view, full screen, prev/next across photos, videos and YouTube (a slide transition for pictures), autoplay only for the item you clicked.
- Info panel: Photos, Videos (video files plus YouTube links), Files and Links tabs, constant width (4 photo columns), contact cards and maps.
- Chat: a scroll anchored to the bottom, a jump-to-newest button, file, video, link and photo-stack cards, a responsive sidebar (when narrow: avatars with their names under them, in rounded rectangles like the pins).
- New message: picking or typing a contact opens the existing conversation with that person (any of their addresses, found on the Mac even when older than the chats list's 100), and the sidebar glides to it.

### Not yet checked on a real run

- Clicking a desktop notification opens its chat: checked 2026-11-20 in the installed Flatpak (it did not work under the dev sandbox).
- A group with more than four people has now been seen in the real app: header avatar, "+N" pill, people popover with the contact card swap.
- The viewer's YouTube player and video playback inside the real Flatpak (WebKit's own sandbox warns it "is not usable" under `flatpak run`; audio needs the PulseAudio socket, which `scripts/dev.sh` now grants).

## Decisions

| Topic | Decision | Why |
|---|---|---|
| Language and toolkit | Rust + GTK4 (gtk4-rs), with our own CSS styling. libadwaita only if it earns its place (dark mode, window chrome). | Native rendering, fast and light. Colour emoji, the emoji picker and all keyboard input methods work out of the box. CSS gives us our own look. A webview (Tauri) was rejected for weight, and egui for its emoji and input gaps. |
| Desktops | Tested on KDE Plasma and GNOME, best-effort elsewhere. | GTK apps run on every desktop. The look is our own, so it doesn't have to match any one desktop's style. |
| Distribution | Self-contained Flatpak, ID `io.github.theteknocat.BubbleFairy`. Flathub once Phase 2 works. | A single package that works on every distro, including immutable ones like Bazzite. |
| SSH | A pure-Rust SSH library (`russh`) compiled into the app. BubbleFairy has its own ed25519 key, stored in its data folder, and never reads or writes `~/.ssh`. | No `ssh` binary is needed inside the sandbox and no `~/.ssh` permission is needed. The key can be revoked on its own. Terminal access to the Mac is out of scope. |
| Mac side | The imsg CLI (MIT, <https://github.com/openclaw/imsg>), pinned to a tested release. BubbleFairy installs its own copy and always uses it, never Homebrew or an existing install. | Every user runs the version we tested. Upgrading, or moving to a fork, means changing the pin in a BubbleFairy release. |
| Transport | One SSH channel running `imsg rpc` (JSON-RPC 2.0, one JSON object per line). `watch.subscribe` delivers live messages on the same pipe. | One process instead of one per command plus a separate watch stream. Reconnecting is simpler. |
| Minimum macOS | 14 (Sonoma), because imsg requires it. Tested on 26 (Tahoe). | |
| Repo | `theteknocat/bubblefairy`, private until the first early release, MIT license. | |

## Architecture

```text
BubbleFairy (GTK4, Flatpak)
  └─ russh, key auth ──> Mac sshd (Remote Login)
                          └─ <app dir>/imsg rpc
                               ├─ reads ~/Library/Messages/chat.db (needs Full Disk Access for remote users)
                               └─ sends through Messages.app with AppleScript (needs the Automation permission)
```

- Mac install dir (planned): `~/Library/Application Support/BubbleFairy/` (the imsg binary and its version marker). Already used: `outgoing/` for files being sent.
- Beyond imsg, the app runs macOS's built-in tools over separate SSH channels, under the same login and Full Disk Access: `sqlite3 -readonly` on `chat.db` (per-message service, corrected unread counts, link-preview data) and on the Contacts databases (photos); `qlmanage` and `sips` (photo conversion and rotation, preview images); `cat`/`mkdir`/`find` for outgoing files. imsg is the stable layer; these depend on Apple's database layout and are written to fail soft (plain colour, plain text, initials).
- Linux app data: the XDG config and data dirs, which `glib` provides. Inside the Flatpak these are `~/.var/app/<id>/`.
- The GTK main loop runs the UI. Networking runs on a tokio runtime, because russh needs one, and sends its results back to the UI over a channel. The UI thread never waits on the network.

## Phases

### Phase 1: works for Peter (run from source)

The connection settings (host, user and imsg path) are entered once in a simple settings view. The app already uses its own ed25519 key (its public key is shown there to add to the Mac's `authorized_keys` by hand) and the imsg installed by Homebrew. Phase 2 automates the key install and replaces Homebrew's imsg with the app's own copy.

- Chat list: avatar with initials, name, last message, time, unread dot.
- Conversation: history with timestamps between groups of messages, and live incoming and outgoing messages.
- Sending.
- Bubbles: blue for iMessage, green for SMS/RCS, grey for incoming. Consecutive messages are grouped, and bubbles have tails.
- Images shown inline (received and sent): several photos in a message become a card stack that spreads on hover to the chat's width; clicking opens a full-panel viewer (edge-to-edge, one photo per wheel notch or arrow key, controls that fade out, the 4096 px original fetched in the background with a spinner, stops at the earliest loaded photo).
- Desktop notifications (`gio::Notification`, which goes through the portal inside the Flatpak).
- Auto-reconnect, with a visible connection state.
- Follows the system light/dark setting.

### Phase 1.1

- New 1:1 conversations: a "new message" button (Ctrl+N) swaps the conversation header for a To field. As you type, it suggests people you already have chats with (picking one opens that chat). Otherwise it takes a phone number or email, and the first message goes out as `send {to, service: "auto"}`; the app opens the new chat once it appears. Suggestions come from existing chats only; the full address-book search is in the contacts integration section below.
  - New group chats aren't possible: imsg's `chats.create` needs its bridge, which needs SIP off (out of scope).
- Read state, kept locally. Chats read on other devices already clear (see imsg facts: `mac::fix_unread`, with the chat list refreshed every 60 s). Chats read only in BubbleFairy can't be marked read on the Mac: imsg's `read` needs SIP off, and Messages has no AppleScript for it. So the app remembers, per chat, the newest message seen in BubbleFairy, and hides the dot until something newer arrives. Nothing goes back to the Mac.
- Sending attachments (built): paperclip button, drag and drop onto the conversation, or paste (files or an image). Waiting files show as removable cards above the composer. Each file is copied over SSH to `~/Library/Application Support/BubbleFairy/outgoing/<random>/` on the Mac (copies older than a day are cleared on the next upload) and sent as its own `send {file}`, then the text. VERIFY: Messages can read files from that folder.
- Showing other received attachment types (built): link-style cards for files with their kind, length and size, large first-frame previews for videos, middle-truncated names; a video plays in the viewer, any other file is copied over SSH and opened.
- Contact photos as avatars (built for the chat list, header, group senders and the To suggestions): `mac::fetch_contacts` reads every `AddressBook-v22.abcddb` once per connection (`ZTHUMBNAILIMAGEDATA`: type byte 1 = the picture follows; type 2 = a file name in the database's hidden `_EXTERNAL_DATA` folder, usually a full-size original that the Mac shrinks to 160 px with `sips`), matched to a person by email or the last ten digits of a phone number. No photo means coloured initials, or a generic person for numbers. Group chats show their members' circles inside one disc (equal sizes; Messages may size them by member order), with three circles and a "+N" circle past four people; the conversation header shows them as an overlapping stack instead. Not done: photos people share through Messages' Name & Photo (Memoji etc., in `~/Library/Messages/NickNameCache`, format unchecked), group chat photos, and refreshing photos while the app runs (restart to pick up changes).
- Link previews:
  - Built: Apple's own saved preview (title, site, image) is read from chat.db's `payload_data` over SSH (`mac::link_preview`, using the `plist` crate) and drawn as a card; clicking opens the link. The site icon isn't drawn yet.
  - Built (2026-11-18): our own preview for a single plain link Messages made no preview for (`web_preview`/`request_web_preview`/`fetch_web_preview` in ui.rs, parsing and URL checks in `webpreview.rs`, with tests). Setting "Make previews for links Messages didn't" (Settings > Privacy, off by default, config `link_previews`); this computer fetches the page (libsoup through `webkit6::soup`, 10 s timeout, UA `facebookexternalhit`-style), only public http(s) addresses; title, site name and picture from the Open Graph tags (the picture's size read from its header, `image_size`), YouTube links included (the card plays in the viewer like Messages' own). Results are kept in `data/bubblefairy/web_links` and the pictures in the image cache (`web:<url>` keys). Dev: `BF_LINKS=1`, `BF_DEMO_WEBLINK=<url>`. Decided against previews for links in the middle of text or several links in one message (Messages doesn't either). Not done: the page's own icon. Earlier note: links with no saved preview (the sender's device didn't make one) showed as plain text. BubbleFairy makes its own preview card for them (title, site, picture from the page's Open Graph tags), behind a privacy setting. It is local only: whoever receives a link we send won't see a preview on their device. **Known limitation, accepted:** previews of links we send can't be made for the recipient. The plain `imsg send` goes through AppleScript and creates a plain-text message (checked in chat.db on 2026-11-17: links sent from our app have no `URLBalloonProvider` row or payload; links sent from Messages' own UI do). imsg 0.21's `send-rich --url` can send a real preview, but only through its IMCore bridge, which needs SIP turned off on the Mac. We don't use it and never ask users to disable SIP. Open question for the build: fetch the page from Linux or from the Mac's `curl` (either way, the linked site learns that address).
  - Layout follows Messages: a preview picture of 400x200 px or more goes across the top in its own proportions (never cropped); a smaller one (a site logo) sits beside the title as an icon. Messages saves no "is this a logo" flag, so this is a size rule. The card is off-white in both colour schemes.
  - Never fetch automatically for links from unknown senders, because that reveals the user's IP address to the link's owner.

### Phase 1.2: conversation info (built)

- An info button in the conversation header opens a panel over the chat's right side: participants and contact cards (phones, emails, addresses with maps, birthday), and Photos, Videos, Files and Links tabs for the whole chat, read from chat.db over SSH (imsg has no attachments-only query) and paged ("Show more"). Details are in `CLAUDE.md`.
- The viewer walks the loaded photos, or the loaded videos and YouTube links, of the tab it was opened from. It never loads older history on its own (a design decision).

### Pinned conversations (built)

- Our own per-instance list (Messages' pins can't be read from the Mac and don't sync between devices): `data/bubblefairy/pins`, one chat `identifier` per line in pin order. Pin and unpin from the right-click menu on a list row or a tile.
- Pinned chats show as avatar tiles above the list (name under the avatar, unread dot, group avatars as in the list), three per line; people show their first name only. In the narrow sidebar they become a column of the same rounded rectangles as the list rows, with the same side margins. Drag a tile to reorder: the others slide aside and the order is saved (`src/tilegrid.rs`). Pins and mutes match any of a merged conversation's chats. A pinned chat's list row stays in the list, hidden, so row indexes keep matching the chat list.
- Dropped on purpose: hover pin button (tried; GTK's negative margins and sibling draw order made it fiddly). Drag to reorder was dropped at first and built later (2026-11-18).

### Group chats (built 2026-11-18)

- A group without a name is titled by first names, known contacts first (ranked by how recently you wrote to each person one to one), numbers and emails last: "Jo & Kari", "Jo, Rob & Drayven". Over four people the header reads "Name1, Name2, Name3 & N others" (or "Chat with N People") with the shared group avatar and a purple "+N" pill; clicking it opens a list of the people, and a person opens their contact card with a back button. One-person chats keep the full name in the list and header (first name only under a pin).
- The header's second line is only the service ("Text Message · RCS", "iMessage"). Date headings: weekday for this year, `MMM d, YYYY` before.

### Times (built)

- One scheme everywhere (`rel_time`, `TimeKind`): the chat list and "Read" lines show `17:38` today, `Yesterday`, a weekday for 2-6 days ago, `Nov 8` this year, `Nov 8, 2025` before; the separators in a chat show `Today 17:38`, `Yesterday 17:38`, `Mon 17:38`, `Nov 8, 17:38`, `Nov 8, 2025, 17:38`; tooltips keep the full date and time. Every such label is tracked (`track_time`) and rewritten in place at midnight (`schedule_midnight`) or when the window becomes active after a day change, with no rebuild.

### Small features (built)

- Hovering a message shows its full date and time; the window title carries the number of unread conversations (`(3) BubbleFairy`); each chat keeps its own unsent text and waiting files while you look at another (in memory only, lost on quit); the chat names its service ("iMessage", "RCS", "Text Message") at the start and wherever it changes.
- Sidebar search (Ctrl+F) also finds chats older than the loaded list, by group name or by a contact's name (`Cmd::FindNamed` → `mac::find_named`, 3+ characters, at most 15 chats, added to the list like a found chat; re-run when the Contacts arrive). Filters the list by name, a group member's name, or part of a number or address; pinned chats show as ordinary rows while searching; hidden in the avatars-only sidebar.

### Delivery line (built)

- Under my newest sent message in the open chat: "Read 10:32" (date when not today; only when the recipient sends read receipts) or "Delivered". Data is chat.db's `is_delivered`, `date_delivered`, `date_read`, read over SSH by `Cmd::Receipts` (`mac::parse_receipt`), asked when a chat's history arrives and every 10 s while the window is active; `Event::Receipt` redraws just that message. `is_delivered` counts as delivered even where `date_delivered` was never filled in (it is sparse). RCS and SMS rows rarely carry a delivered flag, so those show nothing until read.
- Under ANY message of mine that failed: a red "Not delivered" (chat.db `message.error` non-zero, added to every message by `add_services`; imsg's JSON has no such field). Checked against a real iPhone (a photo with error 35 shows "Not Delivered" there).
- A failed message also moves in from the edge for a red "!" button; its popup says "This message (photo, video, file) was not delivered." with Try Again, which sends the text and the attachments that are already on the Mac (`Cmd::Resend`, paths from imsg's `original_path`). As in Messages the failed message stays in the chat (chat.db keeps it with its error, and imsg still returns it): while the retry runs it is a ghost with a spinner in place (no "!" or "Not delivered"), then it comes back as a failed one with an outline "!" (`State.resent`, file `data/bubblefairy/resent`) and the new message appears at the end. A retried picture reuses what the chat already shows (`PendingItem::Seed`), no fetch back. Not done: "Send to email address / as text message". Checked on a real Mac for a picture: Try Again delivered it. Messages.app itself retries on the same chat.db row (clears `error`, may switch it to SMS, moves the date); we can't (imsg's `delete-message`/`edit` need the IMCore bridge, which needs SIP off), so the old row stays failed on every Apple device and the popup says so. The composer's service label ignores failed messages (a failed SMS retry left the chat labelled SMS).
- "Notifications silenced" was tried as the line for `was_delivered_quietly` and was wrong: the phone said "Delivered" for such a message. The quiet flag is now only a tooltip ("Delivered quietly…"), since the recipient's Focus at the time can't be known.

### Candidate features (investigated, not built)

- **Notifications with sound: built (2026-11-18), sounds tested by ear.** `notify` (ui.rs) honours Settings > Notifications (show / sound / which sound / what the notification shows: sender and message, sender only, nothing), sends the `gio::Notification` (portal in the Flatpak, so the desktop's own per-app controls and do-not-disturb apply; id per chat so a chat's notifications replace each other) and plays the sound itself with GStreamer (`sound.rs`): six original synthesised chimes in `data/sounds` (made by `scripts/make_sounds.py`, committed, no licence to carry; the GNOME runtime has no sound theme) or the user's own audio file ("Custom file…"). Nothing plays for the chat you are looking at in the active window, not within 1.5 s of the last sound, and not while the desktop's `org.freedesktop.Notifications` `Inhibited` is true (KDE; others say nothing, so the sound plays). Config keys `notify`, `notify_sound`, `sound` (`chime`… or `file:<path>`), `notify_text`. Dev: `BF_SOUND=<name>`, `BF_SOUND_LOG=1`, `BF_DEMO_INCOMING=<ms>`. Not done: the Flatpak must install `data/sounds` under `share/bubblefairy/sounds` (like the icons); per-chat mute (chosen: global only); sounds were checked only for reaching the end of playback, not by ear; the notification's own icon and the desktop entry the desktop lists the app under belong to the packaging step.

- **Delivery line: what can't be had.** Without the bridge (SIP off) there are no typing indicators (sending or seeing), no sending of read receipts, no "Notify Anyway", and no seeing a contact's Focus before you send (that status isn't in the database).
- **Show reactions (tapbacks) on messages. Built:** `reactions_pill` (ui.rs) draws a pill of the distinct emoji (count when more reactions than emoji, tooltip says who) over the first part's top corner, on the side away from the tail; a live reaction row triggers `Cmd::Recent` (last 10 messages), so a reaction to an older message shows after a reload. Dev: `BF_DEMO_REACT=1`. Original note: the app skipped reaction rows (`render_conv`, `is_reaction`), so they're invisible. imsg's history already puts a `reactions` list (emoji, type, sender, is_from_me) on the message each one targets, so it needs no new Mac queries; draw a small emoji cluster on the bubble's corner (who reacted in a tooltip). Custom-emoji reactions are in the data too. A reaction arriving live is its own message event: refresh the target's list (e.g. re-fetch the chat's recent history).
- **Sending reactions: decided against (UI automation not wanted).** `imsg react` works without SIP but drives Messages through UI automation: it can only react to the chat's last (or selected) message, needs Messages running with the chat open and Accessibility permission for the SSH session, a retry after a failed confirmation can toggle the reaction off, and custom emoji can't be sent. Targeting a specific message needs the bridge (SIP off). The only honest version is a tapback button on the newest message with the six standard reactions.
- **Save attachments (download icon). Built:** `save_button`/`save_items`/`queue_save` (ui.rs) put a round save button beside each photo/stack, video and file (not for ones missing on the Mac); it saves the original from the Mac (`Cmd::Download` into the cache, then a copy) to the Settings folders (`dir_images` for photos, `dir_files` for the rest; empty = save dialog, several photos = folder chooser), numbering names that exist (`unique`), and turns into a green check for 3 s (`SaveDone`). A default folder that is no longer writable falls back to the dialog with a note. Right-click on a photo: Copy image (the chat copy's texture) and Save as… (always the dialog). Settings has the "Default download folders" group; the chooser gives a Flatpak app a document-portal path (`/run/user/1000/doc/<id>/Folder`) which is what we store and use; the field shows the real folder from the `user.document-portal.host-path` attribute. Original note:  Like Messages: a small download icon just to the right of a received photo, video or file bubble, and for sent ones just to the left of the bubble (Messages doesn't offer it for sent items, but we do). It saves a copy to a chosen place (a save dialog starting in the Downloads folder), using the same copy-over-SSH route as opening a file (`start_download`, the cache's `files/` folder). Replaces the old "save a file from the Files tab" polish item; the Files tab could get the same icon.
- **Search inside messages. Built:** the sidebar search also looks through messages (300 ms after typing, 2+ characters, spinner on the field): `Cmd::Search` reads chat.db directly (imsg's search took ~6 s: it looks up a contact per hit), incl. text kept only in `attributedBody` (over half of all messages; `match_sql` matches only the text span of the blob, after `NSString` up to the first 0x86 byte, so markup words don't hit; the newest 400 candidates are verified on the decoded text, 100 hits kept). "N matches" in the sidebar is a grouped count over everything (`search_counts`); a chat whose matches are all older than the newest 100 still gets a row, and opening a chat asks for its own list (`Cmd::ChatHits`, up to 500). Chats with hits join the list with the newest hit as preview ("N matches · …"), words highlighted in titles, previews and messages (`highlight`, Pango attributes; sand colour, bright orange for the hit jumped to). Opening such a chat loads a window around the hit (`Cmd::Window`, `State.find_view`), a bar shows "2 of 18" with older/newer/Latest/close, and scrolling down loads newer pages (`Cmd::Newer`, `page_after` bounds the page by the n-th later row's date so no gap). Hits in chats older than the loaded list get rows too (`Cmd::FindChats` fetches those chats; the ones a search adds are dropped again when it is cleared). While results show, nothing is open on the right; clearing brings the previous chat back, or keeps the one opened from the results (its row stays hidden until scrolling loads it, like Messages). Esc closes the bar. Not done: filenames and URLs are only found when they are in the message text. Original note: The The sidebar's search box (built) filters conversations by name or number only. Searching message text can use imsg's `messages.search` (RPC method in 0.21); results would need their own view.
- **Merged conversations, group events, "Sent with Siri" (built 2026-11-18):** see CLAUDE.md. Messages merges a person's chats (checked on iOS); group chats are left unmerged (not verified). Group events (joined/left/renamed) show as lines and are ignored for a chat's date, preview and order, as on the iPhone. Sent-with-Siri is `amsa = com.apple.siri` in `message_summary_info`, shown on received messages only.
- **Inline replies: display built (2026-11-18).** `conv_widgets` draws, like Messages, the original in a small outlined bubble (`clip_words`, 160 chars) above the first of a run of replies, a bar (`thread-bar`) down to them, and "N Replies" under the original (click scrolls to the first reply); originals not loaded show "Earlier message". Dev: `BF_DEMO_REPLY=1`. Sending a threaded reply isn't possible without the bridge (SIP off); decided to leave it, see `docs/LIMITATIONS.md`.
- **Copy and save for pictures:** a right-click menu on a photo with "Copy image" and "Save as".
- **Muted chats: built as our own list (2026-11-18).** Messages' mute list (`CKDNDList.plist`) is only half readable: groups are keyed by `chat.group_id`, but 1:1 chats use 40-hex hashes that matched none of SHA-1/SHA-256/MD5/SHA-512 over identifiers, GUIDs and phone forms. So muting is BubbleFairy's own: right-click a row or tile → Mute/Unmute, stored in `data/bubblefairy/muted` (chat identifiers, like pins); `notify` returns early for a muted chat (no popup, no sound); the row shows 🔕. Doesn't sync with Apple devices.
- **Spam and unknown-sender filtering: checked, nothing to build (2026-11-18).** Blocked senders (the Mac's list is in `com.apple.cmfsyncagent`) are dropped by Messages before chat.db: only a few have a chat, each with 0-2 messages, the newest years old. `message.is_spam`, `filter_action` and `chat.is_blackholed` are 0 on every row and `is_pending_review` on every chat. `chat.is_filtered` = 1 (most chats on the test Mac) is Messages' Unknown Senders filter; it isn't a reliable spam signal (a flagged chat can have 60+ messages). Decision: leave the app as is. Blocking and the Unknown Senders choice stay in Messages on the Mac, and the app respects whatever Messages does. Principle for the help dialog: BubbleFairy is a front end for Messages, not a replacement; settings like blocking, filtering unknown senders and the Mac's mute list are managed in Messages on the Mac or Apple devices.
- **Our own link preview card for plain links**: see Link previews under Phase 1.1.

### Polish (later)

- Done 2026-11-18: header title/subtitle and all contact card text are selectable; contact cards have a copy button on each number and email. Info panel tiles and sidebar rows stay unselectable on purpose (clicking them does something else). Selectable labels show no caret or focus ring after a click (CSS).
- Files: saving to a chosen place is covered by "Save attachments" under Candidate features.
- The control-layout question for the viewer is settled: photos and video files use the shared control bar and fading buttons; YouTube keeps its own player controls with our buttons in a strip above it that shrinks away when they hide.

### App icon (done 2026-11-20)

Chosen: variant B3 — a glossy blue bubble on a lavender rounded tile with a gold star on a wand handle (down to the right), in the wizard fairy's style (navy outlines, gloss, glow). Files: `data/icons/hicolor/scalable/apps/io.github.theteknocat.BubbleFairy.svg` and `...-symbolic.svg` (checked at 16/32/64 px; the symbolic one is a bubble with a star cut-out). Other variants (fairy peeking, fairy holding the wand to the bubble) were made and set aside. Still to do: the Flatpak must install them, and the window/desktop entry must use the ID.

Header title (done 2026-11-20): the cropped bubble-and-wand mark (`bf-mark.svg`, no tile) beside the word mark, "Bubble" in blue (reinforces Apple's blue bubbles without saying so) and "Fairy" in pink, also in the Settings header.

### Contacts integration (read-only; built except where noted)

The Mac's Contacts databases are readable over SSH (on the test Mac, with names, organisations, phones, emails, addresses, birthdays, notes and photos), through the same Full Disk Access as chat.db. Editing isn't possible, and isn't wanted. Planned uses:

- Starting a new chat: the To field searches the whole address book by name, number or email, and picks a specific number or email when a contact has several. (Replaces the current suggestions from existing chats only. Earlier notes saying imsg can't list contacts were about imsg; reading the database directly can.)
- Names for numbers and emails that imsg doesn't name: unsaved senders in group chats, unfamiliar conversations.
- Contact cards in the conversation info panel (Phase 1.2): phones, emails, address, birthday.
- Address maps (built): a contact card shows a small OpenStreetMap tile with a pin under each postal address, and clicking it opens the default maps app (`geo:` link, falling back to openstreetmap.org). The Mac's curl does the lookups (Nominatim for the coordinates, then one tile), so the app needs no HTTP library; results are cached on Linux in the image cache as `<hash>.map` files (these say where a contact lives, so treat them as sensitive). **Built: Settings > Privacy has the toggle (`map_previews`, default off: privacy settings are opt-in only; the off state shows a "Show on a map" button whose tooltip says where to enable inline maps) with a note that it uses OpenStreetMap and sends the address there; off = a plain button that opens the address in the maps app.** Nominatim's policy limits the public server to 1 request a second with an identifying User-Agent (done in `mac::map_for`).
- Risk: the queries depend on Apple's database layout, which changes between macOS versions. If one breaks, avatars fall back to initials and the search to existing chats. If the address book is ever cached on disk, treat that file as sensitive.

### Phase 2: setup wizard (no terminal)

**Built 2026-11-19 (`src/setup.rs`), tested end to end against the test Mac:** first run opens the wizard (and Settings has "Set up again…"). Pages: find the Mac (mDNS scan of `_ssh._tcp` into an editable combo: typing completes a found name, a picked Mac locks the field until cleared, a typed name is probed when the field loses focus, green check or red message) → Mac user and password (used once, never stored) → the host key's fingerprint (accepted = pinned in `mac_host_key`) → automatic steps with a progress list (sign in, check macOS 14+ and `sqlite3`/`qlmanage`/`sips`/`unzip`, add the app's key to `authorized_keys`, check the key signs in, install our own pinned imsg into `~/Library/Application Support/BubbleFairy/imsg/`, check chat.db is readable, with the Full Disk Access instructions and "Check again" when not) → optional test message (asks for a number or email, warns to watch the Mac for the Automation prompt) → done (saves host, user and the imsg path). Decisions: we always install and use our own imsg (never an existing or Homebrew one); the zip comes from a bundled copy when the build has one (`/app/share/bubblefairy/imsg-macos.zip`, or `vendor/imsg-macos.zip` from source) and is otherwise downloaded; either way its SHA256 must match the hash in the app (pinned to 0.21.0, a universal Intel + Apple Silicon build). Whether Flathub accepts the bundled zip is unsettled (see Flathub's requirements: "built entirely from source", no binaries in the PR; a manifest `file` source with a hash isn't in the PR); the download fallback keeps us independent of that. Dev profile: `BF_SETUP_TEST=1` (throwaway key, `BubbleFairy-test` folder on the Mac, nothing saved, key removed again after it is verified). Tried against the real Mac with `BF_SETUP_KEYLOGIN=1` (signs in with the existing key instead of the password): every step passed. Not yet tried: password sign-in, the fingerprint page in use, the test message.

Settings (built): two tabs, the app's own settings and a read-only "Mac connection" tab (Mac, user, imsg location and version, fingerprint) with "Set up again…"; no key-copy command, no "Forget" button. Decided: "Set up again…" covers "Forget this Mac".

Original plan, for reference:

The user does only these things on the Mac:

1. Turn on Remote Login.
2. Tick "Allow full disk access for remote users".
3. Click Allow on the Automation prompt the first time a message is sent.

The wizard does everything else:

1. Finds Macs on the network over mDNS (`_ssh._tcp`, using the `mdns-sd` crate), or takes a hostname typed by hand.
2. Asks for the Mac username and password, used once.
3. Shows the Mac's host key fingerprint and pins it.
4. Checks that the macOS version is 14 or newer.
5. Generates BubbleFairy's key and appends it to the Mac's `~/.ssh/authorized_keys` (directory mode 700, file mode 600).
6. Downloads the pinned `imsg-macos.zip` on the Mac, checks its SHA256 against the hash built into the app, and unpacks it into the app dir.
7. Checks that chat.db and the Contacts databases can be read, and that `sqlite3`, `qlmanage` and `sips` exist. If a database can't be read, it shows how to tick the Full Disk Access box, then checks again.
8. Sends a test message (optional). It warns the user to watch the Mac for the Automation prompt.
9. Flatpak packaging and a GitHub Actions release workflow (push a tag `vX.Y.Z` to build the Flatpak bundle), then submission to Flathub.

Flatpak needs found while building (not yet in a manifest): the PulseAudio socket (video and YouTube audio), the GNOME runtime's WebKitGTK 6.0 (the `webkit6` crate), GStreamer's `gtk4paintablesink` for local video, our icons from `data/icons` installed under `share/icons`, network access, and Wayland/X11 and DRI. Check that WebKit works without its own sandbox when it runs inside Flatpak.

### Later

Tray icon (built 2026-11-21, needs checking in the installed Flatpak on KDE): shown automatically when a tray host is on the bus; Settings > Background has only "Start hidden in the tray when I log in" (`autostart`; "Start minimized" when there is no tray). `src/tray.rs`: a StatusNotifierItem through `ksni` (`disable_dbus_name` in a sandbox, Flatpak needs `--talk-name=org.kde.StatusNotifierWatcher`), a menu with Open / Quit, a click toggles the window, the unread dot is the NeedsAttention state with its own icon. `ui::start_tray` makes the item once and holds the app alive; the window's close hides it while the tray runs (first time: a notification says so). Login entry through the Background portal (`ashpd`, command `bubblefairy --tray`; `--tray` is removed before GTK sees the arguments). A login start retries the tray for ~10 s, then opens the window minimized. No tray: closing quits. Decided against a YouTube privacy setting (a click starts it, and the embedded player tells YouTube less than a browser).

Photos people share through Messages' Name & Photo. A Mac companion app is a possibility. An iPhone-only route is unlikely, because iOS gives third-party apps no access to iMessage (not researched).

## Design direction

- Unique. It must not imitate Apple Messages or Liquid Glass.
- Dimensional, not flat: layered shadows, bevels, rounded corners, gradients, and glassiness where it fits. Subtle and tasteful, never over the top.
- Friendly, bright and inviting, not institutional and not like a toy. Dark mode should be just as bright and inviting.
- The sibling project to draw on is airscope: its effects live in `~/system-troubleshooting/airscope/src/fx.rs` and its style notes in `docs/STYLE.md`.
- GTK4 CSS has no `backdrop-filter`. Glass is suggested with translucent gradients and edge highlights. Real blur is used only for the app's own content scrolling under a panel (GTK's snapshot blur).
- Contrast: the blue and green sent-message bubbles are deep enough for white text (about 4.4 to 6:1; they were 2.5:1 and 1.7:1 at the light end). Like iOS's increased contrast, but always on.
- Dates: `MMM d, YYYY` with 24 h times, to match airscope. Change this if Peter prefers otherwise.

## imsg facts (verified on 0.21.0)

- `chats` gives `id` (the rowid used everywhere), `guid`, `identifier`, `participants[]`, `contact_name` (1:1 chats only), `display_name` (often empty), `is_group`, `service` (iMessage, SMS or RCS), `unread_count`, `last_message_at`.
- `history` returns messages newest first, with `id`, `guid`, `chat_id`, `is_from_me`, `sender`, `text`, `created_at`, `attachments[]`, `reactions[]`.
- RPC methods: `initialize`, `status`, `chats.list|get|create`, `messages.history|search|after|stats`, `watch.subscribe|unsubscribe` (sends `message`, `edited` and `unsent` events, and a final `watch.overflow`), `send`, `send.tracked`, `message.send_status`. Limits: 128 outstanding requests and 64 subscriptions. The bridge methods (typing indicators, read receipts, edits and so on) need SIP turned off, so they are out of scope.
- A 1:1 chat's `service` comes from chat.db's `chat.service_name`, which can be stale (a chat can say SMS while its recent messages are all iMessage). The app shows the latest message's service and sends 1:1 messages as `send {to, service: "auto"}` so Messages picks, as on the Mac. VERIFY: the first such send lands in the same conversation.
- imsg's `unread_count` counts messages with `is_read = 0`, but that flag can stay unset after a chat is read on another device. Messages goes by `chat.last_read_message_timestamp` too, so `mac::fix_unread` counts only messages unread by both. Read-state changes produce no watch event.
- Every release ships a signed `imsg-macos.zip` plus `SHA256SUMS`.
- Health on 2026-11-14: 1.4k stars, 199 forks, 15 releases between 2026-09-07 and 2026-11-05, and 100 commits in the 10 weeks before. The bus factor is 1: steipete has 367 commits and the next contributor has 31.

## Gotchas carried over from the proof of concept

1. Contact lookups are slow on the Mac. `chats` takes 0.69 s with them and 0.07 s without, and `history` 0.41 s and 0.08 s. Fetch history without contacts and build a handle → name map from the chat list.
2. Attachment-only messages have `text` = `"￼"` (the object replacement character).
3. Unnamed group chats have an empty `display_name` and an `identifier` like `chat7402…`. Build the title from participant names.
4. The first send triggers the Automation prompt on the Mac, and the send blocks until someone clicks Allow. The UI must say "check your Mac".
5. Non-interactive SSH doesn't load Homebrew's PATH. Always call imsg by its full path.
6. Quote every argument that goes through the remote shell. With `rpc`, message text travels inside JSON over the pipe and never reaches the shell.
7. Killing the SSH client used to leave `imsg watch` running on the Mac. `imsg rpc` exits when its stdin closes (documented), so closing the channel is enough.
8. Debounce history fetches when switching chats quickly (about 150 ms).

## To verify before or while building

Answered from imsg's `docs/rpc.md` and `docs/json.md` (v0.21.0):

- Messages carry no `service` field, so `mac::add_services` reads each message's service from chat.db (`sqlite3 -readonly`) and every bubble is coloured by its own service.
- Apple's link previews are exposed. When imsg folds the preview row into its text message, that message carries `url_preview` (and `balloon_bundle_id` = `com.apple.messages.URLBalloonProvider`). That's the source for Phase 1.1.
- `imsg rpc` exits cleanly when its stdin closes, so closing the SSH channel is enough and no wrapper is needed.
- Messages include `sender_name` (from Contacts), `edited_at`, `unsent_at` and `reply_to_guid`. Live updates add `type: edited|unsent`.
- Attachments have `original_path` on the Mac. Images are fetched over extra SSH exec channels (at most 6 at once; sshd allows 10): Quick Look (`qlmanage -t`, which applies HEIC rotation) then `sips` to JPEG, 1200 px for the chat and 4096 px for the viewer, cached on disk. Link-preview pictures arrive as `*.pluginPayloadAttachment` files (plain PNG/JPEG/ICO): they go through `sips` only, because Quick Look hangs on that extension, and `fetch_image` kills a stuck `qlmanage` after 8 s. Photos still need Quick Look, because `sips` ignores HEIC rotation (most test HEICs came out sideways).

## Risks

- imsg churns quickly and has one maintainer. Mitigation: the version pin, and the option to fork (it's MIT).
- Apple can change the chat.db or Contacts schema, or Messages' AppleScript support, in any macOS release. That breaks every tool of this kind; our own queries are written to fail soft.
- The Mac has to be awake and reachable. Outside the home network that means Tailscale or similar (future docs).
- Prior art: BlueBubbles and OpenBubbles. BubbleFairy differs in needing no server app on the Mac and in being a native GTK app on Linux.
