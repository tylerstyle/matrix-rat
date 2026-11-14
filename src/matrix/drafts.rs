//! Unsent text that survives a restart. It is kept in the SDK's state store (which is
//! encrypted like the rest of the session), one draft per room and one per thread.

use anyhow::{Result, anyhow};
use matrix_sdk::ruma::RoomId;
use matrix_sdk::ruma::{EventId, OwnedEventId};
use matrix_sdk::{Client, ComposerDraft, ComposerDraftType, Room};
use matrix_sdk_base::StateStoreDataKey;

fn thread_root(thread: Option<&str>) -> Result<Option<OwnedEventId>> {
    thread
        .map(|root| EventId::parse(root).map_err(|_| anyhow!("invalid event id")))
        .transpose()
}

/// Stores `text` as the draft of the room, or of the thread that starts at `thread`.
/// Empty text removes what is stored.
pub async fn save(room: &Room, thread: Option<&str>, text: String) -> Result<()> {
    let root = thread_root(thread)?;
    if text.is_empty() {
        room.clear_composer_draft(root.as_deref()).await?;
    } else {
        // Always a new message: an edit is never stored, and of a reply only the text
        // is (the UI does not restore what it answered).
        let draft = ComposerDraft {
            plain_text: text,
            html_text: None,
            draft_type: ComposerDraftType::NewMessage,
            attachments: Vec::new(),
        };
        room.save_composer_draft(draft, root.as_deref()).await?;
    }
    Ok(())
}

/// The stored draft; empty if there is none.
pub async fn load(room: &Room, thread: Option<&str>) -> Result<String> {
    let root = thread_root(thread)?;
    let draft = room.load_composer_draft(root.as_deref()).await?;
    Ok(draft
        // We never store an edit; one would not be text for a new message.
        .filter(|draft| !matches!(draft.draft_type, ComposerDraftType::Edit { .. }))
        .map(|draft| draft.plain_text)
        .unwrap_or_default())
}

/// Removes the stored drafts of a room and of these threads. Forgetting a room does
/// not remove them, and afterwards the client has no room to ask; so this goes to
/// the store itself, and works before and after.
pub async fn forget(client: &Client, room_id: &str, threads: &[String]) -> Result<()> {
    let room_id = RoomId::parse(room_id).map_err(|_| anyhow!("invalid room id"))?;
    let store = client.state_store();
    store
        .remove_kv_data(StateStoreDataKey::ComposerDraft(&room_id, None))
        .await?;
    for thread in threads {
        // One that is no event id was never stored.
        if let Some(root) = thread_root(Some(thread)).ok().flatten() {
            store
                .remove_kv_data(StateStoreDataKey::ComposerDraft(&room_id, Some(&root)))
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::matrix::{MatrixCmd, MatrixEvent, MatrixHandle, RoomCategory, spawn};
    use crate::paths::Paths;

    /// Waits for the room list, logging in if the worker asks for it; opens the shared
    /// test room and returns its id with the draft the worker reports for it.
    async fn open_first_room(
        handle: &MatrixHandle,
        events: &mut UnboundedReceiver<MatrixEvent>,
    ) -> (String, String) {
        let wait = async {
            let mut opened: Option<String> = None;
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::NeedLogin { .. } => handle.send(MatrixCmd::LoginPassword {
                        homeserver: std::env::var("MATRIX_RAT_TEST_HOMESERVER")
                            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned()),
                        username: std::env::var("MATRIX_RAT_TEST_USER")
                            .expect("MATRIX_RAT_TEST_USER"),
                        password: std::env::var("MATRIX_RAT_TEST_PASSWORD")
                            .expect("MATRIX_RAT_TEST_PASSWORD"),
                    }),
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) if opened.is_none() => {
                        let joined = rooms
                            .into_iter()
                            // Always the same room: the order of the list differs
                            // between a first and a later start.
                            .find(|room| {
                                room.category != RoomCategory::Invite && room.name == "Test Lounge"
                            });
                        if let Some(room) = joined {
                            handle.send(MatrixCmd::OpenRoom {
                                room_id: room.id.clone(),
                                generation: 1,
                            });
                            opened = Some(room.id);
                        }
                    }
                    MatrixEvent::Draft {
                        room_id,
                        thread: None,
                        text,
                    } if opened.as_deref() == Some(room_id.as_str()) => return (room_id, text),
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .expect("timed out waiting for the room's draft")
    }

    async fn shut_down(handle: MatrixHandle, mut events: UnboundedReceiver<MatrixEvent>) {
        handle.send(MatrixCmd::Shutdown);
        drop(handle);
        while events.recv().await.is_some() {}
    }

    /// A draft is saved right before the client stops, is there after a restart on the
    /// same store, and is gone after it was cleared. The user must have joined a room.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn draft_survives_a_restart_and_can_be_cleared() {
        let root = std::env::temp_dir().join(format!("matrix-rat-draft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();

        // First run: nothing stored yet. The save is the last thing before the
        // shutdown, as when the app quits.
        let (handle, mut events) = spawn(paths.clone());
        let (room_id, text) = open_first_room(&handle, &mut events).await;
        assert_eq!(text, "");
        handle.send(MatrixCmd::SaveDraft {
            room_id: room_id.clone(),
            text: "unsent words\nsecond line".to_owned(),
        });
        shut_down(handle, events).await;

        // Second run on the same store: the draft is back. Then it is cleared.
        let (handle, mut events) = spawn(paths.clone());
        let (again, text) = open_first_room(&handle, &mut events).await;
        assert_eq!(again, room_id);
        assert_eq!(text, "unsent words\nsecond line");
        handle.send(MatrixCmd::SaveDraft {
            room_id: room_id.clone(),
            text: String::new(),
        });
        shut_down(handle, events).await;

        // Third run: nothing is stored any more.
        let (handle, mut events) = spawn(paths.clone());
        let (again, text) = open_first_room(&handle, &mut events).await;
        assert_eq!(again, room_id);
        assert_eq!(text, "");
        shut_down(handle, events).await;
        let _ = std::fs::remove_dir_all(root);
    }
}
