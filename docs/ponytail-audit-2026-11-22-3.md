# Ponytail audit, 2026-11-22 (round 3)

**Scope:** the parts of the app not read in rounds 1 and 2, looking first for panics, byte slicing, file writes and work on
the GTK thread. Read so far: `ui.rs` `word_spans`, `snippet`, `clip_middle`, `pretty_handle`, `pending_bubble`,
`read_map_cache`, the image cache writes and the `fetch_full` retry, the pins file write.
**Load assumed:** one person on one desktop.

**Checked and fine (no finding):** `clip_middle` and `pretty_handle` slice only at positions that are valid for their
input (character counts, or ASCII digits only); `word_spans` and `snippet` were covered in round 2; image-cache and pins writes
ignore errors on purpose (a cache); the `fetch_full` retry can't loop (a bad download goes through `image_done`, not
back to `fetch_full`).

## Findings

1. **Should fix: a photo being sent is decoded twice, on the GTK thread, every time the chat redraws** (`ui.rs pending_bubble`).
   Status: fixed (basic version), background version left for later.
   - Case: attach a 12 MP photo and send. Until the real message replaces the ghost (up to 20 s), each redraw of the chat
     (a new message, a receipt) loads the whole file twice with `Texture::from_filename` to draw a 160 px ghost. That is
     tens of ms of work on the thread that draws the window, which the app rules say never to do. The second call also
     uses `.expect("checked")`, which panics if the file went away between the two calls.
   - Fixed: the file is decoded once (`if let Ok`), so the redraw cost halves and the `expect` is gone.
   - Left for later (Peter's call, 2026-11-22): decode on a worker thread. It needs a shared texture slot in
     `PendingItem::File`, a `bg(|| decode_file(..), ..)` call at the two places a pending file is created (`ui.rs` near
     the `PendingItem::File(f.clone())` lines), `pending_bubble` drawing from the slot (a paperclip line or spinner until
     it is ready), and `settle_pending` reusing the slot's texture instead of decoding the file a third time. About
     30 to 40 lines. Worth doing if a large photo send is seen to stutter. Marked in the code with a `shortcut:` comment
     (found by `/ponytail-debt`).

Also read, no finding: `player.rs` (GStreamer `playbin` on a local file URI, errors reported not unwrapped), the guards on every
division and modulo in `tilegrid.rs`, `bubble.rs`, `viewer.rs` (column counts are `.max(1)`, texture sizes `.max(1)`).

2. **Should fix: a picture from a web page is decoded whatever size it claims** (`ui.rs fetch_web_picture`). Status: fixed.
   - Case: with "Make previews for links Messages didn't" on, a stranger's link whose page names a small PNG that says it is
     20000 x 20000 pixels (a few hundred KB, since flat colour compresses well) passes the 8 MB byte limit.
     `webpreview::image_size` reads the claimed size from the header and nothing checks it. The file is then decoded on a
     worker thread, which needs about 1.6 GB. A negative size in a PNG header passes too, into the card layout.
   - Fixed: `webpreview::image_size` now returns None unless each side is 1 to 8000 and the total is at most 40 million
     pixels, so `fetch_web_picture` drops the picture (the card keeps its title) before anything is decoded. Rejecting was
     chosen over scaling down: scaling needs the decode that is the danger. Test: 20000 x 20000, 8000 x 6000, zero and a
     negative width all give None. Not covered: frame counts of animated GIF or WebP. Not tested with a real oversized file.

Also checked, no finding: every `set_markup` in `ui.rs` and `setup.rs` gets static text or `markup_escape_text` output
(wizard steps, time labels, "photo not downloaded"); no `Command::new` outside a test; no `unwrap` or `expect` that
outside input can reach (the remaining `expect`s are on widgets the code itself just built).

## Result of round 3

Two findings, both should fix, both fixed (finding 1 in its basic form; the background-thread version is left for later and
marked with a `shortcut:` comment). Tests: 30 pass (a new case in `reads_image_sizes`). Clippy clean. Not tested live: a
large photo send, or an oversized web picture.

## Not read (round 4, if wanted)

`src/ui.rs`: the layout code in `build`, `render_conv` and `message_parts` beyond the paths above, the settings dialog's
widgets, the sidebar list and pins, search widgets, the info panel's tiles, the contact card body, `relayout`.
`src/viewer.rs` beyond YouTube and the size checks, `src/bubble.rs` and `src/chatscroll.rs` drawing, `src/pilllabel.rs`,
`src/demo.rs` (dev only), and the wizard's page code in `src/setup.rs`. These are mostly layout and drawing with no outside
input, so the expected yield is low.
