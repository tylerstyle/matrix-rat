//! The D-Bus Secret Service (`org.freedesktop.secrets`) as a [`KeyringBackend`],
//! through `zbus`: connecting to the session bus, the entries' attributes, and what
//! the service's errors mean.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use zbus::proxy::{Builder, CacheProperties, Proxy};
use zbus::{
    Connection, Error as ZbusError, ObjectPath, OwnedObjectPath, OwnedVariant, Variant, fdo,
};

use super::{KeyringBackend, KeyringError, KeyringKey};
use crate::rt;

const SERVICE_DEST: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_IFACE: &str = "org.freedesktop.Secret.Item";
const SESSION_IFACE: &str = "org.freedesktop.Secret.Session";
const PROMPT_IFACE: &str = "org.freedesktop.Secret.Prompt";

const ATTR_APP: &str = "application";
const ATTR_APP_VAL: &str = "matrix-rat";
const ATTR_PROFILE: &str = "profile";
const ATTR_USER_ID: &str = "user_id";
const ATTR_DEVICE_ID: &str = "device_id";

const SECRET_TIMEOUT: Duration = Duration::from_millis(500);

impl KeyringKey {
    fn owned_attributes(&self) -> HashMap<String, String> {
        HashMap::from([
            (ATTR_APP.to_owned(), ATTR_APP_VAL.to_owned()),
            (ATTR_PROFILE.to_owned(), self.profile.clone()),
            (ATTR_USER_ID.to_owned(), self.user_id.clone()),
            (ATTR_DEVICE_ID.to_owned(), self.device_id.clone()),
        ])
    }

    fn ref_attributes(&self) -> HashMap<&'static str, &str> {
        HashMap::from([
            (ATTR_APP, ATTR_APP_VAL),
            (ATTR_PROFILE, self.profile.as_str()),
            (ATTR_USER_ID, self.user_id.as_str()),
            (ATTR_DEVICE_ID, self.device_id.as_str()),
        ])
    }
}

/// Real D-Bus `org.freedesktop.secrets` client backed by `zbus`.
#[derive(Clone)]
pub struct SecretServiceKeyring {
    timeout: Duration,
    address: Option<String>,
}

/// Hand-written: a bus address names a path under the user's runtime folder.
impl std::fmt::Debug for SecretServiceKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretServiceKeyring")
            .field("timeout", &self.timeout)
            .field("address", &self.address.is_some())
            .finish()
    }
}

impl Default for SecretServiceKeyring {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretServiceKeyring {
    pub fn new() -> Self {
        Self {
            timeout: SECRET_TIMEOUT,
            address: None,
        }
    }

    #[cfg(test)]
    pub fn with_address(address: impl Into<String>) -> Self {
        Self {
            timeout: Duration::from_secs(2),
            address: Some(address.into()),
        }
    }

    async fn connect(&self) -> Result<Connection, KeyringError> {
        // A test never talks to the keyring of whoever runs it: forgetting a login
        // asks for an entry to be deleted, and a locked keyring answers that with an
        // unlock prompt on that person's screen. Tests give an address of their own.
        #[cfg(test)]
        if self.address.is_none() {
            return Err(KeyringError::Unavailable(
                "tests do not use the session's keyring".into(),
            ));
        }
        if self.address.is_none() && !has_session_bus_hint() {
            return Err(KeyringError::Unavailable("no D-Bus session bus".into()));
        }
        let fut = async {
            match &self.address {
                Some(addr) => zbus::connection::Builder::address(addr.as_str())
                    .build()
                    .await
                    .map_err(|err| KeyringError::Unavailable(err.to_string())),
                None => Connection::session()
                    .await
                    .map_err(|err| KeyringError::Unavailable(err.to_string())),
            }
        };
        rt::timeout(self.timeout, fut).await.map_err(|_| {
            KeyringError::Unavailable("timed out connecting to session D-Bus".into())
        })?
    }

