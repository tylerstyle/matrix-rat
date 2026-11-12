//! The Matrix side of the app. Everything that talks to the SDK lives in this module and
//! runs on its own task; the UI only sees [`MatrixCmd`] and [`MatrixEvent`].

mod session;

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::{StreamExt, pin_mut};
use matrix_sdk::encryption::recovery::RecoveryState;
use matrix_sdk::{Client, RoomState, SessionChange};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::room_list_service::RoomListItem;
use matrix_sdk_ui::room_list_service::filters::new_filter_non_left;
use matrix_sdk_ui::sync_service::{State, SyncService};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::paths::Paths;

/// Rooms shown before the user scrolls for more. Paging arrives with the timeline work.
const ROOM_PAGE: usize = 10_000;

#[derive(Debug)]
pub enum MatrixCmd {
    LoginPassword {
        homeserver: String,
        username: String,
        password: String,
    },
    /// Log in through the homeserver's web page (OAuth 2.0).
    LoginBrowser {
        homeserver: String,
    },
    CancelLogin,
    Recover {
        key: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Connecting,
    Synced,
    Offline,
    Error(String),
    Stopped,
}

/// Whether this device can read encrypted history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStatus {
    Unknown,
    /// Nothing to do: recovery is set up and this device has the keys.
    Ready,
    /// The account has no recovery set up.
    NotSetUp,
    /// The account has recovery, but this device still needs the recovery key.
    NeedsKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomCategory {
    Invite,
    Favourite,
    Person,
    Room,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSummary {
    pub id: String,
    pub name: String,
    pub category: RoomCategory,
    /// Notification count, shown as a badge.
    pub notifications: u64,
    pub mention: bool,
    /// Has unread messages, even if they do not notify.
    pub unread: bool,
}

#[derive(Debug)]
pub enum MatrixEvent {
    /// No usable session; show the login form. `reason` explains a failed restore.
    NeedLogin {
        reason: Option<String>,
    },
    LoginFailed(String),
    /// The page the user has to open to continue a browser login.
    BrowserLoginUrl(String),
    LoggedIn {
        user_id: String,
    },
    Sync(SyncStatus),
    Rooms(Vec<RoomSummary>),
    Recovery(RecoveryStatus),
    /// Outcome of [`MatrixCmd::Recover`]: `Err` carries a message for the user.
    RecoverResult(Result<(), String>),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct MatrixHandle {
    commands: UnboundedSender<MatrixCmd>,
}

impl MatrixHandle {
    pub fn send(&self, command: MatrixCmd) {
        // A closed channel means the worker is gone, which only happens at shutdown.
        let _ = self.commands.send(command);
    }
}

/// Starts the worker on the current tokio runtime.
pub fn spawn(paths: Paths) -> (MatrixHandle, UnboundedReceiver<MatrixEvent>) {
    let (commands, command_rx) = unbounded_channel();
    let (events, event_rx) = unbounded_channel();
    tokio::spawn(async move {
        Worker {
            paths,
            commands: command_rx,
            events,
        }
        .run()
        .await;
    });
    (MatrixHandle { commands }, event_rx)
}

enum SessionEnd {
    Shutdown,
    /// The server no longer accepts our token.
    Expired,
}

struct Worker {
    paths: Paths,
    commands: UnboundedReceiver<MatrixCmd>,
    events: UnboundedSender<MatrixEvent>,
}

impl Worker {
    fn emit(&self, event: MatrixEvent) {
        let _ = self.events.send(event);
    }

    async fn run(mut self) {
        let mut reason = None;
        loop {
            let restored = match session::restore(&self.paths).await {
                Ok(found) => found,
                Err(err) => {
                    tracing::warn!(?err, "could not restore session");
                    reason = Some(format!("Could not restore your session: {err:#}"));
                    None
                }
            };
            let (client, passphrase) = match restored {
                Some(pair) => pair,
                None => {
                    self.emit(MatrixEvent::NeedLogin {
                        reason: reason.take(),
                    });
                    match self.wait_for_login().await {
                        Some(pair) => pair,
                        None => return,
                    }
                }
            };
            match self.run_session(&client, &passphrase).await {
                Ok(SessionEnd::Shutdown) => return,
                Ok(SessionEnd::Expired) => {
                    session::forget(&self.paths);
                    reason = Some("Your session has expired. Please log in again.".to_owned());
                }
                Err(err) => {
                    tracing::error!(?err, "session failed");
                    self.emit(MatrixEvent::Error(format!("{err:#}")));
                    // Keep the session on disk: this may be a temporary problem.
                    match self.commands.recv().await {
                        Some(MatrixCmd::Shutdown) | None => return,
                        Some(_) => {}
                    }
                }
            }
        }
    }

    /// Handles login commands until one succeeds. `None` means shut down.
    async fn wait_for_login(&mut self) -> Option<(Client, String)> {
        loop {
            let result = match self.commands.recv().await? {
                MatrixCmd::Shutdown => return None,
                MatrixCmd::LoginPassword {
                    homeserver,
                    username,
                    password,
                } => session::login_password(&self.paths, &homeserver, &username, &password).await,
                MatrixCmd::LoginBrowser { homeserver } => {
                    session::login_browser(
                        &self.paths,
                        &homeserver,
                        &self.events,
                        &mut self.commands,
                    )
                    .await
                }
                MatrixCmd::CancelLogin | MatrixCmd::Recover { .. } => continue,
            };
            match result {
                Ok(pair) => return Some(pair),
                Err(err) => {
                    tracing::warn!(?err, "login failed");
                    self.emit(MatrixEvent::LoginFailed(format!("{err:#}")));
                }
            }
        }
    }

    async fn run_session(&mut self, client: &Client, passphrase: &str) -> Result<SessionEnd> {
        let user_id = client
            .user_id()
            .ok_or_else(|| anyhow!("logged in without a user id"))?
            .to_string();
        self.emit(MatrixEvent::LoggedIn { user_id });
        self.emit(MatrixEvent::Sync(SyncStatus::Connecting));

        let sync = SyncService::builder(client.clone())
            .build()
            .await
            .context("could not set up sync")?;
        sync.start().await;

        let room_list = sync
            .room_list_service()
            .all_rooms()
            .await
            .context("could not open the room list")?;
        let (entries, controller) = room_list.entries_with_dynamic_adapters(ROOM_PAGE);
        // Nothing is emitted until a filter is set.
        controller.set_filter(Box::new(new_filter_non_left()));
        pin_mut!(entries);

        let mut sync_state = sync.state();
        // The subscriber only reports changes; the service may already be running.
        self.emit(MatrixEvent::Sync(sync_status(&sync_state.get())));
        let mut session_changes = client.subscribe_to_session_changes();
        let recovery = client.encryption().recovery();
        let recovery_states = recovery.state_stream();
        pin_mut!(recovery_states);
        self.emit(MatrixEvent::Recovery(recovery_status(recovery.state())));

        let mut rooms: Vector<RoomListItem> = Vector::new();
        let end = loop {
            tokio::select! {
                Some(diffs) = entries.next() => {
                    for diff in diffs {
                        diff.apply(&mut rooms);
                    }
                    self.emit(MatrixEvent::Rooms(summarize(&rooms).await));
                }
                Some(state) = sync_state.next() => {
                    self.emit(MatrixEvent::Sync(sync_status(&state)));
                }
                Some(state) = recovery_states.next() => {
                    self.emit(MatrixEvent::Recovery(recovery_status(state)));
                }
                change = session_changes.recv() => match change {
                    Ok(SessionChange::TokensRefreshed) => {
                        if let Err(err) = session::save(&self.paths, client, passphrase) {
                            tracing::error!(?err, "could not save refreshed tokens");
                        }
                    }
                    Ok(SessionChange::UnknownToken { .. }) => break SessionEnd::Expired,
                    Err(_) => {}
                },
                command = self.commands.recv() => match command {
                    Some(MatrixCmd::Recover { key }) => {
                        // Can take a while (downloads keys); must not block the loop.
                        let recovery = client.encryption().recovery();
                        let events = self.events.clone();
                        tokio::spawn(async move {
                            let result = recovery
                                .recover(key.trim())
                                .await
                                .map_err(|err| err.to_string());
                            let _ = events.send(MatrixEvent::RecoverResult(result));
                        });
                    }
                    Some(MatrixCmd::Shutdown) | None => break SessionEnd::Shutdown,
                    Some(_) => {}
                },
            }
        };
        // Do not hang on exit if the server is unreachable.
        let _ = tokio::time::timeout(Duration::from_secs(2), sync.stop()).await;
        Ok(end)
    }
}

fn sync_status(state: &State) -> SyncStatus {
    match state {
        State::Idle => SyncStatus::Connecting,
        State::Running => SyncStatus::Synced,
        State::Offline => SyncStatus::Offline,
        State::Terminated => SyncStatus::Stopped,
        State::Error(err) => SyncStatus::Error(err.to_string()),
    }
}

fn recovery_status(state: RecoveryState) -> RecoveryStatus {
    match state {
        RecoveryState::Unknown => RecoveryStatus::Unknown,
        RecoveryState::Enabled => RecoveryStatus::Ready,
        RecoveryState::Disabled => RecoveryStatus::NotSetUp,
        RecoveryState::Incomplete => RecoveryStatus::NeedsKey,
    }
}

async fn summarize(rooms: &Vector<RoomListItem>) -> Vec<RoomSummary> {
    let mut summaries = Vec::with_capacity(rooms.len());
    for room in rooms {
        // Spaces get their own navigation later; they are not chats.
        if room.is_space() {
            continue;
        }
        let category = if room.state() == RoomState::Invited {
            RoomCategory::Invite
        } else if room.is_favourite() {
            RoomCategory::Favourite
        } else if room.is_direct().await.unwrap_or(false) {
            RoomCategory::Person
        } else {
            RoomCategory::Room
        };
        let name = match room.cached_display_name() {
            Some(name) => name.to_string(),
            None => room
                .display_name()
                .await
                .map(|name| name.to_string())
                .unwrap_or_else(|_| room.room_id().to_string()),
        };
        let notifications = room.num_unread_notifications();
        summaries.push(RoomSummary {
            id: room.room_id().to_string(),
            name,
            category,
            notifications,
            mention: room.num_unread_mentions() > 0,
            unread: notifications > 0 || room.num_unread_messages() > 0 || room.is_marked_unread(),
        });
    }
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end check against a local homeserver. Start one with
    /// `scripts/dev-homeserver.sh`, create the user, then run:
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn password_login_sync_and_restore() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");

        let root = std::env::temp_dir().join(format!("matrix-rat-it-{}", std::process::id()));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();

        async fn until_rooms(
            events: &mut UnboundedReceiver<MatrixEvent>,
        ) -> (bool, Vec<RoomSummary>) {
            let mut needed_login = false;
            let wait = async {
                loop {
                    match events.recv().await.expect("worker stopped") {
                        MatrixEvent::NeedLogin { .. } => needed_login = true,
                        MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                        MatrixEvent::Rooms(rooms) if !rooms.is_empty() => return rooms,
                        _ => {}
                    }
                    if needed_login {
                        return Vec::new();
                    }
                }
            };
            let rooms = tokio::time::timeout(Duration::from_secs(30), wait)
                .await
                .expect("timed out waiting for rooms");
            (needed_login, rooms)
        }

        // First run: no session, so the worker asks for a login.
        let (handle, mut events) = spawn(paths.clone());
        let (needed_login, _) = until_rooms(&mut events).await;
        assert!(needed_login);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username,
            password,
        });
        let (_, rooms) = until_rooms(&mut events).await;
        assert!(!rooms.is_empty());
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}

        // Second run: the saved session is restored without logging in again.
        let (handle, mut events) = spawn(paths.clone());
        let (needed_login, rooms) = until_rooms(&mut events).await;
        assert!(!needed_login, "session was not restored");
        assert!(!rooms.is_empty());
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }
}
