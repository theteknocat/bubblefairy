//! First-run setup: find the Mac, sign in once with its password, install BubbleFairy's key and its own copy of imsg,
//! check that Messages can be read, and (optionally) send a test message. The backend half (this file's top) runs on
//! tokio; the wizard window (below) runs on the GTK thread and calls it through `rt`.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib};
use russh::client::{self, KeyboardInteractiveAuthResponse};
use russh::keys::PrivateKeyWithHashAlg;
use russh::ChannelMsg;
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
                        let name = r.fullname.strip_suffix("._ssh._tcp.local.").unwrap_or(&r.fullname).to_string();
                        let _ = tx.send(Some(Found { name, host: r.host.trim_end_matches('.').to_string() }));
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
        _ => Err(format!("{host} is on the network but doesn't answer on the SSH port. Is Remote Login turned on?")),
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
    let config = client::Config { keepalive_interval: Some(Duration::from_secs(15)), nodelay: true, ..Default::default() };
    let handler = Handler { known: None, seen: seen.clone() }; // the person decides on the fingerprint
    let session = match tokio::time::timeout(Duration::from_secs(10), client::connect(Arc::new(config), addr, handler)).await {
        Err(_) => return Err(format!("{host} didn't answer. Is Remote Login on, and is the Mac awake?")),
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
        let r = self.session.authenticate_password(user, password).await.map_err(|e| format!("sign-in failed: {e}"))?;
        if r.success() {
            return Ok(());
        }
        // some Macs only offer "keyboard-interactive" (the same password, asked as a prompt)
        let mut r = self.session.authenticate_keyboard_interactive_start(user, None).await.map_err(|e| format!("sign-in failed: {e}"))?;
        for _ in 0..4 {
            match r {
                KeyboardInteractiveAuthResponse::Success => return Ok(()),
                KeyboardInteractiveAuthResponse::Failure { .. } => return Err(bad()),
                KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                    let answers = prompts.iter().map(|_| password.to_string()).collect();
                    r = self.session.authenticate_keyboard_interactive_respond(answers).await.map_err(|e| format!("sign-in failed: {e}"))?;
                }
            }
        }
        Err(bad())
    }

    /// Dev aid (`BF_SETUP_KEYLOGIN`): sign in with an existing key instead of a password, to try the later steps.
    async fn login_key(&mut self, user: &str, key_path: &std::path::Path) -> Result<(), String> {
        let key = russh::keys::load_secret_key(key_path, None).map_err(|e| e.to_string())?;
        let r = self.session.authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), None)).await.map_err(|e| e.to_string())?;
        if r.success() { Ok(()) } else { Err("the key was refused".into()) }
    }

    /// Run a command; `input` goes to its stdin. Returns the exit status and everything it printed.
    async fn run(&self, cmd: &str, input: Option<&[u8]>, wait: Duration) -> Result<(u32, String), String> {
        let work = async {
            let mut ch = self.session.channel_open_session().await.map_err(|e| e.to_string())?;
            ch.exec(true, cmd.to_string()).await.map_err(|e| e.to_string())?;
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
        tokio::time::timeout(wait, work).await.map_err(|_| "the Mac took too long to answer".to_string())?
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
        let major: u32 = version.split('.').next().and_then(|m| m.parse().ok()).unwrap_or(0);
        if major == 0 || home.is_empty() {
            return Err("this doesn't look like a Mac".into());
        }
        if major < 14 {
            return Err(format!("this Mac runs macOS {version}; BubbleFairy needs macOS 14 or newer"));
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
        Ok(format!("{home}/Library/Application Support/{folder}/imsg/imsg"))
    }

    /// Can the Messages database (and the Contacts) be read from here? (The "full disk access for remote users" switch.)
    async fn check_access(&self) -> Result<bool, String> {
        let cmd = "sqlite3 -readonly ~/Library/Messages/chat.db 'select count(*) from chat' >/dev/null 2>&1 && echo OK || echo NO";
        let (_, out) = self.run(cmd, None, Duration::from_secs(30)).await?;
        Ok(out.trim() == "OK")
    }

    /// Send a short message to `to` through Messages. The first send makes macOS ask for permission, so this waits.
    async fn test_send(&self, imsg: &str, to: &str) -> Result<(), String> {
        let cmd = format!("{} send --to {} --text 'BubbleFairy setup test' --service auto 2>&1", sh_quote(imsg), sh_quote(to));
        match self.run(&cmd, None, Duration::from_secs(180)).await? {
            (0, _) => Ok(()),
            (_, out) => Err(if out.is_empty() { "the Mac didn't send it".into() } else { out }),
        }
    }

    /// Dev profile only: take the throwaway key out again.
    async fn remove_key(&self, key: &str) {
        let id = key.split_whitespace().nth(1).unwrap_or("").to_string();
        if id.len() > 20 {
            let _ = self.run(&format!("grep -vF {} ~/.ssh/authorized_keys > ~/.ssh/ak.tmp; cat ~/.ssh/ak.tmp > ~/.ssh/authorized_keys; rm -f ~/.ssh/ak.tmp", sh_quote(&id)), None, Duration::from_secs(20)).await;
        }
    }
}

/// Does signing in with the key work now?
async fn verify_key(host: &str, user: &str, key_path: PathBuf) -> Result<Link, String> {
    let mut link = open(host).await?;
    let key = russh::keys::load_secret_key(&key_path, None).map_err(|e| format!("can't read the key: {e}"))?;
    let r = link.session.authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), None)).await.map_err(|e| format!("{e}"))?;
    if r.success() { Ok(link) } else { Err("the Mac didn't accept BubbleFairy's key (is the Mac set to allow key sign-in?)".into()) }
}