    async fn run_with_session<T, F, Fut>(&self, op: F) -> Result<T, KeyringError>
    where
        F: FnOnce(Connection, Proxy<'static>, OwnedObjectPath) -> Fut,
        Fut: Future<Output = Result<T, KeyringError>>,
    {
        let conn = self.connect().await?;
        let timeout = self.timeout;
        rt::timeout(timeout, async move {
            let service = make_proxy(&conn, SERVICE_PATH, SERVICE_IFACE).await?;
            let reply = service
                .call_method("OpenSession", &("plain", Variant::from("")))
                .await
                .map_err(map_zbus_error)?;
            let (_output, session_path): (OwnedVariant, OwnedObjectPath) =
                reply.body().deserialize().map_err(map_zbus_error)?;

            let result = op(conn.clone(), service, session_path.clone()).await;
            close_session(&conn, &session_path).await;
            result
        })
        .await
        .map_err(|_| KeyringError::Unavailable("timed out waiting for system keyring".into()))?
    }
}

impl KeyringBackend for SecretServiceKeyring {
    fn is_auto_allowed(&self) -> bool {
        if self.address.is_some() {
            return true;
        }
        !crate::portal::in_ssh_session() && has_session_bus_hint()
    }

    async fn read(&self, key: &KeyringKey) -> Result<Option<String>, KeyringError> {
        self.run_with_session(|conn, service, session_path| async move {
            let reply = service
                .call_method("SearchItems", &(key.ref_attributes(),))
                .await
                .map_err(map_zbus_error)?;
            let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
                reply.body().deserialize().map_err(map_zbus_error)?;

            if let Some(item_path) = unlocked.into_iter().next() {
                let items = [ObjectPath::from(&item_path)];
                let reply = service
                    .call_method("GetSecrets", &(&items[..], ObjectPath::from(&session_path)))
                    .await
                    .map_err(map_zbus_error)?;
                let mut secrets: HashMap<
                    OwnedObjectPath,
                    (OwnedObjectPath, Vec<u8>, Vec<u8>, String),
                > = reply.body().deserialize().map_err(map_zbus_error)?;
                let Some((_sess, _params, value_bytes, _ctype)) = secrets.remove(&item_path) else {
                    return Ok(None);
                };
                let secret = String::from_utf8(value_bytes)
                    .map_err(|_| KeyringError::Failed("keyring entry is not valid UTF-8".into()))?;
                return Ok(Some(secret));
            }
            if !locked.is_empty() {
                return Err(KeyringError::Locked("default keyring is locked".into()));
            }
            // KeePassXC and strict Secret Service daemons encrypt attributes while locked and
            // return empty SearchItems results rather than locked object paths.
            if let Ok(col) = default_collection(&conn, &service).await
                && is_collection_locked(&conn, &col).await.unwrap_or(false)
            {
                return Err(KeyringError::Locked("default keyring is locked".into()));
            }
            Ok(None)
        })
        .await
    }

    async fn write(&self, key: &KeyringKey, secret: &str) -> Result<(), KeyringError> {
        self.run_with_session(|conn, service, session_path| async move {
            let col_path = default_collection(&conn, &service).await?;
            if is_collection_locked(&conn, &col_path).await? {
                return Err(KeyringError::Locked("default keyring is locked".into()));
            }
            let col_proxy = make_proxy(&conn, col_path.as_str(), COLLECTION_IFACE).await?;
            let attrs = key.owned_attributes();
            let mut props: HashMap<&str, Variant<'_>> = HashMap::new();
            props.insert(
                "org.freedesktop.Secret.Item.Label",
                Variant::from(key.label()),
            );
            props.insert(
                "org.freedesktop.Secret.Item.Attributes",
                Variant::from(attrs),
            );

            let secret_arg = (
                ObjectPath::from(&session_path),
                &[] as &[u8],
                secret.as_bytes(),
                "text/plain",
            );
            let reply = col_proxy
                .call_method("CreateItem", &(props, secret_arg, true))
                .await
                .map_err(map_zbus_error)?;
            let (item, prompt): (OwnedObjectPath, OwnedObjectPath) =
                reply.body().deserialize().map_err(map_zbus_error)?;
            if prompt.as_str() != "/" {
                dismiss_prompt(&conn, &prompt).await;
                return Err(KeyringError::Locked(
                    "keyring requires interactive unlock or confirmation".into(),
                ));
            }
            if item.as_str() == "/" {
                return Err(KeyringError::Failed(
                    "keyring did not return an item path".into(),
                ));
            }
            Ok(())
        })
        .await
    }

