# Known limitations

BubbleFairy doesn't talk to Apple's servers. Your Mac does the messaging, and BubbleFairy drives it through the
[imsg](https://github.com/openclaw/imsg) command-line tool, using what macOS allows without switching off its
security protections. Some things Messages does can't be done that way, so BubbleFairy goes without them or
works around them. This page lists those, so none of it is a surprise.

This file is the source for the app's future help dialog: keep each entry short and in plain words, and update it when
a limitation appears, changes or goes away.

## Things you can't do from BubbleFairy

These need Messages' private "bridge", which only works with System Integrity Protection (SIP) turned off on the Mac.
BubbleFairy never asks you to do that.

- **Send tapback reactions.** You see reactions on messages, but you can't add one. (The only other route is
  automating Messages' screen, which can react only to a chat's newest message and is unreliable.)
- **Send inline replies.** Replies are shown like in Messages (the original in a small bubble above the replies, "N Replies"
  under the original), but BubbleFairy can't send a threaded reply. Messages can't be told which message to answer.
- **Typing indicators.** You don't see that someone is typing, and they don't see that you are.
- **Send read receipts.** Reading a chat in BubbleFairy doesn't tell the sender you read it, and doesn't mark it
  read on your Mac or other devices.
- **Edit a sent message.**
- **"Notify Anyway", and seeing a contact's Focus status** before you send.

## What stays in Messages

BubbleFairy is a front end for Messages on your Mac, not a replacement for it. Some things are managed there (or on
your Apple devices), and BubbleFairy simply respects the result:

- **Blocking numbers and filtering unknown senders.** Set these up in Messages on your Mac, iPhone or iPad.
  Blocked senders' messages never reach BubbleFairy, because Messages drops them first. BubbleFairy has no
  block list or spam filter of its own.
- **Your account, iMessage and SMS forwarding settings.**

## Where BubbleFairy keeps its own state

Some things Messages keeps can't be read from the Mac, or can't be changed from outside. BubbleFairy keeps its own
copy on this computer, so it **doesn't sync** with your Apple devices:

- **Unread dots.** A chat you read on another device clears here. A chat you read only in BubbleFairy loses its dot
  here, but stays unread on your Mac, iPhone and iPad.
- **Pinned chats.** Pins in BubbleFairy are separate from the pins in Messages.
- **Muted chats.** Mute a chat in BubbleFairy (right-click it) to stop its notifications and sound. Chats muted in
  Messages are not muted here (Messages stores that list in a form BubbleFairy can't match to chats), and the other
  way round.

## Chats

- **A person's chats are merged, group chats are not.** Messages shows everything with one contact as one conversation,
  and so does BubbleFairy. Two group chats with the same people stay separate, and a person who isn't in your
  contacts (a number or email with no card) can't be matched, so their chats stay separate.

## Files and pictures

- **Anything that isn't on the Mac can't be shown.** If a photo or file hasn't been downloaded by Messages on the
  Mac, BubbleFairy shows a dimmed placeholder. It can't ask the Mac to download it.
- **No previews for some files.** Photos, PDFs and video first frames get previews. Audio files and contact cards
  show an icon and details, not a picture.
- **Photos shown in chats are reduced to 900 px**, for speed. Opening one in the viewer fetches a larger copy (up to
  4096 px).

## Sending

- **A failed message stays.** "Try Again" sends it again as a new message; the failed one stays in the chat, and
  stays on your Apple devices until you delete it there.
- **SMS and RCS depend on your iPhone.** Text Message Forwarding must be on, and Messages on the Mac decides which
  service is used, as it does there.

## Link previews

- **Messages' own previews are shown** when Messages made one. A link sent without one (some sent through
  automation never get one) shows a plain link card.
- **BubbleFairy's own previews for plain links are off by default.** Turning them on in Settings makes this
  computer fetch the page, which tells the website your IP address. They are only fetched for public addresses.
- **Links in the middle of a message, or several links in one message, get no preview card** (Messages doesn't make
  them either). They stay clickable in the text.

## Chats and history

- **The chat list loads 100 chats at a time**, and more as you scroll to the end of it. A pinned chat that's older
  than what's loaded has no tile until more are loaded.
- **The photo viewer only walks through photos and videos already loaded** in the chat (or the info panel's tab).
  It never loads older history by itself.
- **Name & Photo** pictures that people share through Messages aren't shown.

## Needs your Mac

- The Mac has to be on, awake and reachable on your network (or through a VPN), and signed in to Messages. When
  it isn't, BubbleFairy shows "Offline" and keeps trying every few seconds.
- macOS 14 (Sonoma) or newer, because imsg needs it.
