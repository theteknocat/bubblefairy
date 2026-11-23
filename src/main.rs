mod bubble;
mod chatscroll;
mod demo;
mod mac;
mod pilllabel;
mod player;
mod setup;
mod snapshot;
mod sound;
mod tilegrid;
mod tray;
mod ui;
mod viewer;
mod webpreview;

use std::path::Path;

use gtk::prelude::*;
use gtk::{gdk, glib};

pub const APP_ID: &str = "io.github.theteknocat.BubbleFairy";
// ponytail: Phase 1 uses the Homebrew imsg; Phase 2 installs a pinned copy into the app's own dir on the Mac
pub const DEFAULT_IMSG: &str = "/opt/homebrew/bin/imsg";

fn load_config(path: &Path) -> mac::Config {
    let mut c = mac::Config {
        imsg: DEFAULT_IMSG.into(),
        notify: true,
        notify_sound: true,
        local_cache: true,
        sound: sound::DEFAULT.into(),
        ..Default::default()
    };
    for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
        match line.split_once('=') {
            Some(("host", v)) => c.host = v.trim().into(),
            Some(("user", v)) => c.user = v.trim().into(),
            Some(("imsg", v)) if !v.trim().is_empty() => c.imsg = v.trim().into(),
            Some(("map_previews", v)) => c.map_previews = v.trim() == "1",
            Some(("link_previews", v)) => c.link_previews = v.trim() == "1",
            Some(("notify", v)) => c.notify = v.trim() == "1",
            Some(("notify_sound", v)) => c.notify_sound = v.trim() == "1",
            Some(("sound", v)) if !v.trim().is_empty() => c.sound = v.trim().into(),
            Some(("notify_text", v)) => c.notify_text = v.trim().parse().unwrap_or(0),
            Some(("dir_images", v)) => c.dir_images = v.trim().into(),
            Some(("dir_files", v)) => c.dir_files = v.trim().into(),
            Some(("skip_intro", v)) => c.skip_intro = v.trim() == "1",
            Some(("autostart", v)) => c.autostart = v.trim() == "1",
            Some(("local_cache", v)) => c.local_cache = v.trim() == "1",
            _ => {}
        }
    }
    c
}

/// Write a small state file, creating its folder first. Errors are ignored: these files only remember
/// settings, so a failed write must not interrupt the app.
pub fn write_state(path: &Path, data: impl AsRef<[u8]>) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, data);
}

pub fn save_config(path: &Path, c: &mac::Config) {
    write_state(
        path,
        format!(
            "host={}\nuser={}\nimsg={}\nmap_previews={}\nlink_previews={}\nnotify={}\nnotify_sound={}\nsound={}\nnotify_text={}\ndir_images={}\ndir_files={}\nskip_intro={}\nautostart={}\nlocal_cache={}\n",
            c.host,
            c.user,
            c.imsg,
            u8::from(c.map_previews),
            u8::from(c.link_previews),
            u8::from(c.notify),
            u8::from(c.notify_sound),
            c.sound,
            c.notify_text,
            c.dir_images,
            c.dir_files,
            u8::from(c.skip_intro),
            u8::from(c.autostart),
            u8::from(c.local_cache)
        ),
    );
}

/// Image cache size cap. Everything in it can be fetched again from the Mac.
const CACHE_MAX: u64 = 500 * 1024 * 1024;
const FILES_DAYS: u64 = 30; // how long a copied attachment is kept

