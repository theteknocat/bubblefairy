//! Notification sounds: the bundled chimes (`data/sounds`, made by `scripts/make_sounds.py`) or a file the user chose,
//! played through GStreamer. The Flatpak runtime has no sound theme to borrow from, so the app brings its own.

use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gtk::prelude::*;
use gtk::{gio, glib};

/// (name in the config, name shown in Settings)
pub const BUNDLED: [(&str, &str); 6] = [("chime", "Chime"), ("bell", "Bell"), ("marimba", "Marimba"), ("pop", "Pop"), ("drop", "Drop"), ("ping", "Ping")];
pub const DEFAULT: &str = "chime";

/// Where the bundled sounds are: next to the sources when run from them, else where the Flatpak installs them.
fn dir() -> Option<PathBuf> {
    [concat!(env!("CARGO_MANIFEST_DIR"), "/data/sounds"), "/app/share/bubblefairy/sounds", "/usr/share/bubblefairy/sounds"].iter().map(PathBuf::from).find(|p| p.is_dir())
}

/// The file a setting stands for: a bundled name, or `file:<path>` for the user's own.
fn path_of(choice: &str) -> Option<PathBuf> {
    match choice.strip_prefix("file:") {
        Some(f) => Some(PathBuf::from(f)),
        None => dir().map(|d| d.join(format!("{choice}.wav"))),
    }
}

/// Play the sound once. A missing or unreadable file falls back to the default chime; nothing here can fail loudly.
pub fn play(choice: &str) {
    let Some(path) = path_of(choice).filter(|p| p.exists()).or_else(|| path_of(DEFAULT)) else { return };
    let uri = gio::File::for_path(&path).uri();
    let Ok(pipe) = gst::ElementFactory::make("playbin").property("uri", uri.as_str()).build() else { return };
    let Some(bus) = pipe.bus() else { return };
    let p2 = pipe.clone();
    let Ok(watch) = bus.add_watch_local(move |_, msg| match msg.view() {
        gst::MessageView::Eos(_) | gst::MessageView::Error(_) => {
            if std::env::var_os("BF_SOUND_LOG").is_some() {
                eprintln!("sound: {:?}", msg.view()); // dev aid: did it play to the end?
            }
            let _ = p2.set_state(gst::State::Null);
            glib::ControlFlow::Break
        }
        _ => glib::ControlFlow::Continue,
    }) else {
        return;
    };
    // the source is removed when the watch function breaks; keeping the guard would only remove it a second time
    std::mem::forget(watch);
    let _ = pipe.set_state(gst::State::Playing);
}

thread_local! {
    static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// The sound for an arriving message: not again within a second and a half of the last one (a burst is one ping).
pub fn play_arrival(choice: &str) {
    let now = Instant::now();
    if LAST.get().is_some_and(|t| now.duration_since(t) < Duration::from_millis(1500)) {
        return;
    }
    LAST.set(Some(now));
    play(choice);
}

/// Is the desktop's do-not-disturb on? (KDE and others expose `Inhibited` on the notification service.) Anything we
/// can't find out counts as off, so a sound is never lost to a failed question.
pub async fn desktop_inhibited() -> bool {
    let Ok(bus) = gio::bus_get_future(gio::BusType::Session).await else { return false };
    let args = ("org.freedesktop.Notifications", "Inhibited").to_variant();
    let reply = bus
        .call_future(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&args),
            glib::VariantTy::new("(v)").ok(),
            gio::DBusCallFlags::NONE,
            300,
        )
        .await;
    reply.ok().and_then(|v| v.child_value(0).as_variant()?.get::<bool>()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_sounds_exist() {
        for (name, _) in super::BUNDLED {
            let f = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/sounds").join(format!("{name}.wav"));
            assert!(f.exists(), "{name}");
        }
        assert!(super::BUNDLED.iter().any(|(n, _)| *n == super::DEFAULT));
    }
}
