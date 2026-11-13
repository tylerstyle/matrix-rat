//! Fetching pictures: download through the SDK (which caches and decrypts), decode on a
//! blocking thread, hand the pixels to the UI.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings};
use matrix_sdk::ruma::UInt;
use matrix_sdk::ruma::events::room::MediaSource;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;

use super::MatrixEvent;
use crate::files;
use crate::images::{self, Decoded, FULL_PIXELS, MAX_DOWNLOAD, THUMBNAIL_PIXELS, human_size};
use crate::model::{Attachment, MediaRef};

/// Downloads running at the same time.
const PARALLEL: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(60);
/// Saving is asked for by the user and may be a large file.
const SAVE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Below this size the file itself is fetched instead of a thumbnail: it is needed for
/// the viewer anyway and saves the server the scaling.
const SMALL_FILE: u64 = 256 * 1024;
/// Why a picture is not shown when the details are of no use to the user.
const FAILED: &str = "could not be loaded";

pub struct Media {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    /// What is being fetched right now: key and whether it is the whole file.
    busy: Arc<Mutex<HashSet<(String, bool)>>>,
    slots: Arc<Semaphore>,
}

impl Media {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        Self {
            client,
            events,
            busy: Arc::default(),
            slots: Arc::new(Semaphore::new(PARALLEL)),
        }
    }

    /// Fetches and decodes in the background; the outcome arrives as
    /// [`MatrixEvent::Image`]. A picture that is already on its way is not fetched twice.
    pub fn fetch(&self, key: String, media: MediaRef, size: Option<u64>, full: bool) {
        let id = (key, full);
        if !self.busy.lock().expect("not poisoned").insert(id.clone()) {
            return;
        }
        let client = self.client.clone();
        let events = self.events.clone();
        let busy = self.busy.clone();
        let slots = self.slots.clone();
        tokio::spawn(async move {
            let result = async {
                let _slot = slots.acquire().await.map_err(|_| FAILED.to_owned())?;
                let work = load(&client, &media, size, full);
                match tokio::time::timeout(TIMEOUT, work).await {
                    Ok(result) => result,
                    Err(_) => Err("took too long to load".to_owned()),
                }
            }
            .await;
            busy.lock().expect("not poisoned").remove(&id);
            let (key, full) = id;
            let _ = events.send(MatrixEvent::Image { key, full, result });
        });
    }
}

impl Media {
    /// Downloads a file (decrypting it if needed) and writes it into `dir` under a
    /// name that is free there. The outcome arrives as [`MatrixEvent::FileSaved`].
    pub fn save(&self, file: Attachment, dir: PathBuf, open: bool) {
        let id = (format!("save {}", file.key), true);
        if !self.busy.lock().expect("not poisoned").insert(id.clone()) {
            return;
        }
        let client = self.client.clone();
        let events = self.events.clone();
        let busy = self.busy.clone();
        tokio::spawn(async move {
            let request = MediaRequestParameters {
                source: *file.source,
                format: MediaFormat::File,
            };
            let work = async {
                // Not kept in the SDK's cache: the copy on disk is the point.
                let bytes = client
                    .media()
                    .get_media_content(&request, false)
                    .await
                    .map_err(|err| {
                        tracing::debug!(%err, "could not download file");
                        "the download failed".to_owned()
                    })?;
                let name = file.name;
                tokio::task::spawn_blocking(move || files::write_new(&dir, &name, &bytes))
                    .await
                    .map_err(|_| "writing the file failed".to_owned())?
                    .map_err(|err| format!("{err}"))
            };
            let result = match tokio::time::timeout(SAVE_TIMEOUT, work).await {
                Ok(result) => result,
                Err(_) => Err("the download took too long".to_owned()),
            };
            busy.lock().expect("not poisoned").remove(&id);
            let _ = events.send(MatrixEvent::FileSaved {
                key: file.key,
                open,
                result,
            });
        });
    }
}

async fn load(
    client: &Client,
    media: &MediaRef,
    size: Option<u64>,
    full: bool,
) -> Result<Decoded, String> {
    let bytes = download(client, media, size, full).await?;
    let max = if full { FULL_PIXELS } else { THUMBNAIL_PIXELS };
    // Decoding is CPU work and must not hold up the async threads.
    tokio::task::spawn_blocking(move || images::decode(&bytes, max))
        .await
        .map_err(|err| {
            tracing::warn!(%err, "image decoder crashed");
            FAILED.to_owned()
        })?
}

fn too_large(size: u64) -> String {
    format!("too large to show ({})", human_size(size))
}

async fn get(
    client: &Client,
    source: &MediaSource,
    format: MediaFormat,
) -> Result<Vec<u8>, String> {
    let request = MediaRequestParameters {
        source: source.clone(),
        format,
    };
    let bytes = client
        .media()
        .get_media_content(&request, true)
        .await
        .map_err(|err| {
            tracing::debug!(%err, "could not download media");
            FAILED.to_owned()
        })?;
    if bytes.len() as u64 > MAX_DOWNLOAD {
        return Err(too_large(bytes.len() as u64));
    }
    Ok(bytes)
}

