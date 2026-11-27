//! Link previews: asks the homeserver what a link is about
//! (`GET /_matrix/client/v1/media/preview_url`). The homeserver fetches the page, so this
//! reveals the link to it; the UI only asks when the user turned previews on for that
//! kind of room.
//!
//! Any failure (the server has no such feature, previews are disabled, the page could not
//! be fetched, a rate limit, a timeout) is the same silent "nothing to show". Nothing
//! about the address or the answer is logged.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::config::RequestConfig;
use matrix_sdk::ruma::api::client::authenticated_media::get_media_preview::v1::Request;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;

use super::MatrixEvent;
use crate::previews::Raw;
use crate::rt::{self, JoinSet};

/// Requests to the server at the same time (the UI keeps to its own limit as well).
const PARALLEL: usize = 3;
/// The server may have to fetch a slow page: this long, then it is nothing.
const TIMEOUT: Duration = Duration::from_secs(10);
/// A string longer than this is not a title or a description anyone reads.
const FIELD_CAP: usize = 2000;

pub struct Previews {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    slots: Arc<Semaphore>,
    /// The running requests. They end with the session: dropping this stops them.
    fetches: Mutex<JoinSet<()>>,
}

impl Previews {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        Self {
            client,
            events,
            slots: Arc::new(Semaphore::new(PARALLEL)),
            fetches: Mutex::default(),
        }
    }

    /// Asks in the background; the outcome arrives as [`MatrixEvent::LinkPreview`].
    pub fn fetch(&self, url: String) {
        let client = self.client.clone();
        let events = self.events.clone();
        let slots = self.slots.clone();
        let mut fetches = self.fetches.lock().expect("not poisoned");
        // Forget the ones that are done.
        while fetches.try_join_next().is_some() {}
        fetches.spawn(async move {
            let result = async {
                let _slot = slots.acquire().await.ok()?;
                rt::timeout(TIMEOUT, request(&client, &url))
                    .await
                    .ok()
                    .flatten()
            }
            .await;
            let _ = events.send(MatrixEvent::LinkPreview { url, result });
        });
    }
}

async fn request(client: &Client, url: &str) -> Option<Raw> {
    let config = RequestConfig::new().disable_retry().timeout(TIMEOUT);
    let response = client
        .send(Request::new(url.to_owned()))
        .with_request_config(config)
        .await
        .ok()?;
    parse(response.data?.get())
}

/// The OpenGraph fields of the server's answer that are shown. `None` when it has none
/// of them, or is not an object.
pub fn parse(json: &str) -> Option<Raw> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let field = |name: &str| {
        let text = value.get(name)?.as_str()?;
        Some(text.chars().take(FIELD_CAP).collect::<String>())
    };
    let raw = Raw {
        title: field("og:title"),
        site: field("og:site_name"),
        description: field("og:description"),
    };
    (raw != Raw::default()).then_some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fields_that_are_shown_are_taken_from_the_answer() {
        let raw = parse(
            r#"{"og:title":"A page","og:site_name":"Example","og:description":"About it",
                "og:image":"mxc://example.org/x","matrix:image:size":1234,"og:url":"https://e.org"}"#,
        )
        .unwrap();
        assert_eq!(raw.title.as_deref(), Some("A page"));
        assert_eq!(raw.site.as_deref(), Some("Example"));
        assert_eq!(raw.description.as_deref(), Some("About it"));
    }

    #[test]
    fn answers_with_nothing_to_show_are_nothing() {
        for json in [
            "{}",
            "null",
            "[]",
            "\"text\"",
            "not json",
            r#"{"og:title":42,"og:description":null,"og:image":"mxc://a/b"}"#,
        ] {
            assert_eq!(parse(json), None, "{json}");
        }
        // One field is enough.
        assert!(parse(r#"{"og:description":"x"}"#).is_some());
    }

    #[test]
    fn a_huge_field_is_cut() {
        let json = format!(r#"{{"og:title":"{}"}}"#, "x".repeat(10_000));
        let raw = parse(&json).unwrap();
        assert_eq!(raw.title.unwrap().chars().count(), FIELD_CAP);
    }
}
