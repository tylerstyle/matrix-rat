//! Sending files: read and prepared off the worker's loop, then handed to the SDK's
//! send queue, which shows them in the timeline at once, uploads (encrypting in
//! encrypted rooms), reports progress and keeps what failed for another try.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::attachment::{
    AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseVideoInfo, Thumbnail,
};
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::events::room::message::TextMessageEventContent;
use matrix_sdk::ruma::{OwnedEventId, UInt};
use matrix_sdk_ui::timeline::{AttachmentConfig, AttachmentSource, Timeline};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinSet;

use super::MatrixEvent;
use crate::format::Outgoing;
use crate::text::{sanitize_line, truncate};
use crate::upload::{self, Prepared};

/// How long sending waits to learn the server's size limit before going without.
const LIMIT_WAIT: Duration = Duration::from_secs(5);
const LIMIT_RETRY: Duration = Duration::from_secs(60);

pub struct Uploads {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    /// Files being prepared, and the question about the size limit. They end with the
    /// session: dropping this stops them. What the send queue has taken stays queued.
    tasks: JoinSet<()>,
}

impl Uploads {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        client.send_queue().enable_upload_progress(true);
        let mut tasks = JoinSet::new();
        // The UI refuses files that are too large before they are attached; for that
        // it has to know the limit. Asked again until the server answers.
        let (asker, tell) = (client.clone(), events.clone());
        tasks.spawn(async move {
            loop {
                match asker.load_or_fetch_max_upload_size().await {
                    Ok(limit) => {
                        let _ = tell.send(MatrixEvent::UploadLimit(limit.into()));
                        return;
                    }
                    Err(err) => tracing::debug!(?err, "could not ask for the upload limit"),
                }
                tokio::time::sleep(LIMIT_RETRY).await;
            }
        });
        Self {
            client,
            events,
            tasks,
        }
    }

    /// Sends `files` in order. The caption and the reply belong to the first: if that
    /// one cannot be sent, none is ([`MatrixEvent::FilesNotSent`]). A later file that
    /// cannot be read is reported and the rest still goes.
    pub fn send(
        &mut self,
        timeline: Arc<Timeline>,
        files: Vec<PathBuf>,
        caption: Option<Outgoing>,
        reply_to: Option<OwnedEventId>,
    ) {
        while self.tasks.try_join_next().is_some() {}
        let client = self.client.clone();
        let events = self.events.clone();
        self.tasks.spawn(async move {
            let limit = tokio::time::timeout(LIMIT_WAIT, client.load_or_fetch_max_upload_size())
                .await
                .ok()
                .and_then(Result::ok)
                .map(u64::from);
            let (mut caption, mut caption_mentions) =
                caption.map(caption_content).unwrap_or((None, None));
            let mut reply_to = reply_to;
            for (n, path) in files.into_iter().enumerate() {
                let prepared =
                    tokio::task::spawn_blocking(move || upload::prepare(&path, limit)).await;
                let outcome = match prepared {
                    Ok(Ok(prepared)) => {
                        let name = prepared.name.clone();
                        match attachment(prepared) {
                            Some((source, mime, mut config)) => {
                                // Only ever with the first file.
                                config.caption = caption.take();
                                config.mentions = caption_mentions.take();
                                config.in_reply_to = reply_to.take();
                                timeline
                                    .send_attachment(source, mime, config)
                                    .use_send_queue()
                                    .await
                                    .map_err(|err| {
                                        tracing::warn!(?err, "could not queue a file for sending");
                                        short(&format!("Could not send {name}: {err}"))
                                    })
                            }
                            None => Err(short(&format!("Could not send {name}"))),
                        }
                    }
                    // The reason names the file; the path stays out of the log.
                    Ok(Err(reason)) => {
                        tracing::warn!("a file could not be prepared for sending");
                        Err(short(&reason))
                    }
                    Err(err) => {
                        tracing::warn!(?err, "preparing a file for sending failed");
                        Err("Could not send the file".to_owned())
                    }
                };
                match outcome {
                    Ok(()) => {}
                    Err(reason) if n == 0 => {
                        let _ = events.send(MatrixEvent::FilesNotSent(reason));
                        return;
                    }
                    Err(reason) => {
                        let _ = events.send(MatrixEvent::ActionFailed(reason));
                    }
                }
                // What belonged to the first file goes with no other.
                caption = None;
                caption_mentions = None;
                reply_to = None;
            }
        });
    }
}

