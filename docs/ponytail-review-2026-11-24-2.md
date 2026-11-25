# Ponytail review, 2026-11-24 (second run)

Scope: uncommitted changes after commit 4e87a2d: the contact card and people list moved onto the status popover's `issues` style, the `anchored_popover` and `unparent_on_close` helpers, the `BF_CARD` dev switch, `CLAUDE.md`. Assumed load: one person, one Mac.

## Findings

1. **Should fix:** the people list (big-group popover) now gets `popover.issues button.flat { padding: 6px 10px; border-radius: 12px; … }` (`style.css`). Its rows and back button are `flat` buttons, and that rule is more specific than `.person-row { padding: 4px 6px }`, so every row grows 2 px taller and 4 px wider in padding, and the list gets taller and a little wider. Not looked at on screen. Fix: either one line `popover.issues button.person-row { padding: 4px 6px; }`, or look at it and accept the new size. Status: fixed (`popover.issues button.person-row { padding: 4px 6px; }`).
2. **Should fix (duplication):** `BF_ISSUE` and `BF_CARD` each carry their own "wait 600 ms, draw the open popover to a file, close the window" block (`ui.rs`, `build`). Fix: one helper (`popshot_soon`) used by both (it also checks the status popover, which hangs off the pill, not the window). `BF_CARD` now opens the people list for a big group, so it can be drawn too. Status: fixed.
3. **Nice to have:** the `CLAUDE.md` sentence about the header avatar now reads awkwardly ("... both with the `issues` popover class ..., a Popover parented to the window ..."). Fix: split it in two. Status: fixed.

Not read: the other popovers (pin menu, image menu, failed-message menu) beyond the `unparent_on_close` swap; behaviour there is the same closure as before.