/// Where a bundled copy of the imsg zip is, if this build has one (the Flatpak does).
fn bundled_zip() -> Option<PathBuf> {
    ["/app/share/bubblefairy/imsg-macos.zip", "/usr/share/bubblefairy/imsg-macos.zip", concat!(env!("CARGO_MANIFEST_DIR"), "/vendor/imsg-macos.zip")]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
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
            let bytes = session.send_and_read_future(&msg, glib::Priority::DEFAULT).await.map_err(|e| format!("couldn't download imsg: {e}"))?;
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
    test_btn: gtk::Button,
    // connection state
    online: RefCell<Option<Arc<Link>>>,
    imsg: RefCell<String>,
    page: Cell<usize>,
    /// the last thing to act on the arrow button was the keyboard (so the list's entries take focus), not the pointer
    btn_key: Cell<bool>,
    dots: Vec<gtk::Box>,
}

const PAGES: [&str; 5] = ["mac", "account", "work", "test", "done"];
const STEPS: [&str; 6] = ["Signing in", "Checking the Mac", "Adding BubbleFairy's key", "Checking that the key works", "Installing imsg", "Checking that Messages can be read"];

/// Open the wizard over `parent`.
pub fn open_wizard(app: &gtk::Application, parent: Option<&gtk::Window>, ctx: Ctx) {
    let win = gtk::Window::builder().application(app).title("Set up BubbleFairy").default_width(640).default_height(660).build();
    if let Some(p) = parent {
        win.set_transient_for(Some(p));
        win.set_modal(true);
    }
    // the app's own look: the gradient window, a glassy panel, the brand in the title bar
    win.add_css_class("bf");
    win.add_css_class("wizard");
    let head = gtk::HeaderBar::new();
    head.add_css_class("bf-head");
    let brand = gtk::Label::new(None);
    brand.set_markup("<span foreground='#7b5cff'>Bubble</span><span foreground='#ff5fa8'>Fairy</span> ✨");
    brand.add_css_class("app-title");
    head.set_title_widget(Some(&brand));
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
    let page = |title: &str| {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
        b.set_margin_top(8);
        b.append(&label(title, "setup-title"));
        b
    };

    // 1: the Mac
    let p1 = page("Find your Mac");
    p1.append(&label("On the Mac, open System Settings → General → Sharing and turn on Remote Login. Then pick the Mac here, or type its name or address.", "dim"));
    let host = gtk::Entry::builder().placeholder_text("my-mac.local").hexpand(true).css_classes(["wiz-field"]).build();
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
    field.set_size_request(420, -1);
    p1.append(&field);
    let host_msg = label("", "wiz-err");
    host_msg.set_halign(gtk::Align::Center);
    host_msg.set_justify(gtk::Justification::Center);
    p1.append(&host_msg);
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
    let pick_pop = gtk::Popover::builder().child(&pick_box).autohide(true).has_arrow(false).css_classes(["wiz-drop"]).build();
    pick_pop.set_parent(&host);
    pick_pop.set_position(gtk::PositionType::Bottom);
    pick_pop.set_offset(0, 2);

    // 2: the account
    let p2 = page("Your Mac account");
    p2.append(&label("BubbleFairy signs in once with your Mac's user name and password to set things up. The password is used for that and then forgotten.", "dim"));
    let user = gtk::Entry::builder().text(&ctx.default_user).placeholder_text("Mac user name").css_classes(["wiz-field"]).build();
    let pass = gtk::PasswordEntry::builder().show_peek_icon(true).placeholder_text("Mac password").css_classes(["wiz-field"]).build();
    p2.append(&user);
    p2.append(&pass);
    let acct_msg = label("", "step-bad");
    p2.append(&acct_msg);

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
    work_btn.set_halign(gtk::Align::Start);
    work_btn.set_visible(false);
    p4.append(&work_btn);

    // 5: the test message
    let p5 = page("Send a test message");
    p5.append(&label("This sends a short message through Messages on your Mac, so you know it works. Have your Mac in view: the first time, macOS asks whether to allow control of Messages. Click Allow. You can send it to yourself: use your own phone number or Apple ID email.", "dim"));
    let to = gtk::Entry::builder().placeholder_text("your phone number or email").css_classes(["wiz-field"]).build();
    p5.append(&to);
    let test_btn = gtk::Button::with_label("Send test message");
    test_btn.set_halign(gtk::Align::Start);
    p5.append(&test_btn);
    let test_msg = label("", "");
    p5.append(&test_msg);

    // 6: done
    let p6 = page("All set");
    p6.append(&label("BubbleFairy is connected to your Mac.", "dim"));

    let pages = gtk::Stack::new();
    for (name, p) in PAGES.iter().zip([&p1, &p2, &p4, &p5, &p6]) {
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
    let dots: Vec<gtk::Box> = PAGES
        .iter()
        .map(|_| {
            let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            d.add_css_class("wiz-dot");
            dot_row.append(&d);
            d
        })
        .collect();
    let bar = gtk::CenterBox::new();
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
        test_btn,
        online: RefCell::new(None),
        imsg: RefCell::new(String::new()),
        page: Cell::new(0),
        btn_key: Cell::new(false),
        dots,
    });
    for i in 0..STEPS.len() {
        set_step(&w, i, Step::Wait);
    }
    wire_host(&w);
    wire_nav(&w);
    show_page(&w, 0);
    win.present();
    // dev aids: BF_SETUP_PAGE=<n> starts on that page; BF_SETUP_SHOT=<file.png> saves the wizard after BF_SETUP_MS (default 3 s)
    if let Some(n) = std::env::var("BF_SETUP_PAGE").ok().and_then(|v| v.parse::<usize>().ok()) {
        show_page(&w, n.min(PAGES.len() - 1));
    }
    // dev aid BF_SETUP_POP=<png>: open the Mac list, hover its first entry, and save the list's own picture
    if let (Some(out), Some(ms)) = (std::env::var_os("BF_SETUP_POP"), std::env::var("BF_SETUP_MS").ok().and_then(|v| v.parse::<u64>().ok())) {
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
                p.snapshot(&snap, w3.pick_pop.width() as f64, w3.pick_pop.height() as f64);
                if let (Some(node), Some(r)) = (snap.to_node(), w3.win.renderer()) {
                    let _ = r.render_texture(&node, None).save_to_png(&out);
                }
            });
        });
    }
    if let Some(out) = std::env::var_os("BF_SETUP_SHOT") {
        let ms = std::env::var("BF_SETUP_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
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
        check_host(&w);
    }
    // dev aid: BF_SETUP_AUTO=<mac user> clicks Next through the first three pages (with BF_SETUP_KEYLOGIN, no password)
    if let Ok(user) = std::env::var("BF_SETUP_AUTO") {
        w.user.set_text(&user);
        w.pass.set_text("x");
        let w2 = w.clone();
        glib::timeout_add_local(Duration::from_millis(1500), move || {
            if w2.page.get() <= 1 && w2.next.is_sensitive() {
                w2.next.emit_clicked();
            }
            if w2.page.get() >= 2 { glib::ControlFlow::Break } else { glib::ControlFlow::Continue }
        });
    }
}

