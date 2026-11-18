# BubbleFairy plan

BubbleFairy is an open-source (MIT) Linux desktop app for iMessage. The Mac does all of the iMessage work. BubbleFairy is the window on Linux that ferries messages to and from it over the local network.

Status (2026-11-17): Phases 1, 1.1 and 1.2 are built and running from source against Peter's Mac, and so is the contacts integration. What's left is the app icon, the polish list, and Phase 2 (setup wizard, Flatpak packaging, release workflow, Flathub). Features built beyond the original plan (details in `CLAUDE.md`'s Layout notes):

- Viewer: video files play in it (copied from the Mac with progress, GStreamer player), YouTube links play in an embedded WebKitGTK view, full screen, prev/next across photos, videos and YouTube (a slide transition for pictures), autoplay only for the item you clicked.
- Info panel: Photos, Videos (video files plus YouTube links), Files and Links tabs, constant width (4 photo columns), contact cards and maps.
- Chat: a scroll anchored to the bottom, a jump-to-newest button, file, video, link and photo-stack cards, a responsive sidebar (avatars only when narrow).
- New message: picking or typing a contact opens the existing conversation with that person (any of their addresses, found on the Mac even when older than the chats list's 100), and the sidebar glides to it.

### Not yet checked on a real run

- Clicking a desktop notification opens its chat.
- Local read state: opening an unread chat clears its dot (the headless test window never counts as focused).
- A real group chat with more than four people (the "+N" circle).
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
  - Built (2026-11-18): our own preview for a single plain link Messages made no preview for (`web_preview`/`request_web_preview`/`fetch_web_preview` in ui.rs, parsing and URL checks in `webpreview.rs`, with tests). Setting "Make previews for links Messages didn't" (Settings > Privacy, off by default, config `link_previews`); this computer fetches the page (libsoup through `webkit6::soup`, 10 s timeout, UA `facebookexternalhit`-style), only public http(s) addresses; title, site name and picture from the Open Graph tags (the picture's size read from its header, `image_size`), YouTube links included (the card plays in the viewer like Messages' own). Results are kept in `data/bubblefairy/web_links` and the pictures in the image cache (`web:<url>` keys). Dev: `BF_LINKS=1`, `BF_DEMO_WEBLINK=<url>`. Not done: links in the middle of text or several links; the page's own icon. Earlier note: links with no saved preview (the sender's device didn't make one) showed as plain text. BubbleFairy makes its own preview card for them (title, site, picture from the page's Open Graph tags), behind a privacy setting. It is local only: whoever receives a link we send won't see a preview on their device. **Known limitation, accepted:** previews of links we send can't be made for the recipient. The plain `imsg send` goes through AppleScript and creates a plain-text message (checked in chat.db on 2026-11-17: links sent from our app have no `URLBalloonProvider` row or payload; links sent from Messages' own UI do). imsg 0.21's `send-rich --url` can send a real preview, but only through its IMCore bridge, which needs SIP turned off on the Mac. We don't use it and never ask users to disable SIP. Open question for the build: fetch the page from Linux or from the Mac's `curl` (either way, the linked site learns that address).
  - Layout follows Messages: a preview picture of 400x200 px or more goes across the top in its own proportions (never cropped); a smaller one (a site logo) sits beside the title as an icon. Messages saves no "is this a logo" flag, so this is a size rule. The card is off-white in both colour schemes.
  - Never fetch automatically for links from unknown senders, because that reveals the user's IP address to the link's owner.

### Phase 1.2: conversation info (built)

- An info button in the conversation header opens a panel over the chat's right side: participants and contact cards (phones, emails, addresses with maps, birthday), and Photos, Videos, Files and Links tabs for the whole chat, read from chat.db over SSH (imsg has no attachments-only query) and paged ("Show more"). Details are in `CLAUDE.md`.
- The viewer walks the loaded photos, or the loaded videos and YouTube links, of the tab it was opened from. It never loads older history on its own (Peter's call).

### Pinned conversations (built)

- Our own per-instance list (Messages' pins can't be read from the Mac and don't sync between devices): `data/bubblefairy/pins`, one chat `identifier` per line in pin order. Pin and unpin from the right-click menu on a list row or a tile.
- Pinned chats show as avatar tiles above the list (name under the avatar, unread dot, group avatars as in the list), three per line. In the avatars-only sidebar they become a column of circles spaced like the list rows. A pinned chat's list row stays in the list, hidden, so row indexes keep matching the chat list.
- Dropped on purpose: hover pin button (tried; GTK's negative margins and sibling draw order made it fiddly), drag to reorder (tiles stay in pin order).

### Times (built)

- One scheme everywhere (`rel_time`, `TimeKind`): the chat list and "Read" lines show `17:38` today, `Yesterday`, a weekday for 2-6 days ago, `Nov 8` this year, `Nov 8, 2025` before; the separators in a chat show `Today 17:38`, `Yesterday 17:38`, `Mon 17:38`, `Nov 8, 17:38`, `Nov 8, 2025, 17:38`; tooltips keep the full date and time. Every such label is tracked (`track_time`) and rewritten in place at midnight (`schedule_midnight`) or when the window becomes active after a day change, with no rebuild.

### Small features (built)

- Hovering a message shows its full date and time; the window title carries the number of unread conversations (`(3) BubbleFairy`); each chat keeps its own unsent text and waiting files while you look at another (in memory only, lost on quit); the chat names its service ("iMessage", "RCS", "Text Message") at the start and wherever it changes.
- Sidebar search (Ctrl+F): filters the list by name, a group member's name, or part of a number or address; pinned chats show as ordinary rows while searching; hidden in the avatars-only sidebar.

### Delivery line (built)

- Under my newest sent message in the open chat: "Read 10:32" (date when not today; only when the recipient sends read receipts) or "Delivered". Data is chat.db's `is_delivered`, `date_delivered`, `date_read`, read over SSH by `Cmd::Receipts` (`mac::parse_receipt`), asked when a chat's history arrives and every 10 s while the window is active; `Event::Receipt` redraws just that message. `is_delivered` counts as delivered even where `date_delivered` was never filled in (it is sparse). RCS and SMS rows rarely carry a delivered flag, so those show nothing until read.
- Under ANY message of mine that failed: a red "Not delivered" (chat.db `message.error` non-zero, added to every message by `add_services`; imsg's JSON has no such field). Checked against Peter's phone (a photo with error 35 shows "Not Delivered" there).
- A failed message also moves in from the edge for a red "!" button; its popup says "This message (photo, video, file) was not delivered." with Try Again, which sends the text and the attachments that are already on the Mac (`Cmd::Resend`, paths from imsg's `original_path`). As in Messages the failed message stays in the chat (chat.db keeps it with its error, and imsg still returns it): while the retry runs it is a ghost with a spinner in place (no "!" or "Not delivered"), then it comes back as a failed one with an outline "!" (`State.resent`, file `data/bubblefairy/resent`) and the new message appears at the end. A retried picture reuses what the chat already shows (`PendingItem::Seed`), no fetch back. Not done: "Send to email address / as text message". Checked on a real Mac for a picture: Try Again delivered it. Messages.app itself retries on the same chat.db row (clears `error`, may switch it to SMS, moves the date); we can't (imsg's `delete-message`/`edit` need the IMCore bridge, which needs SIP off), so the old row stays failed on every Apple device and the popup says so. The composer's service label ignores failed messages (a failed SMS retry left the chat labelled SMS).
- "Notifications silenced" was tried as the line for `was_delivered_quietly` and was wrong: the phone said "Delivered" for such a message. The quiet flag is now only a tooltip ("Delivered quietly…"), since the recipient's Focus at the time can't be known.

### Candidate features (investigated, not built)

- **NEXT PRIORITY (after the link previews / polish batch): proper notifications with sound** (Peter, 2026-11-18; don't start until he says). Today `notify` (ui.rs) only sends a plain `gio::Notification` (title, body, default action `app.open-chat`) when the window is inactive or another chat is open: no sound, no settings, nothing tested against desktop notification controls. Wanted: notifications that work with the desktop's own notification settings (KDE/GNOME per-app controls, do-not-disturb, grouping, click opens the chat), a sound on arrival, and **a choice of sounds** (where to get them is open: needs freely licensed sounds we can ship, or the desktop's own sound theme through `canberra`/the freedesktop sound-theme names such as `message-new-instant`; check licences before bundling anything). Questions for then: sound per chat or global, mute while the window is focused on that chat, notification content privacy (hide text), reply-from-notification (not possible without more of imsg?), and what the Flatpak needs (`org.freedesktop.portal.Notification`, sound through GStreamer which we already link).

- **Delivery line: what can't be had.** Without the bridge (SIP off) there are no typing indicators (sending or seeing), no sending of read receipts, no "Notify Anyway", and no seeing a contact's Focus before you send (that status isn't in the database).
- **Show reactions (tapbacks) on messages. Built:** `reactions_pill` (ui.rs) draws a pill of the distinct emoji (count when more reactions than emoji, tooltip says who) over the first part's top corner, on the side away from the tail; a live reaction row triggers `Cmd::Recent` (last 10 messages), so a reaction to an older message shows after a reload. Dev: `BF_DEMO_REACT=1`. Original note: the app skipped reaction rows (`render_conv`, `is_reaction`), so they're invisible. imsg's history already puts a `reactions` list (emoji, type, sender, is_from_me) on the message each one targets, so it needs no new Mac queries; draw a small emoji cluster on the bubble's corner (who reacted in a tooltip). Custom-emoji reactions are in the data too. A reaction arriving live is its own message event: refresh the target's list (e.g. re-fetch the chat's recent history).
- **Sending reactions: needs testing, probably skip.** `imsg react` works without SIP but drives Messages through UI automation: it can only react to the chat's last (or selected) message, needs Messages running with the chat open and Accessibility permission for the SSH session, a retry after a failed confirmation can toggle the reaction off, and custom emoji can't be sent. Targeting a specific message needs the bridge (SIP off). The only honest version is a tapback button on the newest message with the six standard reactions.
- **Save attachments (download icon). Built:** `save_button`/`save_items`/`queue_save` (ui.rs) put a round save button beside each photo/stack, video and file (not for ones missing on the Mac); it saves the original from the Mac (`Cmd::Download` into the cache, then a copy) to the Settings folders (`dir_images` for photos, `dir_files` for the rest; empty = save dialog, several photos = folder chooser), numbering names that exist (`unique`), and turns into a green check for 3 s (`SaveDone`). A default folder that is no longer writable falls back to the dialog with a note. Right-click on a photo: Copy image (the chat copy's texture) and Save as… (always the dialog). Settings has the "Default download folders" group; the chooser gives a Flatpak app a document-portal path (`/run/user/1000/doc/<id>/Folder`) which is what we store and use; the field shows the real folder from the `user.document-portal.host-path` attribute. Original note:  Like Messages: a small download icon just to the right of a received photo, video or file bubble, and for sent ones just to the left of the bubble (Messages doesn't offer it for sent items, but we do). It saves a copy to a chosen place (a save dialog starting in the Downloads folder), using the same copy-over-SSH route as opening a file (`start_download`, the cache's `files/` folder). Replaces the old "save a file from the Files tab" polish item; the Files tab could get the same icon.
- **Search inside messages. Built:** the sidebar search also looks through messages (300 ms after typing, 2+ characters, spinner on the field): `Cmd::Search` reads chat.db directly (imsg's search took ~6 s: it looks up a contact per hit), incl. text kept only in `attributedBody` (over half of all messages; `match_sql` matches only the text span of the blob, after `NSString` up to the first 0x86 byte, so markup words don't hit; the newest 400 candidates are verified on the decoded text, 100 hits kept). "N matches" in the sidebar is a grouped count over everything (`search_counts`); a chat whose matches are all older than the newest 100 still gets a row, and opening a chat asks for its own list (`Cmd::ChatHits`, up to 500). Chats with hits join the list with the newest hit as preview ("N matches · …"), words highlighted in titles, previews and messages (`highlight`, Pango attributes; sand colour, bright orange for the hit jumped to). Opening such a chat loads a window around the hit (`Cmd::Window`, `State.find_view`), a bar shows "2 of 18" with older/newer/Latest/close, and scrolling down loads newer pages (`Cmd::Newer`, `page_after` bounds the page by the n-th later row's date so no gap). Not done: Esc to close the bar; hits in chats outside the loaded list; filenames and URLs are only found when they are in the message text. Original note: The The sidebar's search box (built) filters conversations by name or number only. Searching message text can use imsg's `messages.search` (RPC method in 0.21); results would need their own view.
- **Inline replies:** messages carry `reply_to_guid`; show the quoted message above a reply. Medium effort.
- **Copy and save for pictures:** a right-click menu on a photo with "Copy image" and "Save as".
- **Respect muted chats ("Hide Alerts").** Messages keeps the list in `~/Library/Preferences/com.apple.MobileSMS.CKDNDList.plist` (key `CKDNDListKey`, entries value a far-future date; 13 on 2026-11-17). The keys are hashed (40 hex characters for 1:1 chats, GUID-looking ones for groups). The obvious SHA-1 of a chat's identifier or guid didn't match, so the hashing still needs working out. If it can be matched, desktop notifications could skip muted chats and the row could show a muted icon.
- **Spam and unknown-sender filtering.** chat.db has `chat.is_filtered`, `message.is_spam` and `message.filter_action`; check how many of the user's chats carry them before deciding how the list should treat them.
- **Our own link preview card for plain links**: see Link previews under Phase 1.1.

### Polish (later)

- Done 2026-11-18: header title/subtitle and all contact card text are selectable; contact cards have a copy button on each number and email. Still not selectable: info panel tiles and sidebar rows (clicking them does something else).
- Files: saving to a chosen place is covered by "Save attachments" under Candidate features.
- The control-layout question for the viewer is settled: photos and video files use the shared control bar and fading buttons; YouTube keeps its own player controls with our buttons in a strip above it that shrinks away when they hide.

### App icon (before the first release)

A chat bubble with a cartoon magic wand: a stick with a five-point star on the end, drawn in a cartoon style with a few little sparkles around the star. Peter will generate options with Draw Things on his Mac; Claude writes the prompt (a bold, simple, flat-colour sticker look that still reads at 16 px) and later turns the chosen picture into the Flatpak icon set (scalable SVG plus a symbolic version). Needs to be done before Flathub submission.

The header's "BubbleFairy ✨" title (with the round unread-count badge after it) is a stand-in: revisit it together with the icon.

### Contacts integration (read-only; built except where noted)

The Mac's Contacts databases are readable over SSH (about 360 contacts on Peter's Mac, with names, organisations, phones, emails, addresses, birthdays, notes and photos), through the same Full Disk Access as chat.db. Editing isn't possible, and isn't wanted. Planned uses:

- Starting a new chat: the To field searches the whole address book by name, number or email, and picks a specific number or email when a contact has several. (Replaces the current suggestions from existing chats only. Earlier notes saying imsg can't list contacts were about imsg; reading the database directly can.)
- Names for numbers and emails that imsg doesn't name: unsaved senders in group chats, unfamiliar conversations.
- Contact cards in the conversation info panel (Phase 1.2): phones, emails, address, birthday.
- Address maps (built): a contact card shows a small OpenStreetMap tile with a pin under each postal address, and clicking it opens the default maps app (`geo:` link, falling back to openstreetmap.org). The Mac's curl does the lookups (Nominatim for the coordinates, then one tile), so the app needs no HTTP library; results are cached on Linux in the image cache as `<hash>.map` files (these say where a contact lives, so treat them as sensitive). It loads automatically for now. **Planned: a privacy setting that turns automatic previews on, and says plainly that it uses OpenStreetMap and sends the address there.** Nominatim's policy limits the public server to 1 request a second with an identifying User-Agent (done in `mac::map_for`).
- Risk: the queries depend on Apple's database layout, which changes between macOS versions. If one breaks, avatars fall back to initials and the search to existing chats. If the address book is ever cached on disk, treat that file as sensitive.

### Phase 2: setup wizard (no terminal)

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

Reactions/tapbacks, message search, running in the background or in the tray, a privacy setting for the address maps and for YouTube (both contact third parties from the Linux machine or the Mac when used), photos people share through Messages' Name & Photo. A Mac companion app is a possibility. An iPhone-only route is unlikely, because iOS gives third-party apps no access to iMessage (not researched).

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
- A 1:1 chat's `service` comes from chat.db's `chat.service_name`, which can be stale (Sue's chat says SMS, but its recent messages are all iMessage). The app shows the latest message's service and sends 1:1 messages as `send {to, service: "auto"}` so Messages picks, as on the Mac. VERIFY: the first such send lands in the same conversation.
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
- Attachments have `original_path` on the Mac. Images are fetched over extra SSH exec channels (at most 6 at once; sshd allows 10): Quick Look (`qlmanage -t`, which applies HEIC rotation) then `sips` to JPEG, 1200 px for the chat and 4096 px for the viewer, cached on disk. Link-preview pictures arrive as `*.pluginPayloadAttachment` files (plain PNG/JPEG/ICO): they go through `sips` only, because Quick Look hangs on that extension, and `fetch_image` kills a stuck `qlmanage` after 8 s. Photos still need Quick Look, because `sips` ignores HEIC rotation (8 of 13 test HEICs came out sideways).

Still open:

- Is `imsg-macos.zip` a universal build (arm64 plus x86_64)?
- Does "full disk access for remote users" cover a binary in the app dir? Expected yes, because the permission belongs to the SSH session.

## Risks

- imsg churns quickly and has one maintainer. Mitigation: the version pin, and the option to fork (it's MIT).
- Apple can change the chat.db or Contacts schema, or Messages' AppleScript support, in any macOS release. That breaks every tool of this kind; our own queries are written to fail soft.
- The Mac has to be awake and reachable. Outside the home network that means Tailscale or similar (future docs).
- Prior art: BlueBubbles and OpenBubbles. BubbleFairy differs in needing no server app on the Mac and in being a native GTK app on Linux.