/// A small version if there is one (unless the whole file is wanted), else the file.
async fn download(
    client: &Client,
    media: &MediaRef,
    size: Option<u64>,
    full: bool,
) -> Result<Vec<u8>, String> {
    let small_file = size.is_some_and(|size| size <= SMALL_FILE);
    if !full && !small_file {
        match (&media.thumbnail, &media.full) {
            // The sender's own thumbnail; the only kind there is for encrypted files.
            (Some((thumbnail, thumbnail_size)), _)
                if thumbnail_size.is_none_or(|size| size <= MAX_DOWNLOAD) =>
            {
                if let Ok(bytes) = get(client, thumbnail, MediaFormat::File).await {
                    return Ok(bytes);
                }
            }
            // The server can scale an unencrypted file down for us.
            (None, MediaSource::Plain(_)) => {
                let (width, height) = THUMBNAIL_PIXELS;
                let settings = MediaThumbnailSettings::new(UInt::from(width), UInt::from(height));
                if let Ok(bytes) = get(client, &media.full, MediaFormat::Thumbnail(settings)).await
                {
                    return Ok(bytes);
                }
            }
            _ => {}
        }
    }
    if let Some(size) = size.filter(|size| *size > MAX_DOWNLOAD) {
        return Err(too_large(size));
    }
    get(client, &media.full, MediaFormat::File).await
}

#[cfg(test)]
mod tests {
    use matrix_sdk::attachment::AttachmentConfig;
    use matrix_sdk::config::SyncSettings;

    use super::*;
    use crate::matrix::{MatrixCmd, spawn};
    use crate::model::ImageSource;
    use crate::paths::Paths;

    /// Uploads a PNG into an unencrypted room of the test user, then checks that the
    /// worker presents it as a picture and can fetch and decode both the small version
    /// and the whole file. Setup as for the other end-to-end tests:
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn uploaded_picture_is_fetched_and_decoded() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");

        // The upload is done by a plain SDK client: the app cannot send pictures yet.
        let uploader = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        uploader
            .matrix_auth()
            .login_username(&username, &password)
            .initial_device_display_name("matrix-rat image test")
            .send()
            .await
            .unwrap();
        uploader.sync_once(SyncSettings::default()).await.unwrap();
        let room = uploader
            .joined_rooms()
            .into_iter()
            .find(|room| !room.encryption_state().is_encrypted())
            .expect("the test user has joined an unencrypted room");
        let name = format!("rat-{}.png", std::process::id());
        let mut png = Vec::new();
        images::demo_picture(1200, 700)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let uploaded = png.clone();
        room.send_attachment(&name, &mime::IMAGE_PNG, png, AttachmentConfig::new())
            .await
            .unwrap();
        let room_id = room.room_id().to_string();

        let root = std::env::temp_dir().join(format!("matrix-rat-image-{}", std::process::id()));
        let downloads = root.join("downloads");
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
            password,
        });

        let wait = async {
            let mut opened = false;
            let mut requested = false;
            let mut small = None;
            let mut whole = None;
            let mut saved = None;
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err)
                    | MatrixEvent::Error(err)
                    | MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) if !opened => {
                        if rooms.iter().any(|room| room.id == room_id) {
                            opened = true;
                            handle.send(MatrixCmd::OpenRoom {
                                room_id: room_id.clone(),
                                generation: 1,
                            });
                        }
                    }
                    MatrixEvent::Timeline { messages, .. } if !requested => {
                        let found = messages
                            .iter()
                            .filter_map(|m| m.image.as_ref())
                            .find(|image| image.name == name);
                        if let Some(image) = found {
                            requested = true;
                            assert!(image.key.starts_with("mxc://"));
                            let ImageSource::Matrix(media) = &image.source else {
                                panic!("not a Matrix source");
                            };
                            // The duplicate is dropped while the first one is running.
                            for full in [false, true, true] {
                                handle.send(MatrixCmd::FetchImage {
                                    key: image.key.clone(),
                                    media: media.clone(),
                                    size: image.size,
                                    full,
                                });
                            }
                            handle.send(MatrixCmd::SaveFile {
                                file: image.attachment().expect("can be saved"),
                                dir: downloads.clone(),
                                open: false,
                            });
                        }
                    }
                    MatrixEvent::Image { full, result, .. } => {
                        let decoded = result.expect("picture could not be loaded");
                        let size = (decoded.pixels.width(), decoded.pixels.height());
                        if full {
                            assert!(whole.is_none(), "fetched twice");
                            whole = Some((decoded.original, size));
                        } else {
                            small = Some((decoded.original, size));
                        }
                    }
                    MatrixEvent::FileSaved { result, .. } => {
                        saved = Some(result.expect("file could not be saved"));
                    }
                    _ => {}
                }
                if let (Some(small), Some(whole), Some(saved)) = (small, whole, &saved) {
                    return (small, whole, saved.clone());
                }
            }
        };
        let (small, whole, saved) = tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .expect("timed out waiting for the picture");
        // The whole file as uploaded; the small version within the thumbnail bounds
        // and of the same shape.
        assert_eq!(whole, ((1200, 700), (1200, 700)));
        let (_, (width, height)) = small;
        assert!(width <= THUMBNAIL_PIXELS.0 && height <= THUMBNAIL_PIXELS.1);
        assert!((width * 700).abs_diff(height * 1200) <= 1200, "{small:?}");
        // The saved file is the upload, byte for byte, under its own name.
        assert_eq!(saved, downloads.join(&name));
        assert_eq!(std::fs::read(&saved).unwrap(), uploaded);
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }
}