fn show_page(w: &Rc<W>, i: usize) {
    w.page.set(i);
    for (n, d) in w.dots.iter().enumerate() {
        if n == i { d.add_css_class("on") } else { d.remove_css_class("on") }
    }
    w.pages.set_visible_child_name(PAGES[i]);
    w.back.set_visible(i == 1);
    w.next.set_visible(true);
    w.next.set_label(match i {
        1 => "Connect",
        3 => "Skip",
        4 => "Finish",
        _ => "Next",
    });
    w.next.set_sensitive(match i {
        0 => w.host_ok.get(),
        2 => false,
        _ => true,
    });
}

// ----- page 1: the Mac picker -----

fn set_host_status(w: &Rc<W>, state: &str, msg: &str) {
    w.host_status.set_visible_child_name(state);
    w.host_msg.set_text(msg);
    w.host_ok.set(state == "ok");
    if w.page.get() == 0 {
        w.next.set_sensitive(state == "ok");
    }
}

/// The name of a Mac in the list is final: it can't be edited until it is cleared.
fn lock_host(w: &Rc<W>, host: &str) {
    w.editing.set(true);
    w.host.set_text(host);
    w.editing.set(false);
    w.host.set_editable(false);
    w.host.set_sensitive(false); // like a disabled input: it can't take focus, so Tab lands on the clear button
    w.locked.set(true);
    w.drop_btn.set_icon_name("edit-clear-symbolic");
    w.drop_btn.set_tooltip_text(Some("Choose a different Mac"));
    set_host_status(w, "ok", "");
    // the next thing is the button; the field is locked, so no cursor in it. After a mouse pick GTK wouldn't draw the
    // focus ring on its own, so ask for it
    w.win.set_focus_visible(true);
    w.next.grab_focus();
}

