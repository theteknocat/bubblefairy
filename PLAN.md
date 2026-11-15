# BubbleFairy plan

BubbleFairy is an open-source (MIT) Linux desktop app for iMessage. The Mac does all of the iMessage work. BubbleFairy is the window on Linux that ferries messages to and from it over the local network.

Status: planning is done and implementation has not started.

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

- Mac install dir (planned): `~/Library/Application Support/BubbleFairy/` (the imsg binary and its version marker).
- Linux app data: the XDG config and data dirs, which `glib` provides. Inside the Flatpak these are `~/.var/app/<id>/`.
- The GTK main loop runs the UI. Networking runs on a tokio runtime, because russh needs one, and sends its results back to the UI over a channel. The UI thread never waits on the network.

## Phases

### Phase 1: works for Peter (run from source)

The connection settings (host, user and imsg path) are entered once in a simple settings view. The app already uses its own ed25519 key (its public key is shown there to add to the Mac's `authorized_keys` by hand) and the imsg installed by Homebrew. Phase 2 automates the key install and replaces Homebrew's imsg with the app's own copy.

- Chat list: avatar with initials, name, last message, time, unread dot.
- Conversation: history with timestamps between groups of messages, and live incoming and outgoing messages.
- Sending.
- Bubbles: blue for iMessage, green for SMS/RCS, grey for incoming. Consecutive messages are grouped, and bubbles have tails.
- Images shown inline (received and sent).
- Desktop notifications (`gio::Notification`, which goes through the portal inside the Flatpak).
- Auto-reconnect, with a visible connection state.
- Follows the system light/dark setting.

### Phase 1.1

- New 1:1 conversations: a "new message" button (Ctrl+N) swaps the conversation header for a To field. As you type, it suggests people you already have chats with (picking one opens that chat). Otherwise it takes a phone number or email, and the first message goes out as `send {to, service: "auto"}`; the app opens the new chat once it appears. imsg can't list the Mac's Contacts, so there's no address-book search.
  - New group chats aren't possible: imsg's `chats.create` needs its bridge, which needs SIP off (out of scope).
- Read state, kept locally. The unread dots mirror the Mac's chat.db (`unread_count`), so a chat read only on the iPhone stays unread until the Mac catches up. Marking read on the Mac isn't possible: imsg's `read` needs SIP off, and Messages has no AppleScript for it. So the app remembers, per chat, the newest message seen in BubbleFairy, and hides the dot until something newer arrives. Nothing goes back to the Mac.
- Sending attachments, and other attachment types (video, PDF, audio).
- Link previews:
  - Use Apple's own rich-link data when imsg exposes it.
  - Otherwise fetch the page from Linux. A setting turns this off.
  - Never fetch automatically for links from unknown senders, because that reveals the user's IP address to the link's owner.

### Phase 1.2: conversation info

- An info button in the conversation header opens a panel for the chat:
  - Participants (name, handle). Contact cards need the Mac's Contacts, so not for now.
  - Tabs to browse the whole chat's Photos, Attachments and Links.
- imsg has no "messages with attachments only" query, so read chat.db directly over SSH (as `mac::add_services` already does): `attachment` joined through `message_attachment_join` and `chat_message_join` for one chat, newest first, paged. Links come from message text.
- The photo viewer can then walk every photo in the chat, not only the ones loaded in the conversation (for now it stops at the earliest loaded photo).

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
7. Checks that chat.db can be read. If it can't, it shows how to tick the Full Disk Access box, then checks again.
8. Sends a test message (optional). It warns the user to watch the Mac for the Automation prompt.
9. Flatpak packaging and a GitHub Actions release workflow (push a tag `vX.Y.Z` to build the Flatpak bundle), then submission to Flathub.

### Later

Reactions/tapbacks, search, contact photos, running in the background or in the tray. A Mac companion app is a possibility. An iPhone-only route is unlikely, because iOS gives third-party apps no access to iMessage (not researched).

## Design direction

- Unique. It must not imitate Apple Messages or Liquid Glass.
- Dimensional, not flat: layered shadows, bevels, rounded corners, gradients, and glassiness where it fits. Subtle and tasteful, never over the top.
- Friendly, bright and inviting, not institutional and not like a toy. Dark mode should be just as bright and inviting.
- The sibling project to draw on is airscope: its effects live in `~/system-troubleshooting/airscope/src/fx.rs` and its style notes in `docs/STYLE.md`.
- GTK4 CSS has no `backdrop-filter`. Glass is suggested with translucent gradients and edge highlights. Real blur is used only for the app's own content scrolling under a panel (GTK's snapshot blur).
- Dates: `MMM d, YYYY` with 24 h times, to match airscope. Change this if Peter prefers otherwise.

## imsg facts (verified on 0.21.0)

- `chats` gives `id` (the rowid used everywhere), `guid`, `identifier`, `participants[]`, `contact_name` (1:1 chats only), `display_name` (often empty), `is_group`, `service` (iMessage, SMS or RCS), `unread_count`, `last_message_at`.
- `history` returns messages newest first, with `id`, `guid`, `chat_id`, `is_from_me`, `sender`, `text`, `created_at`, `attachments[]`, `reactions[]`.
- RPC methods: `initialize`, `status`, `chats.list|get|create`, `messages.history|search|after|stats`, `watch.subscribe|unsubscribe` (sends `message`, `edited` and `unsent` events, and a final `watch.overflow`), `send`, `send.tracked`, `message.send_status`. Limits: 128 outstanding requests and 64 subscriptions. The bridge methods (typing indicators, read receipts, edits and so on) need SIP turned off, so they are out of scope.
- A 1:1 chat's `service` comes from chat.db's `chat.service_name`, which can be stale (Sue's chat says SMS, but its recent messages are all iMessage). The app shows the latest message's service and sends 1:1 messages as `send {to, service: "auto"}` so Messages picks, as on the Mac. VERIFY: the first such send lands in the same conversation.
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
- Attachments have `original_path` on the Mac. Images are fetched over extra SSH exec channels (at most 6 at once; sshd allows 10): Quick Look (`qlmanage -t`, which applies HEIC rotation) then `sips` to JPEG, 1200 px for the chat and 4096 px for the viewer, cached on disk. Link-preview data arrives as `*.pluginPayloadAttachment` files, which are hidden until Phase 1.1 draws previews.

Still open:

- Is `imsg-macos.zip` a universal build (arm64 plus x86_64)?
- Does "full disk access for remote users" cover a binary in the app dir? Expected yes, because the permission belongs to the SSH session.

## Risks

- imsg churns quickly and has one maintainer. Mitigation: the version pin, and the option to fork (it's MIT).
- Apple can change the chat.db schema or Messages' AppleScript support in any macOS release. That breaks every tool of this kind.
- The Mac has to be awake and reachable. Outside the home network that means Tailscale or similar (future docs).
- Prior art: BlueBubbles and OpenBubbles. BubbleFairy differs in needing no server app on the Mac and in being a native GTK app on Linux.
