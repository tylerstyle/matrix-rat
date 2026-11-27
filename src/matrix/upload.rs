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

use super::MatrixEvent;
use crate::format::Outgoing;
use crate::logsafe;
use crate::rt::{self, JoinSet};
use crate::text::{sanitize_line, truncate};
use crate::upload::{self, Prepared};

/// How long sending waits to learn the server's size limit before going without.
const LIMIT_WAIT: Duration = Duration::from_secs(5);
const LIMIT_RETRY: Duration = Duration::from_secs(60);
/// How long quitting waits for files already handed to `Uploads::send` to reach the
/// SDK's send queue before giving up on them.
pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
/// What the room sees as the name of a voice message: the same for every one, so
/// that nothing of where or when it was recorded goes along.
const VOICE_NAME: &str = "voice-message.ogg";
/// A voice message is Ogg/Opus, whatever the name of its file suggests.
const VOICE_MIME: &str = "audio/ogg";
/// What a voice message is called where the user reads about it: the file is one of
/// this app's own, and its name would mean nothing.
const VOICE_TOLD: &str = "The voice message";
const VOICE_TOLD_WITHIN: &str = "the voice message";
/// How many values stand in for a waveform that was not given.
const FLAT_WAVEFORM: usize = 16;

/// What makes a file a voice message (MSC3245): how long the recording is and how
/// loud it is along the way.
#[derive(Clone, PartialEq, Eq)]
pub struct VoiceMeta {
    pub duration: Duration,
    /// Loudness along the sound, each value from 0 to 1024. Louder values are cut to
    /// that, and of more than [`crate::audio::MAX_WAVEFORM`] the loudest of each
    /// stretch is sent.
    pub waveform: Vec<u16>,
}

/// Hand-written: only the shape, as for a received one (`AudioMeta`).
impl std::fmt::Debug for VoiceMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceMeta")
            .field("duration", &self.duration)
            .field("waveform", &self.waveform.len())
            .finish()
    }
}

impl VoiceMeta {
    /// The waveform as the SDK takes it: values from 0 to 1, bounded in number, and
    /// never empty. The SDK leaves the whole block about the sound out of the event
    /// when there is no waveform, and other clients then show no voice message.
    fn levels(&self) -> Vec<f32> {
        let values: Vec<u64> = self.waveform.iter().copied().map(u64::from).collect();
        let bounded = crate::audio::bounded_waveform(&values);
        if bounded.is_empty() {
            return vec![0.0; FLAT_WAVEFORM];
        }
        bounded
            .into_iter()
            .map(|value| f32::from(value) / f32::from(crate::audio::MAX_AMPLITUDE))
            .collect()
    }
}

/// A file to send, with what makes it a voice message if it is one. One thing, so
/// that the details of a recording can never end up with another file.
#[derive(Clone)]
pub struct OutgoingFile {
    pub path: PathBuf,
    pub voice: Option<VoiceMeta>,
}

/// Hand-written: a path can say a lot about the user.
impl std::fmt::Debug for OutgoingFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.voice {
            Some(_) => "OutgoingFile(voice)",
            None => "OutgoingFile",
        })
    }
}

impl From<PathBuf> for OutgoingFile {
    fn from(path: PathBuf) -> Self {
        Self { path, voice: None }
    }
}

/// For tests that only ask which files were sent.
#[cfg(test)]
impl PartialEq<PathBuf> for OutgoingFile {
    fn eq(&self, path: &PathBuf) -> bool {
        self.path == *path
    }
}

pub struct Uploads {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    /// The background loop that learns the server's size limit; stopped immediately on
    /// exit so an offline retry sleep never delays shutdown.
    limit_task: JoinSet<()>,
    /// Files being prepared and handed to the send queue. On shutdown we wait briefly
    /// for these to finish so voice notes sent right before quitting reach the store;
    /// dropping this aborts any that are still left.
    sends: JoinSet<()>,
}

