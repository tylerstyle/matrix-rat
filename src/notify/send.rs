//! Sending notifications: to the desktop's notification service over D-Bus, from a
//! thread of its own that also hears of clicks, or as escape sequences to the
//! terminal. Which icon the desktop is told is the program's own is settled here too,
//! from where icons are installed.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use super::{
    AppIcon, Click, Clicks, DueReminder, NotifyIcon, NotifyMode, escape_markup,
    reminder_title_and_text, terminal_sequence, title_and_text,
};
use crate::matrix::Notice;
use crate::shell::terminal::icon_cache::IconCache;
use crate::time::Instant;

/// The desktop's own icon for a new message: every icon theme has it.
const GENERIC_ICON: &str = "mail-message-new";

/// The icon name notifications use, settled once at start.
static APP_ICON: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Settles which icon notifications name. matrix-rat's own icons are only named when
/// they are installed where the desktop looks for icons (the Nix package does that): a
/// name the desktop cannot find would leave notifications without any icon.
pub fn set_app_icon(choice: AppIcon) {
    let _ = APP_ICON.set(pick_app_icon(choice, &icon_data_dirs()));
}

/// The directories whose `icons/` the desktop searches, most important first.
fn icon_data_dirs() -> Vec<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    let mut dirs = Vec::new();
    match var("XDG_DATA_HOME") {
        Some(home) => dirs.push(PathBuf::from(home)),
        None => {
            if let Some(home) = var("HOME") {
                dirs.push(Path::new(&home).join(".local/share"));
            }
        }
    }
    match var("XDG_DATA_DIRS") {
        Some(list) => dirs.extend(std::env::split_paths(&list)),
        None => dirs.extend(["/usr/local/share", "/usr/share"].map(PathBuf::from)),
    }
    dirs
}

fn pick_app_icon(choice: AppIcon, data_dirs: &[PathBuf]) -> &'static str {
    let (name, file) = match choice {
        AppIcon::Color => ("matrix-rat", "icons/hicolor/scalable/apps/matrix-rat.svg"),
        AppIcon::Outline => (
            "matrix-rat-symbolic",
            "icons/hicolor/symbolic/apps/matrix-rat-symbolic.svg",
        ),
        AppIcon::Generic => return GENERIC_ICON,
    };
    if data_dirs.iter().any(|dir| dir.join(file).is_file()) {
        name
    } else {
        GENERIC_ICON
    }
}

impl NotifyMode {
    /// What `Auto` means on this machine.
    pub fn resolved(self) -> NotifyMode {
        if self != NotifyMode::Auto {
            return self;
        }
        let remote = crate::portal::in_ssh_session();
        if session_bus() && !remote {
            NotifyMode::Desktop
        } else {
            NotifyMode::Terminal
        }
    }
}

/// Whether a session bus is announced (`DBUS_SESSION_BUS_ADDRESS`): what `Auto` goes
/// by to use the desktop's notification service (see [`NotifyMode::resolved`]).
pub fn session_bus() -> bool {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
}

struct DesktopJob {
    title: String,
    text: String,
    urgent: bool,
    preview: bool,
    icon: NotifyIcon,
    cache: Option<(Arc<IconCache>, u64)>,
    /// Where to go when the notification is clicked, and whom to tell.
    click: Option<(Click, Clicks)>,
}

#[derive(Clone, PartialEq, Eq)]
struct PreparedDesktop {
    summary: String,
    body: String,
    urgent: bool,
    icon_path: Option<PathBuf>,
}

/// Hand-written: who wrote, what, and where the icon is stay out of logs.
impl std::fmt::Debug for PreparedDesktop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedDesktop")
            .field("body_bytes", &self.body.len())
            .field("urgent", &self.urgent)
            .field("icon", &self.icon_path.is_some())
            .finish_non_exhaustive()
    }
}