fn caption_content(
    caption: Outgoing,
) -> (
    Option<TextMessageEventContent>,
    Option<matrix_sdk::ruma::events::Mentions>,
) {
    match caption {
        Outgoing::Text {
            plain,
            html: None,
            mentions,
        } => (Some(TextMessageEventContent::plain(plain)), mentions),
        Outgoing::Text {
            plain,
            html: Some(html),
            mentions,
        } => (Some(TextMessageEventContent::html(plain, html)), mentions),
        // The UI does not offer these as captions.
        Outgoing::Emote { .. } | Outgoing::Rejected(_) => (None, None),
    }
}

/// What the SDK needs to send a prepared file. `None` if its type cannot be expressed
/// (never the case for the types this app assigns).
fn attachment(prepared: Prepared) -> Option<(AttachmentSource, mime::Mime, AttachmentConfig)> {
    let mime: mime::Mime = prepared.mime.parse().ok()?;
    let size = UInt::new(prepared.bytes.len() as u64);
    // The kind of message follows the type, and so must the details given with it.
    let info = match mime.type_() {
        mime::IMAGE => AttachmentInfo::Image(BaseImageInfo {
            width: prepared.picture.as_ref().map(|p| p.width.into()),
            height: prepared.picture.as_ref().map(|p| p.height.into()),
            size,
            ..Default::default()
        }),
        mime::VIDEO => AttachmentInfo::Video(BaseVideoInfo {
            size,
            ..Default::default()
        }),
        mime::AUDIO => AttachmentInfo::Audio(BaseAudioInfo {
            size,
            ..Default::default()
        }),
        _ => AttachmentInfo::File(BaseFileInfo { size }),
    };
    let thumbnail = prepared
        .picture
        .and_then(|picture| picture.thumbnail)
        .and_then(|small| {
            Some(Thumbnail {
                size: UInt::new(small.bytes.len() as u64)?,
                data: small.bytes,
                content_type: small.mime.parse().ok()?,
                width: small.width.into(),
                height: small.height.into(),
            })
        });
    let config = AttachmentConfig {
        info: Some(info),
        thumbnail,
        ..Default::default()
    };
    let source = AttachmentSource::Data {
        bytes: prepared.bytes,
        filename: prepared.name,
    };
    Some((source, mime, config))
}

/// Why a message of ours could not be sent, in a few words for the user.
pub fn failure_reason(error: &matrix_sdk::Error) -> String {
    let matrix_sdk::Error::Http(http) = error else {
        return short(&error.to_string());
    };
    if matches!(http.client_api_error_kind(), Some(ErrorKind::TooLarge)) {
        return "too large for the server".to_owned();
    }
    match http.as_client_api_error() {
        Some(api) if api.status_code.as_u16() == 413 => "too large for the server".to_owned(),
        Some(api) if api.status_code.as_u16() == 403 => "not allowed in this room".to_owned(),
        Some(api) if api.status_code.as_u16() == 429 => "the server asks to slow down".to_owned(),
        Some(api) => short(&format!("the server said: {api}")),
        None => "the server could not be reached".to_owned(),
    }
}

