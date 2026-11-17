//! Talking to the Mac: one SSH session (russh) running `imsg rpc` (JSON-RPC, one JSON object per line),
//! plus short extra exec channels for fetching images. Runs on tokio; the UI only sees `Cmd` and `Event`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, client};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub host: String,
    pub user: String,
    pub imsg: String,
    /// show a map picture under postal addresses on contact cards (sends the address to OpenStreetMap)
    pub map_previews: bool,
}

pub enum Cmd {
    Configure(Config),
    Chats,
    /// The newest 1:1 chat with any of these addresses (one person's numbers and emails: Messages keeps one conversation
    /// per contact), which can be older than the chats list's 100 (answer: `Event::FoundChat`)
    FindChat(Vec<String>),
    /// the chat's newest `limit` messages
    History(i64, usize),
    /// Just the newest message, for the chat list's preview line.
    Preview(i64),
    /// Older messages, before the given ISO time (the oldest one loaded).
    Older { id: i64, before: String },
    /// `to`: the other person in a 1:1 chat, sent directly so Messages picks the service (see handle)
    /// `files` go first, one message each, then `text` (if any) as its own message
    /// `token` names this send, so the answer (`Event::Sent`) can be matched to its waiting bubbles
    Send { chat_id: i64, to: Option<String>, text: String, files: Vec<PathBuf>, token: u64 },
    /// Copy a file of a chat from the Mac to `dest` (to open it on this computer)
    /// `size` (from chat.db, 0 if unknown) is for the progress and for spotting a copy that was cut short
    Download { path: String, dest: PathBuf, size: i64 },
    /// A chat copy for the info panel's grid: same as `Image` but at most two at a time, so a screenful
    /// of thumbnails can't crowd out the conversation's own pictures, previews and maps
    Bulk(String),
    /// `full`: the viewer wants the big version (up to 4096 px) instead of the 1200 px chat copy
    Image { path: String, full: bool },
    /// Apple's link preview stored on this message row
    Link(i64),
    /// What the Mac can say about a file beyond the database: a recording's length, or the name on a contact card
    FileInfo { path: String, contact: bool },
    /// The Mac's contact photos, to use as avatars
    Contacts,
    /// A small OpenStreetMap picture of this postal address (see `map_for`)
    Map(String),
    /// One page of a chat's photos, files or links, newest first, read from chat.db (for the info panel)
    Media { chat_id: i64, kind: Kind, offset: usize },
    /// The chat's newest few messages, to merge in by guid (e.g. a sent link once Messages has made its preview)
    Recent(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Photos,
    Files,
    Links,
}

/// More about a file than chat.db knows (see `file_info`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileInfo {
    /// seconds, for audio and video
    pub duration: Option<f64>,
    /// a contact card's name, company and first phone number
    pub name: String,
    pub org: String,
    pub phone: String,
}

/// One photo, file or link of a chat.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaItem {
    /// attachment path on the Mac (photos, files)
    pub path: String,
    pub name: String,
    pub size: i64,
    /// unix seconds
    pub date: i64,
    pub url: String,
    /// the file isn't on the Mac (Messages hasn't downloaded it from iCloud, or it was deleted)
    pub missing: bool,
    /// links: the message row, and whether it is Apple's link-preview row (`Cmd::Link` can load the card)
    pub row: i64,
    pub preview: bool,
}

pub const MEDIA_PAGE: usize = 60;
const PHOTO_PAGE: usize = 30;
static BULK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub enum Event {
    Connecting,
    Connected,
    Offline(String),
    /// Connected, but something on the Mac needs fixing (e.g. Full Disk Access).
    Warn(String),
    HostKey(String),
    Chats(Vec<Value>),
    /// the address that was typed, and the chat found for it or its person's other addresses
    FoundChat(String, Option<Value>),
    History(i64, Vec<Value>),
    Preview(i64, Value),
    Older(i64, Vec<Value>),
    Message(Value),
    /// The watch stream lost messages; reload everything.
    Resync,
    /// on failure, `text` and `files` are what didn't go out (to put back in the composer)
    Sent { text: String, files: Vec<PathBuf>, err: Option<String>, token: u64 },
    SendSlow,
    Image { path: String, full: bool, bytes: Option<Vec<u8>> },
    Link(i64, Option<LinkPreview>),
    FileInfo(String, Option<FileInfo>),
    Contacts(Contacts),
    Map(String, Option<MapData>),
    /// `next`: where the following page starts; `more`: whether there is one
    Media { chat_id: i64, kind: Kind, offset: usize, items: Vec<MediaItem>, next: usize, more: bool },
    /// oldest first
    Recent(i64, Vec<Value>),
    Downloaded { path: String, dest: PathBuf, err: Option<String> },
    /// a copy from the Mac is under way: bytes so far of `total` (0 = unknown)
    DownloadProgress { path: String, got: usize, total: i64 },
}

pub struct Paths {
    pub key: PathBuf,
    pub known_host: PathBuf,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Clone)]
struct Rpc {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: Arc<AtomicU64>,
}

impl Rpc {
    async fn call(&self, method: &str, params: Value, wait: Duration) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string() + "\n";
        if self.tx.send(line).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err("not connected".into());
        }
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("connection lost".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err("the Mac took too long to answer".into())
            }
        }
    }
}

struct Handler {
    known: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else { return Ok(false) };
        let line = key.to_openssh().unwrap_or_default();
        *self.seen.lock().unwrap() = Some(line.clone());
        // ponytail: trust on first use, like ssh's StrictHostKeyChecking=accept-new
        Ok(self.known.as_deref().is_none_or(|k| k == line))
    }
}

/// Quote for the Mac's shell (zsh/sh): everything inside single quotes, `'` becomes `'\''`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Load the app's own ed25519 key, creating it on first run. Returns the public key line.
pub fn ensure_key(path: &PathBuf) -> std::io::Result<String> {
    use russh::keys::ssh_key::{LineEnding, private::Ed25519Keypair};
    if !path.exists() {
        let mut seed = [0u8; 32];
        std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut seed)?;
        let key = PrivateKey::new(Ed25519Keypair::from_seed(&seed).into(), "BubbleFairy")
            .map_err(std::io::Error::other)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let pem = key.to_openssh(LineEnding::LF).map_err(std::io::Error::other)?;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        std::io::Write::write_all(&mut f, pem.as_bytes())?;
    }
    let key = russh::keys::load_secret_key(path, None).map_err(std::io::Error::other)?;
    key.public_key().to_openssh().map_err(std::io::Error::other)
}

pub fn fingerprint(openssh_line: &str) -> String {
    russh::keys::PublicKey::from_openssh(openssh_line)
        .map(|k| k.fingerprint(HashAlg::Sha256).to_string())
        .unwrap_or_default()
}

async fn resolve(host: &str) -> Result<std::net::SocketAddr, String> {
    if let Ok(mut it) = tokio::net::lookup_host((host, 22)).await
        && let Some(a) = it.next()
    {
        return Ok(a);
    }
    // Flatpak runtimes can't resolve `.local` names (no nss-mdns), so ask the network ourselves.
    if host.ends_with(".local") {
        let h = format!("{host}.");
        let ip = tokio::task::spawn_blocking(move || {
            let d = mdns_sd::ServiceDaemon::new().ok()?;
            let rx = d.resolve_hostname(&h, Some(3000)).ok()?;
            let mut found = None;
            while let Ok(ev) = rx.recv_timeout(Duration::from_millis(3500)) {
                match ev {
                    mdns_sd::HostnameResolutionEvent::AddressesFound(_, ips) => {
                        // prefer IPv4: IPv6 link-local needs a scope id
                        found = ips.iter().map(|i| i.to_ip_addr()).min_by_key(|ip| ip.is_ipv6());
                        break;
                    }
                    mdns_sd::HostnameResolutionEvent::SearchTimeout(_) => break,
                    _ => {}
                }
            }
            let _ = d.shutdown();
            found
        })
        .await
        .ok()
        .flatten();
        if let Some(ip) = ip {
            return Ok((ip, 22).into());
        }
    }
    Err(format!("can't find {host} on the network (is the Mac awake?)"))
}

/// sshd allows 10 channels per connection by default (MaxSessions); the rpc channel uses one.
static CHANNELS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(6);

struct Conn {
    session: Arc<client::Handle<Handler>>,
    rpc: Rpc,
    dead: oneshot::Receiver<()>,
}