fn prepare_desktop(job: &DesktopJob) -> PreparedDesktop {
    let icon_path = if job.preview {
        job.cache
            .as_ref()
            .and_then(|(cache, epoch)| cache.ensure_file(&job.icon, *epoch))
    } else {
        None
    };
    PreparedDesktop {
        summary: escape_markup(&job.title),
        body: escape_markup(&job.text),
        urgent: job.urgent,
        icon_path,
    }
}

/// Shows a notification. Never blocks and never fails loudly: a notification that
/// cannot be shown is only logged. For desktop notifications, attaches an optional room
/// picture or generated letter tile (`preview = false` and terminal modes ignore `icon`
/// and never touch `cache`). With `clicks`, a click on a desktop notification sends
/// the id of the notice's room there, where the notification service can do that.
pub fn show(
    mode: NotifyMode,
    notice: &Notice,
    preview: bool,
    icon: &NotifyIcon,
    cache: Option<(Arc<IconCache>, u64)>,
    clicks: Option<&Clicks>,
) {
    let (title, text) = title_and_text(notice, preview);
    let click = Click {
        room_id: notice.room_id.clone(),
        event: None,
    };
    show_as(
        mode,
        (title, text),
        (notice, click),
        preview,
        icon,
        cache,
        clicks,
    );
}

/// Shows the notification of a reminder that came due: as [`show`] does a message's,
/// under a title and text of its own. A click on it tells which message it is for.
pub fn show_reminder(
    mode: NotifyMode,
    due: &DueReminder,
    preview: bool,
    icon: &NotifyIcon,
    cache: Option<(Arc<IconCache>, u64)>,
    clicks: Option<&Clicks>,
) {
    let shown = reminder_title_and_text(due, preview);
    let click = Click {
        room_id: due.notice.room_id.clone(),
        event: due.event.clone(),
    };
    show_as(
        mode,
        shown,
        (&due.notice, click),
        preview,
        icon,
        cache,
        clicks,
    );
}

fn show_as(
    mode: NotifyMode,
    (title, text): (String, String),
    (notice, click): (&Notice, Click),
    preview: bool,
    icon: &NotifyIcon,
    cache: Option<(Arc<IconCache>, u64)>,
    clicks: Option<&Clicks>,
) {
    match mode.resolved() {
        NotifyMode::Desktop => {
            let _ = desktop().unbounded_send(DesktopJob {
                title,
                text,
                urgent: notice.highlight,
                preview,
                icon: if preview {
                    icon.clone()
                } else {
                    NotifyIcon::Generic
                },
                cache: if preview { cache } else { None },
                click: clicks.map(|clicks| (click, clicks.clone())),
            });
        }
        NotifyMode::Terminal => {
            let mut out = std::io::stdout();
            let id = SERIAL.fetch_add(1, Ordering::Relaxed);
            let _ = out
                .write_all(terminal_sequence(&title, &text, kitty(), id).as_bytes())
                .and_then(|()| out.flush());
        }
        NotifyMode::Auto | NotifyMode::Off => {}
    }
}

static SERIAL: AtomicU32 = AtomicU32::new(1);

