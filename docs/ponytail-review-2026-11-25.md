# Ponytail review, 2026-11-25

Scope: the uncommitted work after commit 11ccd4d: the people list growing to fit a contact card (`show_people` in `ui.rs`), its spacing (`style.css`), the demo's contacts and 11-digit numbers (`demo.rs`), the `BF_PEOPLE_ROW` and `BF_POPSHOT_MS` dev switches, `CLAUDE.md`. Assumed load: one person, one Mac.

## Checked and cleared

- Closing the popover during the 220 ms slide: the per-frame `present()` calls do not bring it back (tested: `visible=false` afterwards, with a temporary patch that was reverted).

## Findings

1. **Should fix:** every time the people list is opened, its popover and everything in it stay in memory after it closes. Tested with a temporary patch (reverted): a weak reference to the popover was still alive 2 s after it closed. The cause is strong references in a loop: the stack's `transition-running` handler holds `pop`, and each person row's click handler holds `pop`, `stack` and `list` while those hold the rows. The loop existed before (`pop2`, `stack2`); this change adds `list2` and the `transition-running` handler. Fix: capture weak references (`downgrade()`) in those closures, as `pin_menu` already does. Status: fixed (weak references in the slide handler, the row click handlers and the back button; the same temporary test then showed the popover freed, both with the list only and with a card opened).

Not read: `show_card`/`card_body` closures (likely the same pattern, outside this change).