/// Waits up to `wait` for every task in `tasks` to finish, then aborts and drains any
/// that did not.
async fn drain_set(tasks: &mut JoinSet<()>, wait: Duration) {
    let drained = async { while tasks.join_next().await.is_some() {} };
    if rt::timeout(wait, drained).await.is_err() {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

impl Uploads {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        client.send_queue().enable_upload_progress(true);
        let mut limit_task = JoinSet::new();
        // The UI refuses files that are too large before they are attached; for that
        // it has to know the limit. Asked again until the server answers.
        let (asker, tell) = (client.clone(), events.clone());
        limit_task.spawn(async move {
            loop {
                match asker.load_or_fetch_max_upload_size().await {
                    Ok(limit) => {
                        let _ = tell.send(MatrixEvent::UploadLimit(limit.into()));
                        return;
                    }
                    Err(err) => {
                        tracing::debug!(why = %logsafe::kind(&err), "could not ask for the upload limit")
                    }
                }
                rt::sleep(LIMIT_RETRY).await;
            }
        });
        Self {
            client,
            events,
            limit_task,
            sends: JoinSet::new(),
        }
    }

    /// Waits up to `wait` for files already handed to `send` to reach the send queue,
    /// stopping the limit-fetch loop immediately.
    pub async fn finish(&mut self, wait: Duration) {
        self.limit_task.abort_all();
        drain_set(&mut self.sends, wait).await;
    }

    /// Sends `files` in order. The caption and the reply belong to the first: if that
    /// one cannot be sent, none is ([`MatrixEvent::FilesNotSent`]). A later file that
    /// cannot be read is reported and the rest still goes, and then
    /// [`MatrixEvent::FilesSent`] says that the files are not needed any more.
    pub fn send(
        &mut self,
        send: u64,
        timeline: Arc<Timeline>,
        files: Vec<OutgoingFile>,
        caption: Option<Outgoing>,
        reply_to: Option<OwnedEventId>,
    ) {
        while self.sends.try_join_next().is_some() {}
        let client = self.client.clone();
        let events = self.events.clone();
        self.sends.spawn(async move {
            let limit = rt::timeout(LIMIT_WAIT, client.load_or_fetch_max_upload_size())
                .await
                .ok()
                .and_then(Result::ok)
                .map(u64::from);
            let (mut caption, mut caption_mentions) =
                caption.map(caption_content).unwrap_or((None, None));
            let mut reply_to = reply_to;
            for (n, file) in files.into_iter().enumerate() {
                let ready = tokio::task::spawn_blocking(move || ready(file, limit)).await;
                let outcome = match ready {
                    Ok(Ok((told, source, mime, mut config))) => {
                        // Only ever with the first file.
                        config.caption = caption.take();
                        config.mentions = caption_mentions.take();
                        config.in_reply_to = reply_to.take();
                        timeline
                            .send_attachment(source, mime, config)
                            .use_send_queue()
                            .await
                            .map_err(|err| {
                                tracing::warn!(why = %logsafe::kind(&err), "could not queue a file for sending");
                                short(&format!("Could not send {told}: {err}"))
                            })
                    }
                    // The reason names the file; the path stays out of the log.
                    Ok(Err(reason)) => {
                        tracing::warn!("a file could not be prepared for sending");
                        Err(short(&reason))
                    }
                    Err(err) => {
                        tracing::warn!(why = %logsafe::kind(&err), "preparing a file for sending failed");
                        Err("Could not send the file".to_owned())
                    }
                };
                match outcome {
                    Ok(()) => {}
                    Err(reason) if n == 0 => {
                        let _ = events.send(MatrixEvent::FilesNotSent { send, reason });
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
            let _ = events.send(MatrixEvent::FilesSent(send));
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

/// A file read and ready for the SDK, under the name the user is told about it; or
/// why it cannot be sent, as a sentence for the user. Reads the file: not for the
/// worker's loop.
fn ready(
    file: OutgoingFile,
    limit: Option<u64>,
) -> Result<(String, AttachmentSource, mime::Mime, AttachmentConfig), String> {
    let prepared = match file.voice {
        // The file of a recording has a name of this app's making.
        Some(_) => upload::prepare_as(&file.path, VOICE_TOLD.to_owned(), limit)?,
        None => upload::prepare(&file.path, limit)?,
    };
    let told = match file.voice {
        Some(_) => VOICE_TOLD_WITHIN.to_owned(),
        None => prepared.name.clone(),
    };
    let (source, mime, config) = attachment(prepared, file.voice)?;
    Ok((told, source, mime, config))
}

/// What the SDK needs to send a prepared file, or why it cannot be sent (its type
/// cannot be expressed, which is never the case for the types this app assigns; or it
/// is no voice message that can be believed). With `voice` it is a voice message,
/// whatever the file is called.
fn attachment(
    prepared: Prepared,
    voice: Option<VoiceMeta>,
) -> Result<(AttachmentSource, mime::Mime, AttachmentConfig), String> {
    let size = UInt::new(prepared.bytes.len() as u64);
    if let Some(voice) = voice {
        // It goes out as Ogg, so it has to be Ogg: other clients would show a voice
        // message that does not play.
        if !prepared.bytes.starts_with(b"OggS") {
            return Err(format!(
                "{VOICE_TOLD} is not an Ogg recording and was not sent"
            ));
        }
        // Nobody would believe the length, this app included.
        if crate::audio::bounded_duration(voice.duration).is_none() {
            return Err(format!("{VOICE_TOLD} is too long and was not sent"));
        }
        // The SDK writes the block about the sound only for an audio type, and only
        // with both a length and a waveform.
        let info = AttachmentInfo::Voice(BaseAudioInfo {
            duration: Some(voice.duration),
            size,
            waveform: Some(voice.levels()),
        });
        let config = AttachmentConfig {
            info: Some(info),
            ..Default::default()
        };
        let source = AttachmentSource::Data {
            bytes: prepared.bytes,
            filename: VOICE_NAME.to_owned(),
        };
        let mime = VOICE_MIME.parse().map_err(|_| cannot(VOICE_TOLD_WITHIN))?;
        return Ok((source, mime, config));
    }
    let mime: mime::Mime = prepared.mime.parse().map_err(|_| cannot(&prepared.name))?;
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
    Ok((source, mime, config))
}

fn cannot(name: &str) -> String {
    format!("Could not send {name}")
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

    use super::super::{MatrixCmd, spawn};
    use super::*;
    use crate::model::{ImageSource, ItemRef, SendState};
    use crate::paths::Paths;

    /// `len` bytes that begin as an Ogg file does. Not a real sound: nothing plays it.
    fn ogg(len: usize) -> Vec<u8> {
        b"OggS"
            .iter()
            .copied()
            .chain(std::iter::repeat(7))
            .take(len)
            .collect()
    }

    #[test]
    fn the_kind_of_message_follows_the_type() {
        let prepared = |name: &str, mime: &str, picture| Prepared {
            name: name.to_owned(),
            bytes: vec![1, 2, 3],
            mime: mime.to_owned(),
            picture,
        };
        let (source, mime, config) = attachment(
            prepared(
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
            ),
            None,
        )
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
        let (_, _, config) =
            attachment(prepared("drawing.svg", "image/svg+xml", None), None).unwrap();
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
            // A sound that was only attached is no voice message, and says nothing
            // about its length.
            ("audio/ogg", |info| {
                matches!(
                    info,
                    AttachmentInfo::Audio(BaseAudioInfo {
                        duration: None,
                        waveform: None,
                        size: Some(_),
                    })
                )
            }),
            ("application/pdf", |info| {
                matches!(info, AttachmentInfo::File(BaseFileInfo { size: Some(_) }))
            }),
            ("text/plain", |info| matches!(info, AttachmentInfo::File(_))),
        ] {
            let (_, _, config) = attachment(prepared("x", mime, None), None).unwrap();
            assert!(check(config.info.as_ref().unwrap()), "{mime}");
        }
    }

    #[test]
    fn a_file_with_voice_details_goes_out_as_a_voice_message() {
        // As the recorder leaves it: a name of its own, and a type guessed from that.
        let recorded = |mime: &str| Prepared {
            name: "voice-0123456789abcdef.bin".to_owned(),
            bytes: ogg(40),
            mime: mime.to_owned(),
            picture: None,
        };
        let voice = VoiceMeta {
            duration: Duration::from_millis(12_340),
            waveform: vec![0, 256, 512, 1024],
        };
        for guessed in ["audio/ogg", "application/octet-stream", "video/ogg", "?"] {
            let (source, mime, config) =
                attachment(recorded(guessed), Some(voice.clone())).unwrap();
            // An audio type, or the SDK sends a file or a video without the details.
            assert_eq!(mime.essence_str(), "audio/ogg", "{guessed}");
            assert!(matches!(
                source,
                AttachmentSource::Data { ref filename, ref bytes }
                    if filename == "voice-message.ogg" && bytes.len() == 40
            ));
            let Some(AttachmentInfo::Voice(info)) = config.info else {
                panic!("not a voice message: {guessed}");
            };
            assert_eq!(info.duration, Some(Duration::from_millis(12_340)));
            assert_eq!(info.size, UInt::new(40));
            assert_eq!(info.waveform, Some(vec![0.0, 0.25, 0.5, 1.0]));
            assert!(config.thumbnail.is_none());
        }
    }

    #[test]
    fn the_waveform_of_a_voice_message_is_bounded_and_never_missing() {
        let levels = |waveform: Vec<u16>| {
            let voice = VoiceMeta {
                duration: Duration::from_secs(3),
                waveform,
            };
            let prepared = Prepared {
                name: "x.ogg".to_owned(),
                bytes: ogg(4),
                mime: "audio/ogg".to_owned(),
                picture: None,
            };
            let (_, _, config) = attachment(prepared, Some(voice)).unwrap();
            let Some(AttachmentInfo::Voice(info)) = config.info else {
                panic!("not a voice message");
            };
            assert_eq!(info.duration, Some(Duration::from_secs(3)));
            info.waveform
                .expect("without one the SDK leaves the details out")
        };
        // Too loud is cut to the loudest.
        assert_eq!(levels(vec![1024, 1025, u16::MAX]), [1.0, 1.0, 1.0]);
        // Too many: the loudest of each stretch, so every kept value is the loud one.
        let long: Vec<u16> = (0..10_000)
            .map(|n| if n % 10 == 0 { 768 } else { 3 })
            .collect();
        let kept = levels(long);
        assert_eq!(kept.len(), crate::audio::MAX_WAVEFORM);
        assert!(kept.iter().all(|level| *level == 0.75));
        // None at all: a short flat one stands in.
        let flat = levels(Vec::new());
        assert_eq!(flat.len(), 16);
        assert!(flat.iter().all(|level| *level == 0.0));
        // What the SDK makes of the values is what was given (it multiplies by 1024).
        let odd = vec![1, 333, 1023];
        let back: Vec<u16> = levels(odd.clone())
            .into_iter()
            .map(|level| (level.clamp(0.0, 1.0) * 1024.0) as u16)
            .collect();
        assert_eq!(back, odd);
    }

    #[test]
    fn what_is_no_believable_voice_message_is_refused_in_plain_words() {
        let recorded = |bytes: Vec<u8>| Prepared {
            name: "voice-0123456789abcdef.ogg".to_owned(),
            bytes,
            mime: "audio/ogg".to_owned(),
            picture: None,
        };
        let voice = |duration| {
            Some(VoiceMeta {
                duration,
                waveform: vec![1, 2, 3],
            })
        };
        let refused = |prepared, voice| match attachment(prepared, voice) {
            Ok(_) => panic!("it would have been sent"),
            Err(reason) => reason,
        };
        // Not Ogg: empty, something else, or only nearly.
        for bytes in [
            Vec::new(),
            b"RIFF....WAVE".to_vec(),
            b"Ogg".to_vec(),
            b" OggS".to_vec(),
        ] {
            let reason = refused(recorded(bytes), voice(Duration::from_secs(3)));
            assert_eq!(
                reason,
                "The voice message is not an Ogg recording and was not sent"
            );
        }
        // The same bytes go out as a plain sound: the rule is about voice messages.
        assert!(attachment(recorded(b"RIFF....WAVE".to_vec()), None).is_ok());
        // A length nobody would believe; the longest believable one is taken.
        let reason = refused(
            recorded(ogg(40)),
            voice(crate::audio::MAX_DURATION + Duration::from_millis(1)),
        );
        assert_eq!(reason, "The voice message is too long and was not sent");
        assert!(attachment(recorded(ogg(40)), voice(crate::audio::MAX_DURATION)).is_ok());
    }

    #[test]
    fn each_file_of_a_send_is_what_it_was_given_as() {
        let root =
            std::env::temp_dir().join(format!("matrix-rat-voice-order-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let plain = root.join("song.ogg");
        let recording = root.join("voice-0123456789abcdef.ogg");
        std::fs::write(&plain, ogg(30)).unwrap();
        std::fs::write(&recording, ogg(50)).unwrap();
        let voice = VoiceMeta {
            duration: Duration::from_secs(4),
            waveform: vec![512; 8],
        };
        // The voice message second, after a plain file of the same type.
        let files = vec![
            OutgoingFile::from(plain.clone()),
            OutgoingFile {
                path: recording.clone(),
                voice: Some(voice.clone()),
            },
        ];
        let ready: Vec<_> = files
            .into_iter()
            .map(|file| ready(file, None).unwrap())
            .collect();
        let [
            (first_told, first_source, _, first),
            (told, source, mime, second),
        ] = <[_; 2]>::try_from(ready).ok().unwrap();
        assert_eq!(first_told, "song.ogg");
        assert!(matches!(
            first_source,
            AttachmentSource::Data { ref filename, ref bytes }
                if filename == "song.ogg" && bytes.len() == 30
        ));
        assert!(matches!(
            first.info,
            Some(AttachmentInfo::Audio(BaseAudioInfo {
                duration: None,
                waveform: None,
                ..
            }))
        ));
        assert_eq!(told, "the voice message");
        assert_eq!(mime.essence_str(), "audio/ogg");
        assert!(matches!(
            source,
            AttachmentSource::Data { ref filename, ref bytes }
                if filename == "voice-message.ogg" && bytes.len() == 50
        ));
        let Some(AttachmentInfo::Voice(info)) = second.info else {
            panic!("the second file is not a voice message");
        };
        assert_eq!(info.duration, Some(Duration::from_secs(4)));
        assert_eq!(info.waveform, Some(vec![0.5; 8]));

        // A recording that cannot be read or sent is not called by its file's name.
        let failed = |path: &std::path::Path, limit| {
            let file = OutgoingFile {
                path: path.to_owned(),
                voice: Some(voice.clone()),
            };
            match super::ready(file, limit) {
                Ok(_) => panic!("it would have been sent"),
                Err(reason) => reason,
            }
        };
        std::fs::remove_file(&recording).unwrap();
        assert_eq!(failed(&recording, None), "The voice message does not exist");
        let large = failed(&plain, Some(10));
        assert!(
            large.starts_with("The voice message is too large"),
            "{large}"
        );
        assert!(!large.contains("song"), "{large}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn voice_details_print_only_their_shape() {
        let voice = VoiceMeta {
            duration: Duration::from_secs(12),
            waveform: vec![4, 4, 4],
        };
        assert_eq!(
            format!("{voice:?}"),
            "VoiceMeta { duration: 12s, waveform: 3 }"
        );
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

    #[tokio::test]
    async fn shutting_down_waits_for_quick_uploads_and_aborts_stuck_ones_within_the_bound() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let mark = Arc::clone(&done);
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            mark.store(true, Ordering::SeqCst);
        });
        drain_set(&mut tasks, Duration::from_secs(1)).await;
        assert!(done.load(Ordering::SeqCst));
        assert!(tasks.is_empty());

        // A stuck task is aborted once the bound expires instead of hanging exit.
        let mut stuck = JoinSet::new();
        stuck.spawn(std::future::pending());
        let started = tokio::time::Instant::now();
        drain_set(&mut stuck, Duration::from_millis(40)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(stuck.is_empty());
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
            password: password.into(),
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
                    MatrixEvent::FilesNotSent { reason, .. } => refused = Some(reason),
                    MatrixEvent::Rooms(rooms) if room_id.is_none() => {
                        let joined = rooms.iter().find(|r| !r.is_pending());
                        if let Some(room) = joined {
                            room_id = Some(room.id.clone());
                            handle.send(MatrixCmd::OpenRoom {
                                room_id: room.id.clone(),
                                generation: 1,
                            });
                            // First a batch whose first file is gone: the caption was
                            // meant for that one, so the second file must not go either.
                            handle.send(MatrixCmd::SendFiles {
                                send: 0,
                                room_id: room.id.clone(),
                                files: vec![root.join("gone.txt").into(), text_path.clone().into()],
                                caption: Some(crate::format::parse("for the file that is gone")),
                                reply_to: None,
                            });
                            let command = MatrixCmd::SendFiles {
                                send: 0,
                                room_id: room.id.clone(),
                                files: vec![text_path.clone().into(), picture_path.clone().into()],
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

    /// Sends a small file as a voice message through the worker and waits until the
    /// server has it: the event says it is a voice message, how long it is and how
    /// loud along the way, under the fixed name; and this app reads it back as one.
    /// Setup as for [`files_are_sent_and_arrive`].
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_voice_message_is_sent_and_read_back_as_one() {
        use crate::model::{AudioMeta, MediaKind};

        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let root = std::env::temp_dir().join(format!("matrix-rat-voice-{}", std::process::id()));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();

        // Not a real sound: nothing plays it here. Named as a recording would be.
        let recorded_name = format!("voice-{:016x}.ogg", std::process::id());
        let sound: Vec<u8> = b"OggS"
            .iter()
            .copied()
            .chain((0..3000u32).map(|n| (n % 251) as u8))
            .collect();
        let path = root.join(&recorded_name);
        std::fs::write(&path, &sound).unwrap();
        // Earlier runs may have left voice messages in the room: this one is known by
        // its caption.
        let caption = format!("the voice caption {}", std::process::id());
        let duration = Duration::from_millis(2_500);
        let waveform: Vec<u16> = vec![0, 128, 512, 1000, 1024, 300];

        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username,
            password: password.into(),
        });

        let wait = async {
            let mut room_id: Option<String> = None;
            let mut arrived: Option<crate::model::Message> = None;
            let mut asked = 0u64;
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err)
                    | MatrixEvent::Error(err)
                    | MatrixEvent::ActionFailed(err) => panic!("{err}"),
                    MatrixEvent::FilesNotSent { reason, .. } => panic!("{reason}"),
                    MatrixEvent::Rooms(rooms) if room_id.is_none() => {
                        let joined = rooms.iter().find(|r| !r.is_pending());
                        if let Some(room) = joined {
                            room_id = Some(room.id.clone());
                            handle.send(MatrixCmd::OpenRoom {
                                room_id: room.id.clone(),
                                generation: 1,
                            });
                            let command = MatrixCmd::SendFiles {
                                send: 1,
                                room_id: room.id.clone(),
                                files: vec![OutgoingFile {
                                    path: path.clone(),
                                    voice: Some(VoiceMeta {
                                        duration,
                                        waveform: waveform.clone(),
                                    }),
                                }],
                                caption: Some(crate::format::parse(&caption)),
                                reply_to: None,
                            };
                            assert_eq!(format!("{command:?}"), "SendFiles(1 files, 1 voice)");
                            handle.send(command);
                        }
                    }
                    MatrixEvent::Timeline {
                        generation: 1,
                        messages,
                        ..
                    } if arrived.is_none() => {
                        let found = messages.iter().find(|m| {
                            m.body.contains(&caption)
                                && m.state == SendState::Sent
                                && m.own
                                && matches!(m.item, Some(ItemRef::Event(_)))
                        });
                        if let (Some(message), Some(room_id)) = (found, &room_id) {
                            arrived = Some(message.clone());
                            asked += 1;
                            handle.send(MatrixCmd::FetchSource {
                                request: asked,
                                room_id: room_id.clone(),
                                item: message.item.clone().unwrap(),
                            });
                        }
                    }
                    MatrixEvent::Source { request, result } if request == asked => match result {
                        Ok(source) => return (arrived.unwrap(), *source),
                        // Sent, but the event itself has not come back from the
                        // server yet: asked again shortly.
                        Err(_) => {
                            let (Some(message), Some(room_id)) = (&arrived, &room_id) else {
                                panic!("a source nobody asked for");
                            };
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            asked += 1;
                            handle.send(MatrixCmd::FetchSource {
                                request: asked,
                                room_id: room_id.clone(),
                                item: message.item.clone().unwrap(),
                            });
                        }
                    },
                    _ => {}
                }
            }
        };
        let (message, source) = tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .expect("timed out waiting for the voice message");

        // The event as the server has it (in an encrypted room: as it was decrypted).
        assert_ne!(source.kind, crate::matrix::SourceKind::Encrypted);
        assert!(!source.cut);
        assert!(
            !source.json.contains(&recorded_name),
            "the name of the recording went along"
        );
        let event: serde_json::Value = serde_json::from_str(&source.json).unwrap();
        let content = &event["content"];
        assert_eq!(content["msgtype"], "m.audio");
        assert!(
            content["org.matrix.msc3245.voice"].is_object(),
            "not marked as a voice message"
        );
        let details = &content["org.matrix.msc1767.audio"];
        assert_eq!(details["duration"], 2_500);
        assert_eq!(
            details["waveform"],
            serde_json::json!([0, 128, 512, 1000, 1024, 300])
        );
        assert_eq!(content["info"]["duration"], 2_500);
        assert_eq!(content["info"]["mimetype"], "audio/ogg");
        assert_eq!(content["info"]["size"], sound.len());
        assert_eq!(content["filename"], "voice-message.ogg");
        assert_eq!(content["body"], caption.as_str());

        // And as this app shows it.
        let attached = message.file.as_ref().expect("no attachment");
        assert_eq!(attached.kind, MediaKind::Voice);
        assert_eq!(attached.name, "voice-message.ogg");
        assert_eq!(attached.size, Some(sound.len() as u64));
        // That the file is on the server is not asked here: the event is only sent
        // once the upload is done, and fetching the file back would be answered from
        // this device's own copy, which the send queue keeps.
        assert_eq!(
            attached.audio.as_deref(),
            Some(&AudioMeta {
                duration: Some(duration),
                waveform: waveform.clone(),
                mime: Some("audio/ogg".to_owned()),
            })
        );
        assert!(message.is_playable());
        assert!(message.body.contains("🎤"), "{}", message.body);

        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }
}