const SERVICE: &str = "org.freedesktop.Notifications";
const SERVICE_PATH: &str = "/org/freedesktop/Notifications";
/// Notifications whose click is still listened for. Older ones stay on screen, but a
/// click on them does nothing any more.
const CLICKABLE_MOST: usize = 64;
/// Signals of the notification service that may wait to be looked at.
const SIGNALS_MOST: std::num::NonZeroUsize = std::num::NonZeroUsize::new(1024).unwrap();
/// After the notification service could not be reached, clicks are not tried again
/// for this long (the notifications themselves are, each time, as before).
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// One thread shows all desktop notifications, one after the other: the service may be
/// slow or absent, which must not hold up the interface, and a burst must not start a
/// thread and a bus connection each. The same thread hears of clicks on them.
fn desktop() -> &'static futures::channel::mpsc::UnboundedSender<DesktopJob> {
    static SENDER: std::sync::OnceLock<futures::channel::mpsc::UnboundedSender<DesktopJob>> =
        std::sync::OnceLock::new();
    SENDER.get_or_init(|| {
        let (sender, mut jobs) = futures::channel::mpsc::unbounded::<DesktopJob>();
        std::thread::spawn(move || {
            use futures::StreamExt;
            use futures::future::Either;
            let mut clickable = Clickable::new(None);
            #[expect(
                clippy::disallowed_methods,
                reason = "the notification thread is this loop"
            )]
            futures::executor::block_on(async {
                loop {
                    // Whichever comes first: something to show, or news from the
                    // notification service about what was shown.
                    let event = {
                        let signal = std::pin::pin!(clickable.next_signal());
                        match futures::future::select(jobs.next(), signal).await {
                            Either::Left((job, _)) => Either::Left(job),
                            Either::Right((signal, _)) => Either::Right(signal),
                        }
                    };
                    match event {
                        Either::Left(Some(job)) => show_desktop(job, &mut clickable).await,
                        Either::Left(None) => break,
                        Either::Right(Some(signal)) => clickable.on_signal(&signal).await,
                        Either::Right(None) => {}
                    }
                }
            });
        });
        sender
    })
}

fn desktop_notification(prepared: &PreparedDesktop) -> notify_rust::Notification {
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("matrix-rat")
        .summary(&prepared.summary)
        .body(&prepared.body)
        .icon(APP_ICON.get().copied().unwrap_or(GENERIC_ICON));
    if let Some(path) = prepared.icon_path.as_deref().and_then(|p| p.to_str()) {
        notification.image_path(path);
    }
    if prepared.urgent {
        notification.urgency(notify_rust::Urgency::Critical);
    }
    notification
}

async fn show_desktop(job: DesktopJob, clickable: &mut Clickable) {
    let prepared = prepare_desktop(&job);
    let notification = desktop_notification(&prepared);
    if clickable.service_stuck() {
        tracing::debug!("the notification service does not answer: notification dropped");
        return;
    }
    if let Some((click, clicks)) = job.click {
        // The same notification with a default action: what a click on it does.
        let mut with_action = notification.clone();
        with_action.action("default", "Open");
        if clickable.show(&with_action, click, clicks).await || clickable.service_stuck() {
            return;
        }
    }
    // Without clicks (the service does not do them, or our own connection to it
    // failed): exactly as before there were any.
    if let Err(err) = notification.show() {
        tracing::debug!(why = %crate::logsafe::kind(&err), "could not show a desktop notification");
    }
}

/// Our own connection to the notification service, for notifications that can be
/// clicked. The service tells the connection that showed a notification when it is
/// clicked or closed (some services tell nobody else), so those are sent from here
/// and not through `notify_rust`, which opens a connection per notification and
/// would need a waiting thread for each. This is one connection and no thread.
struct Clickable {
    link: Link,
    /// The bus to use instead of the session's (tests).
    address: Option<String>,
    /// What was shown and can still be clicked: the service's id for it, the room,
    /// and where to report the click. Oldest first.
    shown: VecDeque<(u32, Click, Clicks)>,
    /// When the service last failed to answer in time. For a while after that
    /// nothing is sent to it at all, not the plain way either: that way has no
    /// time limit of its own, and one silent service would hold up the thread that
    /// shows notifications for good.
    stuck: Option<Instant>,
}

/// Whether a call failed because the service did not answer in time.
fn timed_out(err: &zbus::Error) -> bool {
    matches!(err, zbus::Error::InputOutput(err) if err.kind() == std::io::ErrorKind::TimedOut)
}

enum Link {
    Untried,
    Up(Box<Live>),
    /// Not reachable when last tried.
    Down(Instant),
    /// The service shows notifications but knows no actions.
    NoActions,
}

struct Live {
    connection: zbus::Connection,
    signals: zbus::MessageStream,
    /// The connection the service has on the bus; signals of anyone else are not
    /// the service's.
    owner: Option<String>,
}

