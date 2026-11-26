//! First-run setup: find the Mac, sign in once with its password, install BubbleFairy's key and its own copy of imsg,
//! check that Messages can be read, and (optionally) send a test message. The backend half (this file's top) runs on
//! tokio; the wizard window (below) runs on the GTK thread and calls it through `rt`.

// A poisoned Mutex means another thread panicked mid-update; crashing on `lock().unwrap()` surfaces that bug
// instead of running on with half-updated state.
#![allow(clippy::unwrap_used)]

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib};
use russh::ChannelMsg;
use russh::client::{self, KeyboardInteractiveAuthResponse};
use russh::keys::PrivateKeyWithHashAlg;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::mac::{Handler, sh_quote};

/// The tokio runtime, set once by `main`.
pub static RT: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// The imsg release we install and test against. The zip is a universal (Intel + Apple Silicon) build.
pub const IMSG_VERSION: &str = "0.21.0";
pub const IMSG_SHA256: &str = "0da64a1c2c125eba033c4092ea17a6997f1ccc51dbd8be68ce77aa43df6d8885";
const IMSG_URL: &str = "https://github.com/openclaw/imsg/releases/download/v0.21.0/imsg-macos.zip";

/// Run `fut` on the tokio runtime and wait for it from the GTK thread.
async fn rt<T: Send + 'static>(fut: impl Future<Output = T> + Send + 'static) -> T {
    let (tx, rx) = tokio::sync::oneshot::channel();
    RT.get().expect("tokio runtime").spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.await.expect("setup task ended")
}

// ---------- finding Macs ----------

/// A computer on the network offering SSH.
#[derive(Clone, Debug)]
pub struct Found {
    pub name: String,
    pub host: String,
}

/// Look for SSH hosts over mDNS for a few seconds. Each is sent as it is found; `None` marks the end of the scan.
fn discover(tx: mpsc::UnboundedSender<Option<Found>>) {
    std::thread::spawn(move || {
        let Ok(daemon) = mdns_sd::ServiceDaemon::new() else {
            let _ = tx.send(None);
            return;
        };
        if let Ok(rx) = daemon.browse("_ssh._tcp.local.") {
            let end = Instant::now() + Duration::from_secs(6);
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                match rx.recv_timeout(left) {
                    Ok(mdns_sd::ServiceEvent::ServiceResolved(r)) => {
                        let name = r
                            .fullname
                            .strip_suffix("._ssh._tcp.local.")
                            .unwrap_or(&r.fullname)
                            .to_string();
                        let _ = tx.send(Some(Found {
                            name,
                            host: r.host.trim_end_matches('.').to_string(),
                        }));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        let _ = daemon.shutdown();
        let _ = tx.send(None);
    });
}

/// Does something answer on the SSH port there?
async fn probe(host: &str) -> Result<(), String> {
    let addr = crate::mac::resolve(host).await?;
    match tokio::time::timeout(Duration::from_secs(4), tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_)) => Ok(()),
        _ => Err(format!(
            "{host} is on the network but doesn't answer on the SSH port. Is Remote Login turned on?"
        )),
    }
}

// ---------- talking to the Mac ----------

/// One SSH connection to the Mac, first without and then with a login.
pub struct Link {
    session: client::Handle<Handler>,
    /// the host key the Mac showed (an OpenSSH public key line)
    pub host_key: String,
}

async fn open(host: &str) -> Result<Link, String> {
    let addr = crate::mac::resolve(host).await?;
    let seen = Arc::new(Mutex::new(None));
    let config = client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        nodelay: true,
        ..Default::default()
    };
    let handler = Handler {
        known: None,
        seen: seen.clone(),
    }; // the person decides on the fingerprint
    let session = match tokio::time::timeout(
        Duration::from_secs(10),
        client::connect(Arc::new(config), addr, handler),
    )
    .await
    {
        Err(_) => {
            return Err(format!(
                "{host} didn't answer. Is Remote Login on, and is the Mac awake?"
            ));
        }
        Ok(Err(e)) => return Err(format!("couldn't connect: {e}")),
        Ok(Ok(s)) => s,
    };
    let host_key = seen.lock().unwrap().clone().unwrap_or_default();
    Ok(Link { session, host_key })
}