fn unlock_host(w: &Rc<W>) {
    w.locked.set(false);
    w.host.set_sensitive(true);
    w.host.set_editable(true);
    w.editing.set(true);
    w.host.set_text("");
    w.editing.set(false);
    w.drop_btn.set_icon_name("pan-down-symbolic");
    w.drop_btn.set_tooltip_text(Some("Macs found on the network"));
    set_host_status(w, "none", "");
    w.host.grab_focus();
}

/// A typed name: look for it on the network (unless it is one the scan already found).
fn check_host(w: &Rc<W>) {
    let host = w.host.text().trim().to_string();
    if host.is_empty() || w.locked.get() {
        return;
    }
    if w.found.borrow().iter().any(|f| f.host.eq_ignore_ascii_case(&host)) {
        lock_host(w, &host);
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
        lock_host(&w2, &host);
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
            w2.scan_text.set_text("No Macs found. Type its name in the field.");
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
        if w2.locked.get() && matches!(key, gdk::Key::BackSpace | gdk::Key::Delete | gdk::Key::Escape) {
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
        let hit = w2.found.borrow().iter().find(|f| f.host.to_lowercase().starts_with(&lower) || f.name.to_lowercase().starts_with(&lower)).cloned();
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
            if matches!(key, gdk::Key::BackSpace | gdk::Key::Delete | gdk::Key::Escape) {
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
            check_host(&w2);
        }
    });
    w.host.add_controller(focus);
    let w2 = w.clone();
    w.host.connect_activate(move |_| check_host(&w2));
}

// ----- navigation -----

fn wire_nav(w: &Rc<W>) {
    let w2 = w.clone();
    w.back.connect_clicked(move |_| {
        show_page(&w2, w2.page.get().saturating_sub(1));
    });
    let w2 = w.clone();
    w.next.connect_clicked(move |_| match w2.page.get() {
        0 => {
            check_host(&w2);
            if w2.host_ok.get() {
                show_page(&w2, 1);
                w2.pass.grab_focus();
            }
        }
        1 => connect(&w2),
        3 => show_page(&w2, 4),
        _ => finish(&w2),
    });
    let w2 = w.clone();
    w.pass.connect_activate(move |_| {
        if w2.page.get() == 1 {
            connect(&w2);
        }
    });
    let w2 = w.clone();
    w.work_btn.connect_clicked(move |_| provision(&w2));
    let w2 = w.clone();
    w.test_btn.connect_clicked(move |_| test_send(&w2));
}