/// How long the notification service may take to answer a call.
const CALL_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

impl Clickable {
    fn new(address: Option<String>) -> Self {
        Self {
            link: Link::Untried,
            address,
            shown: VecDeque::new(),
            stuck: None,
        }
    }

    async fn connect(address: Option<&str>) -> zbus::Result<Link> {
        // A notification service that does not answer must not hold up everything
        // after it: a call that takes this long has failed, and the plain way is
        // taken instead.
        let builder = match address {
            Some(address) => zbus::connection::Builder::address(address),
            None => zbus::connection::Builder::session(),
        };
        let connection = builder.method_timeout(CALL_PATIENCE).build().await?;
        let capabilities: Vec<String> = connection
            .call_method(
                Some(SERVICE),
                SERVICE_PATH,
                Some(SERVICE),
                "GetCapabilities",
                &(),
            )
            .await?
            .body()
            .deserialize()?;
        if !capabilities
            .iter()
            .any(|capability| capability == "actions")
        {
            return Ok(Link::NoActions);
        }
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(SERVICE)
            .interface(SERVICE)
            .path(SERVICE_PATH)
            .build()?;
        // The connection stops reading (answers to our own calls too) while this
        // queue is full, and it is only emptied between two notifications: room
        // for more signals than can arrive in that time.
        let signals =
            zbus::MessageStream::for_match_rule(rule, &connection, Some(SIGNALS_MOST)).await?;
        let owner = Self::owner(&connection).await;
        Ok(Link::Up(Box::new(Live {
            connection,
            signals,
            owner,
        })))
    }

    async fn owner(connection: &zbus::Connection) -> Option<String> {
        let bus = zbus::fdo::DBusProxy::new(connection).await.ok()?;
        let name = zbus::names::BusName::try_from(SERVICE).ok()?;
        Some(bus.get_name_owner(name).await.ok()?.to_string())
    }

    /// The service did not answer a moment ago: better to lose a notification than
    /// to wait on it without end.
    fn service_stuck(&self) -> bool {
        self.stuck
            .is_some_and(|since| since.elapsed() < RETRY_AFTER)
    }

    /// Shows a notification that has an action and remembers whose it is. False if
    /// it was not shown, so that it can be shown the plain way.
    async fn show(
        &mut self,
        notification: &notify_rust::Notification,
        click: Click,
        clicks: Clicks,
    ) -> bool {
        let retry = match &self.link {
            Link::Untried => true,
            Link::Down(since) => since.elapsed() >= RETRY_AFTER,
            Link::Up(_) | Link::NoActions => false,
        };
        if retry {
            self.link = match Self::connect(self.address.as_deref()).await {
                Ok(link) => link,
                Err(err) => {
                    tracing::debug!(why = %crate::logsafe::kind(&err), "no connection for notification clicks");
                    if timed_out(&err) {
                        self.stuck = Some(Instant::now());
                    }
                    Link::Down(Instant::now())
                }
            };
        }
        let Link::Up(live) = &self.link else {
            return false;
        };
        // What `notify_rust` would send, with the same hints.
        let hints: std::collections::HashMap<&str, zbus::Variant<'_>> =
            notification.hints.iter().map(Into::into).collect();
        let sent = live
            .connection
            .call_method(
                Some(SERVICE),
                SERVICE_PATH,
                Some(SERVICE),
                "Notify",
                &(
                    &notification.appname,
                    0u32,
                    &notification.icon,
                    &notification.summary,
                    &notification.body,
                    &notification.actions,
                    hints,
                    i32::from(notification.timeout),
                ),
            )
            .await
            .and_then(|reply| reply.body().deserialize::<u32>());
        match sent {
            Ok(id) => {
                self.shown.retain(|(shown, _, _)| *shown != id);
                self.shown.push_back((id, click, clicks));
                while self.shown.len() > CLICKABLE_MOST {
                    self.shown.pop_front();
                }
                true
            }
            Err(err) => {
                tracing::debug!(why = %crate::logsafe::kind(&err), "could not show a notification with an action");
                if timed_out(&err) {
                    self.stuck = Some(Instant::now());
                }
                self.lost();
                false
            }
        }
    }

