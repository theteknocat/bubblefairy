# Ponytail review, 2026-11-21

Reviewed the tray commit (f5c3ac9) and the changes made from its findings.

1. **Must fix: a failed tray start left the login entry registered** (`start_tray`). Fixed: login start no longer depends on the tray (commit 29e4798).
2. **Minor: tray unread dot could be left stale** (`update_title`, updates spawned on two threads). Fixed: one shared flag the icon re-reads (c1f6073).
3. **Minor: a held notification could pop up after the window came to the front** (my own notification change). Fixed: `show_notification` returns when the window is active (65ea940).
4. **Related bug found by testing: a message whose photo Messages hadn't downloaded yet showed "not downloaded" for good, and its notification had no picture.** Fixed: re-check at 2/5/10/20/40 s, and hold the notification for the picture (65ea940).
5. **Minor: the first-close tray hint is used up even when do-not-disturb hides it** (`tray_hint`). Tested: do-not-disturb holds it in the notification list, so it isn't lost, but you'd only see it after turning do-not-disturb off. Urgent priority sent through GNotification never showed at all; `x-kde-show-when-inhibited` (from a search summary) isn't in Plasma's source. Fixed: the hint goes straight to the notification service with `x-kde-user-action-feedback`, which Plasma's filter shows through do-not-disturb (`notificationfilterproxymodel.cpp`, `notification.cpp`). Checked from the shell, then in the app.
6. **Minor: the login option could show "on" when the portal made no login entry** (`tray::set_autostart` only reported errors, never the portal's answer). Fixed: it returns whether the portal's `auto_start()` matches the request; if not, the setting is saved back as it was and a banner says so.

**Verdict:** all six findings fixed and committed. Not checked: the tray, notification icon and login entry in the installed Flatpak (only the dev run was tried), a no-tray desktop, and the minimized login start on a real login.