impl Link {
    /// Sign in with the Mac account's password (once; it is not kept).
    async fn login(&mut self, user: &str, password: &str) -> Result<(), String> {
        let bad = || "The Mac didn't accept that user name and password.".to_string();
        let r = self
            .session
            .authenticate_password(user, password)
            .await
            .map_err(|e| format!("sign-in failed: {e}"))?;
        if r.success() {
            return Ok(());
        }
        // some Macs only offer "keyboard-interactive" (the same password, asked as a prompt)
        let mut r = self
            .session
            .authenticate_keyboard_interactive_start(user, None)
            .await
            .map_err(|e| format!("sign-in failed: {e}"))?;
        for _ in 0..4 {
            match r {
                KeyboardInteractiveAuthResponse::Success => return Ok(()),
                KeyboardInteractiveAuthResponse::Failure { .. } => return Err(bad()),
                KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                    let answers = prompts.iter().map(|_| password.to_string()).collect();
                    r = self
                        .session
                        .authenticate_keyboard_interactive_respond(answers)
                        .await
                        .map_err(|e| format!("sign-in failed: {e}"))?;
                }
            }
        }
        Err(bad())
    }

    /// Dev aid (`BF_SETUP_KEYLOGIN`): sign in with an existing key instead of a password, to try the later steps.
    async fn login_key(&mut self, user: &str, key_path: &std::path::Path) -> Result<(), String> {
        let key = russh::keys::load_secret_key(key_path, None).map_err(|e| e.to_string())?;
        let r = self
            .session
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), None))
            .await
            .map_err(|e| e.to_string())?;
        if r.success() {
            Ok(())
        } else {
            Err("the key was refused".into())
        }
    }

    /// Run a command; `input` goes to its stdin. Returns the exit status and everything it printed.
    async fn run(
        &self,
        cmd: &str,
        input: Option<&[u8]>,
        wait: Duration,
    ) -> Result<(u32, String), String> {
        let work = async {
            let mut ch = self
                .session
                .channel_open_session()
                .await
                .map_err(|e| e.to_string())?;
            ch.exec(true, cmd.to_string())
                .await
                .map_err(|e| e.to_string())?;
            if let Some(bytes) = input {
                ch.data(bytes).await.map_err(|e| e.to_string())?;
                ch.eof().await.map_err(|e| e.to_string())?;
            }
            let (mut out, mut code) = (Vec::new(), 0);
            while let Some(msg) = ch.wait().await {
                match msg {
                    ChannelMsg::Data { data } => out.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, .. } => out.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => code = exit_status,
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
            Ok::<_, String>((code, String::from_utf8_lossy(&out).trim().to_string()))
        };
        tokio::time::timeout(wait, work)
            .await
            .map_err(|_| "the Mac took too long to answer".to_string())?
    }

    /// macOS version, home folder, and the tools BubbleFairy needs.
    async fn check_mac(&self) -> Result<(String, String), String> {
        let script = "sw_vers -productVersion; printf '%s\\n' \"$HOME\"; scutil --get ComputerName 2>/dev/null || hostname; for t in sqlite3 qlmanage sips unzip; do command -v $t >/dev/null || echo \"MISSING $t\"; done";
        let (_, out) = self.run(script, None, Duration::from_secs(20)).await?;
        let mut lines = out.lines();
        let version = lines.next().unwrap_or("").to_string();
        let home = lines.next().unwrap_or("").to_string();
        let name = lines.next().unwrap_or("").to_string();
        let missing: Vec<&str> = lines.filter_map(|l| l.strip_prefix("MISSING ")).collect();
        let major: u32 = version
            .split('.')
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        if major == 0 || home.is_empty() {
            return Err("this doesn't look like a Mac".into());
        }
        if major < 14 {
            return Err(format!(
                "this Mac runs macOS {version}; BubbleFairy needs macOS 14 or newer"
            ));
        }
        if !missing.is_empty() {
            return Err(format!("this Mac is missing {}", missing.join(", ")));
        }
        Ok((home, format!("Connected to {name} · macOS {version}")))
    }

    /// Add `key` to the Mac's authorized keys (once), with the modes sshd insists on.
    async fn install_key(&self, key: &str) -> Result<(), String> {
        let k = sh_quote(key.trim());
        let cmd = format!(
            "umask 077; mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && \
             {{ [ -z \"$(tail -c1 ~/.ssh/authorized_keys)\" ] || echo >> ~/.ssh/authorized_keys; }} && \
             {{ grep -qxF {k} ~/.ssh/authorized_keys || printf '%s\\n' {k} >> ~/.ssh/authorized_keys; }}"
        );
        match self.run(&cmd, None, Duration::from_secs(20)).await? {
            (0, _) => Ok(()),
            (_, out) => Err(format!("couldn't add the key on the Mac: {out}")),
        }
    }

    /// Put our own copy of imsg in `<home>/Library/Application Support/<folder>/imsg/` and return the path of the program.
    async fn install_imsg(&self, zip: &[u8], home: &str, folder: &str) -> Result<String, String> {
        let base = format!("$HOME/Library/Application Support/{folder}");
        let cmd = format!(
            "b=\"{base}\"; d=\"$b/imsg\"; rm -rf \"$d.new\" && mkdir -p \"$d.new\" && cat > \"$d.new/imsg-macos.zip\" && cd \"$d.new\" && \
             unzip -q -o imsg-macos.zip && rm -f imsg-macos.zip && chmod +x imsg && ./imsg --version && cd \"$b\" && rm -rf \"$d\" && mv \"$d.new\" \"$d\""
        );
        let (code, out) = self.run(&cmd, Some(zip), Duration::from_secs(120)).await?;
        if code != 0 || !out.contains(IMSG_VERSION) {
            return Err(format!("couldn't install imsg on the Mac: {out}"));
        }
        Ok(format!(
            "{home}/Library/Application Support/{folder}/imsg/imsg"
        ))
    }

    /// Can the Messages database (and the Contacts) be read from here? (The "full disk access for remote users" switch.)
    async fn check_access(&self) -> Result<bool, String> {
        let cmd = "sqlite3 -readonly ~/Library/Messages/chat.db 'select count(*) from chat' >/dev/null 2>&1 && echo OK || echo NO";
        let (_, out) = self.run(cmd, None, Duration::from_secs(30)).await?;
        Ok(out.trim() == "OK")
    }

    /// Send a short message to `to` through Messages, then check that Messages has it as sent. The first send makes
    /// macOS ask for permission, so this waits for the person to answer on the Mac.
    async fn test_send(&self, imsg: &str, to: &str) -> Result<(), String> {
        const TEXT: &str = "BubbleFairy setup test";
        let cmd = format!(
            "{} send --to {} --text {} --service auto 2>&1",
            sh_quote(imsg),
            sh_quote(to),
            sh_quote(TEXT)
        );
        match self.run(&cmd, None, Duration::from_secs(180)).await? {
            (0, _) => {}
            (_, out) => {
                return Err(if out.is_empty() {
                    "the Mac didn't send it".into()
                } else {
                    out
                });
            }
        }
        // imsg is done once Messages took the message; the database says whether it went out
        let q = format!(
            "sqlite3 -readonly ~/Library/Messages/chat.db \"select error from message where is_from_me=1 and text='{TEXT}' and date/1000000000+978307200 > strftime('%s','now')-300 order by ROWID desc limit 1\""
        );
        for _ in 0..8 {
            let (_, out) = self.run(&q, None, Duration::from_secs(15)).await?;
            if let Ok(code) = out.trim().parse::<i64>() {
                return if code == 0 {
                    Ok(())
                } else {
                    Err(format!("Messages couldn't send it (error {code})."))
                };
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err("Messages took the message, but it doesn't show as sent yet. Look at Messages on the Mac.".into())
    }

    /// Dev profile only: take the throwaway key out again.
    async fn remove_key(&self, key: &str) {
        let id = key.split_whitespace().nth(1).unwrap_or("").to_string();
        if id.len() > 20 {
            let _ = self
                .run(&remove_key_cmd(&id), None, Duration::from_secs(20))
                .await;
        }
    }
}

/// Shell line that drops the lines containing `id` from the Mac's authorized_keys. grep exits 1 when nothing is left
/// (fine) and 2 on an error: then the file must be left alone, never overwritten by a half-written copy.
fn remove_key_cmd(id: &str) -> String {
    format!(
        "grep -vF {} ~/.ssh/authorized_keys > ~/.ssh/ak.tmp; [ $? -le 1 ] && cat ~/.ssh/ak.tmp > ~/.ssh/authorized_keys; rm -f ~/.ssh/ak.tmp",
        sh_quote(id)
    )
}

/// Does signing in with the key work now? Tried a few times: right after the key was added, the first sign-in has
/// been seen to fail and the next one to work.
async fn verify_key(host: &str, user: &str, key_path: PathBuf) -> Result<Link, String> {
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        match verify_key_once(host, user, key_path.clone()).await {
            Ok(l) => return Ok(l),
            Err(e) => last = e,
        }
    }
    Err(last)
}

async fn verify_key_once(host: &str, user: &str, key_path: PathBuf) -> Result<Link, String> {
    let mut link = open(host).await?;
    let key = russh::keys::load_secret_key(&key_path, None)
        .map_err(|e| format!("can't read the key: {e}"))?;
    let r = link
        .session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), None))
        .await
        .map_err(|e| format!("{e}"))?;
    if r.success() {
        Ok(link)
    } else {
        Err("the Mac didn't accept BubbleFairy's key (is the Mac set to allow key sign-in?)".into())
    }
}