    async fn delete(&self, key: &KeyringKey) -> Result<(), KeyringError> {
        self.run_with_session(|conn, service, _session_path| async move {
            let reply = service
                .call_method("SearchItems", &(key.ref_attributes(),))
                .await
                .map_err(map_zbus_error)?;
            let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
                reply.body().deserialize().map_err(map_zbus_error)?;
            if unlocked.is_empty() && !locked.is_empty() {
                return Err(KeyringError::Locked("default keyring is locked".into()));
            }
            for item_path in unlocked {
                let item_proxy = make_proxy(&conn, item_path.as_str(), ITEM_IFACE).await?;
                let reply = item_proxy
                    .call_method("Delete", &())
                    .await
                    .map_err(map_zbus_error)?;
                let prompt: OwnedObjectPath = reply.body().deserialize().map_err(map_zbus_error)?;
                if prompt.as_str() != "/" {
                    dismiss_prompt(&conn, &prompt).await;
                    return Err(KeyringError::Locked(
                        "keyring requires unlock to delete entry".into(),
                    ));
                }
            }
            if !locked.is_empty() {
                return Err(KeyringError::Locked(
                    "remaining keyring entry is locked".into(),
                ));
            }
            Ok(())
        })
        .await
    }
}

fn has_session_bus_hint() -> bool {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .is_some_and(|dir| std::path::Path::new(&dir).join("bus").exists())
}

async fn make_proxy(
    conn: &Connection,
    path: &str,
    iface: &str,
) -> Result<Proxy<'static>, KeyringError> {
    Builder::<Proxy<'static>>::new(conn)
        .destination(SERVICE_DEST)
        .path(path.to_owned())
        .interface(iface.to_owned())
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(map_zbus_error)
}

async fn default_collection(
    conn: &Connection,
    service: &Proxy<'static>,
) -> Result<OwnedObjectPath, KeyringError> {
    let reply = service
        .call_method("ReadAlias", &("default",))
        .await
        .map_err(map_zbus_error)?;
    let path: OwnedObjectPath = reply.body().deserialize().map_err(map_zbus_error)?;
    if path.as_str() != "/" {
        return Ok(path);
    }
    let collections: Vec<OwnedObjectPath> = service
        .get_property("Collections")
        .await
        .map_err(map_zbus_error)?;
    for col in collections {
        if col.as_str().ends_with("/session") {
            continue;
        }
        if let Ok(col_proxy) = make_proxy(conn, col.as_str(), COLLECTION_IFACE).await
            && col_proxy
                .get_property::<String>("Label")
                .await
                .is_ok_and(|label| label.eq_ignore_ascii_case("session"))
        {
            continue;
        }
        return Ok(col);
    }
    Err(KeyringError::Unavailable(
        "no persistent default collection in system keyring".into(),
    ))
}

async fn is_collection_locked(
    conn: &Connection,
    col_path: &OwnedObjectPath,
) -> Result<bool, KeyringError> {
    let proxy = make_proxy(conn, col_path.as_str(), COLLECTION_IFACE).await?;
    proxy.get_property("Locked").await.map_err(map_zbus_error)
}

async fn close_session(conn: &Connection, session_path: &OwnedObjectPath) {
    if let Ok(proxy) = make_proxy(conn, session_path.as_str(), SESSION_IFACE).await {
        let _ = proxy.call_method("Close", &()).await;
    }
}

async fn dismiss_prompt(conn: &Connection, prompt_path: &OwnedObjectPath) {
    if let Ok(proxy) = make_proxy(conn, prompt_path.as_str(), PROMPT_IFACE).await {
        let _ = proxy.call_method("Dismiss", &()).await;
    }
}

fn map_zbus_error(err: ZbusError) -> KeyringError {
    #[expect(clippy::wildcard_enum_match_arm, reason = "foreign zbus::Error")]
    match &err {
        ZbusError::FDO(fdo_err)
            if matches!(
                &**fdo_err,
                fdo::Error::ServiceUnknown(_)
                    | fdo::Error::NameHasNoOwner(_)
                    | fdo::Error::UnknownObject(_)
                    | fdo::Error::UnknownInterface(_)
                    | fdo::Error::AccessDenied(_)
                    | fdo::Error::SpawnExecFailed(_)
                    | fdo::Error::SpawnChildExited(_)
                    | fdo::Error::SpawnFailed(_)
            ) =>
        {
            KeyringError::Unavailable(fdo_err.to_string())
        }
        ZbusError::MethodError(name, msg, _)
            if name.as_str() == "org.freedesktop.Secret.Error.IsLocked" =>
        {
            KeyringError::Locked(
                msg.clone()
                    .unwrap_or_else(|| "keyring object is locked".into()),
            )
        }
        ZbusError::InputOutput(_) => KeyringError::Unavailable(err.to_string()),
        _ => KeyringError::Failed(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};

    use futures::StreamExt;
    use zbus::names::UniqueName;

    use super::*;
    use crate::matrix::keyring::write_and_verify_keyring;

    #[tokio::test]
    async fn a_test_never_reaches_the_keyring_of_the_session() {
        // Whatever the environment says: without an address of its own, nothing is
        // connected to.
        let keyring = SecretServiceKeyring::new();
        assert!(matches!(
            keyring.connect().await,
            Err(KeyringError::Unavailable(_))
        ));
    }

    fn sample_key() -> KeyringKey {
        KeyringKey::new("default", "@alice:example.org", "DEV123")
    }

    #[test]
    fn the_bus_address_stays_out_of_debug() {
        let keyring = SecretServiceKeyring::with_address("unix:path=/run/user/1000/bus");
        let shown = format!("{keyring:?}");
        assert!(
            !shown.contains("/run") && shown.contains("address: true"),
            "{shown}"
        );
    }

    struct DbusDaemonGuard {
        child: Child,
        address: String,
    }

    impl Drop for DbusDaemonGuard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn start_private_dbus_daemon() -> DbusDaemonGuard {
        let mut child = Command::new("dbus-daemon")
            // Not `--session`: that could start the desktop's own keyring.
            .args([
                crate::testbus::config_arg().as_str(),
                "--print-address=1",
                "--nofork",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon must be installed to run secret service test");
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut address = String::new();
        reader.read_line(&mut address).unwrap();
        let address = address.trim().to_owned();
        assert!(!address.is_empty(), "dbus-daemon did not print address");
        DbusDaemonGuard { child, address }
    }

    type StoredSecretItem = (HashMap<String, String>, String, Vec<u8>);
    type WireSecretTuple = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);
    type CreateItemArgs = (HashMap<String, OwnedVariant>, WireSecretTuple, bool);

    #[derive(Default)]
    struct FakeSecretDaemonState {
        locked: bool,
        next_id: usize,
        items: HashMap<String, StoredSecretItem>,
    }

    async fn spawn_fake_secret_service(
        address: &str,
        state: Arc<Mutex<FakeSecretDaemonState>>,
    ) -> tokio::task::JoinHandle<()> {
        let conn = zbus::connection::Builder::address(address)
            .build()
            .await
            .unwrap();
        // Listening starts before the name is owned: a stream hears only what comes
        // after it is made, and a client may call as soon as the name is there, before
        // the task below has run (on a busy machine). That call would go unanswered.
        let mut stream = zbus::MessageStream::from(&conn);
        conn.request_name("org.freedesktop.secrets").await.unwrap();

        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                if msg.header().message_type() != zbus::message::Type::MethodCall {
                    continue;
                }
                let hdr = msg.header();
                let path = hdr.path().map(|p| p.as_str()).unwrap_or("");
                let iface = hdr.interface().map(|i| i.as_str()).unwrap_or("");
                let member = hdr.member().map(|m| m.as_str()).unwrap_or("");
                let sender_name: Option<UniqueName<'_>> = hdr.sender().cloned();
                let _ = sender_name;

                match (path, iface, member) {
                    ("/org/freedesktop/secrets", SERVICE_IFACE, "OpenSession") => {
                        let (_alg, _input): (String, OwnedVariant) =
                            msg.body().deserialize().unwrap();
                        let sess = OwnedObjectPath::try_from("/org/freedesktop/secrets/session/s1")
                            .unwrap();
                        let _ = conn.reply(&hdr, &(Variant::from(""), sess)).await;
                    }
                    ("/org/freedesktop/secrets", SERVICE_IFACE, "ReadAlias") => {
                        let alias: String = msg.body().deserialize().unwrap();
                        let target = if alias == "default" {
                            "/org/freedesktop/secrets/collection/default"
                        } else {
                            "/"
                        };
                        let path = OwnedObjectPath::try_from(target).unwrap();
                        let _ = conn.reply(&hdr, &path).await;
                    }
                    ("/org/freedesktop/secrets", SERVICE_IFACE, "SearchItems") => {
                        let attrs: HashMap<String, String> = msg.body().deserialize().unwrap();
                        let (unlocked, locked) = {
                            let st = state.lock().unwrap();
                            let mut unlocked = Vec::new();
                            let mut locked = Vec::new();
                            for (item_path, (item_attrs, _, _)) in &st.items {
                                if attrs.iter().all(|(k, v)| item_attrs.get(k) == Some(v)) {
                                    let p = OwnedObjectPath::try_from(item_path.as_str()).unwrap();
                                    if st.locked {
                                        locked.push(p);
                                    } else {
                                        unlocked.push(p);
                                    }
                                }
                            }
                            (unlocked, locked)
                        };
                        let _ = conn.reply(&hdr, &(unlocked, locked)).await;
                    }
                    ("/org/freedesktop/secrets", SERVICE_IFACE, "GetSecrets") => {
                        let (items, session): (Vec<OwnedObjectPath>, OwnedObjectPath) =
                            msg.body().deserialize().unwrap();
                        let res = {
                            let st = state.lock().unwrap();
                            if st.locked {
                                Err(())
                            } else {
                                let mut out: HashMap<
                                    OwnedObjectPath,
                                    (OwnedObjectPath, Vec<u8>, Vec<u8>, String),
                                > = HashMap::new();
                                for item in items {
                                    if let Some((_, ctype, val)) = st.items.get(item.as_str()) {
                                        out.insert(
                                            item,
                                            (
                                                session.clone(),
                                                Vec::new(),
                                                val.clone(),
                                                ctype.clone(),
                                            ),
                                        );
                                    }
                                }
                                Ok(out)
                            }
                        };
                        match res {
                            Err(()) => {
                                let _ = conn
                                    .reply_error(
                                        &hdr,
                                        "org.freedesktop.Secret.Error.IsLocked",
                                        &"collection is locked",
                                    )
                                    .await;
                            }
                            Ok(out) => {
                                let _ = conn.reply(&hdr, &out).await;
                            }
                        }
                    }
                    (
                        "/org/freedesktop/secrets/collection/default",
                        "org.freedesktop.DBus.Properties",
                        "Get",
                    ) => {
                        let (_if, prop): (String, String) = msg.body().deserialize().unwrap();
                        if prop == "Locked" {
                            let locked = state.lock().unwrap().locked;
                            let _ = conn.reply(&hdr, &Variant::from(locked)).await;
                        }
                    }
                    (
                        "/org/freedesktop/secrets/collection/default",
                        COLLECTION_IFACE,
                        "CreateItem",
                    ) => {
                        let (props, secret, replace): CreateItemArgs =
                            msg.body().deserialize().unwrap();
                        let res = {
                            let mut st = state.lock().unwrap();
                            if st.locked {
                                Err(())
                            } else {
                                let attrs = props
                                    .get("org.freedesktop.Secret.Item.Attributes")
                                    .and_then(|v| {
                                        HashMap::<String, String>::try_from(v.clone()).ok()
                                    })
                                    .unwrap_or_default();
                                let existing = st
                                    .items
                                    .iter()
                                    .find(|(_, (a, _, _))| a == &attrs)
                                    .map(|(p, _)| p.clone());
                                let item_path = match (existing, replace) {
                                    (Some(p), true) => p,
                                    _ => {
                                        st.next_id += 1;
                                        format!(
                                            "/org/freedesktop/secrets/collection/default/i{}",
                                            st.next_id
                                        )
                                    }
                                };
                                st.items
                                    .insert(item_path.clone(), (attrs, secret.3, secret.2));
                                Ok((
                                    OwnedObjectPath::try_from(item_path.as_str()).unwrap(),
                                    OwnedObjectPath::try_from("/").unwrap(),
                                ))
                            }
                        };
                        match res {
                            Err(()) => {
                                let _ = conn
                                    .reply_error(
                                        &hdr,
                                        "org.freedesktop.Secret.Error.IsLocked",
                                        &"collection is locked",
                                    )
                                    .await;
                            }
                            Ok(reply) => {
                                let _ = conn.reply(&hdr, &reply).await;
                            }
                        }
                    }
                    (p, ITEM_IFACE, "Delete") if p.starts_with("/org/freedesktop/secrets/") => {
                        let locked = {
                            let mut st = state.lock().unwrap();
                            if st.locked {
                                true
                            } else {
                                st.items.remove(p);
                                false
                            }
                        };
                        if locked {
                            let _ = conn
                                .reply_error(
                                    &hdr,
                                    "org.freedesktop.Secret.Error.IsLocked",
                                    &"item is locked",
                                )
                                .await;
                        } else {
                            let no_prompt = OwnedObjectPath::try_from("/").unwrap();
                            let _ = conn.reply(&hdr, &no_prompt).await;
                        }
                    }
                    (s, SESSION_IFACE, "Close")
                        if s.starts_with("/org/freedesktop/secrets/session/") =>
                    {
                        let _ = conn.reply(&hdr, &()).await;
                    }
                    _ => {
                        let _ = conn
                            .reply_error(
                                &hdr,
                                "org.freedesktop.DBus.Error.UnknownMethod",
                                &format!("unhandled {path} {iface}.{member}"),
                            )
                            .await;
                    }
                }
            }
        })
    }

