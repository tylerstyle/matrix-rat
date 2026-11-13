//! Fetching pictures: a download that stops at a byte limit, the SDK's cache for what
//! passed it, decoding on a blocking thread, pixels handed to the UI.

use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::encryption::MediaEncryptionInfo;
use matrix_sdk::media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings};
use matrix_sdk::reqwest::{self, StatusCode};
use matrix_sdk::ruma::api::client::media::get_content_thumbnail::v3::Method;
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::{MxcUri, UInt};
use matrix_sdk_base::crypto::AttachmentDecryptor;
use matrix_sdk_base::media::store::IgnoreMediaRetentionPolicy;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinSet;
use url::Url;

use super::MatrixEvent;
use crate::files;
use crate::images::{
    self, AVATAR_PIXELS, Decoded, FULL_PIXELS, MAX_DOWNLOAD, THUMBNAIL_PIXELS, human_size,
};
use crate::model::{Attachment, MediaRef};

/// Downloads running at the same time.
const PARALLEL: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(60);
/// Saving is asked for by the user and may be a large file.
const SAVE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Below this size the file itself is fetched instead of a thumbnail: it is needed for
/// the viewer anyway and saves the server the scaling.
const SMALL_FILE: u64 = 256 * 1024;
/// How much is read of a file that was said to be small before giving that up.
const SMALL_FILE_LIMIT: u64 = 1024 * 1024;
/// An avatar larger than this is not worth showing in two cells.
const AVATAR_DOWNLOAD: u64 = 2 * 1024 * 1024;
/// Why a picture is not shown when the details are of no use to the user.
const FAILED: &str = "could not be loaded";

pub struct Media {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    /// What is being fetched right now: key and whether it is the whole file.
    busy: Arc<Mutex<HashSet<(String, bool)>>>,
    slots: Arc<Semaphore>,
    /// Saving holds a whole file in memory, so only one runs at a time.
    save_slot: Arc<Semaphore>,
    /// The running picture fetches. They end with the session: dropping this stops them.
    fetches: Mutex<JoinSet<()>>,
}