/// Where a bundled copy of the imsg zip is, if this build has one (the Flatpak does).
fn bundled_zip() -> Option<PathBuf> {
    [
        "/app/share/bubblefairy/imsg-macos.zip",
        "/usr/share/bubblefairy/imsg-macos.zip",
        concat!(env!("CARGO_MANIFEST_DIR"), "/vendor/imsg-macos.zip"),
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// The imsg zip: the bundled copy, else downloaded; either way it must match the hash built into the app.
async fn imsg_zip() -> Result<Vec<u8>, String> {
    let bytes = match bundled_zip().and_then(|p| std::fs::read(p).ok()) {
        Some(b) => b,
        None => {
            use webkit6::soup::prelude::*;
            let session = webkit6::soup::Session::new();
            session.set_timeout(60);
            let msg = webkit6::soup::Message::new("GET", IMSG_URL).map_err(|e| e.to_string())?;
            let bytes = session
                .send_and_read_future(&msg, glib::Priority::DEFAULT)
                .await
                .map_err(|e| format!("couldn't download imsg: {e}"))?;
            if msg.status() != webkit6::soup::Status::Ok {
                return Err("couldn't download imsg (the download page didn't answer)".into());
            }
            bytes.to_vec()
        }
    };
    if sha256_hex(&bytes) != IMSG_SHA256 {
        return Err("the imsg file doesn't match the expected checksum, so it was not used".into());
    }
    Ok(bytes)
}

// ---------- the wizard window ----------

/// What the wizard needs from the app.
pub struct Ctx {
    /// the app's public key line, and where the private key is
    pub public_key: String,
    pub key_path: PathBuf,
    /// the Linux user's name, offered as the default Mac user
    pub default_user: String,
    /// Dev profile: a throwaway key, a separate folder on the Mac, and nothing saved
    pub test: bool,
    /// called with (host, user, imsg path on the Mac, host key line) when the wizard finishes
    pub done: Rc<dyn Fn(String, String, String, String)>,
    /// called when the wizard window closes, finished or not (the first run shows the main window then)
    pub closed: Rc<dyn Fn()>,
    /// the welcome page is left out (the person ticked "Don't show this again"), and how to remember that
    pub skip_intro: bool,
    pub set_skip_intro: Rc<dyn Fn(bool)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Wait,
    Run,
    Ok,
    Bad,
}

struct W {
    win: gtk::Window,
    ctx: Ctx,
    pages: gtk::Stack,
    back: gtk::Button,
    next: gtk::Button,
    // page 1: the Mac
    host: gtk::Entry,
    host_status: gtk::Stack,
    drop_btn: gtk::Button,
    host_msg: gtk::Label,
    pick: gtk::Box,
    pick_pop: gtk::Popover,
    scan: gtk::Spinner,
    scan_row: gtk::Box,
    scan_text: gtk::Label,
    found: RefCell<Vec<Found>>,
    locked: Cell<bool>,
    host_ok: Cell<bool>,
    editing: Cell<bool>,
    // page 2: the account
    user: gtk::Entry,
    pass: gtk::PasswordEntry,
    acct_msg: gtk::Label,
    // page 3: the fingerprint
    // page 4: the work
    steps: Vec<(gtk::Image, gtk::Spinner, gtk::Label)>,
    work_msg: gtk::Label,
    work_info: gtk::Label,
    work_btn: gtk::Button,
    // page 5: the test message
    to: gtk::Entry,
    test_msg: gtk::Label,
    test_title: gtk::Label,
    skip_note: gtk::Label,
    test_btn: gtk::Button,
    test_stack: gtk::Stack,
    stop_btn: gtk::Button,
    test_gen: Cell<u64>,
    connecting: gtk::Box,
    connecting_text: gtk::Label,
    // connection state
    online: RefCell<Option<Arc<Link>>>,
    imsg: RefCell<String>,
    page: Cell<usize>,
    /// the last thing to act on the arrow button was the keyboard (so the list's entries take focus), not the pointer
    btn_key: Cell<bool>,
    dots: Vec<gtk::Box>,
    /// the welcome page is showing (before page 0)
    intro: Cell<bool>,
    skip_cb: gtk::CheckButton,
}

const PAGES: [&str; 3] = ["mac", "work", "test"];
const STEPS: [&str; 6] = [
    "Signing in",
    "Checking the Mac",
    "Adding BubbleFairy's key",
    "Checking that the key works",
    "Installing imsg",
    "Checking that Messages can be read",
];

/// Open the wizard over `parent`.
pub fn open_wizard(app: &gtk::Application, parent: Option<&gtk::Window>, ctx: Ctx) {
    let win = gtk::Window::builder()
        .application(app)
        .title("Set up BubbleFairy")
        .default_width(720)
        .default_height(540)
        .resizable(false)
        .build();
    if let Some(p) = parent {
        win.set_transient_for(Some(p));
        win.set_modal(true);
    }
    // the app's own look: the gradient window, a glassy panel, the brand in the title bar
    win.add_css_class("bf");
    win.add_css_class("wizard");
    let head = gtk::HeaderBar::new();
    head.add_css_class("bf-head");
    head.set_title_widget(Some(&crate::ui::brand()));
    win.set_titlebar(Some(&head));
    let closed = ctx.closed.clone();
    win.connect_close_request(move |_| {
        closed();
        glib::Propagation::Proceed
    });
    let label = |t: &str, class: &str| {
        let l = gtk::Label::new(Some(t));
        l.set_wrap(true);
        l.set_xalign(0.0);
        if !class.is_empty() {
            l.add_css_class(class);
        }
        l
    };
    // every page: the title on top, the fairy on the left (a third), the page's own content on the right (two thirds)
    let outers: RefCell<Vec<gtk::Box>> = RefCell::new(Vec::new());
    let arts: RefCell<Vec<gtk::Box>> = RefCell::new(Vec::new());
    let page = |title: &str| {
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 14);
        outer.set_margin_top(8);
        outer.append(&label(title, "setup-title"));
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 22);
        // a Picture keeps the artwork's own shape (a gtk::Image draws icons into a square)
        let icon = gtk::IconTheme::for_display(&gtk::gdk::Display::default().expect("display"))
            .lookup_icon(
                "bf-intro",
                &[],
                260,
                1,
                gtk::TextDirection::Ltr,
                gtk::IconLookupFlags::empty(),
            );
        let art = gtk::Picture::for_file(&icon.file().expect("icon file"));
        art.set_content_fit(gtk::ContentFit::Contain);
        art.set_valign(gtk::Align::Start);
        art.set_halign(gtk::Align::Start);
        art.set_hexpand(false);
        art.set_size_request(260, 169);
        let art_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        art_box.set_valign(gtk::Align::Start);
        art_box.set_hexpand(false);
        art_box.append(&art);
        row.append(&art_box);
        arts.borrow_mut().push(art_box);
        // divider + content share a box that is only as tall as the content, so the divider matches it
        let side = gtk::Box::new(gtk::Orientation::Horizontal, 22);
        side.set_hexpand(true);
        side.set_valign(gtk::Align::Start);
        let divider = gtk::Box::new(gtk::Orientation::Vertical, 0);
        divider.add_css_class("wiz-divider");
        side.append(&divider);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_hexpand(true);
        side.append(&content);
        row.append(&side);
        row.set_vexpand(true);
        outer.append(&row);
        outers.borrow_mut().push(outer);
        content
    };

    // 0: welcome, and the one thing to do on the Mac first
    let p0 = page("Welcome to BubbleFairy");
    // under the fairy: what this is and what the Mac needs
    let intro_l = label("", "dim");
    intro_l.set_markup("<b>BubbleFairy shows your Messages on this computer by talking to your Mac.</b>\n\nThe Mac must be running <b>macOS 14 (Sonoma)</b> or newer, switched on and signed in.");
    intro_l.set_width_chars(30);
    intro_l.set_max_width_chars(30);
    intro_l.set_margin_start(28); // lines up with the title
    intro_l.set_valign(gtk::Align::Start);
    intro_l.set_margin_top(12);
    arts.borrow()[0].append(&intro_l);
    p0.append(&label("To get started:", ""));
    // a numbered step: the number, the instruction, and a smaller dim line of extra help
    let step = |n: u8, text: &str, hint: Option<&str>| {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("wiz-step");
        let num = label("", "");
        num.set_markup(&format!("<b>{n}.</b>"));
        num.set_valign(gtk::Align::Start);
        row.append(&num);
        let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let t = label("", "");
        t.set_markup(text);
        col.append(&t);
        if let Some(h) = hint {
            col.append(&label(h, "wiz-hint"));
        }
        row.append(&col);
        row
    };
    let steps = gtk::Box::new(gtk::Orientation::Vertical, 0);
    p0.append(&steps);
    steps.append(&step(
        1,
        "On the Mac, open <b>System Settings → General → Sharing</b>.",
        None,
    ));
    steps.append(&step(
        2,
        "Turn on <b>Remote Login</b>.",
        Some("Scroll down if you don't see it; some versions keep it under Advanced."),
    ));
    steps.append(&step(
        3,
        "Turn on <b>Allow full disk access for remote users</b>.",
        Some("It sits just under Remote Login; on some versions it's behind the ⓘ next to it."),
    ));
    steps.append(&step(
        4,
        "Come back here and click <b>Get started</b>.",
        None,
    ));
    let skip_cb = gtk::CheckButton::with_label("Don't show this again");
    skip_cb.set_halign(gtk::Align::Start);
    skip_cb.set_margin_top(6);
    p0.append(&skip_cb);

    // 1: the Mac
    let p1 = page("Connect to your Mac");
    p1.append(&label("On the Mac, open System Settings → General → Sharing and turn on Remote Login. Then pick the Mac here (or type its name or address) and sign in with its user name and password. The password is used once, for the setup, and then forgotten.", "dim"));
    let host = gtk::Entry::builder()
        .placeholder_text("my-mac.local")
        .hexpand(true)
        .css_classes(["wiz-field"])
        .build();
    // the button at the right end is a real, focusable button: the list's arrow, or "clear" once a Mac is chosen
    let drop_btn = gtk::Button::from_icon_name("pan-down-symbolic");
    drop_btn.add_css_class("wiz-drop-btn");
    drop_btn.set_halign(gtk::Align::End);
    drop_btn.set_valign(gtk::Align::Center);
    drop_btn.set_margin_end(6);
    drop_btn.set_tooltip_text(Some("Macs found on the network"));
    // the verdict sits inside the left end of the field (the arrow / clear button is at the right end): a computer
    // before anything is entered, a spinner while it is checked, a green circle with a check, a red one with a cross
    let host_status = gtk::Stack::new();
    host_status.set_halign(gtk::Align::Start);
    host_status.set_valign(gtk::Align::Center);
    host_status.set_margin_start(13);
    host_status.set_can_target(false);
    let idle = gtk::Image::from_icon_name("computer-symbolic");
    idle.add_css_class("dim");
    host_status.add_named(&idle, Some("none"));
    let spin = gtk::Spinner::new();
    spin.start();
    host_status.add_named(&spin, Some("checking"));
    let badge = |icon: &str, class: &str| {
        let i = gtk::Image::from_icon_name(icon);
        i.set_pixel_size(10);
        i.add_css_class("wiz-badge");
        i.add_css_class(class);
        i
    };
    host_status.add_named(&badge("object-select-symbolic", "good"), Some("ok"));
    host_status.add_named(&badge("window-close-symbolic", "bad"), Some("bad"));
    host.add_css_class("with-status");
    let field = gtk::Overlay::new();
    field.set_child(Some(&host));
    field.add_overlay(&host_status);
    field.add_overlay(&drop_btn);
    field.set_halign(gtk::Align::Center);
    field.set_size_request(340, -1);
    p1.append(&field);
    let host_msg = label("", "wiz-err");
    host_msg.set_visible(false);
    host_msg.set_halign(gtk::Align::Center);
    host_msg.set_justify(gtk::Justification::Center);
    p1.append(&host_msg);
    // the account: unavailable until a Mac is chosen and found; changing the Mac clears the password
    let user = gtk::Entry::builder()
        .text(&ctx.default_user)
        .placeholder_text("Mac user name")
        .css_classes(["wiz-field"])
        .sensitive(false)
        .build();
    let pass = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .placeholder_text("Mac password")
        .css_classes(["wiz-field"])
        .sensitive(false)
        .build();
    for f in [
        user.upcast_ref::<gtk::Widget>(),
        pass.upcast_ref::<gtk::Widget>(),
    ] {
        f.set_halign(gtk::Align::Center);
        f.set_size_request(340, -1);
    }
    p1.append(&user);
    p1.append(&pass);
    let acct_msg = label("", "wiz-err");
    acct_msg.set_visible(false);
    acct_msg.set_halign(gtk::Align::Center);
    acct_msg.set_justify(gtk::Justification::Center);
    p1.append(&acct_msg);
    let connecting = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    connecting.set_halign(gtk::Align::Center);
    let cspin = gtk::Spinner::new();
    cspin.start();
    connecting.append(&cspin);
    let connecting_text = label("", "dim");
    connecting.append(&connecting_text);
    connecting.set_visible(false);
    p1.append(&connecting);
    let pick = gtk::Box::new(gtk::Orientation::Vertical, 2); // plain buttons: a ListBox's rows add their own square background and focus box
    let scan = gtk::Spinner::new();
    let scan_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let scan_text = label("Looking for Macs…", "dim");
    scan_row.append(&scan);
    scan_row.append(&scan_text);
    let pick_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    pick_box.append(&pick);
    pick_box.append(&scan_row);
    // a drop-down: as wide as the field, directly under it, no arrow
    let pick_pop = gtk::Popover::builder()
        .child(&pick_box)
        .autohide(true)
        .has_arrow(false)
        .css_classes(["wiz-drop"])
        .build();
    pick_pop.set_parent(&host);
    pick_pop.set_position(gtk::PositionType::Bottom);
    pick_pop.set_offset(0, 2);

    // 3: the work
    let p4 = page("Setting up");
    let work_info = label("", "dim");
    p4.append(&work_info);
    let mut steps = Vec::new();
    for t in STEPS {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let icon = gtk::Image::new();
        icon.set_pixel_size(16);
        let sp = gtk::Spinner::new();
        let l = label(t, "");
        row.append(&icon);
        row.append(&sp);
        row.append(&l);
        p4.append(&row);
        steps.push((icon, sp, l));
    }
    let work_msg = label("", "");
    work_msg.set_selectable(true);
    p4.append(&work_msg);
    let work_btn = gtk::Button::with_label("Try again");
    work_btn.add_css_class("wiz-retry");
    work_btn.set_halign(gtk::Align::Start);
    work_btn.set_visible(false);
    p4.append(&work_btn);

    // 5: the test message
    let p5 = page("Send a test message");
    let test_title = outers
        .borrow()
        .last()
        .and_then(|o| o.first_child())
        .and_downcast::<gtk::Label>()
        .expect("page title");
    p5.append(&label("This sends a short message through Messages on your Mac, so you know sending works. Have your Mac in view: the first time, macOS asks whether to allow control of Messages. Click Allow. You can send it to yourself: use your own phone number or Apple ID email.", "dim"));
    let to = gtk::Entry::builder()
        .placeholder_text("your phone number or email")
        .css_classes(["wiz-field", "with-action"])
        .build();
    // the send button sits inside the right end of the field; it becomes a spinner while sending, then a check
    let test_btn = gtk::Button::from_icon_name("bf-send-symbolic");
    test_btn.add_css_class("wiz-drop-btn");
    test_btn.set_tooltip_text(Some("Send test message"));
    let test_stack = gtk::Stack::new();
    test_stack.set_halign(gtk::Align::End);
    test_stack.set_valign(gtk::Align::Center);
    test_stack.set_margin_end(6);
    test_stack.add_named(&test_btn, Some("send"));
    let sending = gtk::Spinner::new();
    sending.start();
    sending.set_margin_end(6);
    sending.set_halign(gtk::Align::Center);
    sending.set_valign(gtk::Align::Center);
    test_stack.add_named(&sending, Some("spin"));
    let sent = gtk::Image::from_icon_name("object-select-symbolic");
    sent.set_pixel_size(10);
    sent.add_css_class("wiz-badge");
    sent.add_css_class("good");
    sent.set_margin_end(6);
    sent.set_halign(gtk::Align::Center);
    sent.set_valign(gtk::Align::Center); // else it stretches to the button's width and is no longer a circle
    test_stack.add_named(&sent, Some("done"));
    let to_field = gtk::Overlay::new();
    to_field.set_child(Some(&to));
    to_field.add_overlay(&test_stack);
    to_field.set_halign(gtk::Align::Center);
    to_field.set_size_request(340, -1);
    p5.append(&to_field);
    let test_msg = label("", "dim");
    test_msg.set_halign(gtk::Align::Center);
    test_msg.set_justify(gtk::Justification::Center);
    p5.append(&test_msg);
    let skip_note = label(
        "You can skip this. Reading your messages works without it. But the first time you send a message from BubbleFairy, macOS will ask you to allow control of Messages on the Mac, so have it handy then.",
        "dim",
    );
    skip_note.set_margin_top(14);
    p5.append(&skip_note);
    let stop_btn = gtk::Button::with_label("Stop waiting");
    stop_btn.add_css_class("flat");
    stop_btn.add_css_class("wiz-link");
    stop_btn.set_halign(gtk::Align::Center);
    stop_btn.set_visible(false);
    p5.append(&stop_btn);

    let pages = gtk::Stack::new();
    let outers = outers.into_inner();
    pages.add_named(&outers[0], Some("intro"));
    for (name, p) in PAGES.iter().zip(&outers[1..]) {
        pages.add_named(p, Some(name));
    }
    let back = gtk::Button::with_label("Back");
    back.add_css_class("wiz-back");
    let next = gtk::Button::with_label("Next");
    next.add_css_class("wiz-next");
    // one dot per page, the current one lit
    let dot_row = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    dot_row.set_halign(gtk::Align::Center);
    dot_row.set_valign(gtk::Align::Center);
    // the first dot is the welcome page, which stays as a dot even when it's switched off
    let dots: Vec<gtk::Box> = (0..=PAGES.len())
        .map(|_| {
            let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            d.add_css_class("wiz-dot");
            dot_row.append(&d);
            d
        })
        .collect();
    let bar = gtk::CenterBox::new();
    bar.add_css_class("wiz-footer");
    bar.set_start_widget(Some(&back));
    bar.set_center_widget(Some(&dot_row));
    bar.set_end_widget(Some(&next));
    let root = gtk::Box::new(gtk::Orientation::Vertical, 16);
    root.add_css_class("wiz-panel");
    pages.set_vexpand(true);
    root.append(&pages);
    root.append(&bar);
    win.set_child(Some(&root));

    let w = Rc::new(W {
        win: win.clone(),
        ctx,
        pages,
        back,
        next,
        host,
        host_status,
        drop_btn,
        host_msg,
        pick,
        pick_pop,
        scan,
        scan_row,
        scan_text,
        found: RefCell::new(Vec::new()),
        locked: Cell::new(false),
        host_ok: Cell::new(false),
        editing: Cell::new(false),
        user,
        pass,
        acct_msg,
        steps,
        work_msg,
        work_info,
        work_btn,
        to,
        test_msg,
        test_title,
        skip_note,
        test_btn,
        test_stack,
        stop_btn,
        test_gen: Cell::new(0),
        connecting,
        connecting_text,
        online: RefCell::new(None),
        imsg: RefCell::new(String::new()),
        page: Cell::new(0),
        btn_key: Cell::new(false),
        dots,
        intro: Cell::new(false),
        skip_cb,
    });
    for i in 0..STEPS.len() {
        set_step(&w, i, Step::Wait);
    }
    wire_host(&w);
    wire_nav(&w);
    show_page(&w, 0);
    let dev_skip = ["BF_SETUP_PAGE", "BF_SETUP_AUTO"]
        .iter()
        .any(|k| std::env::var_os(k).is_some());
    let intro_shown =
        (!w.ctx.skip_intro && !dev_skip) || std::env::var_os("BF_SETUP_INTRO").is_some();
    w.dots[0].set_visible(intro_shown); // no dot for a welcome page that's switched off
    if intro_shown {
        show_intro(&w);
    }
    win.present();
    // dev aids: BF_SETUP_PAGE=<n> starts on that page; BF_SETUP_SHOT=<file.png> saves the wizard after BF_SETUP_MS (default 3 s)
    if let Some(n) = std::env::var("BF_SETUP_PAGE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        show_page(&w, n.min(PAGES.len() - 1));
    }
    // dev aid BF_SETUP_POP=<png>: open the Mac list, hover its first entry, and save the list's own picture
    if let (Some(out), Some(ms)) = (
        std::env::var_os("BF_SETUP_POP"),
        std::env::var("BF_SETUP_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok()),
    ) {
        let w2 = w.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms.saturating_sub(1500)), move || {
            open_list(&w2, false);
            if let Some(b) = w2.pick.first_child() {
                b.set_state_flags(gtk::StateFlags::PRELIGHT, false);
            }
            let w3 = w2.clone();
            glib::timeout_add_local_once(Duration::from_millis(600), move || {
                let p = gtk::WidgetPaintable::new(Some(&w3.pick_pop));
                let snap = gtk::Snapshot::new();
                p.snapshot(
                    &snap,
                    w3.pick_pop.width() as f64,
                    w3.pick_pop.height() as f64,
                );
                if let (Some(node), Some(r)) = (snap.to_node(), w3.win.renderer()) {
                    let _ = r.render_texture(&node, None).save_to_png(&out);
                }
            });
        });
    }
    if let Some(out) = std::env::var_os("BF_SETUP_SHOT") {
        let ms = std::env::var("BF_SETUP_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000);
        let win = w.win.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            let p = gtk::WidgetPaintable::new(Some(&win));
            let snap = gtk::Snapshot::new();
            p.snapshot(&snap, win.width() as f64, win.height() as f64);
            if let (Some(node), Some(r)) = (snap.to_node(), win.renderer()) {
                let _ = r.render_texture(&node, None).save_to_png(&out);
            }
            if let Some(app) = win.application() {
                app.quit();
            }
        });
    }
    // dev aid: BF_SETUP_HOST=<host> fills the Mac field, as if picked
    if let Ok(h) = std::env::var("BF_SETUP_HOST") {
        w.host.set_text(&h);
        check_host(&w, false);
    }
    // dev aid: BF_SETUP_AUTO=<mac user> clicks Next through the first three pages (with BF_SETUP_KEYLOGIN, no password)
    if let Ok(user) = std::env::var("BF_SETUP_AUTO") {
        w.user.set_text(&user);
        w.pass.set_text("x");
        let w2 = w.clone();
        glib::timeout_add_local(Duration::from_millis(1500), move || {
            if w2.page.get() == 0 && w2.next.is_sensitive() {
                w2.next.emit_clicked();
            }
            if w2.page.get() >= 1 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
}

/// The welcome page, before the first real page: no dots lit, no Back, a Get started button.
fn show_intro(w: &Rc<W>) {
    w.intro.set(true);
    for (n, d) in w.dots.iter().enumerate() {
        if n == 0 {
            d.add_css_class("on")
        } else {
            d.remove_css_class("on")
        }
    }
    w.pages.set_visible_child_name("intro");
    w.back.set_visible(false);
    w.next.set_visible(true);
    w.next.set_label("Get started");
    w.next.set_sensitive(true);
}

fn show_page(w: &Rc<W>, i: usize) {
    w.page.set(i);
    for (n, d) in w.dots.iter().enumerate() {
        if n == i + 1 {
            d.add_css_class("on")
        } else {
            d.remove_css_class("on")
        }
    }
    w.pages.set_visible_child_name(PAGES[i]);
    if i == 2 {
        w.to.grab_focus(); // the number or address is what to type next
    }
    w.back.set_visible(false);
    w.next.set_visible(true);
    w.next.set_label(match i {
        0 => "Connect",
        2 => "Skip",
        _ => "Next",
    });
    w.next.set_sensitive(match i {
        0 => ready(w),
        1 => false,
        _ => true,
    });
}

/// Is the first page complete: a Mac found, and a user name and password entered?
fn ready(w: &Rc<W>) -> bool {
    w.host_ok.get() && !w.user.text().trim().is_empty() && !w.pass.text().is_empty()
}

// ----- page 1: the Mac picker -----

fn set_host_status(w: &Rc<W>, state: &str, msg: &str) {
    w.host_status.set_visible_child_name(state);
    w.host_msg.set_text(msg);
    w.host_msg.set_visible(!msg.is_empty());
    w.host_ok.set(state == "ok");
    // the account is for a Mac that was found; another Mac means the password is typed again
    w.user.set_sensitive(state == "ok");
    w.pass.set_sensitive(state == "ok");
    if state != "ok" {
        w.pass.set_text("");
    }
    if w.page.get() == 0 && !w.intro.get() {
        w.next.set_sensitive(ready(w));
    }
}

/// The name of a Mac in the list is final: it can't be edited until it is cleared.
/// `move_focus`: go on to the password (a pick or Enter). Not when the field was left with Tab: focus is already moving
/// on by itself, and moving it here too would leave two fields looking focused.
fn lock_host(w: &Rc<W>, host: &str, move_focus: bool) {
    w.editing.set(true);
    w.host.set_text(host);
    w.editing.set(false);
    w.host.set_editable(false);
    w.host.add_css_class("locked");
    w.host.set_sensitive(false); // like a disabled input: it can't take focus, so Tab lands on the clear button
    w.locked.set(true);
    w.drop_btn.set_icon_name("edit-clear-symbolic");
    w.drop_btn.set_tooltip_text(Some("Choose a different Mac"));
    set_host_status(w, "ok", "");
    // the next thing is the password; the Mac field is locked, so no cursor in it
    if move_focus {
        w.win.set_focus_visible(true);
        w.pass.grab_focus();
    }
}

fn unlock_host(w: &Rc<W>) {
    w.locked.set(false);
    w.host.remove_css_class("locked");
    w.host.set_sensitive(true);
    w.host.set_editable(true);
    w.editing.set(true);
    w.host.set_text("");
    w.editing.set(false);
    w.drop_btn.set_icon_name("pan-down-symbolic");
    w.drop_btn
        .set_tooltip_text(Some("Macs found on the network"));
    set_host_status(w, "none", "");
    w.host.grab_focus();
}

/// A typed name: look for it on the network (unless it is one the scan already found).
fn check_host(w: &Rc<W>, move_focus: bool) {
    let host = w.host.text().trim().to_string();
    if host.is_empty() || w.locked.get() {
        return;
    }
    if w.found
        .borrow()
        .iter()
        .any(|f| f.host.eq_ignore_ascii_case(&host))
    {
        lock_host(w, &host, move_focus);
        return;
    }
    set_host_status(w, "checking", "");
    let w2 = w.clone();
    glib::spawn_future_local(async move {
        let h = host.clone();
        let r = rt(async move { probe(&h).await }).await;
        if w2.host.text().trim() != host || w2.locked.get() {
            return; // changed since
        }
        match r {
            Ok(()) => set_host_status(&w2, "ok", ""),
            Err(e) => set_host_status(&w2, "bad", &e),
        }
    });
}

fn add_found(w: &Rc<W>, f: Found) {
    if std::env::var_os("BF_SETUP_LOG").is_some() {
        eprintln!("setup: found {} ({})", f.name, f.host); // dev aid
    }
    if w.found.borrow().iter().any(|x| x.host == f.host) {
        return;
    }
    let row = gtk::Button::new();
    row.add_css_class("flat");
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.set_hexpand(true);
    row.set_hexpand(true);
    let n = gtk::Label::new(Some(&f.name));
    n.set_xalign(0.0);
    let h = gtk::Label::new(Some(&f.host));
    h.set_xalign(0.0);
    h.add_css_class("dim");
    b.append(&n);
    b.append(&h);
    row.set_child(Some(&b));
    let (w2, host) = (w.clone(), f.host.clone());
    row.connect_clicked(move |_| {
        w2.pick_pop.popdown();
        lock_host(&w2, &host, true);
    });
    w.pick.append(&row);
    w.found.borrow_mut().push(f);
    w.scan_row.set_visible(false); // "Looking for Macs…" is only for an empty list; more may still be added below
}

/// Drop the list of found Macs down from the field.
fn open_list(w: &Rc<W>, by_key: bool) {
    // opened with the pointer, nothing in the list takes focus (so no entry looks selected); with the keyboard, they do
    let mut child = w.pick.first_child();
    while let Some(c) = child {
        c.set_focusable(by_key);
        child = c.next_sibling();
    }
    if let Some(b) = w.pick_pop.child() {
        b.set_size_request(w.host.width() - 16, -1); // the field's width less the popover's own padding and edge
    }
    w.pick_pop.popup();
}

fn wire_host(w: &Rc<W>) {
    // the scan
    let (tx, mut rx) = mpsc::unbounded_channel();
    w.scan.start();
    discover(tx);
    let w2 = w.clone();
    glib::spawn_future_local(async move {
        while let Some(f) = rx.recv().await {
            match f {
                Some(f) => add_found(&w2, f),
                None => break,
            }
        }
        w2.scan.stop();
        w2.scan.set_visible(false);
        if w2.found.borrow().is_empty() {
            w2.scan_text
                .set_text("No Macs found. Type its name in the field.");
        }
    });
    // the list opens from the arrow in the field
    let w2 = w.clone();
    w.drop_btn.connect_clicked(move |_| {
        if w2.locked.get() {
            unlock_host(&w2);
        } else {
            open_list(&w2, w2.btn_key.get());
        }
    });
    let press = gtk::GestureClick::new();
    press.set_propagation_phase(gtk::PropagationPhase::Capture);
    let w2 = w.clone();
    press.connect_pressed(move |_, _, _, _| w2.btn_key.set(false));
    w.drop_btn.add_controller(press);
    let btn_keys = gtk::EventControllerKey::new();
    let w2 = w.clone();
    btn_keys.connect_key_pressed(move |_, key, _, _| {
        w2.btn_key.set(true);
        if w2.locked.get()
            && matches!(
                key,
                gdk::Key::BackSpace | gdk::Key::Delete | gdk::Key::Escape
            )
        {
            unlock_host(&w2);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    w.drop_btn.add_controller(btn_keys);
    // typing completes the name of a Mac that was found, and clears the old verdict
    let w2 = w.clone();
    w.host.connect_changed(move |e| {
        if w2.editing.get() || w2.locked.get() {
            return;
        }
        set_host_status(&w2, "none", "");
        let typed = e.text().to_string();
        if typed.chars().count() < 2 {
            return;
        }
        let lower = typed.to_lowercase();
        let hit = w2
            .found
            .borrow()
            .iter()
            .find(|f| {
                f.host.to_lowercase().starts_with(&lower)
                    || f.name.to_lowercase().starts_with(&lower)
            })
            .cloned();
        if let Some(f) = hit.filter(|f| f.host.to_lowercase().starts_with(&lower)) {
            // fill in the rest, selected, so the next key replaces it
            w2.editing.set(true);
            e.set_text(&f.host);
            e.select_region(typed.chars().count() as i32, -1);
            w2.editing.set(false);
        }
    });
    // keyboard: Down opens the list (Enter on an entry picks it); on a locked field Backspace, Delete or Escape clears it
    let keys = gtk::EventControllerKey::new();
    let w2 = w.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if w2.locked.get() {
            if matches!(
                key,
                gdk::Key::BackSpace | gdk::Key::Delete | gdk::Key::Escape
            ) {
                unlock_host(&w2);
                return glib::Propagation::Stop;
            }
        } else if key == gdk::Key::Down {
            open_list(&w2, true);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    w.host.add_controller(keys);
    // leaving the field checks what was typed; Enter does too
    let focus = gtk::EventControllerFocus::new();
    let w2 = w.clone();
    focus.connect_leave(move |_| {
        if !w2.pick_pop.is_visible() {
            check_host(&w2, false);
        }
    });
    w.host.add_controller(focus);
    let w2 = w.clone();
    w.host.connect_activate(move |_| check_host(&w2, true));
}

// ----- navigation -----

fn wire_nav(w: &Rc<W>) {
    let w2 = w.clone();
    w.back.connect_clicked(move |_| {
        show_page(&w2, w2.page.get().saturating_sub(1));
    });
    let w2 = w.clone();
    w.next.connect_clicked(move |_| match w2.page.get() {
        _ if w2.intro.get() => {
            if w2.skip_cb.is_active() {
                (w2.ctx.set_skip_intro)(true);
            }
            w2.intro.set(false);
            show_page(&w2, 0);
        }
        0 => connect(&w2),
        _ => finish(&w2),
    });
    let w2 = w.clone();
    w.pass.connect_activate(move |_| {
        if w2.page.get() == 0 && ready(&w2) {
            connect(&w2);
        }
    });
    for e in [
        w.user.clone().upcast::<gtk::Editable>(),
        w.pass.clone().upcast::<gtk::Editable>(),
    ] {
        let w2 = w.clone();
        e.connect_changed(move |_| {
            if w2.page.get() == 0 {
                w2.next.set_sensitive(ready(&w2));
            }
        });
    }
    let w2 = w.clone();
    w.work_btn.connect_clicked(move |_| provision(&w2));
    let w2 = w.clone();
    w.test_btn.connect_clicked(move |_| test_send(&w2));
    let w2 = w.clone();
    w.to.connect_activate(move |_| test_send(&w2));
    let w2 = w.clone();
    w.stop_btn.connect_clicked(move |_| {
        w2.test_gen.set(w2.test_gen.get() + 1); // the answer, if it still comes, is ignored
        end_sending(&w2);
        w2.test_msg.set_text(
            "Stopped waiting. If you click Allow on the Mac later, the message will still be sent.",
        );
    });
}

/// The sign-in message under the password (hidden when empty).
fn set_acct(w: &Rc<W>, msg: &str) {
    w.acct_msg.set_text(msg);
    w.acct_msg.set_visible(!msg.is_empty());
}

/// Connect and sign in (a wrong password stays here, with the message), then set everything up.
/// The Mac's host key is pinned as first seen, like `ssh` does on a first connection: the person picked this Mac by
/// name on their own network, and "fingerprints" mean nothing to most people.
fn connect(w: &Rc<W>) {
    if w.user.text().trim().is_empty() || w.pass.text().is_empty() {
        set_acct(w, "Enter the Mac's user name and password.");
        return;
    }
    set_acct(w, "");
    w.next.set_sensitive(false);
    let name = w
        .found
        .borrow()
        .iter()
        .find(|f| f.host.eq_ignore_ascii_case(w.host.text().trim()))
        .map(|f| f.name.clone())
        .unwrap_or_else(|| w.host.text().trim().to_string());
    w.connecting_text
        .set_text(&format!("Connecting to {name}…"));
    w.connecting.set_visible(true);
    for f in [
        w.user.upcast_ref::<gtk::Widget>(),
        w.pass.upcast_ref::<gtk::Widget>(),
        w.drop_btn.upcast_ref::<gtk::Widget>(),
    ] {
        f.set_sensitive(false);
    }
    let (host, user, pass) = (
        w.host.text().trim().to_string(),
        w.user.text().trim().to_string(),
        w.pass.text().to_string(),
    );
    let dev_key = std::env::var_os("BF_SETUP_KEYLOGIN")
        .map(|_| glib::user_data_dir().join("bubblefairy/id_ed25519"));
    let w2 = w.clone();
    glib::spawn_future_local(async move {
        let r = rt(async move {
            let mut link = open(&host).await?;
            match dev_key {
                Some(k) => link.login_key(&user, &k).await?,
                None => link.login(&user, &pass).await?,
            }
            Ok::<Link, String>(link)
        })
        .await;
        match r {
            Ok(link) => {
                *w2.online.borrow_mut() = Some(Arc::new(link));
                show_page(&w2, 1);
                for i in 0..STEPS.len() {
                    set_step(&w2, i, Step::Wait);
                }
                set_step(&w2, 0, Step::Ok);
                w2.work_msg.set_text("");
                w2.work_btn.set_visible(false);
                provision_from(&w2, 1).await;
            }
            Err(e) => {
                w2.connecting.set_visible(false);
                for f in [
                    w2.user.upcast_ref::<gtk::Widget>(),
                    w2.pass.upcast_ref::<gtk::Widget>(),
                    w2.drop_btn.upcast_ref::<gtk::Widget>(),
                ] {
                    f.set_sensitive(true);
                }
                w2.next.set_sensitive(ready(&w2));
                set_acct(&w2, &e);
            }
        }
    });
}

fn set_step(w: &Rc<W>, i: usize, s: Step) {
    let (icon, spin, label) = &w.steps[i];
    icon.remove_css_class("step-ok"); // a step that failed and then worked must not keep its red
    icon.remove_css_class("step-bad");
    spin.set_spinning(s == Step::Run);
    spin.set_visible(s == Step::Run);
    icon.set_visible(s != Step::Run);
    match s {
        Step::Wait => {
            icon.set_icon_name(Some("media-record-symbolic"));
            label.add_css_class("dim");
        }
        Step::Run => label.remove_css_class("dim"),
        Step::Ok => {
            icon.set_icon_name(Some("object-select-symbolic"));
            icon.add_css_class("step-ok");
            label.remove_css_class("dim");
        }
        Step::Bad => {
            icon.set_icon_name(Some("dialog-error-symbolic"));
            icon.add_css_class("step-bad");
        }
    }
}

fn fail(w: &Rc<W>, msg: &str, button: Option<&str>) {
    w.work_msg.set_text(msg);
    w.work_btn.set_label(button.unwrap_or("Try again"));
    w.work_btn.set_visible(true);
    w.work_btn.set_sensitive(true);
}

/// The button under a failed step: sign in again from the password page, or run the steps again.
fn provision(w: &Rc<W>) {
    if w.online.borrow().is_none() {
        show_page(w, 0);
        return;
    }
    w.work_btn.set_visible(false);
    w.work_msg.set_text("");
    let w2 = w.clone();
    glib::spawn_future_local(async move { provision_from(&w2, 1).await });
}

/// Steps 1.. : check the Mac, add the key, check the key, install imsg, check access.
async fn provision_from(w: &Rc<W>, from: usize) {
    let Some(link) = w.online.borrow().clone() else {
        return;
    };
    let (host, user) = (
        w.host.text().trim().to_string(),
        w.user.text().trim().to_string(),
    );
    let folder = if w.ctx.test {
        "BubbleFairy-test"
    } else {
        "BubbleFairy"
    };
    for i in from..STEPS.len() {
        set_step(w, i, Step::Run);
    }
    let mut home = String::new();
    for i in from..STEPS.len() {
        set_step(w, i, Step::Run);
        let r: Result<(), String> = match i {
            1 => {
                let l = link.clone();
                rt(async move { l.check_mac().await })
                    .await
                    .map(|(h, about)| {
                        home = h;
                        w.work_info.set_text(&about); // which Mac this is, in words the person knows
                    })
            }
            2 => {
                let (l, key) = (link.clone(), w.ctx.public_key.clone());
                rt(async move { l.install_key(&key).await }).await
            }
            3 => {
                let (h, u, p) = (host.clone(), user.clone(), w.ctx.key_path.clone());
                let r = rt(async move { verify_key(&h, &u, p).await.map(|_| ()) }).await;
                if w.ctx.test {
                    // dev profile: the throwaway key has done its job, take it off the Mac now
                    let (l, k) = (link.clone(), w.ctx.public_key.clone());
                    rt(async move { l.remove_key(&k).await }).await;
                }
                r
            }
            4 => match imsg_zip().await {
                Ok(bytes) => {
                    let (l, hm, f) = (link.clone(), home.clone(), folder.to_string());
                    match rt(async move { l.install_imsg(&bytes, &hm, &f).await }).await {
                        Ok(path) => {
                            *w.imsg.borrow_mut() = path;
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            },
            _ => {
                let l = link.clone();
                match rt(async move { l.check_access().await }).await {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        set_step(w, i, Step::Bad);
                        fail(
                            w,
                            "macOS isn't letting BubbleFairy read Messages. On the Mac, open System Settings → General → Sharing and turn on \"Allow full disk access for remote users\" (just under Remote Login, or behind the ⓘ next to it on some versions), then check again.",
                            Some("Check again"),
                        );
                        return;
                    }
                    Err(e) => Err(e),
                }
            }
        };
        match r {
            Ok(()) => set_step(w, i, Step::Ok),
            Err(e) => {
                set_step(w, i, Step::Bad);
                fail(w, &e, None);
                return;
            }
        }
        // the home folder is only learned in step 1: a retry from a later step needs it again
        if i == 1 && home.is_empty() {
            return;
        }
    }
    show_page(w, 2);
}

/// Plain words for what went wrong with the test message.
fn explain(e: &str) -> String {
    let low = e.to_lowercase();
    if low.contains("took too long") {
        "The Mac didn't answer. If a permission question is waiting on its screen, click Allow, then try again.".into()
    } else if low.contains("1743") || low.contains("not authorized") || low.contains("not allowed")
    {
        "The Mac didn't allow it. On the Mac, open System Settings → Privacy & Security → Automation, allow the app that runs imsg to control Messages, then try again.".into()
    } else {
        format!("It didn't go: {e}")
    }
}

/// Back to a field that can be used (after a send ends, one way or another).
fn end_sending(w: &Rc<W>) {
    w.to.set_sensitive(true);
    w.next.set_sensitive(true);
    w.stop_btn.set_visible(false);
    w.test_stack.set_visible_child_name("send");
}

fn test_send(w: &Rc<W>) {
    if w.test_stack.visible_child_name().as_deref() != Some("send") {
        return;
    }
    let to = w.to.text().trim().to_string();
    let (Some(link), false) = (w.online.borrow().clone(), to.is_empty()) else {
        w.test_msg
            .set_text("Enter a phone number or email to send it to.");
        return;
    };
    let gen_ = w.test_gen.get() + 1;
    w.test_gen.set(gen_);
    // everything on the page waits while it is sent
    w.to.set_sensitive(false);
    w.next.set_sensitive(false);
    w.test_stack.set_visible_child_name("spin");
    w.test_msg.set_text("Sending…");
    // after a while: say what to look for, and offer a way out
    let w2 = w.clone();
    glib::timeout_add_local_once(Duration::from_secs(12), move || {
        if w2.test_gen.get() == gen_
            && w2.test_stack.visible_child_name().as_deref() == Some("spin")
        {
            w2.test_msg.set_text("Waiting for the Mac. Look at its screen: macOS may be asking you to allow control of Messages. Click Allow.");
            w2.stop_btn.set_visible(true);
        }
    });
    let (w2, imsg) = (w.clone(), w.imsg.borrow().clone());
    glib::spawn_future_local(async move {
        let r = rt(async move { link.test_send(&imsg, &to).await }).await;
        if w2.test_gen.get() != gen_ {
            return; // stopped waiting
        }
        end_sending(&w2);
        match r {
            Ok(()) => {
                w2.to.set_sensitive(false);
                w2.test_stack.set_visible_child_name("done");
                w2.test_title.set_text("You're all set");
                w2.skip_note.set_visible(false);
                w2.test_msg.set_text(&format!("Messages on your Mac sent your test message to {}. Sending from BubbleFairy works.", w2.to.text().trim()));
                w2.next.set_label("Finish");
                crate::sound::play(crate::sound::DEFAULT);
            }
            Err(e) => w2.test_msg.set_text(&explain(&e)),
        }
    });
}

fn finish(w: &Rc<W>) {
    let (host, user, imsg) = (
        w.host.text().trim().to_string(),
        w.user.text().trim().to_string(),
        w.imsg.borrow().clone(),
    );
    let online = w.online.borrow().clone();
    let host_key = online
        .as_ref()
        .map(|l| l.host_key.clone())
        .unwrap_or_default();
    if w.ctx.test {
        // dev profile: take the throwaway key off the Mac again and keep nothing
        if let Some(l) = online {
            let k = w.ctx.public_key.clone();
            let win = w.win.clone();
            glib::spawn_future_local(async move {
                rt(async move { l.remove_key(&k).await }).await;
                win.close();
            });
            return;
        }
    } else {
        (w.ctx.done)(host, user, imsg, host_key);
    }
    w.win.close();
}

/// The Linux user's name, offered as the default Mac user.
pub fn default_user() -> String {
    glib::user_name().to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hash_helper_matches_known_value() {
        assert_eq!(
            super::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn removing_a_key_keeps_the_others_and_never_empties_the_file_on_error() {
        let home = std::env::temp_dir().join(format!("bf-ak-{}", std::process::id()));
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let run = |cmd: String| {
            std::process::Command::new("sh")
                .env("HOME", &home)
                .arg("-c")
                .arg(cmd)
                .status()
                .unwrap()
        };
        let f = ssh.join("authorized_keys");
        std::fs::write(&f, "ssh-ed25519 AAAAkeep one\nssh-ed25519 AAAAdropme two\n").unwrap();
        run(super::remove_key_cmd("AAAAdropme"));
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "ssh-ed25519 AAAAkeep one\n"
        );
        run(super::remove_key_cmd("AAAAkeep")); // the last key goes: an empty file is right
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "");
        std::fs::write(&f, "ssh-ed25519 AAAAkeep one\n").unwrap();
        std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o200)).unwrap(); // write-only: grep can't read it (exit 2) but a blind overwrite would work
        run(super::remove_key_cmd("AAAAdropme"));
        std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "ssh-ed25519 AAAAkeep one\n"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