    #[tokio::test]
    async fn dbus_secret_service_roundtrip_over_private_bus() {
        let daemon = start_private_dbus_daemon();
        let state = Arc::new(Mutex::new(FakeSecretDaemonState::default()));
        let service_task = spawn_fake_secret_service(&daemon.address, state.clone()).await;

        // Wait briefly until well-known name is owned.
        let client = SecretServiceKeyring::with_address(&daemon.address);
        let key = sample_key();

        // Missing initially.
        assert_eq!(client.read(&key).await.unwrap(), None);

        // Write + read back + overwrite.
        write_and_verify_keyring(&client, &key, "pass-v1")
            .await
            .unwrap();
        assert_eq!(client.read(&key).await.unwrap().as_deref(), Some("pass-v1"));

        write_and_verify_keyring(&client, &key, "pass-v2")
            .await
            .unwrap();
        assert_eq!(client.read(&key).await.unwrap().as_deref(), Some("pass-v2"));
        assert_eq!(state.lock().unwrap().items.len(), 1);

        // Different device_id or profile gets distinct entry.
        let other = KeyringKey::new("work", "@alice:example.org", "DEV999");
        assert_eq!(client.read(&other).await.unwrap(), None);

        // Locked keyring returns KeyringError::Locked cleanly on read and write.
        state.lock().unwrap().locked = true;
        assert!(matches!(
            client.read(&key).await,
            Err(KeyringError::Locked(_))
        ));
        assert!(matches!(
            client.write(&key, "pass-v3").await,
            Err(KeyringError::Locked(_))
        ));

        // Unlock and delete cleanly removes the entry.
        state.lock().unwrap().locked = false;
        client.delete(&key).await.unwrap();
        assert_eq!(client.read(&key).await.unwrap(), None);

        service_task.abort();
    }

    #[tokio::test]
    async fn dbus_missing_service_returns_unavailable() {
        let daemon = start_private_dbus_daemon();
        let client = SecretServiceKeyring::with_address(&daemon.address);
        let err = client.read(&sample_key()).await.unwrap_err();
        assert!(matches!(err, KeyringError::Unavailable(_)), "{err:?}");
    }
}
