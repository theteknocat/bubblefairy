# BubbleFairy

**Your iMessages on your Linux desktop, ferried over from your Mac.** BubbleFairy is a native Linux app (Rust, GTK4) that lets you read and send iMessage and SMS conversations from Linux. Your Mac does the actual messaging, and BubbleFairy connects to it securely over your local network.

[![Buy me a coffee](https://img.shields.io/badge/Buy%20me%20a%20coffee-teknocat-ffdd00?logo=buymeacoffee&logoColor=black)](https://buymeacoffee.com/teknocat)
&nbsp;If BubbleFairy is useful to you, you can [buy me a coffee](https://buymeacoffee.com/teknocat).

> **Status: early development.** There is nothing to install yet.

## How it works

```text
BubbleFairy on Linux ──SSH──> your Mac ──> Messages
```

Nothing related to iMessage runs on Linux. BubbleFairy talks to [imsg](https://github.com/openclaw/imsg), a small open-source command-line tool on your Mac. imsg reads your message history and sends messages through the Messages app. Setup turns on SSH access with a key that belongs only to BubbleFairy, and installs imsg for you.

## Requirements

- A Mac running macOS 14 (Sonoma) or newer, signed in to iMessage. It has to be awake and reachable on your network.
- For SMS: Text Message Forwarding turned on for the Mac on your iPhone.
- A Linux desktop that can run Flatpak apps. It's tested on KDE Plasma and GNOME.

## Planned setup

You do only these things on the Mac:

1. Turn on **Remote Login** (System Settings → General → Sharing).
2. Tick **Allow full disk access for remote users** in the Remote Login settings.
3. Click **Allow** when your Mac asks whether BubbleFairy can control Messages (once, on your first send).

BubbleFairy's setup wizard finds your Mac, asks for your Mac username and password once, and handles everything else.

## Not affiliated with Apple

BubbleFairy is an independent project. It is not made, endorsed or supported by Apple. iMessage and macOS are trademarks of Apple Inc.

## License

MIT. See [LICENSE](LICENSE).
