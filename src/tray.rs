//! The tray icon (a StatusNotifierItem) and the login entry that starts the app hidden in it.
//! KDE Plasma shows it; GNOME needs the AppIndicator extension, and without a tray `start` fails.
use ksni::TrayMethods;
use tokio::sync::mpsc::UnboundedSender;

pub enum Event {
    Toggle,
    Show,
    Quit,
}

pub struct Tray {
    pub unread: bool,
    pub tx: UnboundedSender<Event>,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        crate::APP_ID.into()
    }
    fn title(&self) -> String {
        "BubbleFairy".into()
    }
    fn icon_name(&self) -> String {
        format!("{}-symbolic", crate::APP_ID)
    }
    // the same mark with a dot: shown while any chat is unread
    fn attention_icon_name(&self) -> String {
        format!("{}-unread-symbolic", crate::APP_ID)
    }
    fn status(&self) -> ksni::Status {
        if self.unread { ksni::Status::NeedsAttention } else { ksni::Status::Active }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Event::Toggle);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem { label: "Open BubbleFairy".into(), icon_name: self.icon_name(), activate: Box::new(|t: &mut Self| { let _ = t.tx.send(Event::Show); }), ..Default::default() }.into(),
            StandardItem { label: "Quit".into(), icon_name: "application-exit-symbolic".into(), activate: Box::new(|t: &mut Self| { let _ = t.tx.send(Event::Quit); }), ..Default::default() }.into(),
        ]
    }
}

/// Put the icon in the tray. Fails when the desktop has no tray (or nothing showing one).
pub async fn start(tx: UnboundedSender<Event>) -> Result<ksni::Handle<Tray>, ksni::Error> {
    // in a sandbox the app can't own the usual bus name, and doesn't need to
    Tray { unread: false, tx }.disable_dbus_name(ashpd::is_sandboxed()).spawn().await
}

/// Add or remove the login entry (through the Background portal, so it also works in the Flatpak).
pub async fn set_autostart(on: bool) {
    let r = ashpd::desktop::background::Background::request()
        .auto_start(on)
        .command(["bubblefairy", "--tray"])
        .reason("Show BubbleFairy's tray icon and message notifications after you log in")
        .send()
        .await
        .and_then(|r| r.response());
    if let Err(e) = r {
        eprintln!("login entry: {e}");
    }
}