async fn connect(cfg: &Config, paths: &Paths, ev: &mpsc::UnboundedSender<Event>) -> Result<Conn, String> {
    let addr = resolve(&cfg.host).await?;
    let known = std::fs::read_to_string(&paths.known_host).ok().map(|s| s.trim().to_string());
    let seen = Arc::new(Mutex::new(None));
    let config = client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    };
    let handler = Handler { known: known.clone(), seen: seen.clone() };
    let conn = tokio::time::timeout(Duration::from_secs(10), client::connect(Arc::new(config), addr, handler)).await;
    let mut session = match conn {
        Err(_) => return Err(format!("{} didn't answer (is Remote Login on?)", cfg.host)),
        Ok(Err(russh::Error::UnknownKey)) => {
            return Err("the Mac's identity changed since the first connection. If you reinstalled or replaced the Mac, \
                        delete the saved host key in Settings and connect again."
                .into());
        }
        Ok(Err(e)) => return Err(format!("couldn't connect: {e}")),
        Ok(Ok(s)) => s,
    };
    let seen = seen.lock().unwrap().clone().unwrap_or_default();
    let _ = ev.send(Event::HostKey(seen.clone()));

    let key = russh::keys::load_secret_key(&paths.key, None).map_err(|e| format!("can't read the app's key: {e}"))?;
    let auth = session
        .authenticate_publickey(&cfg.user, PrivateKeyWithHashAlg::new(Arc::new(key), None))
        .await
        .map_err(|e| format!("login failed: {e}"))?;
    if !auth.success() {
        return Err("the Mac refused BubbleFairy's key. Add the public key from Settings to ~/.ssh/authorized_keys on the Mac.".into());
    }
    if known.is_none() {
        let _ = std::fs::write(&paths.known_host, &seen);
    }

    let channel = session.channel_open_session().await.map_err(|e| e.to_string())?;
    channel
        .exec(true, format!("{} rpc", sh_quote(&cfg.imsg)))
        .await
        .map_err(|e| e.to_string())?;
    let (mut rd, wr) = channel.split();

    let (tx, mut lines) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(l) = lines.recv().await {
            if wr.data(l.as_bytes()).await.is_err() {
                break;
            }
        }
        // dropping the write half closes imsg's stdin, and imsg exits cleanly on EOF
        let _ = wr.eof().await;
    });

    let pending: Pending = Default::default();
    let (dead_tx, dead) = oneshot::channel();
    let (live_tx, mut live_rx) = mpsc::unbounded_channel::<Value>();
    let (p, ev2) = (pending.clone(), ev.clone());
    tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut stderr = String::new();
        while let Some(msg) = rd.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    buf.extend_from_slice(&data);
                    while let Some(n) = buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=n).collect();
                        if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                            dispatch(v, &p, &ev2, &live_tx);
                        }
                    }
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    stderr.push_str(&String::from_utf8_lossy(&data));
                }
                ChannelMsg::ExitStatus { exit_status } if exit_status != 0 => {
                    let tail: String = stderr.trim().lines().last().unwrap_or("").into();
                    let why = if exit_status == 127 { "imsg isn't installed at the configured path".into() } else { tail };
                    eprintln!("imsg rpc exited {exit_status}: {stderr}");
                    let _ = ev2.send(Event::Warn(format!("imsg stopped: {why}")));
                }
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        for (_, tx) in p.lock().unwrap().drain() {
            let _ = tx.send(Err("connection lost".into()));
        }
        let _ = dead_tx.send(());
    });

    let session = Arc::new(session);
    let (s2, ev3) = (session.clone(), ev.clone());
    tokio::spawn(async move {
        while let Some(mut m) = live_rx.recv().await {
            add_services(&s2, std::slice::from_mut(&mut m)).await;
            let _ = ev3.send(Event::Message(m));
        }
    });
    Ok(Conn { session, rpc: Rpc { tx, pending, next: Arc::new(AtomicU64::new(1)) }, dead })
}

fn dispatch(v: Value, pending: &Pending, ev: &mpsc::UnboundedSender<Event>, live: &mpsc::UnboundedSender<Value>) {
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        if let Some(tx) = pending.lock().unwrap().remove(&id) {
            let r = match v.get("error") {
                Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()),
                None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(r);
        }
        return;
    }
    match v.get("method").and_then(Value::as_str) {
        Some("message") => {
            if let Some(m) = v.pointer("/params/message") {
                let _ = live.send(m.clone());
            }
        }
        Some("watch.overflow") => {
            let _ = ev.send(Event::Resync);
        }
        _ => {}
    }
}

/// Run a shell command on the Mac in its own channel and collect stdout.
async fn exec(session: &client::Handle<Handler>, cmd: String) -> Option<Vec<u8>> {
    let _permit = CHANNELS.acquire().await.ok()?;
    let mut ch = session.channel_open_session().await.ok()?;
    ch.exec(true, cmd).await.ok()?;
    let mut out = Vec::new();
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Fetch an image from the Mac, shrunk and converted to JPEG there (handles HEIC, saves bandwidth).
async fn fetch_image(session: &client::Handle<Handler>, path: &str, max: u32) -> Option<Vec<u8>> {
    // ponytail: animated GIFs arrive as a still frame; keep them as-is when animation matters
    // link-preview payloads are plain PNG/JPEG/ICO, and Quick Look hangs on their unknown extension
    let ql = !path.ends_with(".pluginPayloadAttachment");
    let path = path.strip_prefix("~/").map(|p| format!("\"$HOME\"/{}", sh_quote(p))).unwrap_or_else(|| sh_quote(path));
    let ql = if ql {
        // Quick Look applies the photo's rotation (HEIC `irot`), which sips ignores; sips is the fallback.
        // The watchdog stops a stuck qlmanage from holding this channel forever.
        format!("qlmanage -t -s {max} -o \"$d\" {path} >/dev/null 2>&1 & q=$!; (sleep 8; kill $q 2>/dev/null) & w=$!; wait $q; kill $w 2>/dev/null;")
    } else {
        String::new()
    };
    exec(
        session,
        format!(
            "d=$(mktemp -d) || exit 1; {ql} \
             f=$(ls \"$d\"/*.png 2>/dev/null | head -n 1); [ -n \"$f\" ] || f={path}; \
             sips -s format jpeg -s formatOptions 85 -Z {max} \"$f\" --out \"$d/i.jpg\" >/dev/null 2>&1 && cat \"$d/i.jpg\"; rm -rf \"$d\""
        ),
    )
    .await
}

/// imsg doesn't report which service carried each message (an iMessage chat can fall back to SMS),
/// so read it straight from chat.db. ponytail: drop this once imsg exposes a per-message `service`.
async fn add_services(session: &client::Handle<Handler>, msgs: &mut [Value]) {
    let ids: Vec<String> = msgs.iter().filter_map(|m| m["id"].as_i64()).map(|i| i.to_string()).collect();
    if ids.is_empty() {
        return;
    }
    let sql = format!("select ROWID||'|'||service from message where ROWID in ({})", ids.join(","));
    let Some(out) = exec(session, format!("sqlite3 -readonly ~/Library/Messages/chat.db {}", sh_quote(&sql))).await else { return };
    let map: HashMap<i64, String> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| l.split_once('|'))
        .filter_map(|(id, svc)| Some((id.parse().ok()?, svc.to_string())))
        .collect();
    for m in msgs {
        if let Some(svc) = m["id"].as_i64().and_then(|i| map.get(&i)) {
            m["service"] = Value::String(svc.clone());
        }
    }
}

/// The newest 1:1 chat with `addr` (an email, or a phone number matched on its last ten digits), shaped like an
/// imsg chats.list entry. ponytail: a number with fewer than 10 digits isn't looked up.
async fn find_chat(session: &client::Handle<Handler>, addrs: &[String]) -> Option<Value> {
    let conds: Vec<String> = addrs
        .iter()
        .filter_map(|addr| {
            if addr.contains('@') {
                return Some(format!("lower(c.chat_identifier)=lower({})", sh_sql(addr)));
            }
            let d: String = addr.chars().filter(char::is_ascii_digit).collect();
            let ten = d.get(d.len().checked_sub(10)?..)?;
            Some(format!(
                "(c.chat_identifier not like '%@%' and replace(replace(replace(replace(replace(c.chat_identifier,'+',''),'-',''),' ',''),'(',''),')','') like '%{ten}')"
            ))
        })
        .collect();
    if conds.is_empty() {
        return None;
    }
    let cond = format!("({})", conds.join(" or "));
    let sql = format!(
        "select c.ROWID, hex(c.chat_identifier), coalesce(c.service_name,'iMessage'), \
         strftime('%Y-%m-%dT%H:%M:%SZ', coalesce((select max(m.date) from message m join chat_message_join j on j.message_id=m.ROWID where j.chat_id=c.ROWID),0)/1000000000+978307200, 'unixepoch') \
         from chat c where c.style=45 and {cond} order by 4 desc limit 1"
    );
    let out = exec(session, format!("sqlite3 -readonly ~/Library/Messages/chat.db {}", sh_quote(&sql))).await?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let r: Vec<&str> = text.lines().next()?.split('|').collect();
    let ident = hex_text(r.get(1)?);
    Some(json!({"id": r.first()?.parse::<i64>().ok()?, "display_name": "", "identifier": ident, "participants": [ident],
        "service": r.get(2)?, "is_group": false, "unread_count": 0, "last_message_at": r.get(3)?}))
}

