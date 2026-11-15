# BubbleFairy: project notes

A Linux desktop iMessage app: Rust + GTK4 with its own CSS look, shipped as a Flatpak (`io.github.theteknocat.BubbleFairy`). It drives the Mac's `imsg rpc` over SSH (the `russh` library, with the app's own key). Repo: private `theteknocat/bubblefairy`, branch `main`, MIT license (Peter Epp). The local folder is still named `imsg-linux`.

- **Read `PLAN.md` first.** It holds the decisions, the phases, the design direction, imsg facts, proof-of-concept gotchas and the open questions.
- Status: Phase 1 is being built.
- Not committed and kept locally for reference until the first build: `poc/imsg-tui` (the working Python/curses proof of concept) and `HANDOFF.md` (its notes, already folded into PLAN.md). Both are excluded through `.git/info/exclude`.
- Mac under test: macOS 26, imsg 0.21.0 from Homebrew at `/opt/homebrew/bin/imsg`. It can be reached with `ssh mac` from Peter's terminal. The app itself never uses `~/.ssh`.
- Never put iMessage, Apple or other trademarks in the app name or branding.

## Build

- The host has no GTK dev packages, so cargo runs inside the GNOME 51 Flatpak SDK: `scripts/dev.sh build|run|test|clippy`. The output goes to `target/sdk/`.
- `scripts/dev.sh` points the XDG dirs at `~/.var/app/io.github.theteknocat.BubbleFairy/{data,config,cache}`, the same folders the real Flatpak will use. Without that, the SDK sandbox throws away anything written to `~/.local`.
- Dev switches: `BF_DEMO=1` (invented chats, no Mac), `BF_SHOT=<file.png>` (save the window after 2.5 s, then quit), `BF_DARK=1` / `BF_LIGHT=1` (force a scheme). For headless screenshots: `WLR_BACKENDS=headless cage -- sh -c "env BF_DEMO=1 BF_SHOT=... scripts/dev.sh run"`.
- GTK CSS `@media (prefers-color-scheme)` did not fire for an app provider (GTK 4.24), so dark mode is a `window.dark` class set from `gtk-interface-color-scheme` in `ui.rs`.

## Layout

- `src/main.rs`: paths, config file (`key=value` lines), tokio runtime, GTK app startup, CSS loading.
- `src/mac.rs`: backend. `run()` loops connect → serve → reconnect every 5 s. `connect()` resolves the host (plain DNS, then mDNS for `.local` names, because Flatpak runtimes lack nss-mdns), checks the host key (trust on first use, stored in `mac_host_key`), authenticates with the app's ed25519 key (`ensure_key` creates it), execs `<imsg> rpc`, and runs a writer task and a reader task. `dispatch` routes JSON-RPC replies to pending calls and `message` / `watch.overflow` notifications to `Event`s. Images are fetched on separate exec channels with `sips` → JPEG.
- `src/ui.rs`: every widget and all state, on the GTK thread. `on_event` handles backend events, and `render_chats` / `render_conv` rebuild the list and the conversation from state. Only flags that signal handlers touch are kept in `Cell`s, to avoid RefCell re-borrow panics (selecting a row fires `row-selected` synchronously).
- `src/bubble.rs`: `Bubble`, a gtk::Box subclass that draws its own background as one GSK path (body + tail): gradient fill, shadow, top highlight and border all follow the tail. GTK CSS has no clip-path or pseudo-elements, which is why. Colours are in `palette()` (light/dark, read from the window's `dark` class). Inner spacing is set with margins in `new()`, because CSS padding didn't apply to the subclass. Content goes in with `Bubble::add`.
- Photos render outside bubbles as `picture.photo` cards (`photo_stack` in ui.rs). Several photos in one message become a card stack (first on top, the rest peeking away from the message's side). `Sized` is a paintable that reports the display size and crops like object-fit: cover, because GtkPicture would otherwise ask for the photo's full pixel width.
- `src/style.css`: the whole look. Palette tokens live on `:root`, with dark overrides on `window.dark` (`var()` needs GTK ≥ 4.16).