    /// The connection is gone, and with it the clicks on what was shown through it.
    fn lost(&mut self) {
        self.link = Link::Down(Instant::now());
        self.shown.clear();
    }

    /// The next signal of the notification service. Never resolves while there is
    /// no connection; `None` when the connection just ended.
    async fn next_signal(&mut self) -> Option<zbus::Message> {
        use futures::StreamExt;
        let Link::Up(live) = &mut self.link else {
            return std::future::pending().await;
        };
        match live.signals.next().await {
            Some(Ok(message)) => Some(message),
            Some(Err(_)) | None => {
                self.lost();
                None
            }
        }
    }

    /// A notification was clicked (its room goes to the event loop) or closed (it
    /// is forgotten).
    async fn on_signal(&mut self, message: &zbus::Message) {
        let Link::Up(live) = &mut self.link else {
            return;
        };
        let header = message.header();
        let sender = header.sender().map(|name| name.as_str());
        if live.owner.as_deref() != sender {
            // The service may have been restarted under another name on the bus.
            live.owner = Self::owner(&live.connection).await;
            if live.owner.as_deref() != sender {
                return;
            }
        }
        let body = message.body();
        match header.member().map(|member| member.as_str()) {
            Some("ActionInvoked") => {
                if let Ok((id, action)) = body.deserialize::<(u32, String)>()
                    && action == "default"
                    && let Some((_, click, clicks)) =
                        self.shown.iter().find(|(shown, _, _)| *shown == id)
                {
                    let _ = clicks.send(click.clone());
                }
            }
            Some("NotificationClosed") => {
                if let Ok((id, _reason)) = body.deserialize::<(u32, u32)>() {
                    self.shown.retain(|(shown, _, _)| *shown != id);
                }
            }
            _ => {}
        }
    }
}