/// A string as an SQL literal.
fn sh_sql(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Contact photos from the Mac's Contacts, found by phone number or email.
#[derive(Default)]
pub struct Contacts {
    index: HashMap<String, usize>,
    photos: Vec<Vec<u8>>,
    people: Vec<Person>,
    /// "mobile", "work"... for a phone number or email, by `addr_key`
    labels: HashMap<String, String>,
    /// contact index by `addr_key`, for the contact card
    by_addr: HashMap<String, usize>,
    /// numbers and emails Messages has an iMessage conversation history with (`addr_key`s)
    imessage: std::collections::HashSet<String>,
}

/// One address-book entry: a display name and every phone number / email it has.
#[derive(Default)]
pub struct Person {
    pub name: String,
    /// company, when the contact also has a personal name
    pub org: String,
    /// (number or email, label)
    pub addrs: Vec<(String, String)>,
    /// postal addresses: (label, lines)
    pub places: Vec<(String, Vec<String>)>,
    /// e.g. "Nov 15" or "Nov 15, 1980"
    pub birthday: Option<String>,
}

impl Contacts {
    /// The photo (JPEG/PNG bytes) for a phone number or email address, if the contact has one.
    pub fn photo(&self, handle: &str) -> Option<&[u8]> {
        self.index.get(&addr_key(handle)?).map(|i| &self.photos[*i][..])
    }

    /// The address-book entry that has this number or email.
    pub fn person(&self, handle: &str) -> Option<&Person> {
        self.people.get(*self.by_addr.get(&addr_key(handle)?)?)
    }

    /// True when Messages has an iMessage history with this number or email. (Whether a number is
    /// *registered* with iMessage can't be asked from the Mac, so no history means "unknown".)
    pub fn has_imessage(&self, addr: &str) -> bool {
        addr_key(addr).is_some_and(|k| self.imessage.contains(&k))
    }

    /// The label the contact gave this number or email ("mobile", "work", "home"...), if any.
    pub fn label(&self, addr: &str) -> Option<&str> {
        self.labels.get(&addr_key(addr)?).map(String::as_str)
    }

    /// (name, address) for each contact number or email matching `q` (name, email text, or phone digits).
    pub fn search(&self, q: &str) -> Vec<(&str, &str)> {
        let q = q.trim().to_lowercase();
        let qd: String = q.chars().filter(char::is_ascii_digit).collect();
        let mut out = vec![];
        for p in &self.people {
            let name_hit = p.name.to_lowercase().contains(&q);
            for (a, _) in &p.addrs {
                let d: String = a.chars().filter(char::is_ascii_digit).collect();
                if q.is_empty() || name_hit || a.to_lowercase().contains(&q) || (qd.len() >= 3 && d.contains(&qd)) {
                    out.push((&p.name[..], &a[..]));
                }
            }
        }
        out
    }
}

/// What two spellings of the same address share: an email lowercased, a phone number's last ten
/// digits ("(403) 555-1234" and "+14035551234" match). Short codes have no key.
pub fn addr_key(s: &str) -> Option<String> {
    let s = s.trim();
    if s.contains('@') {
        return Some(s.to_lowercase());
    }
    let d: String = s.chars().filter(char::is_ascii_digit).collect();
    (d.len() >= 10).then(|| d[d.len() - 10..].to_string())
}

/// Read every Contacts database on the Mac (iCloud and local accounts each have one): the photo of
/// each contact that has one, plus the numbers and emails that point at it.
async fn fetch_contacts(session: &client::Handle<Handler>) -> Option<Contacts> {
    // type 1 photos are stored in the row; type 2 name a file next to the database (often a
    // full-size original), which the script shrinks with sips and sends as a type 1 photo
    let sql = "select 'K|'||p.ZOWNER||'|'||p.ZFULLNUMBER||'|'||replace(coalesce(p.ZLABEL,''),'|',' ') from ZABCDPHONENUMBER p join ZABCDRECORD r on r.Z_PK=p.ZOWNER \
               where p.ZFULLNUMBER is not null \
               union all select 'K|'||e.ZOWNER||'|'||lower(e.ZADDRESS)||'|'||replace(coalesce(e.ZLABEL,''),'|',' ') from ZABCDEMAILADDRESS e join ZABCDRECORD r on r.Z_PK=e.ZOWNER \
               where e.ZADDRESS is not null \
               union all select 'N|'||Z_PK||'|'||replace(replace(trim(coalesce(ZFIRSTNAME,'')||' '||coalesce(ZLASTNAME,'')),'|',' '),char(10),' ')||'|'||replace(replace(coalesce(ZORGANIZATION,ZNICKNAME,''),'|',' '),char(10),' ') from ZABCDRECORD \
               union all select 'P|'||Z_PK||'|'||hex(ZTHUMBNAILIMAGEDATA) from ZABCDRECORD where hex(substr(ZTHUMBNAILIMAGEDATA,1,1))='01' \
               union all select 'X|'||Z_PK||'|'||cast(substr(ZTHUMBNAILIMAGEDATA,2) as text) from ZABCDRECORD where hex(substr(ZTHUMBNAILIMAGEDATA,1,1))='02'";
    // postal addresses and birthdays, in a separate query so a column that differs on some macOS
    // version loses only these, not the names and photos
    let sql2 = "select 'A|'||ZOWNER||'|'||replace(coalesce(ZLABEL,''),'|',' ')||'|'||replace(replace(coalesce(ZSTREET,''),'|',' '),char(10),', ')||'|'||replace(coalesce(ZCITY,''),'|',' ')||'|'||replace(coalesce(ZSTATE,''),'|',' ')||'|'||replace(coalesce(ZZIPCODE,''),'|',' ')||'|'||replace(coalesce(ZCOUNTRYNAME,''),'|',' ') from ZABCDPOSTALADDRESS \
                union all select 'B|'||Z_PK||'|'||strftime('%m-%d',ZBIRTHDAY+978307200,'unixepoch')||'|'||coalesce(ZBIRTHDAYYEAR,'') from ZABCDRECORD where ZBIRTHDAY is not null";
    const SCRIPT: &str = r#"q=@SQL@; q2=@SQL2@; find "$HOME/Library/Application Support/AddressBook" -name AddressBook-v22.abcddb | { i=0; while read -r db; do i=$((i+1)); dir=$(dirname "$db"); sqlite3 -readonly "$db" "$q" 2>/dev/null </dev/null | sed "s/^\([KPXN]\)|/\1|$i:/" | while IFS='|' read -r t o v; do if [ "$t" = X ]; then f="$dir/.AddressBook-v22_SUPPORT/_EXTERNAL_DATA/$v"; tmp=$(mktemp); if sips -s format jpeg -Z 160 "$f" --out "$tmp" >/dev/null 2>&1 </dev/null; then printf 'P|%s|01%s\n' "$o" "$(xxd -p "$tmp" | tr -d '\n')"; fi; rm -f "$tmp"; else printf '%s|%s|%s\n' "$t" "$o" "$v"; fi; done; sqlite3 -readonly "$db" "$q2" 2>/dev/null </dev/null | sed "s/^\([AB]\)|/\1|$i:/"; done; }"#;
    let out = exec(session, SCRIPT.replace("@SQL@", &sh_quote(sql)).replace("@SQL2@", &sh_quote(sql2))).await?;
    let mut text = String::from_utf8_lossy(&out).into_owned();
    // which numbers and emails Messages has iMessage conversations with
    let q = "select 'M|'||id from handle where service='iMessage'";
    if let Some(m) = exec(session, format!("sqlite3 -readonly ~/Library/Messages/chat.db {}", sh_quote(q))).await {
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(&m));
    }
    Some(parse_contacts(&text))
}

