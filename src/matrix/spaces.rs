//! Spaces: which of the user's rooms belong to which space, for filtering the room
//! list. The SDK keeps the hierarchy up to date; this turns it into plain data.

use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::spaces::SpaceFilter;

use super::left::LeftRooms;
use crate::text::sanitize_line;

/// A space the room list can be narrowed to.
#[derive(Clone, PartialEq, Eq)]
pub struct SpaceSummary {
    pub id: String,
    pub name: String,
    /// 0 for a top-level space, 1 for a space inside one.
    pub level: u8,
    /// Ids of the rooms in it (for a top-level space: directly in it).
    pub rooms: Vec<String>,
}

/// Hand-written: which space, its name and its rooms stay out of logs.
impl std::fmt::Debug for SpaceSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SpaceSummary(level {}, {} rooms)",
            self.level,
            self.rooms.len()
        )
    }
}

pub fn summarize(filters: &Vector<SpaceFilter>, left: &LeftRooms) -> Vec<SpaceSummary> {
    filters
        .iter()
        // A space (or room) left here can look joined to the SDK (see `super::left`):
        // it is not one of the user's any more.
        .filter(|filter| !left.is_left(&filter.space_room.room_id))
        .map(|filter| {
            let name = sanitize_line(&filter.space_room.display_name);
            SpaceSummary {
                id: filter.space_room.room_id.to_string(),
                name: if name.trim().is_empty() {
                    "Unnamed space".to_owned()
                } else {
                    name
                },
                level: filter.level,
                rooms: filter
                    .descendants
                    .iter()
                    .filter(|id| !left.is_left(id))
                    .map(|id| id.to_string())
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use matrix_sdk::Client;
    use matrix_sdk::config::SyncSettings;
    use matrix_sdk::ruma::api::client::room::create_room::v3::{CreationContent, Request};
    use matrix_sdk::ruma::events::space::child::SpaceChildEventContent;
    use matrix_sdk::ruma::room::RoomType;
    use matrix_sdk::ruma::serde::Raw;

    use crate::matrix::{MatrixCmd, MatrixEvent, spawn};
    use crate::paths::Paths;

    #[test]
    fn a_space_or_room_left_here_is_not_offered() {
        use matrix_sdk::ruma::{OwnedRoomId, room_id};
        use matrix_sdk_ui::spaces::{SpaceFilter, SpaceRoom};

        use super::summarize;
        use crate::matrix::left::LeftRooms;

        let filter = |id: &str, rooms: &[&str]| SpaceFilter {
            space_room: SpaceRoom {
                room_id: OwnedRoomId::try_from(id).unwrap(),
                canonical_alias: None,
                name: None,
                display_name: "A space".to_owned(),
                topic: None,
                avatar_url: None,
                room_type: None,
                num_joined_members: 1,
                join_rule: None,
                world_readable: None,
                guest_can_join: false,
                is_direct: None,
                children_count: 0,
                state: Some(matrix_sdk::RoomState::Joined),
                heroes: None,
                via: Vec::new(),
                suggested: false,
                is_dm: None,
            },
            level: 0,
            descendants: rooms
                .iter()
                .map(|room| OwnedRoomId::try_from(*room).unwrap())
                .collect(),
        };
        let filters: matrix_sdk_ui::eyeball_im::Vector<SpaceFilter> = [
            filter("!kept:localhost", &["!in:localhost", "!out:localhost"]),
            filter("!left:localhost", &["!in:localhost"]),
        ]
        .into_iter()
        .collect();
        let left = LeftRooms::default();
        assert_eq!(summarize(&filters, &left).len(), 2);
        left.mark(room_id!("!left:localhost"));
        left.mark(room_id!("!out:localhost"));
        let spaces = summarize(&filters, &left);
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].id, "!kept:localhost");
        assert_eq!(spaces[0].rooms, ["!in:localhost"]);
    }

    /// A space with one room in it shows up as a space that contains that room.
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_space_and_its_room_are_reported() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");

        // Set up with a plain SDK client: the app cannot create spaces.
        let setup = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        setup
            .matrix_auth()
            .login_username(&username, &password)
            .initial_device_display_name("matrix-rat space test")
            .send()
            .await
            .unwrap();
        let stamp = std::process::id();
        let mut request = Request::new();
        request.name = Some(format!("Test space {stamp}"));
        let mut content = CreationContent::new();
        content.room_type = Some(RoomType::Space);
        request.creation_content = Some(Raw::new(&content).unwrap());
        let space = setup.create_room(request).await.unwrap();
        let mut request = Request::new();
        request.name = Some(format!("Room in space {stamp}"));
        let room = setup.create_room(request).await.unwrap();
        let via = vec![setup.user_id().unwrap().server_name().to_owned()];
        space
            .send_state_event_for_key(room.room_id(), SpaceChildEventContent::new(via))
            .await
            .unwrap();
        setup.sync_once(SyncSettings::default()).await.unwrap();

        let root = std::env::temp_dir().join(format!("matrix-rat-space-{stamp}"));
        let _ = std::fs::remove_dir_all(&root);
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
        let wait = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Spaces(spaces) => {
                        let found = spaces
                            .into_iter()
                            .find(|listed| listed.id == space.room_id().as_str());
                        if let Some(found) = found
                            && found.rooms.iter().any(|id| id == room.room_id().as_str())
                        {
                            return found;
                        }
                    }
                    _ => {}
                }
            }
        };
        let found = tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .expect("timed out waiting for the space");
        assert_eq!(found.name, format!("Test space {stamp}"));
        assert_eq!(found.level, 0);
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        // Leave no clutter for the other tests.
        let _ = room.leave().await;
        let _ = space.leave().await;
        let _ = setup.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
