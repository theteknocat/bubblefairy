mod bubble;
mod chatscroll;
mod demo;
mod mac;
mod ui;
mod viewer;

use std::path::Path;

use gtk::prelude::*;
use gtk::{gdk, glib};

pub const APP_ID: &str = "io.github.theteknocat.BubbleFairy";
// ponytail: Phase 1 uses the Homebrew imsg; Phase 2 installs a pinned copy into the app's own dir on the Mac
pub const DEFAULT_IMSG: &str = "/opt/homebrew/bin/imsg";

fn load_config(path: &Path) -> mac::Config {
    let mut c = mac::Config { imsg: DEFAULT_IMSG.into(), map_previews: true, ..Default::default() };
    for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
        match line.split_once('=') {
            Some(("host", v)) => c.host = v.trim().into(),
            Some(("user", v)) => c.user = v.trim().into(),
            Some(("imsg", v)) if !v.trim().is_empty() => c.imsg = v.trim().into(),
            Some(("map_previews", v)) => c.map_previews = v.trim() == "1",
            _ => {}
        }
    }
    c
}

pub fn save_config(path: &Path, c: &mac::Config) {
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(path, format!("host={}\nuser={}\nimsg={}\nmap_previews={}\n", c.host, c.user, c.imsg, u8::from(c.map_previews)));
}

/// Image cache size cap. Everything in it can be fetched again from the Mac.
const CACHE_MAX: u64 = 500 * 1024 * 1024;

/// Delete the oldest cached images until the folder is under `max` bytes.
// ponytail: oldest by write time, not last use; fine while re-fetching is cheap
fn prune_cache(dir: &Path, max: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
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

fn main() -> glib::ExitCode {
    let config_file = glib::user_config_dir().join("bubblefairy/config");
    let data = glib::user_data_dir().join("bubblefairy");
    let paths = mac::Paths { key: data.join("id_ed25519"), known_host: data.join("mac_host_key") };
    let public_key = match mac::ensure_key(&paths.key) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("can't create BubbleFairy's SSH key at {}: {e}", paths.key.display());
            return glib::ExitCode::FAILURE;
        }
    };
    let demo = std::env::var_os("BF_DEMO").is_some();
    let mut cfg = load_config(&config_file);
    if demo {
        cfg.host = "demo".into();
    }
    let store = ui::Store {
        config_file,
        known_host: paths.known_host.clone(),
        image_cache: glib::user_cache_dir().join("bubblefairy/images"),
        seen_file: data.join("seen"),
        public_key,
    };

    let cache = store.image_cache.clone();
    std::thread::spawn(move || prune_cache(&cache, CACHE_MAX));

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
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
    let app = gtk::Application::builder().application_id(APP_ID).flags(flags).build();
    app.connect_startup(|_| {
        if std::env::var_os("BF_DARK").is_some() {
            // dev aid for screenshots
            gtk::Settings::default().unwrap().set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Dark);
        } else if std::env::var_os("BF_LIGHT").is_some() {
            gtk::Settings::default().unwrap().set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Light);
            
        }
        // our own icons (the chat bubble); installed under share/icons in the Flatpak, found here when run from source
        gtk::IconTheme::for_display(&gdk::Display::default().expect("display")).add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
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
    let code = app.run();
    drop(rt); // closes the SSH session; imsg on the Mac exits when its stdin closes
    code
}

#[cfg(test)]
mod tests {
    #[test]
    fn prune_removes_oldest_first() {
        let d = std::env::temp_dir().join(format!("bf-prune-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            std::fs::write(d.join(name), [0u8; 10]).unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + i as u64);
            std::fs::File::options().write(true).open(d.join(name)).unwrap().set_modified(t).unwrap();
        }
        super::prune_cache(&d, 15);
        assert!(!d.join("a").exists() && !d.join("b").exists() && d.join("c").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