fn kitty() -> bool {
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM").is_ok_and(|term| term.contains("kitty"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::tests::{TempDir, notice};

    #[test]
    fn our_own_icon_is_only_named_where_it_is_installed() {
        let root = std::env::temp_dir().join(format!("matrix-rat-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (empty, with) = (root.join("empty"), root.join("with"));
        std::fs::create_dir_all(&empty).unwrap();
        let apps = with.join("icons/hicolor/scalable/apps");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(apps.join("matrix-rat.svg"), "<svg/>").unwrap();

        let dirs = [empty.clone(), with.clone()];
        assert_eq!(pick_app_icon(AppIcon::Color, &dirs), "matrix-rat");
        // The line drawing is not there: the desktop's own icon instead of none.
        assert_eq!(pick_app_icon(AppIcon::Outline, &dirs), GENERIC_ICON);
        assert_eq!(pick_app_icon(AppIcon::Generic, &dirs), GENERIC_ICON);
        assert_eq!(pick_app_icon(AppIcon::Color, &[empty]), GENERIC_ICON);
        assert_eq!(pick_app_icon(AppIcon::Color, &[]), GENERIC_ICON);

        let symbolic = with.join("icons/hicolor/symbolic/apps");
        std::fs::create_dir_all(&symbolic).unwrap();
        std::fs::write(symbolic.join("matrix-rat-symbolic.svg"), "<svg/>").unwrap();
        assert_eq!(
            pick_app_icon(AppIcon::Outline, &[with]),
            "matrix-rat-symbolic"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_modes_are_kept() {
        assert_eq!(NotifyMode::Off.resolved(), NotifyMode::Off);
        assert_eq!(NotifyMode::Terminal.resolved(), NotifyMode::Terminal);
        assert_ne!(NotifyMode::Auto.resolved(), NotifyMode::Auto);
        let config: crate::config::Config = toml::from_str("notifications = \"terminal\"").unwrap();
        assert_eq!(config.notifications, NotifyMode::Terminal);
    }

    #[test]
    fn preview_off_terminal_and_cache_failures_do_not_break_notifications() {
        let root = TempDir::new("icon-fallback");
        let dir = root.0.join("cache");
        let cache = Arc::new(IconCache::new(dir.clone()));
        let epoch = cache.epoch();
        let tile = NotifyIcon::Tile {
            ch: 'R',
            bg: [40, 60, 120],
            fg: [250, 250, 250],
        };

        // Preview disabled hides the picture and never creates the cache dir.
        let hidden = prepare_desktop(&DesktopJob {
            title: "matrix-rat".into(),
            text: "New message".into(),
            urgent: false,
            preview: false,
            icon: tile.clone(),
            cache: Some((Arc::clone(&cache), epoch)),
            click: None,
        });
        assert_eq!(hidden.icon_path, None);
        assert!(!dir.exists());

        // Terminal mode leaves the icon cache untouched.
        show(
            NotifyMode::Terminal,
            &notice("Rust", "ann", "hi", false),
            true,
            &tile,
            Some((Arc::clone(&cache), epoch)),
            None,
        );
        assert!(!dir.exists());

        // A blocked cache path (symlink instead of directory) still yields a sendable notice.
        let target = root.0.join("outside");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        let prepared = prepare_desktop(&DesktopJob {
            title: "ann in Rust".into(),
            text: "hello".into(),
            urgent: true,
            preview: true,
            icon: tile,
            cache: Some((cache, epoch)),
            click: None,
        });
        assert_eq!(prepared.summary, "ann in Rust");
        assert_eq!(prepared.body, "hello");
        assert!(prepared.urgent);
        assert_eq!(prepared.icon_path, None);
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }

    /// A message bus of its own for a test, with a stand-in for the desktop's
    /// notification service on it. `None` where there is no `dbus-daemon` to run.
    struct TestBus {
        daemon: std::process::Child,
        address: String,
        _dir: TempDir,
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
        }
    }

    /// What the stand-in service was asked to show: its id for it, who asked (the
    /// connection's name on the bus) and the actions.
    type Shown = (u32, String, Vec<String>);

    impl TestBus {
        fn start(name: &str) -> Option<Self> {
            use std::io::BufRead;
            let dir = TempDir::new(name);
            let mut daemon = std::process::Command::new("dbus-daemon")
                // Not `--session`: that could start the desktop's own services.
                .arg(crate::testbus::config_arg())
                .arg("--nofork")
                .arg("--print-address")
                .arg(format!(
                    "--address=unix:path={}",
                    dir.0.join("bus").display()
                ))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;
            let mut address = String::new();
            let read = std::io::BufReader::new(daemon.stdout.take()?).read_line(&mut address);
            let bus = Self {
                daemon,
                address: address.trim().to_owned(),
                _dir: dir,
            };
            (read.is_ok() && !bus.address.is_empty()).then_some(bus)
        }

        /// Starts the service with these capabilities. Gives its connection (to send
        /// signals as the service) and what it is asked to show.
        async fn serve(
            &self,
            capabilities: &'static [&'static str],
        ) -> (
            zbus::Connection,
            tokio::sync::mpsc::UnboundedReceiver<Shown>,
        ) {
            use futures::StreamExt;
            type Notify = (
                String,
                u32,
                String,
                String,
                String,
                Vec<String>,
                std::collections::HashMap<String, zbus::OwnedVariant>,
                i32,
            );
            let service = zbus::connection::Builder::address(self.address.as_str())
                .build()
                .await
                .expect("the stand-in service");
            let (shown, asked) = tokio::sync::mpsc::unbounded_channel();
            let connection = service.clone();
            // Listening from before the first caller can know of the service (the name
            // is taken only after): a call that arrived ahead of the task below would
            // never be answered.
            let mut calls = zbus::MessageStream::from(&service);
            service
                .request_name(SERVICE)
                .await
                .expect("the stand-in service's name");
            tokio::spawn(async move {
                let mut next_id = 7u32;
                while let Some(Ok(call)) = calls.next().await {
                    let header = call.header();
                    if header.message_type() != zbus::message::Type::MethodCall {
                        continue;
                    }
                    match header.member().map(|member| member.as_str()) {
                        Some("GetCapabilities") => {
                            service.reply(&header, &capabilities).await.unwrap();
                        }
                        Some("Notify") => {
                            let body = call.body();
                            let notify: Notify = body.deserialize().unwrap();
                            next_id += 1;
                            service.reply(&header, &next_id).await.unwrap();
                            let sender = header.sender().unwrap().to_string();
                            let _ = shown.send((next_id, sender, notify.5));
                        }
                        _ => {}
                    }
                }
            });
            (connection, asked)
        }
    }

    fn clickable_notification() -> notify_rust::Notification {
        let mut notification = desktop_notification(&PreparedDesktop {
            summary: "ann in Rust".into(),
            body: "hello".into(),
            urgent: true,
            icon_path: None,
        });
        notification.action("default", "Open");
        notification
    }

    /// The next signal of the service, handled; panics if none comes.
    async fn hear(clickable: &mut Clickable) {
        let signal = tokio::time::timeout(Duration::from_secs(10), clickable.next_signal())
            .await
            .expect("no signal arrived")
            .expect("the connection ended");
        clickable.on_signal(&signal).await;
    }

    async fn signal<B>(from: &zbus::Connection, to: &str, name: &str, body: &B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        from.emit_signal(Some(to), SERVICE_PATH, SERVICE, name, body)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_click_on_a_notification_reports_its_room_until_it_is_closed() {
        let Some(bus) = TestBus::start("notify-click") else {
            eprintln!("no dbus-daemon to run: not tested");
            return;
        };
        let (service, mut asked) = bus.serve(&["body", "actions"]).await;
        let (clicks, mut clicked) = tokio::sync::mpsc::unbounded_channel();
        let mut clickable = Clickable::new(Some(bus.address.clone()));

        let notification = clickable_notification();
        assert!(
            clickable
                .show(&notification, "!rust:example.org".into(), clicks.clone())
                .await
        );
        // A reminder's notification carries its message.
        let reminder = Click {
            room_id: "!rust:example.org".into(),
            event: Some(("$event".into(), Some("$root".into()))),
        };
        assert!(
            clickable
                .show(&notification, reminder.clone(), clicks.clone())
                .await
        );
        let (first, us, actions) = asked.recv().await.unwrap();
        let (second, _, _) = asked.recv().await.unwrap();
        assert_eq!(actions, ["default", "Open"]);

        // Clicked: what it was shown for is reported, each time; the one shown
        // later in the same room does not change what the earlier one means.
        signal(&service, &us, "ActionInvoked", &(first, "default")).await;
        hear(&mut clickable).await;
        assert_eq!(
            clicked.try_recv().unwrap(),
            Click::from("!rust:example.org")
        );
        signal(&service, &us, "ActionInvoked", &(second, "default")).await;
        hear(&mut clickable).await;
        assert_eq!(clicked.try_recv().unwrap(), reminder);
        signal(&service, &us, "ActionInvoked", &(first, "default")).await;
        hear(&mut clickable).await;
        assert_eq!(
            clicked.try_recv().unwrap(),
            Click::from("!rust:example.org")
        );
        // Another action, or a notification that is not ours: nothing.
        signal(&service, &us, "ActionInvoked", &(first, "reply")).await;
        hear(&mut clickable).await;
        signal(&service, &us, "ActionInvoked", &(first + 100, "default")).await;
        hear(&mut clickable).await;
        assert!(clicked.try_recv().is_err());
        // Someone else on the bus pretending to be the service: nothing.
        let stranger = zbus::connection::Builder::address(bus.address.as_str())
            .build()
            .await
            .unwrap();
        signal(&stranger, &us, "ActionInvoked", &(first, "default")).await;
        hear(&mut clickable).await;
        assert!(clicked.try_recv().is_err());

        // Closed (clicked away, or timed out): forgotten, a late click does nothing.
        signal(&service, &us, "NotificationClosed", &(first, 2u32)).await;
        hear(&mut clickable).await;
        assert_eq!(clickable.shown.len(), 1);
        signal(&service, &us, "ActionInvoked", &(first, "default")).await;
        hear(&mut clickable).await;
        assert!(clicked.try_recv().is_err());

        // However many are shown and never closed, only so many are remembered.
        for _ in 0..CLICKABLE_MOST + 5 {
            assert!(
                clickable
                    .show(&notification, "!rust:example.org".into(), clicks.clone())
                    .await
            );
        }
        assert_eq!(clickable.shown.len(), CLICKABLE_MOST);

        // The service goes away: what was shown cannot be clicked any more, and the
        // next notification is shown the plain way.
        drop(bus);
        let ended = tokio::time::timeout(Duration::from_secs(10), async {
            while clickable.next_signal().await.is_some() {}
        });
        ended.await.expect("the end of the bus went unnoticed");
        assert!(clickable.shown.is_empty());
        assert!(matches!(clickable.link, Link::Down(_)));
        assert!(
            !clickable
                .show(&notification, "!rust:example.org".into(), clicks)
                .await
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_service_that_never_answers_is_given_up_on_and_left_alone_for_a_while() {
        let Some(bus) = TestBus::start("notify-stuck") else {
            eprintln!("no dbus-daemon to run: not tested");
            return;
        };
        // It has the name, and answers nothing.
        let _silent = zbus::connection::Builder::address(bus.address.as_str())
            .name(SERVICE)
            .build()
            .await
            .expect("the silent service");
        let (clicks, _clicked) = tokio::sync::mpsc::unbounded_channel();
        let mut clickable = Clickable::new(Some(bus.address.clone()));
        assert!(!clickable.service_stuck());
        let started = Instant::now();
        assert!(
            !clickable
                .show(
                    &clickable_notification(),
                    "!rust:example.org".into(),
                    clicks
                )
                .await
        );
        assert!(started.elapsed() >= CALL_PATIENCE);
        assert!(started.elapsed() < CALL_PATIENCE + Duration::from_secs(5));
        // Nothing more is sent its way for now, by either road.
        assert!(clickable.service_stuck());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_service_without_actions_is_left_to_the_plain_way() {
        let Some(bus) = TestBus::start("notify-plain") else {
            eprintln!("no dbus-daemon to run: not tested");
            return;
        };
        let (_service, mut asked) = bus.serve(&["body"]).await;
        let (clicks, _clicked) = tokio::sync::mpsc::unbounded_channel();
        let mut clickable = Clickable::new(Some(bus.address.clone()));
        let notification = clickable_notification();
        for _ in 0..2 {
            assert!(
                !clickable
                    .show(&notification, "!rust:example.org".into(), clicks.clone())
                    .await
            );
        }
        // Nothing was shown with an action, and it is not asked about again.
        assert!(matches!(clickable.link, Link::NoActions));
        assert!(asked.try_recv().is_err());
        // Nor is there anything to wait for.
        let waiting = tokio::time::timeout(Duration::from_millis(50), clickable.next_signal());
        assert!(waiting.await.is_err());

        // No bus at all: the same, and tried again only after a while.
        let mut nowhere = Clickable::new(Some("unix:path=/nonexistent/matrix-rat-bus".into()));
        assert!(
            !nowhere
                .show(&notification, "!rust:example.org".into(), clicks)
                .await
        );
        assert!(matches!(nowhere.link, Link::Down(_)));
    }
}
