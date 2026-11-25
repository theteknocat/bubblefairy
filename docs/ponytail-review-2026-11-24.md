# Ponytail review, 2026-11-24

Scope: the uncommitted connection-status work (status pill + popover, issues, toasts, slow-send note, demo mode) in `src/ui.rs`, `src/mac.rs`, `src/demo.rs`, `src/style.css`. Assumed load: one person, one Mac.

## Findings

1. **Significant:** `Event::Offline` clears `Ui.since` only after the early `return`s for "not set up" and the first failed retry (`ui.rs`, `Event::Offline`). During "Reconnecting" the popover still shows "Connected since…", "Last heard from" and the Refresh button as if connected. Fix: `u.since.set(None)` at the top of the handler. Status: fixed (the later duplicate `since.set(None)` removed).
2. **Minor (was logged as Significant, corrected):** a failed chat-list read has no *immediate* retry. Checked afterwards: `ui.rs` (~L1420) asks for the chat list every 60 s while connected, and any incoming message reloads it after 700 ms, so the issue clears itself within a minute once the Mac answers. The original stuck banner came from the old warning never being cleared, which the issue set now handles. Status: skipped, no code change needed (an earlier retry would be a choice, not a fix).
3. **Minor:** `Event::Sent` for a resend (`resend_of` arm) doesn't remove the token from `State.slow`, so "Still sending…" stays up to 20 s after a slow resend succeeds. Fix: remove the token in that arm (both outcomes already redraw). Status: fixed.
4. **Minor (lean):** `.pop-refresh` repeats the `.head-btn` circle rules; `Ui.status_text` is a hidden Label used as a string holder. Status: fixed (`pop-refresh` now sits on `head-btn` and only sets size; `status_title` is a `RefCell<String>`).
5. **Minor (test):** `ago`, `lasted` and `set_issue`'s change detection are pure logic with no test. Status: fixed for `ago` and `lasted` (`status_times_read_well_at_their_boundaries`); `set_issue` skipped, it needs a live `Ui` (GTK), and its logic is a plain add/replace/remove on a list.

Not read: `settings`/`mac_tab` (untouched), the setup wizard.
