//! Logging in, and keeping the session on disk between runs.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result, anyhow, bail};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::oauth::registration::{
    ApplicationType, ClientMetadata, Localized, OAuthGrantType,
};
use matrix_sdk::authentication::oauth::{
    ClientId, ClientRegistrationData, OAuthSession, UserSession,
};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::utils::local_server::LocalServerBuilder;
use matrix_sdk::{AuthSession, Client};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use super::MatrixEvent;
use crate::paths::Paths;

const DEVICE_NAME: &str = "matrix-rat";
/// Shown to the user by the homeserver during a browser login.
const CLIENT_URI: &str = "https://github.com/tylerstyle/matrix-rat";

/// What is written to `session.json` (mode 0600).
#[derive(Serialize, Deserialize)]
struct Saved {
    homeserver: String,
    /// Encrypts the local SDK stores. Lives next to the tokens it protects no better
    /// than; a system keyring will hold both later.
    passphrase: String,
    auth: SavedAuth,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SavedAuth {
    Password {
        session: MatrixSession,
    },
    Browser {
        client_id: String,
        session: UserSession,
    },
}

/// For a new login: accepts a server name or URL and looks the homeserver up online.
async fn discover_client(paths: &Paths, homeserver: &str, passphrase: &str) -> Result<Client> {
    Client::builder()
        .server_name_or_homeserver_url(homeserver)
        .sqlite_store(paths.store_dir(), Some(passphrase))
        .handle_refresh_tokens()
        .build()
        .await
        .with_context(|| format!("could not reach homeserver {homeserver:?}"))
}

/// For a saved session: uses the known URL and makes no network request, so the app
/// starts from its local stores even when offline.
async fn offline_client(paths: &Paths, homeserver: &str, passphrase: &str) -> Result<Client> {
    Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(paths.store_dir(), Some(passphrase))
        .handle_refresh_tokens()
        .build()
        .await
        .context("could not open the local stores")
}

/// A new login is a new device, so it starts from empty stores. The previous stores hold
/// encryption keys that may exist nowhere else, so they are set aside, not deleted.
fn fresh_store(paths: &Paths) -> Result<String> {
    let store = paths.store_dir();
    if store.exists() {
        let backup = paths.data_dir.join("store.previous");
        let _ = std::fs::remove_dir_all(&backup);
        std::fs::rename(&store, &backup).context("could not set the old stores aside")?;
    }
    let _ = std::fs::remove_file(paths.session_file());
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("could not read random bytes")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Removes the saved session, for when the server no longer accepts it. The stores stay
/// until the next login sets them aside.
pub fn forget(paths: &Paths) {
    let _ = std::fs::remove_file(paths.session_file());
}

pub fn save(paths: &Paths, client: &Client, passphrase: &str) -> Result<()> {
    let auth = if let Some(session) = client.matrix_auth().session() {
        SavedAuth::Password { session }
    } else if let Some(session) = client.oauth().full_session() {
        SavedAuth::Browser {
            client_id: session.client_id.as_str().to_owned(),
            session: session.user,
        }
    } else {
        bail!("no session to save");
    };
    let saved = Saved {
        homeserver: client.homeserver().to_string(),
        passphrase: passphrase.to_owned(),
        auth,
    };
    let json = serde_json::to_vec_pretty(&saved)?;
    // Write a private temp file and rename, so a crash never leaves half a session.
    let path = paths.session_file();
    let temp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("could not write {}", temp.display()))?;
    file.write_all(&json)?;
    file.sync_all()?;
    std::fs::rename(&temp, &path)?;
    Ok(())
}

/// Restores the saved session, if there is one.
pub async fn restore(paths: &Paths) -> Result<Option<(Client, String)>> {
    let path = paths.session_file();
    if !path.exists() {
        return Ok(None);
    }
    let json =
        std::fs::read(&path).with_context(|| format!("could not read {}", path.display()))?;
    let saved: Saved = serde_json::from_slice(&json).context("the session file is damaged")?;
    let client = offline_client(paths, &saved.homeserver, &saved.passphrase).await?;
    let session = match saved.auth {
        SavedAuth::Password { session } => AuthSession::Matrix(session),
        SavedAuth::Browser { client_id, session } => AuthSession::OAuth(Box::new(OAuthSession {
            client_id: ClientId::new(client_id),
            user: session,
        })),
    };
    client.restore_session(session).await?;
    Ok(Some((client, saved.passphrase)))
}

pub async fn login_password(
    paths: &Paths,
    homeserver: &str,
    username: &str,
    password: &str,
) -> Result<(Client, String)> {
    if homeserver.trim().is_empty() || username.trim().is_empty() || password.is_empty() {
        bail!("homeserver, username and password are all needed");
    }
    let passphrase = fresh_store(paths)?;
    let client = discover_client(paths, homeserver.trim(), &passphrase).await?;
    client
        .matrix_auth()
        .login_username(username.trim(), password)
        .initial_device_display_name(DEVICE_NAME)
        .request_refresh_token()
        .send()
        .await
        .map_err(|err| anyhow!("login was refused: {err}"))?;
    save(paths, &client, &passphrase)?;
    Ok((client, passphrase))
}

/// OAuth 2.0 login: the user approves in a browser, which then redirects to a
/// temporary web server on this machine. Dropping the future cancels the login and
/// closes that server.
pub async fn login_browser(
    paths: &Paths,
    homeserver: &str,
    events: &UnboundedSender<MatrixEvent>,
) -> Result<(Client, String)> {
    if homeserver.trim().is_empty() {
        bail!("a homeserver is needed");
    }
    let passphrase = fresh_store(paths)?;
    let client = discover_client(paths, homeserver.trim(), &passphrase).await?;
    client
        .oauth()
        .server_metadata()
        .await
        .map_err(|_| anyhow!("this homeserver does not offer browser login; use a password"))?;

    let (redirect_uri, redirect) = LocalServerBuilder::new()
        .spawn()
        .await
        .context("could not start the local redirect server")?;
    let mut metadata = ClientMetadata::new(
        ApplicationType::Native,
        vec![OAuthGrantType::AuthorizationCode {
            redirect_uris: vec![redirect_uri.clone()],
        }],
        Localized::new(Url::parse(CLIENT_URI)?, []),
    );
    metadata.client_name = Some(Localized::new(DEVICE_NAME.to_owned(), []));
    let registration = ClientRegistrationData::new(Raw::new(&metadata)?);

    let oauth = client.oauth();
    let authorization = oauth
        .login(redirect_uri, None, Some(registration), None)
        .build()
        .await
        .map_err(|err| anyhow!("could not start browser login: {err}"))?;
    let _ = events.send(MatrixEvent::BrowserLoginUrl(authorization.url.to_string()));

    let query = redirect.await;
    let query = query.ok_or_else(|| anyhow!("the browser did not complete the login"))?;
    oauth
        .finish_login(query.into())
        .await
        .map_err(|err| anyhow!("login was refused: {err}"))?;
    save(paths, &client, &passphrase)?;
    Ok((client, passphrase))
}
