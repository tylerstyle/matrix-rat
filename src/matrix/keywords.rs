//! Notification keywords: words that make a message notify like a mention. They are
//! push rules of kind `content` in the account data on the server, so the account's
//! other clients use them too; nothing of them is kept here.
//!
//! The worker owns the SDK's notification settings. The UI sees the list while its
//! popup is open, and the outcome of what it asked for. Keywords say what the user
//! cares about: they stay out of logs and `Debug` output.

use std::sync::Arc;

use matrix_sdk::notification_settings::NotificationSettings;
use matrix_sdk::ruma::api::client::push::set_pushrule_actions;
use matrix_sdk::ruma::push::{Action, HighlightTweakValue, RuleKind, SoundTweakValue, Tweak};
use matrix_sdk::{Client, NotificationSettingsError};
use tokio::sync::Notify;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{JoinHandle, JoinSet};

use super::MatrixEvent;

/// The most keywords sent to the popup; the rest are only counted.
pub const MAX_KEYWORDS: usize = 100;

// ----- what the UI sees ---------------------------------------------------------------

pub enum KeywordsCmd {
    /// The popup opened (with a number, which the lists for it quote) or closed. While
    /// it is open, [`KeywordsEvent::List`] is sent at once and on every change of the
    /// push rules, made here or in another client.
    Watch(Option<u64>),
    Add {
        request: u64,
        keyword: String,
    },
    Remove {
        request: u64,
        keyword: String,
    },
}

/// Never prints a keyword.
impl std::fmt::Debug for KeywordsCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Watch(watch) => write!(f, "Watch({watch:?})"),
            Self::Add { request, .. } => write!(f, "Add(request {request})"),
            Self::Remove { request, .. } => write!(f, "Remove(request {request})"),
        }
    }
}

pub enum KeywordsEvent {
    /// The keywords as the account has them now, sorted, at most [`MAX_KEYWORDS`] of
    /// them; `total` counts them all.
    List {
        /// The [`KeywordsCmd::Watch`] this is news for.
        watch: u64,
        keywords: Vec<String>,
        total: usize,
    },
    /// The outcome of an [`KeywordsCmd::Add`] or [`KeywordsCmd::Remove`]; the error is
    /// for the user.
    Done {
        request: u64,
        result: Result<(), String>,
    },
}

/// Never prints a keyword, nor the reason of a failure.
impl std::fmt::Debug for KeywordsEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::List {
                watch,
                keywords,
                total,
            } => write!(f, "List(watch {watch}, {} of {total})", keywords.len()),
            Self::Done { request, result } => {
                write!(f, "Done(request {request}, ok: {})", result.is_ok())
            }
        }
    }
}

/// Sorts the keywords as the popup lists them and keeps the first [`MAX_KEYWORDS`];
/// also says how many there were. The demo uses it for its own list.
pub fn shown_keywords(mut keywords: Vec<String>) -> (Vec<String>, usize) {
    let total = keywords.len();
    keywords.sort_by_cached_key(|keyword| (keyword.to_lowercase(), keyword.clone()));
    keywords.truncate(MAX_KEYWORDS);
    (keywords, total)
}

// ----- the worker side ----------------------------------------------------------------

/// Lives as long as a session.
pub struct Keywords {
    client: Client,
    /// The session's one instance: changes made through it show in it at once, and
    /// it hears of the changes other clients make.
    settings: NotificationSettings,
    events: UnboundedSender<MatrixEvent>,
    watcher: Option<JoinHandle<()>>,
    /// Asks the watcher to look again: a change made here is not announced by the
    /// settings until the server sends it back.
    refresh: Arc<Notify>,
    tasks: JoinSet<()>,
}

impl Keywords {
    pub fn new(
        client: Client,
        settings: NotificationSettings,
        events: UnboundedSender<MatrixEvent>,
    ) -> Self {
        Self {
            client,
            settings,
            events,
            watcher: None,
            refresh: Arc::new(Notify::new()),
            tasks: JoinSet::new(),
        }
    }

