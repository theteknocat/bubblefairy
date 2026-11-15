# BubbleFairy: project notes

A Linux desktop iMessage app: Rust + GTK4 with its own CSS look, shipped as a Flatpak (`io.github.theteknocat.BubbleFairy`). It drives the Mac's `imsg rpc` over SSH (the `russh` library, with the app's own key). Repo: private `theteknocat/bubblefairy`, branch `main`, MIT license (Peter Epp). The local folder is still named `imsg-linux`.

- **Read `PLAN.md` first.** It holds the decisions, the phases, the design direction, imsg facts, proof-of-concept gotchas and the open questions to verify.
- Status: planning only, no code yet. Don't start implementing without Peter's explicit go-ahead.
- Not committed and kept locally for reference until the first build: `poc/imsg-tui` (the working Python/curses proof of concept) and `HANDOFF.md` (its notes, already folded into PLAN.md). Both are excluded through `.git/info/exclude`.
- Mac under test: macOS 26, imsg 0.21.0 from Homebrew at `/opt/homebrew/bin/imsg`. It can be reached with `ssh mac` from Peter's terminal. The app itself never uses `~/.ssh`.
- Never put iMessage, Apple or other trademarks in the app name or branding.