/// `P|<db>:<contact>|<hex photo>` and `K|<db>:<contact>|<number or email>` lines -> lookup table.
/// Photos are stored as a type byte (1 = an ordinary image file) followed by the image.
pub fn parse_contacts(out: &str) -> Contacts {
    let mut c = Contacts::default();
    let mut owner_photo: HashMap<&str, usize> = HashMap::new();
    let mut owner_person: HashMap<&str, usize> = HashMap::new();
    for l in out.lines() {
        let mut f = l.splitn(3, '|');
        if let (Some("N"), Some(owner), Some(v)) = (f.next(), f.next(), f.next()) {
            let (n, org) = v.split_once('|').unwrap_or((v, ""));
            let name = if n.trim().is_empty() { org.trim() } else { n.trim() };
            if !name.is_empty() {
                owner_person.insert(owner, c.people.len());
                let org = if n.trim().is_empty() { "" } else { org.trim() };
                c.people.push(Person { name: name.to_string(), org: org.to_string(), ..Default::default() });
            }
            continue;
        }
        let mut f = l.splitn(3, '|');
        if let (Some("P"), Some(owner), Some(h)) = (f.next(), f.next(), f.next())
            && let Some(b) = hex(h)
            && b.len() > 4
            && b[0] == 1
        {
            owner_photo.insert(owner, c.photos.len());
            c.photos.push(b[1..].to_vec());
        }
    }
    for l in out.lines() {
        let mut f = l.splitn(3, '|');
        match (f.next(), f.next(), f.next()) {
            (Some("A"), Some(owner), Some(v)) => {
                if let Some(i) = owner_person.get(owner) {
                    let mut p = v.split('|').map(str::trim);
                    let label = clean_label(p.next().unwrap_or(""));
                    let (street, city, state, zip, country) = (p.next().unwrap_or(""), p.next().unwrap_or(""), p.next().unwrap_or(""), p.next().unwrap_or(""), p.next().unwrap_or(""));
                    let region = [state, zip].iter().filter(|x| !x.is_empty()).copied().collect::<Vec<_>>().join(" ");
                    let town = [city, region.as_str()].iter().filter(|x| !x.is_empty()).copied().collect::<Vec<_>>().join(", ");
                    let lines: Vec<String> = [street, town.as_str(), country].iter().filter(|x| !x.is_empty()).map(|x| x.to_string()).collect();
                    if !lines.is_empty() {
                        c.people[*i].places.push((label, lines));
                    }
                }
                continue;
            }
            (Some("B"), Some(owner), Some(v)) => {
                if let (Some(i), Some((md, year))) = (owner_person.get(owner), v.split_once('|'))
                    && let Some((m, d)) = md.split_once('-')
                    && let (Ok(m), Ok(d)) = (m.parse::<usize>(), d.parse::<u32>())
                    && (1..=12).contains(&m)
                {
                    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
                    let year = year.trim().parse::<u32>().ok().filter(|y| *y > 1900);
                    c.people[*i].birthday = Some(match year {
                        Some(y) => format!("{} {d}, {y}", MONTHS[m - 1]),
                        None => format!("{} {d}", MONTHS[m - 1]),
                    });
                }
                continue;
            }
            (Some("M"), _, _) => {
                if let Some(k) = l.strip_prefix("M|").and_then(addr_key) {
                    c.imessage.insert(k);
                }
                continue;
            }
            _ => {}
        }
        let mut f = l.splitn(3, '|');
        if let (Some("K"), Some(owner), Some(v)) = (f.next(), f.next(), f.next()) {
            let (raw, label) = v.rsplit_once('|').unwrap_or((v, ""));
            let label_raw = clean_label(label);
            let label = label_raw.clone();
            if let (Some(k), false) = (addr_key(raw), label.is_empty()) {
                c.labels.entry(k).or_insert(label);
            }
            if let (Some(i), Some(k)) = (owner_photo.get(owner), addr_key(raw)) {
                c.index.entry(k).or_insert(*i);
            }
            if let Some(i) = owner_person.get(owner) {
                c.people[*i].addrs.push((raw.trim().to_string(), label_raw.to_string()));
                if let Some(k) = addr_key(raw) {
                    c.by_addr.entry(k).or_insert(*i);
                }
            }
        }
    }
    c
}

/// Contacts stores the standard labels as `_$!<Mobile>!$_`; custom ones are plain text.
fn clean_label(l: &str) -> String {
    l.trim().trim_start_matches("_$!<").trim_end_matches(">!$_").trim().to_lowercase()
}

/// Copy a file from the Mac to `dest` (through a `.part` file, so a half copy is never mistaken for it).
/// A recording's length (`afinfo`, which reads audio and video alike) or a contact card's name, company and
/// phone number (a vCard is plain text). Spotlight has neither, and Quick Look can't draw a vCard.
async fn file_info(session: &client::Handle<Handler>, path: &str, contact: bool) -> Option<FileInfo> {
    let p = path.strip_prefix("~/").map(|p| format!("\"$HOME\"/{}", sh_quote(p))).unwrap_or_else(|| sh_quote(path));
    let script = if contact {
        format!(
            r#"f={p}; for k in FN ORG TEL; do v=$(tr -d '\r' < "$f" | sed -n "s/^$k[^:]*://p" | head -n 1 | tr ';' ' '); printf '%s\n' "$v"; done"#
        )
    } else {
        format!(r#"afinfo {p} 2>/dev/null | sed -n 's/^estimated duration: *\([0-9.]*\) sec.*/\1/p' | head -n 1"#)
    };
    let out = exec(session, script).await?;
    let text = String::from_utf8_lossy(&out);
    let mut lines = text.lines().map(|l| l.trim().to_string());
    if contact {
        let (name, org, phone) = (lines.next()?, lines.next().unwrap_or_default(), lines.next().unwrap_or_default());
        (!name.is_empty() || !org.is_empty()).then(|| FileInfo { name, org, phone, ..Default::default() })
    } else {
        let d: f64 = lines.next()?.parse().ok()?;
        Some(FileInfo { duration: Some(d), ..Default::default() })
    }
}

/// Copied files (chat attachments opened here, videos played here) older than this much total are deleted, oldest first.
const FILES_CAP: u64 = 2 * 1024 * 1024 * 1024;

async fn download(session: &client::Handle<Handler>, path: &str, dest: &Path, size: i64, progress: impl Fn(usize)) -> Result<(), String> {
    use std::io::Write;
    let _permit = CHANNELS.acquire().await.map_err(|_| "busy".to_string())?;
    let remote = path.strip_prefix("~/").map(|p| format!("\"$HOME\"/{}", sh_quote(p))).unwrap_or_else(|| sh_quote(path));
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let part = dest.with_file_name(format!("{}.part", dest.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut ch = session.channel_open_session().await.map_err(|e| e.to_string())?;
    ch.exec(true, format!("cat {remote}")).await.map_err(|e| e.to_string())?;
    let mut got = 0usize;
    let mut told = std::time::Instant::now();
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::Data { data } => {
                f.write_all(&data).map_err(|e| e.to_string())?;
                got += data.len();
                if told.elapsed() >= Duration::from_millis(100) {
                    told = std::time::Instant::now();
                    progress(got);
                }
            }
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    drop(f);
    if got == 0 {
        let _ = std::fs::remove_file(&part);
        return Err("the file isn't on the Mac (or is empty)".into());
    }
    // a connection cut halfway looks like a short file; chat.db's size says so (with a little room for it being off)
    if size > 0 && (got as i64) < size * 98 / 100 {
        let _ = std::fs::remove_file(&part);
        return Err("the copy was cut short".into());
    }
    std::fs::rename(&part, dest).map_err(|e| e.to_string())?;
    if let Some(root) = dest.parent().and_then(Path::parent) {
        prune_files(root, dest);
    }
    Ok(())
}

/// Keep the folder of copied files under `FILES_CAP`: each file sits in its own subfolder, oldest go first, never `keep`.
fn prune_files(root: &Path, keep: &Path) {
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            if let Ok(md) = f.metadata()
                && md.is_file()
            {
                files.push((md.modified().unwrap_or(std::time::UNIX_EPOCH), md.len(), f.path()));
            }
        }
    }
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, p) in files {
        if total <= FILES_CAP {
            break;
        }
        if p != keep && std::fs::remove_file(&p).is_ok() {
            total -= len;
            if let Some(d) = p.parent() {
                let _ = std::fs::remove_dir(d);
            }
        }
    }
}

/// `http(s)://` words in some message text, minus the punctuation a sentence puts after a link.
pub fn urls_in(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|w| Some(w[scheme_at(w)?..].trim_end_matches(['.', ',', ')', '>', '"', '\'', '!', '?', ';']).to_string()))
        .collect()
}

