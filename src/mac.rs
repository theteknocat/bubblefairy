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
}

pub enum Cmd {
    Configure(Config),
    Chats,
    History(i64),
    /// Just the newest message, for the chat list's preview line.
    Preview(i64),
    /// Older messages, before the given ISO time (the oldest one loaded).
    Older { id: i64, before: String },
    /// `to`: the other person in a 1:1 chat, sent directly so Messages picks the service (see handle)
    /// `files` go first, one message each, then `text` (if any) as its own message
    /// `token` names this send, so the answer (`Event::Sent`) can be matched to its waiting bubbles
    Send { chat_id: i64, to: Option<String>, text: String, files: Vec<PathBuf>, token: u64 },
    /// `full`: the viewer wants the big version (up to 4096 px) instead of the 1200 px chat copy
    Image { path: String, full: bool },
    /// Apple's link preview stored on this message row
    Link(i64),
    /// The Mac's contact photos, to use as avatars
    Contacts,
}

pub enum Event {
    Connecting,
    Connected,
    Offline(String),
    /// Connected, but something on the Mac needs fixing (e.g. Full Disk Access).
    Warn(String),
    HostKey(String),
    Chats(Vec<Value>),
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
    Contacts(Contacts),
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
    if let Some(img) = &p.image {
        let path = img.strip_prefix("~/").map(|r| format!("\"$HOME\"/{}", sh_quote(r))).unwrap_or_else(|| sh_quote(img));
        let out = exec(session, format!("sips -g pixelWidth -g pixelHeight {path}")).await;
        p.image_size = out.and_then(|o| parse_size(&String::from_utf8_lossy(&o)));
    }
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
    Some(LinkPreview { title: text("title").unwrap_or_default(), site, image: file("image"), image_size: None, icon: file("icon"), url, alt_url })
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
        Cmd::History(id) => {
            let r = rpc.call("messages.history", json!({"chat_id": id, "limit": 150, "attachments": true}), READ).await;
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
        Cmd::Image { path, full } => {
            let bytes = fetch_image(&session, &path, if full { FULL_PX } else { CHAT_PX }).await;
            let _ = ev.send(Event::Image { path, full, bytes });
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
