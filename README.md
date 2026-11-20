# BubbleFairy

**Your iMessages on your Linux desktop, ferried over from your Mac.** BubbleFairy is a native Linux app that brings Messages to your desktop: your conversations with photos, videos, links and files, live notifications with sound, and search across everything, in a fast, good-looking window.

It's a front end for Messages on your Mac, which still does the actual messaging (BubbleFairy connects to it securely over your local network). Things you manage in Messages, like blocking numbers and filtering unknown senders, stay there, and a few features Apple keeps locked down, such as sending tapback reactions or typing indicators, aren't possible. See the [known limitations](docs/LIMITATIONS.md) for the full list.

[![Buy me a coffee](https://img.shields.io/badge/Buy%20me%20a%20coffee-teknocat-ffdd00?logo=buymeacoffee&logoColor=black)](https://buymeacoffee.com/teknocat)
If BubbleFairy is useful to you, you can [buy me a coffee](https://buymeacoffee.com/teknocat).

> **Status: early release.** Get the latest `.flatpak` from the [Releases](../../releases) page.

> ### 🤖 Built with AI
> **Every line of this project was written by AI** ([Claude Code](https://claude.com/claude-code), Anthropic's AI
> coding tool). It wasn't one prompt: I design the app, review each piece, test it on my own Mac and conversations,
> and decide what stays. Before release it had an AI code audit and has unit tests for the risky parts, but it has
> not had a review by a human Rust developer, and some of the code will look like what it is. I'm not a Rust developer,
> so reviews, bug reports and pull requests are very welcome. More on [how I work with AI](https://github.com/theteknocat/theteknocat/blob/main/AI.md).

## How it works

```text
BubbleFairy on Linux ──SSH──> your Mac ──> Messages
```

Nothing related to iMessage runs on Linux. BubbleFairy talks to [imsg](https://github.com/openclaw/imsg), a small open-source command-line tool on your Mac. imsg reads your message history and sends messages through the Messages app. Setup turns on SSH access with a key that belongs only to BubbleFairy, and installs imsg for you.

## Requirements

- A Mac running macOS 14 (Sonoma) or newer, signed in to iMessage. It has to be awake and reachable on your network.
- For SMS: Text Message Forwarding turned on for the Mac on your iPhone.
- A Linux desktop that can run Flatpak apps. It's tested on KDE Plasma and GNOME.

## Install

Download `BubbleFairy-vX.Y.Z.flatpak` from the [Releases](../../releases) page, then:

```text
flatpak install --bundle BubbleFairy-vX.Y.Z.flatpak
flatpak run io.github.theteknocat.BubbleFairy
```

The GNOME runtime it needs comes from Flathub, so Flathub should be set up as a Flatpak remote.

## Setup

You do only these things on the Mac:

1. Turn on **Remote Login** (System Settings → General → Sharing).
2. Tick **Allow full disk access for remote users** in the Remote Login settings.
3. Click **Allow** when your Mac asks whether BubbleFairy can control Messages (once, on your first send).

BubbleFairy's setup wizard finds your Mac, asks for your Mac username and password once, and handles everything else.

## Known limitations

BubbleFairy works without turning off any of macOS's security protections, so a few Messages features aren't
possible: you can't send tapback reactions or inline (threaded) replies, there are no typing indicators, and
reading a chat in BubbleFairy doesn't send a read receipt or mark it read on your other devices. Pins and muted
chats are BubbleFairy's own and don't sync with Apple devices. The full list is in
[docs/LIMITATIONS.md](docs/LIMITATIONS.md).

## Not affiliated with Apple

BubbleFairy is an independent project. It is not made, endorsed or supported by Apple. iMessage and macOS are trademarks of Apple Inc.

## License

MIT. See [LICENSE](LICENSE).
