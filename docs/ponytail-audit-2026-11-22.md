# Ponytail audit, 2026-11-22 (round 1)

**Scope:** outside input, shell commands run on the Mac, JSON and file parsing, state files, the local copy.
Read: `src/mac.rs` (connection, shell quoting, every `exec` call site, link plist parsing, upload, download,
contacts parsing, unread fix), `src/setup.rs` (login, key install and removal, imsg download check), `src/webpreview.rs`,
`src/snapshot.rs`, and the parts of `src/ui.rs` that call into them (`web_get`, `fetch_web_preview`, `file_dest`).
Already clean before this round: rustfmt, Clippy (see CLAUDE.md).

**Load assumed:** one person on one desktop, one Mac on the local network. The risk is not volume, it is what a
stranger who sends a message can make the app do, and what one huge file or response does to memory.

**Checked and fine (no finding):** every value put into a shell command on the Mac goes through `sh_quote` (or is a
number); SQL text is escaped and then quoted; `search` uses `instr`, so `%` and `_` don't act as wildcards; the app's
SSH key is created with mode 0600 and `create_new`; `remove_key_cmd` removes only lines containing the key's own
base64 body and never overwrites the file after a grep error; `parse_link` bounds-checks every archive reference;
SSH keepalive (5 s x 3) means a dead link is noticed, so a hung channel doesn't hold a permit forever. Host-key trust on
first use is a documented shortcut (`mac.rs`), not a finding.

## Findings

1. **Must fix: link-preview address filter lets local addresses through, and redirects are not checked**
   (`webpreview::public_url`, `ui.rs web_get`). Status: fixed.
   - Case: with "Make previews for links Messages didn't" on, anyone can message `http://127.1/`,
     `http://[::ffff:192.168.1.1]/` or `http://100.64.0.1/` (Tailscale range), or a public page that redirects to an
     internal address, and this computer makes the request.
   - Fixed: `public_url` now judges an IPv4 address inside an IPv6 one as IPv4, blocks 0.0.0.0/8 and 100.64.0.0/10,
     and lets a non-literal host through only when its last part is letters (or punycode), which rejects `127.1`,
     `192.168.1` and `0x7f.0.0.1`. `web_get` follows redirects itself (up to 5) and runs `public_url` on every hop.
     Tests: the new cases are in `only_public_addresses`. Checked live: an `http://` link that redirects to `https://`
     still gets its title and picture. Still open, as documented: a name that resolves to a private address.
2. **Should fix: a web page is read fully into memory before its size is checked** (`ui.rs web_get`). Status: fixed.
   - Case: a linked server that never stops sending keeps the 10 s idle timeout from firing and fills memory.
   - Fixed: `web_get` now gets the headers first (`send_future`), rejects a `Content-Length` over the limit, then reads
     the body in 64 KB pieces and gives up once it passes the limit. Checked live: a page with a redirect, a picture and an
     icon still previews. Not tested: an endless response (no such server here); the limit check is simple enough to read.
3. **Should fix: attachment upload reads the whole file into memory** (`mac.rs upload`). Status: fixed.
   - Case: attaching a multi-gigabyte video reads all of it into RAM before anything is sent.
   - Fixed: `upload` opens the file with `tokio::fs::File` and hands it to `Channel::data`, which streams it in pieces.
     The error text for an unreadable file is the same. Also added tokio's `fs` feature to `Cargo.toml`: the code only
     compiled because another crate turned it on. Not tested end to end: it needs a real send to the Mac, which the
     project rules keep out of automation. To check by hand: attach a large file in a chat and watch memory.
4. **Should fix (lean): the "path on the Mac, with `~/` meaning home" quoting is written four times** (`mac.rs`:
   `fetch_image`, file info, download, link-preview measure; a fifth place, resend, builds its path differently and is
   not a copy). Status: fixed (DRY: Peter wants duplicates merged unless there is a major trade-off).
   - Fixed: one `remote_path(path)` helper, four calls, and a test covering `~/`, spaces, a quote and `$(...)`.
5. **Should fix (lean): the `sqlite3 -readonly ~/Library/Messages/chat.db '<sql>'` command is built twelve times**
   (`mac.rs`). Status: fixed (DRY, same rule as 4).
   - Fixed: one `chat_db_out(session, sql)` helper (and `chat_db` uses it), twelve call sites now one line each.
     `mac.rs` is about 70 lines shorter. Checked live against the real Mac: chats, history, a photo and a "Delivered"
     receipt all load.
6. **Nice to have: relative picture and icon addresses are resolved against the address asked for, not the one reached
   after a redirect** (`ui.rs fetch_web_preview`). Status: fixed.
   - Case: a short link that redirects to another site, whose page names its picture as `/img/a.jpg`, looks for the
     picture on the short-link host and shows none.
   - Fixed: `web_get` also returns the address it ended on, and the page is parsed against that. Checked live: for
     `http://wikipedia.org/` the icon is now `https://www.wikipedia.org/static/...`.

## Result of round 1

Six findings: 1 must fix, 4 should fix, 1 nice to have. All six fixed. Tests: 28 pass (one new for `remote_path`, new cases in
`only_public_addresses`). Clippy clean. Checked live against the real Mac: connect, chats, history, a photo, a receipt,
link previews with redirects. Not tested: an endless web response, and a large upload (both need things automation
can't do here). Also done in this round's commit: a "Code standard" section in `CLAUDE.md`.

## Not read (next round)

`src/ui.rs` (about 11k lines, only the link-preview and download parts were read), `src/tilegrid.rs`,
`src/chatscroll.rs`, `src/bubble.rs`, `src/viewer.rs`, `src/player.rs`, `src/tray.rs`, `src/sound.rs`,
`src/pilllabel.rs`, `src/demo.rs`, `src/main.rs`. Also not run: `clippy --all-targets` (unwraps in test code),
CI format and Clippy checks.
