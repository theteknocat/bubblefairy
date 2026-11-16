# BubbleFairy plan

BubbleFairy is an open-source (MIT) Linux desktop app for iMessage. The Mac does all of the iMessage work. BubbleFairy is the window on Linux that ferries messages to and from it over the local network.

Status (2026-11-15): Phase 1 is built and running from source against Peter's Mac. Most of Phase 1.1 is built too (new conversations, local read state, sending files, link previews, avatars, a faded "sending" bubble with spinner that shows the local photo until the real one arrives). Not started: Phase 1.2 (conversation info), the contacts integration, the app icon, and Phase 2 (wizard and Flatpak packaging).

### Not yet checked on a real run

- A message to Sue, sent from the app: does it go as iMessage and land in the same conversation?
- Clicking a desktop notification opens its chat.
- Local read state: opening an unread chat clears its dot (the headless test window never counts as focused).
- A message with a link plus other text, and a link card next to a bubble.
- A real group chat with more than four people (the "+N" circle).

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
- Showing other received attachment types (video, PDF, audio) better than a file row.
- Contact photos as avatars (built for the chat list, header, group senders and the To suggestions): `mac::fetch_contacts` reads every `AddressBook-v22.abcddb` once per connection (`ZTHUMBNAILIMAGEDATA`: type byte 1 = the picture follows; type 2 = a file name in the database's hidden `_EXTERNAL_DATA` folder, usually a full-size original that the Mac shrinks to 160 px with `sips`), matched to a person by email or the last ten digits of a phone number. No photo means coloured initials, or a generic person for numbers. Group chats show their members' circles inside one disc (equal sizes; Messages may size them by member order), with three circles and a "+N" circle past four people; the conversation header shows them as an overlapping stack instead. Not done: photos people share through Messages' Name & Photo (Memoji etc., in `~/Library/Messages/NickNameCache`, format unchecked), group chat photos, and refreshing photos while the app runs (restart to pick up changes).
- Link previews:
  - Built: Apple's own saved preview (title, site, image) is read from chat.db's `payload_data` over SSH (`mac::link_preview`, using the `plist` crate) and drawn as a card; clicking opens the link. The site icon isn't drawn yet.
  - Not built: links with no saved preview (the sender's device didn't make one) still show as plain text. Fetching the page from Linux would fill that gap, behind a setting.
  - Layout follows Messages: a preview picture of 400x200 px or more goes across the top in its own proportions (never cropped); a smaller one (a site logo) sits beside the title as an icon. Messages saves no "is this a logo" flag, so this is a size rule. The card is off-white in both colour schemes.
  - Never fetch automatically for links from unknown senders, because that reveals the user's IP address to the link's owner.

### Phase 1.2: conversation info

- An info button in the conversation header opens a panel for the chat:
  - Participants (name, handle, avatar). Contact cards (phones, emails, address, birthday) come with the contacts integration below.
  - Tabs to browse the whole chat's Photos, Attachments and Links.
- imsg has no "messages with attachments only" query, so read chat.db directly over SSH (as `mac::add_services` already does): `attachment` joined through `message_attachment_join` and `chat_message_join` for one chat, newest first, paged. Links come from message text.
- The photo viewer can then walk every photo in the chat, not only the ones loaded in the conversation (for now it stops at the earliest loaded photo).

### Polish (later)

- All text in the app should be selectable to copy (message text already is; contact cards, the info panel, headers and so on aren't).
- Contact cards: a copy-to-clipboard button on each phone number and email, next to the message and email buttons.
- Info panel: open or save a file from the Files tab (they live on the Mac), and a way to scroll the photo viewer past the loaded photos.

### App icon (before the first release)

A chat bubble with a cartoon magic wand: a stick with a five-point star on the end, drawn in a cartoon style with a few little sparkles around the star. Peter will generate options with Draw Things on his Mac; Claude writes the prompt (a bold, simple, flat-colour sticker look that still reads at 16 px) and later turns the chosen picture into the Flatpak icon set (scalable SVG plus a symbolic version). Needs to be done before Flathub submission.

### Contacts integration (later phase, read-only)

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

### Later

Reactions/tapbacks, search, running in the background or in the tray, photos people share through Messages' Name & Photo. A Mac companion app is a possibility. An iPhone-only route is unlikely, because iOS gives third-party apps no access to iMessage (not researched).

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