    /// Every request runs in a task of its own: none may hold up the session.
    pub fn handle(&mut self, command: KeywordsCmd) {
        while self.tasks.try_join_next().is_some() {}
        tracing::debug!(?command, "notification keywords");
        match command {
            KeywordsCmd::Watch(watch_id) => {
                if let Some(old) = self.watcher.take() {
                    old.abort();
                }
                if let Some(id) = watch_id {
                    self.watcher = Some(tokio::spawn(watch(
                        self.settings.clone(),
                        self.events.clone(),
                        self.refresh.clone(),
                        id,
                    )));
                }
            }
            KeywordsCmd::Add { request, keyword } => {
                self.tasks.spawn(answer(
                    self.events.clone(),
                    self.refresh.clone(),
                    request,
                    add(self.client.clone(), self.settings.clone(), keyword),
                ));
            }
            KeywordsCmd::Remove { request, keyword } => {
                self.tasks.spawn(answer(
                    self.events.clone(),
                    self.refresh.clone(),
                    request,
                    remove(self.settings.clone(), keyword),
                ));
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
        self.tasks.abort_all();
    }
}

impl Drop for Keywords {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Sends the keywords now and whenever they may have changed, until it is aborted.
/// One task does all the sending, so that an older list never arrives after a newer.
async fn watch(
    settings: NotificationSettings,
    events: UnboundedSender<MatrixEvent>,
    refresh: Arc<Notify>,
    id: u64,
) {
    let mut changes = settings.subscribe_to_changes();
    let mut open = true;
    let mut last = None;
    loop {
        let now = shown_keywords(settings.enabled_keywords().await.into_iter().collect());
        if last.as_ref() != Some(&now) {
            let (keywords, total) = now.clone();
            let event = KeywordsEvent::List {
                watch: id,
                keywords,
                total,
            };
            if events.send(MatrixEvent::Keywords(event)).is_err() {
                return;
            }
            last = Some(now);
        }
        tokio::select! {
            change = changes.recv(), if open => {
                // Missed news means something changed: look again all the same.
                if matches!(change, Err(RecvError::Closed)) {
                    open = false;
                }
            }
            () = refresh.notified() => {}
        }
    }
}

/// Runs one request and tells its outcome; the open popup is then sent the list as
/// it is now.
async fn answer(
    events: UnboundedSender<MatrixEvent>,
    refresh: Arc<Notify>,
    request: u64,
    work: impl Future<Output = Result<(), String>>,
) {
    let result = work.await;
    let _ = events.send(MatrixEvent::Keywords(KeywordsEvent::Done {
        request,
        result,
    }));
    // Also after a failure: a keyword can be added and then fail to be marked as a
    // mention, and the list must show it for it to be removed.
    refresh.notify_one();
}

async fn add(
    client: Client,
    settings: NotificationSettings,
    keyword: String,
) -> Result<(), String> {
    settings.add_keyword(keyword.clone()).await.map_err(|err| {
        tracing::warn!("a notification keyword could not be added");
        format!("Could not add the keyword: {}", explain(&err))
    })?;
    mark_as_mention(&client, keyword).await.map_err(|_| {
        tracing::warn!("a notification keyword could not be marked as a mention");
        "The keyword was added, but the server did not take the part that marks it like a \
         mention: it notifies without that. Remove it and add it again to retry."
            .to_owned()
    })
}

/// The SDK makes a keyword's rule notify (with a sound) but not highlight. Other
/// clients (Element on the web, for one) highlight it too, and so the user's choice
/// reaches everything that follows the push rules: the "mention" of a notification,
/// the mention badge of a room.
async fn mark_as_mention(client: &Client, keyword: String) -> Result<(), matrix_sdk::HttpError> {
    let actions = vec![
        Action::Notify,
        Action::SetTweak(Tweak::Sound(SoundTweakValue::Default)),
        Action::SetTweak(Tweak::Highlight(HighlightTweakValue::Yes)),
    ];
    let request = set_pushrule_actions::v3::Request::new(RuleKind::Content, keyword, actions);
    client.send(request).await.map(|_| ())
}

async fn remove(settings: NotificationSettings, keyword: String) -> Result<(), String> {
    settings.remove_keyword(&keyword).await.map_err(|err| {
        tracing::warn!("a notification keyword could not be removed");
        format!("Could not remove the keyword: {}", explain(&err))
    })
}

/// The SDK's errors do not say why; none of what they carry is worth quoting.
fn explain(err: &NotificationSettingsError) -> &'static str {
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "foreign matrix_sdk NotificationSettingsError"
    )]
    match err {
        NotificationSettingsError::UnableToAddPushRule
        | NotificationSettingsError::UnableToRemovePushRule
        | NotificationSettingsError::UnableToUpdatePushRule
        | NotificationSettingsError::UnableToSavePushRules => {
            "the server did not take the change; try again in a moment"
        }
        _ => "the server's notification rules did not allow it",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::matrix::{MatrixCmd, spawn};
    use crate::paths::Paths;

    #[test]
    fn keywords_are_listed_in_order_and_cut_at_the_limit() {
        let (shown, total) = shown_keywords(vec!["lunch".into(), "Deploy".into(), "alpha".into()]);
        assert_eq!(shown, ["alpha", "Deploy", "lunch"]);
        assert_eq!(total, 3);
        let many: Vec<String> = (0..MAX_KEYWORDS + 30).map(|n| format!("k{n:03}")).collect();
        let (shown, total) = shown_keywords(many);
        assert_eq!(shown.len(), MAX_KEYWORDS);
        assert_eq!(total, MAX_KEYWORDS + 30);
        assert_eq!(shown[0], "k000");
    }

    #[test]
    fn debug_output_names_no_keyword_and_no_reason() {
        let printed = format!(
            "{:?} {:?} {:?} {:?} {:?}",
            KeywordsCmd::Add {
                request: 1,
                keyword: "secret-project".into(),
            },
            KeywordsCmd::Remove {
                request: 2,
                keyword: "secret-project".into(),
            },
            KeywordsCmd::Watch(Some(3)),
            KeywordsEvent::List {
                watch: 3,
                keywords: vec!["secret-project".into()],
                total: 1,
            },
            KeywordsEvent::Done {
                request: 2,
                result: Err("Could not remove secret-project".into()),
            },
        );
        assert!(!printed.contains("secret"), "{printed}");
        assert!(printed.contains("request 1") && printed.contains("watch 3"));
        let command = MatrixCmd::Keywords(KeywordsCmd::Add {
            request: 4,
            keyword: "secret-project".into(),
        });
        let event = MatrixEvent::Keywords(KeywordsEvent::List {
            watch: 3,
            keywords: vec!["secret-project".into()],
            total: 1,
        });
        assert!(!format!("{command:?} {event:?}").contains("secret"));
    }

    /// Adds a keyword, sees it listed, removes it and sees it gone, against a local
    /// homeserver (see `a_poll_is_started_voted_in_and_ended_and_a_sticker_arrives`).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_keyword_is_added_listed_removed_and_gone() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let root = std::env::temp_dir().join(format!("matrix-rat-keywords-{}", std::process::id()));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();
        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username,
            password: password.into(),
        });

        let keyword = format!("ratword{}", std::process::id());
        let wait = async {
            // 0: waiting for the session, 1: added and waiting to see it, 2: seen and
            // removed, waiting to see it gone.
            let mut step = 0;
            loop {
                let event = events.recv().await.expect("worker stopped");
                assert!(!format!("{event:?}").contains(&keyword));
                match event {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(_) if step == 0 => {
                        step = 1;
                        handle.send(MatrixCmd::Keywords(KeywordsCmd::Watch(Some(1))));
                        handle.send(MatrixCmd::Keywords(KeywordsCmd::Add {
                            request: 1,
                            keyword: keyword.clone(),
                        }));
                    }
                    MatrixEvent::Keywords(KeywordsEvent::Done { request, result }) => {
                        assert!(result.is_ok(), "request {request}: {result:?}");
                    }
                    MatrixEvent::Keywords(KeywordsEvent::List {
                        watch, keywords, ..
                    }) => {
                        assert_eq!(watch, 1);
                        let listed = keywords.contains(&keyword);
                        if step == 1 && listed {
                            step = 2;
                            handle.send(MatrixCmd::Keywords(KeywordsCmd::Remove {
                                request: 2,
                                keyword: keyword.clone(),
                            }));
                        } else if step == 2 && !listed {
                            return;
                        }
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .expect("timed out waiting for the keyword to come and go");
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }
}