/// One line of bounded length without control characters: the text may come from the
/// server.
fn short(text: &str) -> String {
    truncate(sanitize_line(text).trim(), 120)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::{MatrixCmd, RoomCategory, spawn};
    use super::*;
    use crate::model::{ImageSource, ItemRef, SendState};
    use crate::paths::Paths;

    #[test]
    fn the_kind_of_message_follows_the_type() {
        let prepared = |name: &str, mime: &str, picture| Prepared {
            name: name.to_owned(),
            bytes: vec![1, 2, 3],
            mime: mime.to_owned(),
            picture,
        };
        let (source, mime, config) = attachment(prepared(
            "cat.png",
            "image/png",
            Some(upload::Picture {
                width: 1600,
                height: 900,
                thumbnail: Some(upload::Thumbnail {
                    bytes: vec![9; 10],
                    mime: "image/jpeg",
                    width: 800,
                    height: 450,
                }),
            }),
        ))
        .unwrap();
        assert_eq!(mime, mime::IMAGE_PNG);
        assert!(matches!(
            source,
            AttachmentSource::Data { ref filename, ref bytes } if filename == "cat.png" && bytes.len() == 3
        ));
        let Some(AttachmentInfo::Image(info)) = config.info else {
            panic!("not a picture");
        };
        assert_eq!(
            (info.width, info.height, info.size),
            (UInt::new(1600), UInt::new(900), UInt::new(3))
        );
        let thumbnail = config.thumbnail.unwrap();
        assert_eq!(thumbnail.content_type, mime::IMAGE_JPEG);
        assert_eq!(u64::from(thumbnail.size), 10);

        // A picture this app cannot read still goes out as one, without a size in pixels.
        let (_, _, config) = attachment(prepared("drawing.svg", "image/svg+xml", None)).unwrap();
        assert!(matches!(
            config.info,
            Some(AttachmentInfo::Image(BaseImageInfo {
                width: None,
                size: Some(_),
                ..
            }))
        ));
        for (mime, check) in [
            (
                "video/mp4",
                (|info| matches!(info, AttachmentInfo::Video(_))) as fn(&AttachmentInfo) -> bool,
            ),
            ("audio/ogg", |info| matches!(info, AttachmentInfo::Audio(_))),
            ("application/pdf", |info| {
                matches!(info, AttachmentInfo::File(BaseFileInfo { size: Some(_) }))
            }),
            ("text/plain", |info| matches!(info, AttachmentInfo::File(_))),
        ] {
            let (_, _, config) = attachment(prepared("x", mime, None)).unwrap();
            assert!(check(config.info.as_ref().unwrap()), "{mime}");
        }
    }

    #[test]
    fn captions_keep_their_formatting() {
        let caption = caption_content(crate::format::parse("look at **this**"))
            .0
            .unwrap();
        assert_eq!(caption.body, "look at **this**");
        assert_eq!(
            caption.formatted.unwrap().body,
            "look at <strong>this</strong>"
        );
        let plain = caption_content(crate::format::parse("just words"))
            .0
            .unwrap();
        assert!(plain.formatted.is_none());
        assert!(
            caption_content(crate::format::parse("/me waves"))
                .0
                .is_none()
        );
    }

    /// Sends a text file and a picture through the worker and waits until the server
    /// has them: as a file and a picture message with the right name, size, caption
    /// and pixel size, and with content that can be fetched back. Setup as for the
    /// other end-to-end tests:
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn files_are_sent_and_arrive() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let root = std::env::temp_dir().join(format!("matrix-rat-upload-{}", std::process::id()));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();

        let text_name = format!("notes-{}.txt", std::process::id());
        let text_content = format!("some notes\nfrom test run {}\n", std::process::id());
        let picture_name = format!("palette-{}.png", std::process::id());
        let mut png = Vec::new();
        crate::images::demo_picture(1200, 700)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let text_path = root.join(&text_name);
        let picture_path = root.join(&picture_name);
        std::fs::write(&text_path, &text_content).unwrap();
        std::fs::write(&picture_path, &png).unwrap();
        let caption = format!("the caption {}", std::process::id());

        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username,
            password,
        });

        let wait = async {
            let mut room_id = None;
            let mut limit = None;
            let mut saw_echo = false;
            let mut saw_progress = false;
            let mut refused = None;
            let mut fetched = false;
            let mut whole = None;
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err)
                    | MatrixEvent::Error(err)
                    | MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    MatrixEvent::UploadLimit(bytes) => limit = Some(bytes),
                    MatrixEvent::FilesNotSent(reason) => refused = Some(reason),
                    MatrixEvent::Rooms(rooms) if room_id.is_none() => {
                        let joined = rooms.iter().find(|r| r.category != RoomCategory::Invite);
                        if let Some(room) = joined {
                            room_id = Some(room.id.clone());
                            handle.send(MatrixCmd::OpenRoom {
                                room_id: room.id.clone(),
                                generation: 1,
                            });
                            // First a batch whose first file is gone: the caption was
                            // meant for that one, so the second file must not go either.
                            handle.send(MatrixCmd::SendFiles {
                                room_id: room.id.clone(),
                                files: vec![root.join("gone.txt"), text_path.clone()],
                                caption: Some(crate::format::parse("for the file that is gone")),
                                reply_to: None,
                            });
                            let command = MatrixCmd::SendFiles {
                                room_id: room.id.clone(),
                                files: vec![text_path.clone(), picture_path.clone()],
                                caption: Some(crate::format::parse(&format!("**{caption}**"))),
                                reply_to: None,
                            };
                            // Neither the paths nor the caption may reach a log.
                            let debug = format!("{command:?}");
                            assert!(!debug.contains("notes-") && !debug.contains(&caption));
                            handle.send(command);
                        }
                    }
                    MatrixEvent::Timeline {
                        generation: 1,
                        messages,
                        ..
                    } => {
                        let file = messages
                            .iter()
                            .find(|m| m.file.as_ref().is_some_and(|f| f.name == text_name));
                        let picture = messages
                            .iter()
                            .find(|m| m.image.as_ref().is_some_and(|i| i.name == picture_name));
                        saw_echo |= [file, picture]
                            .iter()
                            .flatten()
                            .any(|m| m.state == SendState::Sending);
                        saw_progress |= messages.iter().any(|m| {
                            m.upload
                                .is_some_and(|(sent, total)| sent <= total && total > 0)
                        });
                        let (Some(file), Some(picture)) = (file, picture) else {
                            continue;
                        };
                        let sent = |m: &crate::model::Message| {
                            m.state == SendState::Sent
                                && m.own
                                && matches!(m.item, Some(ItemRef::Event(_)))
                        };
                        if !sent(file) || !sent(picture) || fetched {
                            continue;
                        }
                        // Once only: the batch that could not start sent nothing.
                        let copies = messages
                            .iter()
                            .filter(|m| m.file.as_ref().is_some_and(|f| f.name == text_name))
                            .count();
                        assert_eq!(copies, 1);
                        // The file: name, size, and the caption went with it (the first).
                        let attached = file.file.as_ref().unwrap();
                        assert_eq!(attached.size, Some(text_content.len() as u64));
                        assert!(attached.key.starts_with("mxc://"));
                        assert!(file.body.contains(&text_name), "{}", file.body);
                        assert!(file.body.contains(&caption), "{}", file.body);
                        // The picture: size in bytes and pixels, a small version, no caption.
                        let image = picture.image.as_ref().unwrap();
                        assert_eq!(image.size, Some(png.len() as u64));
                        assert_eq!(image.dimensions, Some((1200, 700)));
                        assert_eq!(image.caption, None);
                        let ImageSource::Matrix(media) = &image.source else {
                            panic!("not a Matrix source");
                        };
                        assert!(media.thumbnail.is_some(), "no small version was sent");
                        // In the order they were attached.
                        let position = |wanted: &crate::model::Message| {
                            messages.iter().position(|m| m.id == wanted.id).unwrap()
                        };
                        assert!(position(file) < position(picture));
                        fetched = true;
                        handle.send(MatrixCmd::FetchImage {
                            key: image.key.clone(),
                            media: media.clone(),
                            size: image.size,
                            full: true,
                        });
                    }
                    MatrixEvent::Image {
                        full: true, result, ..
                    } => {
                        let decoded = result.expect("the sent picture could not be loaded");
                        whole = Some(decoded.original);
                    }
                    _ => {}
                }
                if whole.is_some() && limit.is_some() {
                    return (saw_echo, saw_progress, whole, limit, refused);
                }
            }
        };
        let (saw_echo, saw_progress, whole, limit, refused) =
            tokio::time::timeout(Duration::from_secs(60), wait)
                .await
                .expect("timed out waiting for the sent files");
        assert!(saw_echo, "the files never showed as being sent");
        assert!(saw_progress, "no upload progress was reported");
        assert_eq!(refused.as_deref(), Some("gone.txt does not exist"));
        assert_eq!(whole, Some((1200, 700)));
        assert!(limit.is_some_and(|bytes| bytes > 0));
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }
}