impl Media {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        Self {
            client,
            events,
            busy: Arc::default(),
            slots: Arc::new(Semaphore::new(PARALLEL)),
            save_slot: Arc::new(Semaphore::new(1)),
            fetches: Mutex::default(),
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
        let mut fetches = self.fetches.lock().expect("not poisoned");
        // Forget the ones that are done.
        while fetches.try_join_next().is_some() {}
        fetches.spawn(async move {
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
    /// Fetches the small square picture of a room or person; the outcome arrives as
    /// [`MatrixEvent::Avatar`]. `url` is its `mxc://` address.
    pub fn fetch_avatar(&self, url: String) {
        let id = (format!("avatar {url}"), false);
        if !self.busy.lock().expect("not poisoned").insert(id.clone()) {
            return;
        }
        let client = self.client.clone();
        let events = self.events.clone();
        let busy = self.busy.clone();
        let slots = self.slots.clone();
        let mut fetches = self.fetches.lock().expect("not poisoned");
        while fetches.try_join_next().is_some() {}
        fetches.spawn(async move {
            let result = async {
                let _slot = slots.acquire().await.map_err(|_| FAILED.to_owned())?;
                match tokio::time::timeout(TIMEOUT, load_avatar(&client, &url)).await {
                    Ok(result) => result,
                    Err(_) => Err(FAILED.to_owned()),
                }
            }
            .await;
            busy.lock().expect("not poisoned").remove(&id);
            let _ = events.send(MatrixEvent::Avatar { url, result });
        });
    }

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
        let save_slot = self.save_slot.clone();
        tokio::spawn(async move {
            let _slot = save_slot.acquire().await;
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

/// A square thumbnail from the server; the file itself where the server has none.
async fn load_avatar(client: &Client, url: &str) -> Result<Decoded, String> {
    let source = MediaSource::Plain(<&MxcUri>::from(url).to_owned());
    let thumb = Some(Thumb::crop(AVATAR_PIXELS));
    let bytes = match get(client, &source, thumb, AVATAR_DOWNLOAD).await {
        Ok(bytes) => bytes,
        Err(_) => get(client, &source, None, AVATAR_DOWNLOAD)
            .await
            .map_err(|refused| refused.reason())?,
    };
    tokio::task::spawn_blocking(move || images::decode_avatar(&bytes))
        .await
        .map_err(|_| FAILED.to_owned())?
}

/// Why a download was given up.
#[derive(Debug, PartialEq, Eq)]
enum Refused {
    /// More bytes than allowed; the size if the server stated one.
    TooLarge(Option<u64>),
    Failed,
}

impl Refused {
    fn reason(&self) -> String {
        match self {
            Self::TooLarge(Some(size)) => format!("too large to show ({})", human_size(*size)),
            Self::TooLarge(None) => {
                format!("too large to show (over {})", human_size(MAX_DOWNLOAD))
            }
            Self::Failed => FAILED.to_owned(),
        }
    }
}

/// Reads the body of `url` into memory, but never more than `limit` bytes: a larger
/// `Content-Length` is refused before the body is read, and a body that turns out
/// longer is dropped as soon as it passes the limit. `Ok(None)`: the server does not
/// know this address (an older server without authenticated media).
async fn bounded_get(
    client: &Client,
    url: Url,
    authenticated: bool,
    limit: u64,
) -> Result<Option<Vec<u8>>, Refused> {
    let failed = |err: reqwest::Error| {
        tracing::debug!(%err, "could not download media");
        Refused::Failed
    };
    let mut refreshed = false;
    let mut response = loop {
        let mut request = client.http_client().get(url.clone());
        if authenticated && let Some(token) = client.access_token() {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(failed)?;
        // An expired token: have the SDK get a new one, once.
        if response.status() == StatusCode::UNAUTHORIZED
            && authenticated
            && !refreshed
            && client.refresh_access_token().await.is_ok()
        {
            refreshed = true;
            continue;
        }
        break response;
    };
    let status = response.status();
    if authenticated && matches!(status, StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST) {
        // Either the file or the endpoint is unknown; the caller tries the old one.
        return Ok(None);
    }
    if !status.is_success() {
        tracing::debug!(%status, "media download refused");
        return Err(Refused::Failed);
    }
    if let Some(length) = response.content_length().filter(|length| *length > limit) {
        return Err(Refused::TooLarge(Some(length)));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(failed)? {
        if (bytes.len() + chunk.len()) as u64 > limit {
            return Err(Refused::TooLarge(None));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(bytes))
}

/// The address of a file or its thumbnail on the homeserver.
fn media_url(
    client: &Client,
    uri: &MxcUri,
    thumbnail: Option<Thumb>,
    authenticated: bool,
) -> Result<Url, Refused> {
    let (server, id) = uri.parts().map_err(|_| Refused::Failed)?;
    let kind = if thumbnail.is_some() {
        "thumbnail"
    } else {
        "download"
    };
    let prefix: &[&str] = if authenticated {
        &["_matrix", "client", "v1", "media"]
    } else {
        &["_matrix", "media", "v3"]
    };
    let mut url = client.homeserver();
    url.path_segments_mut()
        .map_err(|_| Refused::Failed)?
        .pop_if_empty()
        .extend(prefix)
        .extend([kind, server.as_str(), id]);
    url.set_query(None);
    if let Some(thumb) = thumbnail {
        url.query_pairs_mut()
            .append_pair("width", &thumb.width.to_string())
            .append_pair("height", &thumb.height.to_string())
            .append_pair("method", if thumb.crop { "crop" } else { "scale" });
    }
    Ok(url)
}

/// A smaller version of a file, made by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Thumb {
    width: u32,
    height: u32,
    /// Cut to exactly this shape instead of fitting inside it.
    crop: bool,
}

impl Thumb {
    fn scale((width, height): (u32, u32)) -> Self {
        Self {
            width,
            height,
            crop: false,
        }
    }

    fn crop((width, height): (u32, u32)) -> Self {
        Self {
            width,
            height,
            crop: true,
        }
    }
}

/// Fetches a file (or, for an unencrypted one, a thumbnail of `thumbnail` pixels) of
/// at most `limit` bytes. The SDK's own download reads a body of any size into memory
/// and into its cache before we get to see it, so the transfer is done here; only
/// what passed the limit is put into the SDK's cache afterwards.
async fn get(
    client: &Client,
    source: &MediaSource,
    thumbnail: Option<Thumb>,
    limit: u64,
) -> Result<Vec<u8>, Refused> {
    let request = MediaRequestParameters {
        source: source.clone(),
        format: match thumbnail {
            Some(thumb) => MediaFormat::Thumbnail(MediaThumbnailSettings::with_method(
                if thumb.crop {
                    Method::Crop
                } else {
                    Method::Scale
                },
                UInt::from(thumb.width),
                UInt::from(thumb.height),
            )),
            None => MediaFormat::File,
        },
    };
    if let Ok(store) = client.media_store().lock().await
        && let Ok(Some(bytes)) = store.get_media_content(&request).await
    {
        return if bytes.len() as u64 > limit {
            Err(Refused::TooLarge(Some(bytes.len() as u64)))
        } else {
            Ok(bytes)
        };
    }

    let uri = match source {
        MediaSource::Plain(uri) => uri,
        MediaSource::Encrypted(file) => &file.url,
    };
    let url = media_url(client, uri, thumbnail, true)?;
    let bytes = match bounded_get(client, url, true, limit).await? {
        Some(bytes) => bytes,
        None => {
            let url = media_url(client, uri, thumbnail, false)?;
            bounded_get(client, url, false, limit)
                .await?
                .ok_or(Refused::Failed)?
        }
    };
    let bytes = match source {
        MediaSource::Plain(_) => bytes,
        // As large as the encrypted file; the hash is checked at the end.
        MediaSource::Encrypted(file) => {
            let info = MediaEncryptionInfo::from((**file).clone());
            let mut input = std::io::Cursor::new(bytes);
            let mut plain = Vec::new();
            let read = match AttachmentDecryptor::new(&mut input, info) {
                Ok(mut reader) => reader.read_to_end(&mut plain).is_ok(),
                Err(_) => false,
            };
            if !read {
                tracing::debug!("could not decrypt media");
                return Err(Refused::Failed);
            }
            plain
        }
    };
    if let Ok(store) = client.media_store().lock().await {
        let _ = store
            .add_media_content(&request, bytes.clone(), IgnoreMediaRetentionPolicy::No)
            .await;
    }
    Ok(bytes)
}

/// A small version if there is one (unless the whole file is wanted), else the file.
/// Sizes stated by the sender only decide what is tried first; what bounds a transfer
/// is always the count of bytes actually received.
async fn download(
    client: &Client,
    media: &MediaRef,
    size: Option<u64>,
    full: bool,
) -> Result<Vec<u8>, String> {
    if !full {
        // A file said to be small is fetched itself: it is needed for the viewer
        // anyway. If it is not small after all, this ends early and costs little.
        if size.is_some_and(|size| size <= SMALL_FILE)
            && let Ok(bytes) = get(client, &media.full, None, SMALL_FILE_LIMIT).await
        {
            return Ok(bytes);
        }
        match (&media.thumbnail, &media.full) {
            // The sender's own thumbnail; the only kind there is for encrypted files.
            (Some((thumbnail, _)), _) => {
                if let Ok(bytes) = get(client, thumbnail, None, MAX_DOWNLOAD).await {
                    return Ok(bytes);
                }
            }
            // The server can scale an unencrypted file down for us.
            (None, MediaSource::Plain(_)) => {
                let pixels = Some(Thumb::scale(THUMBNAIL_PIXELS));
                if let Ok(bytes) = get(client, &media.full, pixels, MAX_DOWNLOAD).await {
                    return Ok(bytes);
                }
            }
            _ => {}
        }
    }
    // Saves a request when the sender is honest about a large file.
    if let Some(size) = size.filter(|size| *size > MAX_DOWNLOAD) {
        return Err(Refused::TooLarge(Some(size)).reason());
    }
    get(client, &media.full, None, MAX_DOWNLOAD)
        .await
        .map_err(|refused| refused.reason())
}

#[cfg(test)]
mod tests {
    use matrix_sdk::attachment::AttachmentConfig;
    use matrix_sdk::config::SyncSettings;

    use super::*;
    use crate::matrix::{MatrixCmd, spawn};
    use crate::model::ImageSource;
    use crate::paths::Paths;

    /// A web server for one test: answers by the last part of the path.
    /// `sent` counts the body bytes it managed to write for `/endless`.
    async fn serve(sent: Arc<std::sync::atomic::AtomicU64>) -> Client {
        serve_with(sent, Vec::new()).await
    }

    /// Like [`serve`]; `/secret` answers with `secret`, `/tampered` with a damaged copy.
    async fn serve_with(sent: Arc<std::sync::atomic::AtomicU64>, secret: Vec<u8>) -> Client {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let sent = sent.clone();
                let secret = secret.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(n) if n > 0 => request.extend_from_slice(&chunk[..n]),
                            _ => return,
                        }
                    }
                    let request = String::from_utf8_lossy(&request).into_owned();
                    let path = request.split(' ').nth(1).unwrap_or("");
                    let path = path.split('?').next().unwrap_or("").to_owned();
                    let with_token = request.to_lowercase().contains("authorization: bearer");
                    let head = |status: &str, extra: &str| {
                        format!("HTTP/1.1 {status}\r\nConnection: close\r\n{extra}\r\n")
                    };
                    if path.ends_with("/small") {
                        let _ = socket
                            .write_all(head("200 OK", "Content-Length: 5\r\n").as_bytes())
                            .await;
                        let _ = socket.write_all(b"hello").await;
                    } else if path.ends_with("/secret") || path.ends_with("/tampered") {
                        let mut body = secret;
                        if path.ends_with("/tampered") {
                            body[7] ^= 1;
                        }
                        let extra = format!("Content-Length: {}\r\n", body.len());
                        let _ = socket.write_all(head("200 OK", &extra).as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                    } else if path.ends_with("/declared") {
                        // Says how large it is; the body must never be asked for.
                        let extra = "Content-Length: 999999999\r\n";
                        let _ = socket.write_all(head("200 OK", extra).as_bytes()).await;
                    } else if path.ends_with("/endless") {
                        // Does not say, and goes on for 256 MB if allowed to.
                        let extra = "Transfer-Encoding: chunked\r\n";
                        let _ = socket.write_all(head("200 OK", extra).as_bytes()).await;
                        let block = vec![b'x'; 64 * 1024];
                        for _ in 0..4096 {
                            let framed = [b"10000\r\n", block.as_slice(), b"\r\n"].concat();
                            if socket.write_all(&framed).await.is_err() {
                                return;
                            }
                            sent.fetch_add(block.len() as u64, Ordering::SeqCst);
                        }
                    } else if path.ends_with("/token") {
                        let status = if with_token {
                            "200 OK"
                        } else {
                            "403 Forbidden"
                        };
                        let extra = "Content-Length: 0\r\n";
                        let _ = socket.write_all(head(status, extra).as_bytes()).await;
                    } else {
                        let extra = "Content-Length: 0\r\n";
                        let _ = socket
                            .write_all(head("404 Not Found", extra).as_bytes())
                            .await;
                    }
                });
            }
        });
        Client::builder()
            .homeserver_url(format!("http://{address}"))
            .build()
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn downloads_stop_at_the_byte_limit() {
        use std::sync::atomic::{AtomicU64, Ordering};
        const LIMIT: u64 = 1024 * 1024;
        let sent = Arc::new(AtomicU64::new(0));
        let client = serve(sent.clone()).await;
        let url = |name: &str| client.homeserver().join(name).unwrap();

        let small = bounded_get(&client, url("small"), false, LIMIT).await;
        assert_eq!(small, Ok(Some(b"hello".to_vec())));
        // Exactly at the limit is fine, one byte less of a limit is not.
        assert!(bounded_get(&client, url("small"), false, 5).await.is_ok());
        let tight = bounded_get(&client, url("small"), false, 4).await;
        assert_eq!(tight, Err(Refused::TooLarge(Some(5))));

        // A stated size above the limit is refused before the body.
        let declared = bounded_get(&client, url("declared"), false, LIMIT).await;
        assert_eq!(declared, Err(Refused::TooLarge(Some(999_999_999))));

        // No stated size: given up as soon as the limit is passed. The server was
        // ready to send 256 MB; it got rid of little more than the limit plus what
        // fits into the buffers of the connection.
        let endless = bounded_get(&client, url("endless"), false, LIMIT).await;
        assert_eq!(endless, Err(Refused::TooLarge(None)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let sent = sent.load(Ordering::SeqCst);
        assert!((LIMIT..32 * LIMIT).contains(&sent), "{sent}");

        // An address the server does not know: try the older one, or give up.
        assert_eq!(bounded_get(&client, url("x"), true, LIMIT).await, Ok(None));
        let missing = bounded_get(&client, url("x"), false, LIMIT).await;
        assert_eq!(missing, Err(Refused::Failed));
        // Not logged in, so there is no token to send.
        let denied = bounded_get(&client, url("token"), true, LIMIT).await;
        assert_eq!(denied, Err(Refused::Failed));

        assert_eq!(
            Refused::TooLarge(Some(34_000_000)).reason(),
            "too large to show (34 MB)"
        );
        assert_eq!(
            Refused::TooLarge(None).reason(),
            "too large to show (over 21 MB)"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_are_fetched_decrypted_checked_and_cached() {
        use matrix_sdk::ruma::events::room::EncryptedFile;
        use matrix_sdk::ruma::mxc_uri;
        use matrix_sdk_base::crypto::AttachmentEncryptor;

        let plain: Vec<u8> = (0..5000u32).map(|n| (n % 251) as u8).collect();
        let mut input = std::io::Cursor::new(plain.clone());
        let mut encryptor = AttachmentEncryptor::new(&mut input);
        let mut secret = Vec::new();
        encryptor.read_to_end(&mut secret).unwrap();
        let keys = encryptor.finish();
        assert_ne!(secret, plain);
        let client = serve_with(Arc::default(), secret).await;
        let encrypted = |name: &str| {
            let uri = <&MxcUri>::from(name).to_owned();
            let file = EncryptedFile::new(uri, keys.encryption_info.clone(), keys.hashes.clone());
            MediaSource::Encrypted(Box::new(file))
        };

        // Decrypted on the way; over the limit it is refused all the same.
        let source = encrypted("mxc://example.org/secret");
        assert_eq!(
            get(&client, &source, None, 100).await,
            Err(Refused::TooLarge(Some(5000)))
        );
        assert_eq!(get(&client, &source, None, 10_000).await, Ok(plain.clone()));
        // A file that does not match its hash is not shown, and not kept.
        let bad = encrypted("mxc://example.org/tampered");
        assert_eq!(get(&client, &bad, None, 10_000).await, Err(Refused::Failed));
        let kept = |source: &MediaSource| {
            let request = MediaRequestParameters {
                source: source.clone(),
                format: MediaFormat::File,
            };
            let client = client.clone();
            async move {
                let store = client.media_store().lock().await.unwrap();
                store.get_media_content(&request).await.unwrap()
            }
        };
        assert_eq!(kept(&source).await, Some(plain));
        assert_eq!(kept(&bad).await, None);

        // Unencrypted: the file and its thumbnail, each under its own cache entry;
        // what was refused for its size never reaches the cache.
        let small = MediaSource::Plain(mxc_uri!("mxc://example.org/small").to_owned());
        assert_eq!(
            get(&client, &small, Some(Thumb::scale((640, 480))), 100).await,
            Ok(b"hello".to_vec())
        );
        assert_eq!(kept(&small).await, None);
        assert_eq!(get(&client, &small, None, 100).await, Ok(b"hello".to_vec()));
        assert_eq!(kept(&small).await, Some(b"hello".to_vec()));
        let endless = MediaSource::Plain(mxc_uri!("mxc://example.org/endless").to_owned());
        assert_eq!(
            get(&client, &endless, None, 100_000).await,
            Err(Refused::TooLarge(None))
        );
        assert_eq!(kept(&endless).await, None);
    }

    #[tokio::test]
    async fn media_addresses() {
        use matrix_sdk::ruma::mxc_uri;
        let client = Client::builder()
            .homeserver_url("https://example.org/base/")
            .build()
            .await
            .unwrap();
        let uri = mxc_uri!("mxc://other.org/AbC123");
        let url = |thumbnail, authenticated| {
            media_url(&client, uri, thumbnail, authenticated)
                .unwrap()
                .to_string()
        };
        assert_eq!(
            url(None, true),
            "https://example.org/base/_matrix/client/v1/media/download/other.org/AbC123"
        );
        assert_eq!(
            url(None, false),
            "https://example.org/base/_matrix/media/v3/download/other.org/AbC123"
        );
        assert_eq!(
            url(Some(Thumb::scale((640, 480))), true),
            "https://example.org/base/_matrix/client/v1/media/thumbnail/other.org/AbC123?width=640&height=480&method=scale"
        );
        assert_eq!(
            url(Some(Thumb::crop((96, 96))), false),
            "https://example.org/base/_matrix/media/v3/thumbnail/other.org/AbC123?width=96&height=96&method=crop"
        );
        // Not an mxc address at all.
        let bad = <&MxcUri>::from("https://evil.example/x");
        assert_eq!(media_url(&client, bad, None, true), Err(Refused::Failed));
    }

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

    /// Gives an unencrypted room of the test user a picture, then checks that the
    /// room list reports it and that the worker fetches it as a small square.
    /// Setup as for the other end-to-end tests.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn room_avatar_is_reported_and_fetched() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");

        let uploader = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        uploader
            .matrix_auth()
            .login_username(&username, &password)
            .initial_device_display_name("matrix-rat avatar test")
            .send()
            .await
            .unwrap();
        uploader.sync_once(SyncSettings::default()).await.unwrap();
        let mut png = Vec::new();
        images::demo_picture(300, 200)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        // The first room where the test user is allowed to.
        let mut room_id = None;
        for room in uploader.joined_rooms() {
            if !room.encryption_state().is_encrypted()
                && room
                    .upload_avatar(&mime::IMAGE_PNG, png.clone(), None)
                    .await
                    .is_ok()
            {
                room_id = Some(room.room_id().to_string());
                break;
            }
        }
        let room_id = room_id.expect("an unencrypted room whose picture the test user may set");

        let root = std::env::temp_dir().join(format!("matrix-rat-avatar-{}", std::process::id()));
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
            let mut asked = None;
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) if asked.is_none() => {
                        let avatar = rooms
                            .iter()
                            .find(|room| room.id == room_id)
                            .and_then(|room| room.avatar.clone());
                        if let Some(url) = avatar {
                            assert!(url.starts_with("mxc://"));
                            // Opening a room is what gets the worker's room machinery
                            // going in the app; the fetch itself needs no open room.
                            handle.send(MatrixCmd::FetchAvatar { url: url.clone() });
                            handle.send(MatrixCmd::FetchAvatar { url: url.clone() });
                            asked = Some(url);
                        }
                    }
                    MatrixEvent::Avatar { url, result } => {
                        assert_eq!(Some(&url), asked.as_ref());
                        return result.expect("avatar could not be loaded");
                    }
                    _ => {}
                }
            }
        };
        let avatar = tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .expect("timed out waiting for the avatar");
        // A square, whatever the shape of the upload, and no larger than kept.
        let (width, height) = (avatar.pixels.width(), avatar.pixels.height());
        assert_eq!(width, height);
        assert!(width > 0 && width <= AVATAR_PIXELS.0, "{width}");
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }
}