/// Where `http://` or `https://` starts in a word (also after other characters, as in "see:https://...").
pub fn scheme_at(w: &str) -> Option<usize> {
    match (w.find("https://"), w.find("http://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The link of a message that has exactly one, and ends with it: Messages shows such a link as its own
/// little card under the text. With several links, or one in the middle, the text stays as it is.
pub fn single_link(text: &str) -> Option<String> {
    let urls = urls_in(text);
    let [u] = &urls[..] else { return None };
    text.trim_end().trim_end_matches(['.', ',', ')', '>', '"', '\'', '!', '?', ';']).ends_with(u.as_str()).then(|| u.clone())
}

/// `text` without its trailing `url` (and what follows it).
pub fn strip_url(text: &str, url: &str) -> String {
    text.rfind(url).map_or(text, |p| &text[..p]).trim().to_string()
}

/// The message text out of an `attributedBody` blob (a typedstream: the string follows "NSString",
/// a `+`, then its length), for newer messages that keep no plain `text`.
// ponytail: handles the 1-byte and 2-byte length forms; longer messages give None (their links are skipped)
fn text_from_blob(b: &[u8]) -> Option<String> {
    let at = b.windows(8).position(|w| w == b"NSString")? + 8;
    let plus = b.get(at..at + 8)?.iter().position(|c| *c == 0x2b)? + at + 1;
    let (len, start) = match *b.get(plus)? {
        n if n < 0x80 => (usize::from(n), plus + 1),
        0x81 => (usize::from(u16::from_le_bytes([*b.get(plus + 1)?, *b.get(plus + 2)?])), plus + 3),
        _ => return None,
    };
    Some(String::from_utf8_lossy(b.get(start..start + len)?).into_owned())
}

fn hex_text(s: &str) -> String {
    hex(s).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

/// A page of a chat's attachments (`Photos` = images, `Files` = the rest) or of the links in its
/// message text, newest first, straight from chat.db: imsg can't list these. `offset` counts rows
/// (messages, for links). Returns the items, where the next page starts and whether there is one.
// ponytail: links come from `message.text` only; newer macOS keeps some text only in `attributedBody`
async fn chat_media(session: &client::Handle<Handler>, chat_id: i64, kind: Kind, offset: usize) -> Option<(Vec<MediaItem>, usize, bool)> {
    let join = "join chat_message_join c on c.message_id=m.ROWID";
    let page = if kind == Kind::Photos { PHOTO_PAGE } else { MEDIA_PAGE };
    let sql = match kind {
        Kind::Links => format!(
            "select m.ROWID, m.date/1000000000+978307200, hex(coalesce(m.text,'')), coalesce(m.balloon_bundle_id,''), \
             case when coalesce(m.text,'')='' then hex(coalesce(m.attributedBody,'')) else '' end \
             from message m {join} where c.chat_id={chat_id} and (m.text like '%http%' or hex(coalesce(m.attributedBody,'')) like '%68747470%') \
             order by m.date desc limit {} offset {offset}",
            page + 1
        ),
        _ => {
            let cond = if kind == Kind::Photos {
                "a.mime_type like 'image/%'"
            } else {
                "coalesce(a.mime_type,'') not like 'image/%' and a.filename not like '%.pluginPayloadAttachment'"
            };
            format!(
                "select m.date/1000000000+978307200, coalesce(a.total_bytes,0), hex(a.filename), hex(coalesce(a.transfer_name,'')) \
                 from attachment a join message_attachment_join j on j.attachment_id=a.ROWID join message m on m.ROWID=j.message_id {join} \
                 where c.chat_id={chat_id} and a.filename is not null and {cond} order by m.date desc, a.ROWID desc limit {} offset {offset}",
                page + 1
            )
        }
    };
    let out = exec(session, format!("sqlite3 -readonly ~/Library/Messages/chat.db {}", sh_quote(&sql))).await;
    let text = out.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
    let rows: Vec<Vec<&str>> = text.lines().map(|l| l.split('|').collect()).collect();
    let more = rows.len() > page;
    let mut items = vec![];
    for r in rows.iter().take(page) {
        if kind == Kind::Links {
            let row = r.first().and_then(|d| d.parse().ok()).unwrap_or(0);
            let date = r.get(1).and_then(|d| d.parse().ok()).unwrap_or(0);
            let mut text = hex_text(r.get(2).copied().unwrap_or(""));
            if text.is_empty() {
                text = hex(r.get(4).copied().unwrap_or("")).and_then(|b| text_from_blob(&b)).unwrap_or_default();
            }
            if r.get(3) == Some(&"com.apple.messages.URLBalloonProvider") {
                items.push(MediaItem { url: urls_in(&text).first().cloned().unwrap_or_default(), date, row, preview: true, ..Default::default() });
            } else if let Some(url) = single_link(&text) {
                // like the chat itself: a message with exactly one link, ending with it; others are just text
                items.push(MediaItem { url, date, row, ..Default::default() });
            }
            continue;
        }
        let date = r.first().and_then(|d| d.parse().ok()).unwrap_or(0);
        if let (Some(size), Some(path), Some(name)) = (r.get(1), r.get(2), r.get(3)) {
            let path = hex_text(path);
            let name = hex_text(name);
            let name = if name.is_empty() { path.rsplit('/').next().unwrap_or("").to_string() } else { name };
            items.push(MediaItem { path, name, size: size.parse().unwrap_or(0), date, ..Default::default() });
        }
    }
    if kind == Kind::Links {
        // a link with a preview card shouldn't also be listed as plain text
        let cards: std::collections::HashSet<String> = items.iter().filter(|i| i.preview).map(|i| i.url.clone()).collect();
        items.retain(|i| i.preview || !cards.contains(&i.url));
    } else if !items.is_empty() {
        // which of them are actually on the Mac
        let list = items.iter().map(|i| sh_quote(&i.path)).collect::<Vec<_>>().join(" ");
        let script = format!(r#"for p in {list}; do case "$p" in "~/"*) p="$HOME/${{p#\~/}}";; esac; if [ -e "$p" ]; then echo 1; else echo 0; fi; done"#);
        if let Some(out) = exec(session, script).await {
            for (it, here) in items.iter_mut().zip(String::from_utf8_lossy(&out).lines()) {
                it.missing = here.trim() == "0";
            }
        }
    }
    // a query that failed (empty output, e.g. no Full Disk Access) looks like "no items": fine for a panel
    Some((items, offset + rows.len().min(page), more))
}

/// OpenStreetMap tiles around an address, as a `cols` x `rows` grid (row by row), and where the
/// address sits on the grid (pixels, 256 per tile).
#[derive(Clone)]
pub struct MapData {
    pub lat: f64,
    pub lon: f64,
    pub cols: usize,
    pub rows: usize,
    pub pin_x: f64,
    pub pin_y: f64,
    pub tiles: Vec<Vec<u8>>,
}

pub const TILE_ZOOM: u32 = 17;
/// The preview shows this many tile pixels around the address (so the grid always covers it).
pub const MAP_WIN: f64 = 192.0;

/// Which tile holds this point, and the point's pixel position inside it.
pub fn tile_pos(lat: f64, lon: f64, z: u32) -> (u32, u32, f64, f64) {
    let n = f64::from(1u32 << z);
    let x = (lon + 180.0) / 360.0 * n;
    let lr = lat.to_radians();
    let y = (1.0 - (lr.tan() + 1.0 / lr.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x.floor() as u32, y.floor() as u32, x.fract() * 256.0, y.fract() * 256.0)
}

/// Tile offsets (first, last) along one axis that cover `MAP_WIN` pixels centred on pixel `p` of the
/// address's own tile: -1..=0, 0..=0, 0..=1 or -1..=1 is impossible since the window is under 256.
fn tile_span(p: f64) -> (i32, i32) {
    (((p - MAP_WIN / 2.0) / 256.0).floor() as i32, ((p + MAP_WIN / 2.0) / 256.0).floor() as i32)
}

pub fn url_encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Nominatim's public server allows one request a second.
static NOMINATIM_LAST: tokio::sync::Mutex<Option<std::time::Instant>> = tokio::sync::Mutex::const_new(None);

/// Find an address on OpenStreetMap (Nominatim) and fetch the map tile around it. Both go out from the
/// Mac with its curl, so the app needs no HTTP library. Fails soft (None).
// ponytail: one tile, so the pin can sit near its edge; stitch neighbours if that looks bad
async fn map_for(session: &client::Handle<Handler>, query: &str) -> Option<MapData> {
    let ua = format!("BubbleFairy/{}", env!("CARGO_PKG_VERSION"));
    let curl = |url: String| exec(session, format!("curl -sSf --max-time 15 -A {} {}", sh_quote(&ua), sh_quote(&url)));
    // "305-1961 Durnin Road" (unit-street) often finds nothing; then try it without the unit
    let mut tries = vec![query.to_string()];
    if let Some((street, rest)) = query.split_once(", ")
        && let Some((unit, no_unit)) = street.split_once('-')
        && [unit, no_unit].iter().all(|p| p.trim().chars().next().is_some_and(|c| c.is_ascii_digit()))
    {
        tries.push(format!("{}, {rest}", no_unit.trim()));
    }
    let mut spot = None;
    {
        let mut last = NOMINATIM_LAST.lock().await;
        for q in &tries {
            if let Some(t) = *last {
                tokio::time::sleep(Duration::from_millis(1100).saturating_sub(t.elapsed())).await;
            }
            let out = curl(format!("https://nominatim.openstreetmap.org/search?format=json&limit=1&q={}", url_encode(q))).await;
            *last = Some(std::time::Instant::now());
            let found = out.and_then(|o| serde_json::from_slice::<Value>(&o).ok()).and_then(|v| {
                let first = v.get(0)?;
                Some((first["lat"].as_str()?.parse::<f64>().ok()?, first["lon"].as_str()?.parse::<f64>().ok()?))
            });
            if found.is_some() {
                spot = found;
                break;
            }
        }
    }
    let Some((lat, lon)) = spot else {
        eprintln!("map: address not found (or the Mac's curl got no answer)"); // not the address itself: it's private
        return None;
    };
    let (x, y, px, py) = tile_pos(lat, lon, TILE_ZOOM);
    let ((c0, c1), (r0, r1)) = (tile_span(px), tile_span(py));
    let mut tiles = vec![];
    for r in r0..=r1 {
        for c in c0..=c1 {
            let url = format!("https://tile.openstreetmap.org/{TILE_ZOOM}/{}/{}.png", i64::from(x) + i64::from(c), i64::from(y) + i64::from(r));
            match curl(url).await {
                Some(png) if png.starts_with(&[0x89, b'P', b'N', b'G']) => tiles.push(png),
                _ => {
                    eprintln!("map: tile fetch failed");
                    return None;
                }
            }
        }
    }
    Some(MapData {
        lat,
        lon,
        cols: (c1 - c0 + 1) as usize,
        rows: (r1 - r0 + 1) as usize,
        pin_x: px - f64::from(c0) * 256.0,
        pin_y: py - f64::from(r0) * 256.0,
        tiles,
    })
}

/// What Messages saved about a shared link. `image` / `icon` are payload files on the Mac.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkPreview {
    pub url: String,
    /// the other saved address, when it differs (the message text usually holds this one)
    pub alt_url: Option<String>,
    pub title: String,
    pub site: String,
    pub image: Option<String>,
    /// the picture's size in pixels; Messages shows small ones (a site logo) as an icon, not a preview
    pub image_size: Option<(i32, i32)>,
    pub icon: Option<String>,
    pub icon_size: Option<(i32, i32)>,
}

/// A picture big enough to show as a preview above the title (rather than a logo or icon beside it).
pub fn is_preview_size(w: i32, h: i32) -> bool {
    w >= 400 && h >= 200
}

/// `sips -g pixelWidth -g pixelHeight` output -> (width, height)
fn parse_size(out: &str) -> Option<(i32, i32)> {
    let get = |k: &str| out.lines().find_map(|l| l.trim().strip_prefix(k)?.trim().parse::<i32>().ok());
    Some((get("pixelWidth:")?, get("pixelHeight:")?))
}

/// The preview row's `payload_data` (an NSKeyedArchiver plist) plus its payload files, in the order
/// the archive's image indexes refer to.
async fn link_preview(session: &client::Handle<Handler>, rowid: i64) -> Option<LinkPreview> {
    let sql = format!(
        "select hex(payload_data) from message where ROWID={rowid}; \
         select a.filename from attachment a join message_attachment_join j on j.attachment_id=a.ROWID \
         where j.message_id={rowid} order by j.ROWID;"
    );
    let out = exec(session, format!("sqlite3 -readonly ~/Library/Messages/chat.db {}", sh_quote(&sql))).await?;
    let text = String::from_utf8_lossy(&out);
    let mut lines = text.lines();
    let bytes = hex(lines.next()?)?;
    let files: Vec<String> = lines.map(String::from).collect();
    let mut p = parse_link(&bytes, &files)?;
    let measure = |f: Option<String>| async move {
        let img = f?;
        let path = img.strip_prefix("~/").map(|r| format!("\"$HOME\"/{}", sh_quote(r))).unwrap_or_else(|| sh_quote(&img));
        let out = exec(session, format!("sips -g pixelWidth -g pixelHeight {path}")).await;
        out.and_then(|o| parse_size(&String::from_utf8_lossy(&o)))
    };
    p.image_size = measure(p.image.clone()).await;
    p.icon_size = measure(p.icon.clone()).await;
    Some(p)
}

fn hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

pub fn parse_link(bytes: &[u8], files: &[String]) -> Option<LinkPreview> {
    use plist::Value;
    let v = Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
    let objs = v.as_dictionary()?.get("$objects")?.as_array()?;
    // archive references are UIDs (binary) or {CF$UID: n} dictionaries (XML)
    let get = |v: &Value| -> Option<Value> {
        let i = match v {
            Value::Uid(u) => u.get(),
            Value::Dictionary(d) if d.len() == 1 => d.get("CF$UID")?.as_unsigned_integer()?,
            other => return Some(other.clone()),
        };
        objs.get(i as usize).cloned()
    };
    let meta = objs.iter().find_map(|o| o.as_dictionary()?.get("richLinkMetadata").and_then(get))?;
    let meta = meta.as_dictionary()?;
    let text = |k: &str| -> Option<String> {
        match get(meta.get(k)?)? {
            Value::String(s) => Some(s),
            Value::Dictionary(d) => get(d.get("NS.relative")?)?.as_string().map(String::from), // NSURL
            _ => None,
        }
    };
    let file = |k: &str| -> Option<String> {
        let i = get(meta.get(k)?)?.as_dictionary()?.get("richLinkImageAttachmentSubstituteIndex")?.as_unsigned_integer()?;
        files.get(i as usize).cloned()
    };
    let (typed, resolved) = (text("originalURL"), text("URL"));
    let url = typed.clone().or_else(|| resolved.clone())?;
    let alt_url = resolved.filter(|r| *r != url).or(typed.filter(|t| *t != url));
    let site = text("siteName").unwrap_or_else(|| url.split('/').nth(2).unwrap_or("").trim_start_matches("www.").to_string());
    Some(LinkPreview { title: text("title").unwrap_or_default(), site, image: file("image"), image_size: None, icon: file("icon"), icon_size: None, url, alt_url })
}

/// One `send` call: `target` plus `key` = `value` (a text or a file path on the Mac).
async fn send_one(rpc: &Rpc, ev: &mpsc::UnboundedSender<Event>, target: &Value, key: &str, value: &str) -> Result<(), String> {
    let mut params = target.clone();
    params[key] = Value::from(value);
    // The first send ever waits on the Mac's Automation prompt, so allow a long time and say so.
    let call = rpc.call("send", params, Duration::from_secs(180));
    tokio::pin!(call);
    tokio::select! {
        r = &mut call => r,
        _ = tokio::time::sleep(Duration::from_secs(6)) => {
            let _ = ev.send(Event::SendSlow);
            call.await
        }
    }
    .map(|_| ())
}

/// Copy a local file to the Mac for sending; returns its path there. Copies older than a day are
/// cleared out first (Messages keeps its own copy of what it sent).
async fn upload(session: &client::Handle<Handler>, file: &Path) -> Result<String, String> {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "attachment".into());
    let owned = file.to_path_buf();
    let bytes = tokio::task::spawn_blocking(move || std::fs::read(owned))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
        .map_err(|e| format!("can't read {name}: {e}"))?;
    let fail = || format!("couldn't copy {name} to the Mac");
    let cmd = format!(
        "o=\"$HOME/Library/Application Support/BubbleFairy/outgoing\"; \
         find \"$o\" -mindepth 1 -maxdepth 1 -mtime +1 -exec rm -rf {{}} + 2>/dev/null; \
         d=$(mkdir -p \"$o\" && mktemp -d \"$o/XXXXXX\") && cat > \"$d\"/{q} && printf %s \"$d\"/{q}",
        q = sh_quote(&name)
    );
    let _permit = CHANNELS.acquire().await.map_err(|_| fail())?;
    let mut ch = session.channel_open_session().await.map_err(|_| fail())?;
    ch.exec(true, cmd).await.map_err(|_| fail())?;
    ch.data(&bytes[..]).await.map_err(|_| fail())?;
    ch.eof().await.map_err(|_| fail())?;
    let mut out = Vec::new();
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    let path = String::from_utf8_lossy(&out).into_owned();
    if path.is_empty() { Err(fail()) } else { Ok(path) }
}

/// imsg counts unread from each message's `is_read` flag, which can stay unset after the chat was
/// read on another device. Messages itself also honours the chat's `last_read_message_timestamp`,
/// so count only messages that are unread by both.
async fn fix_unread(session: &client::Handle<Handler>, chats: &mut [Value]) {
    let ids: Vec<String> = chats.iter().filter_map(|c| c["id"].as_i64()).map(|i| i.to_string()).collect();
    if ids.is_empty() {
        return;
    }
    let sql = format!(
        "select c.ROWID||'|'||(select count(*) from chat_message_join j join message m on m.ROWID=j.message_id \
         where j.chat_id=c.ROWID and m.is_from_me=0 and m.is_read=0 and m.date > c.last_read_message_timestamp) \
         from chat c where c.ROWID in ({})",
        ids.join(",")
    );
    let Some(out) = exec(session, format!("sqlite3 -readonly ~/Library/Messages/chat.db {}", sh_quote(&sql))).await else { return };
    for (id, n) in String::from_utf8_lossy(&out).lines().filter_map(|l| l.split_once('|')) {
        let (Ok(id), Ok(n)) = (id.parse::<i64>(), n.parse::<i64>()) else { continue };
        if let Some(c) = chats.iter_mut().find(|c| c["id"].as_i64() == Some(id)) {
            c["unread_count"] = Value::from(n);
        }
    }
}

/// Longest side of the copy shown in the chat, and of the one fetched for the viewer.
pub const CHAT_PX: u32 = 1200;
pub const FULL_PX: u32 = 4096;

const READ: Duration = Duration::from_secs(30);

async fn handle(cmd: Cmd, rpc: Rpc, session: Arc<client::Handle<Handler>>, ev: mpsc::UnboundedSender<Event>) {
    match cmd {
        Cmd::Chats => match rpc.call("chats.list", json!({"limit": 100}), READ).await {
            Ok(r) => {
                let mut chats = r.get("chats").and_then(Value::as_array).cloned().unwrap_or_default();
                fix_unread(&session, &mut chats).await;
                let _ = ev.send(Event::Chats(chats));
            }
            Err(e) => {
                let _ = ev.send(Event::Warn(e));
            }
        },
        Cmd::FindChat(addr) => {
            let found = find_chat(&session, &addr).await;
            let _ = ev.send(Event::FoundChat(addr.first().cloned().unwrap_or_default(), found));
        }
        Cmd::History(id, limit) => {
            let r = rpc.call("messages.history", json!({"chat_id": id, "limit": limit, "attachments": true}), READ).await;
            if let Ok(r) = r {
                let mut msgs = r.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
                msgs.reverse(); // imsg returns newest first
                add_services(&session, &mut msgs).await;
                let _ = ev.send(Event::History(id, msgs));
            }
        }
        Cmd::Older { id, before } => {
            let q = json!({"chat_id": id, "limit": 100, "end": before, "attachments": true});
            let mut msgs = match rpc.call("messages.history", q, READ).await {
                Ok(r) => r.get("messages").and_then(Value::as_array).cloned().unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            msgs.reverse();
            add_services(&session, &mut msgs).await;
            let _ = ev.send(Event::Older(id, msgs));
        }
        Cmd::Recent(id) => {
            let r = rpc.call("messages.history", json!({"chat_id": id, "limit": 10, "attachments": true}), READ).await;
            if let Ok(r) = r {
                let mut msgs = r.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
                msgs.reverse();
                add_services(&session, &mut msgs).await;
                let _ = ev.send(Event::Recent(id, msgs));
            }
        }
        Cmd::Preview(id) => {
            let r = rpc.call("messages.history", json!({"chat_id": id, "limit": 1, "attachments": true}), READ).await;
            if let Some(mut m) = r.ok().and_then(|r| r.pointer("/messages/0").cloned()) {
                add_services(&session, std::slice::from_mut(&mut m)).await;
                let _ = ev.send(Event::Preview(id, m));
            }
        }
        Cmd::Send { chat_id, to, text, files, token } => {
            // chat.db keeps a 1:1 chat's service from whenever it was set (an iMessage chat with some
            // SMS history can stay "SMS"), and a chat_id send uses it. A direct send with "auto" lets
            // Messages choose, as it does on the Mac.
            let target = match &to {
                Some(to) => json!({"to": to, "service": "auto"}),
                None => json!({"chat_id": chat_id}),
            };
            let mut sent = 0; // files that went out
            let mut err = None;
            for f in &files {
                let r = match upload(&session, f).await {
                    Ok(path) => send_one(&rpc, &ev, &target, "file", &path).await,
                    Err(e) => Err(e),
                };
                match r {
                    Ok(()) => sent += 1,
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            let mut text_left = text;
            if err.is_none() && !text_left.is_empty() {
                match send_one(&rpc, &ev, &target, "text", &text_left).await {
                    Ok(()) => text_left.clear(),
                    Err(e) => err = Some(e),
                }
            }
            let files_left = if err.is_some() { files[sent..].to_vec() } else { Vec::new() };
            let _ = ev.send(Event::Sent { text: if err.is_some() { text_left } else { String::new() }, files: files_left, err, token });
        }
        Cmd::Contacts => {
            let _ = ev.send(Event::Contacts(fetch_contacts(&session).await.unwrap_or_default()));
        }
        Cmd::Link(rowid) => {
            let _ = ev.send(Event::Link(rowid, link_preview(&session, rowid).await));
        }
        Cmd::FileInfo { path, contact } => {
            let info = file_info(&session, &path, contact).await;
            let _ = ev.send(Event::FileInfo(path, info));
        }
        Cmd::Download { path, dest, size } => {
            let (ev2, p2) = (ev.clone(), path.clone());
            let progress = move |got: usize| {
                let _ = ev2.send(Event::DownloadProgress { path: p2.clone(), got, total: size });
            };
            let err = download(&session, &path, &dest, size, progress).await.err();
            let _ = ev.send(Event::Downloaded { path, dest, err });
        }
        Cmd::Bulk(path) => {
            let _lane = BULK.acquire().await;
            let bytes = fetch_image(&session, &path, CHAT_PX).await;
            let _ = ev.send(Event::Image { path, full: false, bytes });
        }
        Cmd::Image { path, full } => {
            let bytes = fetch_image(&session, &path, if full { FULL_PX } else { CHAT_PX }).await;
            let _ = ev.send(Event::Image { path, full, bytes });
        }
        Cmd::Media { chat_id, kind, offset } => {
            let (items, next, more) = chat_media(&session, chat_id, kind, offset).await.unwrap_or_default();
            let _ = ev.send(Event::Media { chat_id, kind, offset, items, next, more });
        }
        Cmd::Map(query) => {
            let data = map_for(&session, &query).await;
            let _ = ev.send(Event::Map(query, data));
        }
        Cmd::Configure(_) => {}
    }
}

/// The backend loop: connect, serve commands, reconnect every few seconds when the link drops.
pub async fn run(mut cfg: Config, paths: Paths, mut cmds: mpsc::UnboundedReceiver<Cmd>, ev: mpsc::UnboundedSender<Event>) {
    loop {
        if cfg.host.is_empty() || cfg.user.is_empty() {
            let _ = ev.send(Event::Offline("set up your Mac in Settings".into()));
            match wait_offline(&mut cmds, &ev, None).await {
                Some(c) => cfg = c,
                None => return,
            }
            continue;
        }
        let _ = ev.send(Event::Connecting);
        let mut conn = match connect(&cfg, &paths, &ev).await {
            Ok(c) => c,
            Err(e) => {
                let _ = ev.send(Event::Offline(e));
                match wait_offline(&mut cmds, &ev, Some(Duration::from_secs(5))).await {
                    Some(c) => cfg = c,
                    None if cmds.is_closed() => return,
                    None => {}
                }
                continue;
            }
        };

        let status = conn.rpc.call("status", json!({}), READ).await;
        match status {
            Ok(s) if s.pointer("/database/ready").and_then(Value::as_bool) == Some(false) => {
                let _ = ev.send(Event::Warn(
                    "your Mac won't let BubbleFairy read Messages. On the Mac, open System Settings → General → Sharing, \
                     click ⓘ next to Remote Login and turn on \"Allow full disk access for remote users\"."
                        .into(),
                ));
            }
            Ok(_) => {}
            Err(e) => {
                let _ = ev.send(Event::Offline(format!("imsg didn't start: {e}")));
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        }
        let sub = conn.rpc.call("watch.subscribe", json!({"attachments": true}), READ).await;
        if let Err(e) = sub {
            let _ = ev.send(Event::Warn(format!("live updates unavailable: {e}")));
        }
        let _ = ev.send(Event::Connected);

        loop {
            tokio::select! {
                _ = &mut conn.dead => break,
                c = cmds.recv() => match c {
                    None => return,
                    Some(Cmd::Configure(c)) if c != cfg => { cfg = c; break; }
                    Some(Cmd::Configure(_)) => {}
                    Some(c) => { tokio::spawn(handle(c, conn.rpc.clone(), conn.session.clone(), ev.clone())); }
                },
            }
        }
        let _ = conn.session.disconnect(russh::Disconnect::ByApplication, "", "en").await;
        let _ = ev.send(Event::Offline("lost the connection to the Mac, retrying".into()));
    }
}

/// While offline: fail sends right away, drop reads (the UI reloads on Connected), wake on new settings.
async fn wait_offline(cmds: &mut mpsc::UnboundedReceiver<Cmd>, ev: &mpsc::UnboundedSender<Event>, wait: Option<Duration>) -> Option<Config> {
    let sleep = tokio::time::sleep(wait.unwrap_or(Duration::MAX / 4));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return None,
            c = cmds.recv() => match c? {
                Cmd::Configure(c) => return Some(c),
                Cmd::Send { text, files, token, .. } => {
                    let _ = ev.send(Event::Sent { text, files, err: Some("not connected to the Mac".into()), token });
                }
                _ => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_shell_tricks() {
        assert_eq!(sh_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn dispatch_routes_replies_and_notifications() {
        let pending: Pending = Default::default();
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().insert(7, tx);
        let (ev, mut evs) = mpsc::unbounded_channel();
        let (live, mut lives) = mpsc::unbounded_channel();
        dispatch(json!({"jsonrpc":"2.0","id":7,"result":{"ok":true}}), &pending, &ev, &live);
        assert_eq!(rx.try_recv().unwrap().unwrap(), json!({"ok":true}));
        dispatch(json!({"jsonrpc":"2.0","method":"message","params":{"subscription":1,"message":{"id":5}}}), &pending, &ev, &live);
        assert_eq!(lives.try_recv().unwrap()["id"], 5);
        dispatch(json!({"jsonrpc":"2.0","method":"watch.overflow","params":{}}), &pending, &ev, &live);
        assert!(matches!(evs.try_recv(), Ok(Event::Resync)));
    }
}

#[cfg(test)]
mod link_tests {
    #[test]
    fn link_preview_archive() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>$objects</key><array>
<string>$null</string>
<dict><key>richLinkMetadata</key><dict><key>CF$UID</key><integer>2</integer></dict></dict>
<dict><key>URL</key><dict><key>CF$UID</key><integer>3</integer></dict>
      <key>title</key><dict><key>CF$UID</key><integer>5</integer></dict>
      <key>image</key><dict><key>CF$UID</key><integer>6</integer></dict></dict>
<dict><key>NS.relative</key><dict><key>CF$UID</key><integer>4</integer></dict></dict>
<string>https://www.example.com/a</string>
<string>Example</string>
<dict><key>richLinkImageAttachmentSubstituteIndex</key><integer>1</integer></dict>
</array></dict></plist>"#;
        let files = ["icon.png".to_string(), "big.jpg".to_string()];
        let p = super::parse_link(xml.as_bytes(), &files).unwrap();
        assert_eq!(p.url, "https://www.example.com/a");
        assert_eq!(p.title, "Example");
        assert_eq!(p.site, "example.com");
        assert_eq!(p.image.as_deref(), Some("big.jpg"));
        assert_eq!(p.icon, None);
        assert_eq!(super::hex("0aff"), Some(vec![10, 255]));
        assert_eq!(super::urls_in("see https://a.com/x, and (http://b.org/y). ok"), vec!["https://a.com/x", "http://b.org/y"]);
        assert_eq!(super::urls_in("look:https://a.com/x"), vec!["https://a.com/x"]);
        assert_eq!(super::single_link("pfft! brilliant. https://youtu.be/x?si=1"), Some("https://youtu.be/x?si=1".into()));
        assert_eq!(super::single_link("https://a.com/x and https://b.com/y"), None);
        assert_eq!(super::single_link("see https://a.com/x for more"), None);
        assert_eq!(super::strip_url("pfft! brilliant. https://youtu.be/x", "https://youtu.be/x"), "pfft! brilliant.");
        let mut blob = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x95\x84\x01+".to_vec();
        blob.extend_from_slice(b"\x0bhello https\x86\x84");
        assert_eq!(super::text_from_blob(&blob), Some("hello https".into()));
        // (-114.0631 + 180) / 360 * 2^16 = 12003.3
        let (x, _, px, py) = super::tile_pos(51.0442, -114.0631, 16);
        assert_eq!(x, 12003);
        assert!((0.0..256.0).contains(&px) && (0.0..256.0).contains(&py));
        // the 192 px window around a point always lies within the span of tiles asked for
        assert_eq!(super::tile_span(128.0), (0, 0));
        assert_eq!(super::tile_span(20.0), (-1, 0));
        assert_eq!(super::tile_span(240.0), (0, 1));
        assert_eq!(super::url_encode("1 Main St, A&B"), "1%20Main%20St%2C%20A%26B");
        let c = super::parse_contacts(
            "N|1:7|Sue Smith|\nN|1:9||Acme Co\nP|1:7|01FFD8FF00\nK|1:7|(403) 555-1234\nK|1:7|Sue@iCloud.com\nK|1:9|587 555 0000\n",
        );
        assert_eq!(c.search("sue"), vec![("Sue Smith", "(403) 555-1234"), ("Sue Smith", "Sue@iCloud.com")]);
        assert_eq!(c.search("587 55"), vec![("Acme Co", "587 555 0000")]);
        assert_eq!(c.search("acme").len(), 1);
        assert_eq!(c.photo("+14035551234").map(|p| p.len()), Some(4));
        let c3 = super::parse_contacts(
            "N|1:7|Sue Smith|Acme\nK|1:7|(403) 555-1234|_$!<Mobile>!$_\nA|1:7|_$!<Home>!$_|1 Main St|Calgary|AB|T2P 1A1|Canada\nB|1:7|11-15|1980\nM|+14035551234\n",
        );
        let p = c3.person("4035551234").unwrap();
        assert_eq!((p.org.as_str(), p.birthday.as_deref()), ("Acme", Some("Nov 15, 1980")));
        assert_eq!(p.places, vec![("home".to_string(), vec!["1 Main St".to_string(), "Calgary, AB T2P 1A1".into(), "Canada".into()])]);
        assert!(c3.has_imessage("(403) 555-1234") && !c3.has_imessage("5550001111"));
        let c2 = super::parse_contacts("K|1:7|(403) 555-1234|_$!<Mobile>!$_\nK|1:7|sue@x.com|Work\nK|1:7|555 000 1111|\n");
        assert_eq!(c2.label("+14035551234"), Some("mobile"));
        assert_eq!(c2.label("SUE@x.com"), Some("work"));
        assert_eq!(c2.label("5550001111"), None);
        let c = super::parse_contacts(
            "P|1:7|01FFD8FF00\nP|1:8|02424633AA\nK|1:7|(403) 555-1234\nK|1:7|Sue@iCloud.com\nK|1:8|403 555 9999\nK|2:7|+1 555 000 1111\n",
        );
        assert_eq!(c.photo("+14035551234"), Some(&[0xFF, 0xD8, 0xFF, 0x00][..]));
        assert_eq!(c.photo("sue@icloud.com"), c.photo("+14035551234"));
        assert_eq!(c.photo("+14035559999"), None); // that contact's photo isn't an image file
        assert_eq!(c.photo("562528"), None); // short code
        assert_eq!(c.photo("+15550001111"), None); // same contact number, different database
        assert_eq!(super::parse_size("/x/y.png\n  pixelWidth: 286\n  pixelHeight: 142\n"), Some((286, 142)));
        // sizes seen in Jo's chat: logos and icons are small, previews are big
        assert!(!super::is_preview_size(286, 142) && !super::is_preview_size(180, 180));
        assert!(super::is_preview_size(1024, 496) && super::is_preview_size(740, 923));
    }
}