/// Delete the oldest cached images until the folder is under `max` bytes.
// ponytail: oldest by write time, not last use; fine while re-fetching is cheap
fn prune_cache(dir: &Path, max: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = rd
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then(|| (m.modified().ok(), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= max {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

/// Opened attachments are kept one folder each under `files/`: remove the folders older than `days` (by the time the
/// copy was made; opening it again after that just copies it from the Mac again).
fn prune_old_dirs(dir: &Path, days: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
    for e in rd.flatten() {
        if e.metadata()
            .is_ok_and(|m| m.is_dir() && m.modified().is_ok_and(|t| t < cutoff))
        {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn main() -> glib::ExitCode {
    let _ = gstreamer::init(); // for the video player
    let config_file = glib::user_config_dir().join("bubblefairy/config");
    let data = glib::user_data_dir().join("bubblefairy");
    let paths = mac::Paths {
        key: data.join("id_ed25519"),
        known_host: data.join("mac_host_key"),
    };
    let public_key = match mac::ensure_key(&paths.key) {
        Ok(k) => k,
        Err(e) => {
            eprintln!(
                "can't create BubbleFairy's SSH key at {}: {e}",
                paths.key.display()
            );
            return glib::ExitCode::FAILURE;
        }
    };
    // shortcut: the BF_* dev switches (screenshots, demos, wizard tests) are read in release builds too, about 100 reads.
    // Only BF_LINKS=1 plus a Settings save could write a privacy setting by accident, and only on purpose. Audit
    // finding 4 was skipped on 2026-11-20; gate them on `cfg!(debug_assertions)` through one helper if that ever matters.
    let demo = std::env::var_os("BF_DEMO").is_some();
    let mut cfg = load_config(&config_file);
    if demo {
        cfg.host = "demo".into();
    }
    let store = ui::Store {
        config_file,
        known_host: paths.known_host.clone(),
        image_cache: glib::user_cache_dir().join("bubblefairy/images"),
        chats_cache: glib::user_cache_dir().join("bubblefairy/chats.json"),
        contacts_cache: glib::user_cache_dir().join("bubblefairy/contacts.json"),
        seen_file: data.join("seen"),
        resent_file: data.join("resent"),
        pins_file: data.join("pins"),
        muted_file: data.join("muted"),
        web_file: data.join("web_links"),
        public_key,
    };

    let cache = store.image_cache.clone();
    std::thread::spawn(move || {
        prune_cache(&cache, CACHE_MAX);
        prune_old_dirs(&cache.with_file_name("files"), FILES_DAYS);
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _ = setup::RT.set(rt.handle().clone());
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
    if demo {
        rt.spawn(demo::run(cmd_rx, ev_tx));
    } else {
        rt.spawn(mac::run(cfg.clone(), paths, cmd_rx, ev_tx));
    }

    let mut flags = gtk::gio::ApplicationFlags::empty();
    if std::env::var_os("BF_SHOT").is_some() {
        flags |= gtk::gio::ApplicationFlags::NON_UNIQUE; // don't hand off to a running copy
    }
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(flags)
        .build();
    app.connect_startup(|_| {
        // dev aid for screenshots
        if let Some(settings) = gtk::Settings::default() {
            if std::env::var_os("BF_DARK").is_some() {
                settings.set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Dark);
            } else if std::env::var_os("BF_LIGHT").is_some() {
                settings.set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Light);
            }
        }
        // our own icons (the chat bubble); installed under share/icons in the Flatpak, found here when run from source
        gtk::IconTheme::for_display(&gdk::Display::default().expect("display"))
            .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("style.css"));
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("display"),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
    let parts = std::cell::RefCell::new(Some((store, cfg, cmd_tx, ev_rx)));
    app.connect_activate(move |app| match parts.borrow_mut().take() {
        Some((store, cfg, cmd, ev)) => ui::build(app, store, cfg, cmd, ev),
        None => {
            if let Some(w) = app.active_window() {
                w.present();
            }
        }
    });
    // --tray (from the login entry) is ours, not GTK's
    let args: Vec<String> = std::env::args().filter(|a| a != "--tray").collect();
    let code = app.run_with_args(&args);
    drop(rt); // closes the SSH session; imsg on the Mac exits when its stdin closes
    code
}

#[cfg(test)]
mod tests {
    #[test]
    fn autostart_survives_a_save_and_load() {
        let f = std::env::temp_dir().join(format!("bf-cfg-{}", std::process::id()));
        super::save_config(
            &f,
            &crate::mac::Config {
                autostart: true,
                ..Default::default()
            },
        );
        assert!(super::load_config(&f).autostart);
        super::save_config(&f, &crate::mac::Config::default());
        assert!(!super::load_config(&f).autostart, "off unless chosen");
        std::fs::remove_file(&f).unwrap();
    }

    #[test]
    fn prune_removes_oldest_first() {
        let d = std::env::temp_dir().join(format!("bf-prune-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            std::fs::write(d.join(name), [0u8; 10]).unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_000 + i as u64);
            std::fs::File::options()
                .write(true)
                .open(d.join(name))
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        super::prune_cache(&d, 15);
        assert!(!d.join("a").exists() && !d.join("b").exists() && d.join("c").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn old_attachment_folders_go_and_new_ones_stay() {
        let d = std::env::temp_dir().join(format!("bf-files-{}", std::process::id()));
        for (name, age_days) in [("old", 40u64), ("new", 2)] {
            std::fs::create_dir_all(d.join(name)).unwrap();
            std::fs::write(d.join(name).join("f.pdf"), b"x").unwrap();
            let t =
                std::time::SystemTime::now() - std::time::Duration::from_secs(age_days * 86_400);
            std::fs::File::open(d.join(name))
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        super::prune_old_dirs(&d, 30);
        assert!(!d.join("old").exists() && d.join("new").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