/// Page 2 → 3: connect and sign in (a wrong password stays here, with the message), then set everything up.
/// The Mac's host key is pinned as first seen, like `ssh` does on a first connection: the person picked this Mac by
/// name on their own network, and "fingerprints" mean nothing to most people.
fn connect(w: &Rc<W>) {
    if w.user.text().trim().is_empty() || w.pass.text().is_empty() {
        w.acct_msg.set_text("Enter the Mac's user name and password.");
        return;
    }
    w.acct_msg.set_text("");
    w.next.set_sensitive(false);
    let (host, user, pass) = (w.host.text().trim().to_string(), w.user.text().trim().to_string(), w.pass.text().to_string());
    let dev_key = std::env::var_os("BF_SETUP_KEYLOGIN").map(|_| glib::user_data_dir().join("bubblefairy/id_ed25519"));
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
        w2.next.set_sensitive(true);
        match r {
            Ok(link) => {
                *w2.online.borrow_mut() = Some(Arc::new(link));
                show_page(&w2, 2);
                for i in 0..STEPS.len() {
                    set_step(&w2, i, Step::Wait);
                }
                set_step(&w2, 0, Step::Ok);
                w2.work_msg.set_text("");
                w2.work_btn.set_visible(false);
                provision_from(&w2, 1).await;
            }
            Err(e) => w2.acct_msg.set_text(&e),
        }
    });
}

fn set_step(w: &Rc<W>, i: usize, s: Step) {
    let (icon, spin, label) = &w.steps[i];
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
        show_page(w, 1);
        return;
    }
    w.work_btn.set_visible(false);
    w.work_msg.set_text("");
    let w2 = w.clone();
    glib::spawn_future_local(async move { provision_from(&w2, 1).await });
}

/// Steps 1.. : check the Mac, add the key, check the key, install imsg, check access.
async fn provision_from(w: &Rc<W>, from: usize) {
    let Some(link) = w.online.borrow().clone() else { return };
    let (host, user) = (w.host.text().trim().to_string(), w.user.text().trim().to_string());
    let folder = if w.ctx.test { "BubbleFairy-test" } else { "BubbleFairy" };
    for i in from..STEPS.len() {
        set_step(w, i, Step::Run);
    }
    let mut home = String::new();
    for i in from..STEPS.len() {
        set_step(w, i, Step::Run);
        let r: Result<(), String> = match i {
            1 => {
                let l = link.clone();
                rt(async move { l.check_mac().await }).await.map(|(h, about)| {
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
                            "macOS isn't letting BubbleFairy read Messages. On the Mac, open System Settings → General → Sharing, click ⓘ next to Remote Login, turn on \"Allow full disk access for remote users\", then check again.",
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
    show_page(w, 3);
}

fn test_send(w: &Rc<W>) {
    let to = w.to.text().trim().to_string();
    let (Some(link), false) = (w.online.borrow().clone(), to.is_empty()) else {
        w.test_msg.set_text("Enter a phone number or email to send it to.");
        return;
    };
    w.test_btn.set_sensitive(false);
    w.next.set_sensitive(false);
    w.test_msg.set_text("Sending… watch your Mac: if it asks for permission to control Messages, click Allow.");
    let (w2, imsg) = (w.clone(), w.imsg.borrow().clone());
    glib::spawn_future_local(async move {
        let r = rt(async move { link.test_send(&imsg, &to).await }).await;
        w2.test_btn.set_sensitive(true);
        w2.next.set_sensitive(true);
        match r {
            Ok(()) => {
                w2.test_msg.set_text("Sent. It should arrive in a moment.");
                w2.next.set_label("Continue");
            }
            Err(e) => w2.test_msg.set_text(&format!("It didn't go: {e}\nIf macOS asked for permission and you missed it, try again.")),
        }
    });
}

fn finish(w: &Rc<W>) {
    let (host, user, imsg) = (w.host.text().trim().to_string(), w.user.text().trim().to_string(), w.imsg.borrow().clone());
    let online = w.online.borrow().clone();
    let host_key = online.as_ref().map(|l| l.host_key.clone()).unwrap_or_default();
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
        assert_eq!(super::sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
