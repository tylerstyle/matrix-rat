//! Desktop colour-scheme tracking via the XDG Desktop Portal on D-Bus.
//!
//! Queries `org.freedesktop.portal.Settings` (`org.freedesktop.appearance` /
//! `color-scheme`) at startup and subscribes to live `SettingChanged` signals on a
//! background task. In SSH sessions, when no session bus is available, or when the
//! portal is missing, live following silently stays inactive and startup falls back to
//! terminal queries (`CSI ? 996 n` / `OSC 11`), `COLORFGBG`, and finally dark mode.

use std::ffi::OsString;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::task::JoinHandle;
use zbus::proxy::{Builder, CacheProperties, Proxy, SignalStream};
use zbus::{Connection, Error, Message, Variant, fdo};

use crate::termquery;
use crate::theme::ColorScheme;

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS_IFACE: &str = "org.freedesktop.portal.Settings";
const APPEARANCE_NS: &str = "org.freedesktop.appearance";
const COLOR_SCHEME_KEY: &str = "color-scheme";
const START_TIMEOUT: Duration = Duration::from_millis(300);

/// Live stream of desktop dark/light changes from the XDG Desktop Portal.
pub struct SchemeWatch {
    changes: UnboundedReceiver<ColorScheme>,
    task: Option<JoinHandle<()>>,
}

impl Drop for SchemeWatch {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl SchemeWatch {
    fn inactive() -> Self {
        let (_tx, changes) = unbounded_channel();
        Self {
            changes,
            task: None,
        }
    }

    /// Reads any current desktop preference and starts watching for live changes.
    /// Silently becomes inactive in SSH sessions or when no bus/portal is available.
    pub async fn start() -> (Option<ColorScheme>, Self) {
        if in_ssh_session() {
            return (None, Self::inactive());
        }
        let connected = tokio::time::timeout(START_TIMEOUT, connect_portal())
            .await
            .ok()
            .flatten();
        let Some((initial, proxy, mut stream)) = connected else {
            return (None, Self::inactive());
        };
        let (tx, changes) = unbounded_channel();
        let task = tokio::spawn(async move {
            let _keep_proxy = proxy;
            while let Some(msg) = stream.next().await {
                if let Some(scheme) = parse_setting_changed_message(&msg)
                    && tx.send(scheme).is_err()
                {
                    break;
                }
            }
        });
        (
            initial,
            Self {
                changes,
                task: Some(task),
            },
        )
    }

    /// Waits for the next desktop colour-scheme change, coalescing bursts. When inactive
    /// (headless, SSH, or no portal), sleeps indefinitely without waking `select!`.
    pub async fn changed(&mut self) -> ColorScheme {
        let Some(mut latest) = self.changes.recv().await else {
            return std::future::pending::<ColorScheme>().await;
        };
        while let Ok(newer) = self.changes.try_recv() {
            latest = newer;
        }
        latest
    }
}

/// Precedence at startup: desktop portal preference, then terminal query (`CSI ?997` or
/// `OSC 11`), then `COLORFGBG`, then dark mode.
pub fn resolve_initial_scheme(
    portal: Option<ColorScheme>,
    term_reply: Option<termquery::Reply>,
) -> ColorScheme {
    portal.unwrap_or_else(|| termquery::resolve_color_scheme(term_reply))
}

/// Maps XDG portal `org.freedesktop.appearance` `color-scheme` integers:
/// `0` = no preference, `1` = prefer dark, `2` = prefer light.
pub fn parse_portal_color_scheme(value: u32) -> Option<ColorScheme> {
    match value {
        1 => Some(ColorScheme::Dark),
        2 => Some(ColorScheme::Light),
        _ => None,
    }
}

fn variant_u32(value: &Variant<'_>) -> Option<u32> {
    match value {
        Variant::U32(n) => Some(*n),
        Variant::Variant(inner) => variant_u32(inner),
        _ => None,
    }
}

fn parse_setting_changed_message(msg: &Message) -> Option<ColorScheme> {
    let body = msg.body();
    let (ns, key, value): (&str, &str, Variant<'_>) = body.deserialize().ok()?;
    if ns != APPEARANCE_NS || key != COLOR_SCHEME_KEY {
        return None;
    }
    variant_u32(&value).and_then(parse_portal_color_scheme)
}

pub(crate) fn in_ssh_session() -> bool {
    is_ssh_env(|name| std::env::var_os(name))
}

fn is_ssh_env<F>(get: F) -> bool
where
    F: Fn(&str) -> Option<OsString>,
{
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .into_iter()
        .any(|name| get(name).is_some_and(|val| !val.is_empty()))
}

async fn connect_portal() -> Option<(Option<ColorScheme>, Proxy<'static>, SignalStream<'static>)> {
    let conn = Connection::session().await.ok()?;
    let proxy = Builder::<Proxy<'static>>::new(&conn)
        .destination(PORTAL_DEST)
        .path(PORTAL_PATH)
        .interface(SETTINGS_IFACE)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .ok()?;
    // Subscribe before querying the initial value so a change in between is never missed.
    let stream = proxy
        .receive_signal_with_args(
            "SettingChanged",
            &[(0, APPEARANCE_NS), (1, COLOR_SCHEME_KEY)],
        )
        .await
        .ok()?;
    let initial = read_initial_scheme(&proxy).await?;
    Some((initial, proxy, stream))
}

async fn read_initial_scheme(proxy: &Proxy<'static>) -> Option<Option<ColorScheme>> {
    let args = (APPEARANCE_NS, COLOR_SCHEME_KEY);
    match proxy.call_method("ReadOne", &args).await {
        Ok(msg) => {
            let body = msg.body();
            let val: Variant<'_> = body.deserialize().ok()?;
            return Some(variant_u32(&val).and_then(parse_portal_color_scheme));
        }
        Err(err) if is_unknown_method(&err) => {}
        Err(err) if is_missing_service(&err) => return None,
        Err(err) if is_not_found(&err) => return Some(None),
        Err(err) => {
            tracing::debug!(?err, "portal ReadOne failed; trying Read");
        }
    }
    // Settings v1 fallback: `Read` returns a nested variant (`v(v(u))`).
    match proxy.call_method("Read", &args).await {
        Ok(msg) => {
            let body = msg.body();
            let val: Variant<'_> = body.deserialize().ok()?;
            Some(variant_u32(&val).and_then(parse_portal_color_scheme))
        }
        Err(err) if is_missing_service(&err) => None,
        Err(err) if is_not_found(&err) => Some(None),
        Err(err) => {
            tracing::debug!(?err, "portal Read failed");
            Some(None)
        }
    }
}

fn is_unknown_method(err: &Error) -> bool {
    matches!(err, Error::FDO(fdo) if matches!(&**fdo, fdo::Error::UnknownMethod(_)))
}

fn is_missing_service(err: &Error) -> bool {
    matches!(
        err,
        Error::FDO(fdo)
            if matches!(
                &**fdo,
                fdo::Error::ServiceUnknown(_)
                    | fdo::Error::NameHasNoOwner(_)
                    | fdo::Error::UnknownObject(_)
                    | fdo::Error::UnknownInterface(_)
                    | fdo::Error::AccessDenied(_)
            )
    )
}

fn is_not_found(err: &Error) -> bool {
    matches!(
        err,
        Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.portal.Error.NotFound"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::wire::{LE, serialized::Context, to_bytes};

    #[test]
    fn portal_values_and_variants_map_cleanly() {
        assert_eq!(parse_portal_color_scheme(0), None);
        assert_eq!(parse_portal_color_scheme(1), Some(ColorScheme::Dark));
        assert_eq!(parse_portal_color_scheme(2), Some(ColorScheme::Light));
        assert_eq!(parse_portal_color_scheme(99), None);

        // ReadOne returns `v(u)`; Settings v1 `Read` returns `v(v(u))`.
        let direct = Variant::U32(2);
        let nested = Variant::Variant(Box::new(Variant::U32(1)));
        let no_pref = Variant::Variant(Box::new(Variant::U32(0)));
        let wrong_type = Variant::from("light");
        assert_eq!(
            variant_u32(&direct).and_then(parse_portal_color_scheme),
            Some(ColorScheme::Light)
        );
        assert_eq!(
            variant_u32(&nested).and_then(parse_portal_color_scheme),
            Some(ColorScheme::Dark)
        );
        assert_eq!(
            variant_u32(&no_pref).and_then(parse_portal_color_scheme),
            None
        );
        assert_eq!(variant_u32(&wrong_type), None);

        // Verify round-trip D-Bus wire encoding of the signal body `(ssv)`.
        let ctxt = Context::new(LE, 0);
        let encoded =
            to_bytes(ctxt, &(APPEARANCE_NS, COLOR_SCHEME_KEY, Variant::U32(2u32))).unwrap();
        let (ns, key, val): (&str, &str, Variant<'_>) = encoded.deserialize().unwrap().0;
        assert_eq!(ns, APPEARANCE_NS);
        assert_eq!(key, COLOR_SCHEME_KEY);
        assert_eq!(
            variant_u32(&val).and_then(parse_portal_color_scheme),
            Some(ColorScheme::Light)
        );
    }

    #[test]
    fn setting_changed_signal_decodes_real_dbus_message() {
        let msg = Message::signal(PORTAL_PATH, SETTINGS_IFACE, "SettingChanged")
            .build(&(APPEARANCE_NS, COLOR_SCHEME_KEY, Variant::U32(2u32)))
            .unwrap();
        assert_eq!(
            parse_setting_changed_message(&msg),
            Some(ColorScheme::Light)
        );

        let no_pref = Message::signal(PORTAL_PATH, SETTINGS_IFACE, "SettingChanged")
            .build(&(APPEARANCE_NS, COLOR_SCHEME_KEY, Variant::U32(0u32)))
            .unwrap();
        assert_eq!(parse_setting_changed_message(&no_pref), None);

        let unrelated = Message::signal(PORTAL_PATH, SETTINGS_IFACE, "SettingChanged")
            .build(&("org.freedesktop.appearance", "contrast", Variant::U32(1u32)))
            .unwrap();
        assert_eq!(parse_setting_changed_message(&unrelated), None);
    }

    #[test]
    fn initial_scheme_precedence_prefers_portal_then_terminal_then_default() {
        let dark_reply = termquery::Reply {
            reported_scheme: Some(ColorScheme::Dark),
            background_scheme: Some(ColorScheme::Light),
            complete: true,
            ..termquery::Reply::default()
        };
        // Portal preference wins over terminal report.
        assert_eq!(
            resolve_initial_scheme(Some(ColorScheme::Light), Some(dark_reply)),
            ColorScheme::Light
        );
        // No portal preference (0 / missing / SSH) falls back to terminal report.
        assert_eq!(
            resolve_initial_scheme(None, Some(dark_reply)),
            ColorScheme::Dark
        );
        let bg_only = termquery::Reply {
            background_scheme: Some(ColorScheme::Light),
            complete: true,
            ..termquery::Reply::default()
        };
        assert_eq!(
            resolve_initial_scheme(None, Some(bg_only)),
            ColorScheme::Light
        );
    }

    #[test]
    fn ssh_detection_checks_standard_env_vars() {
        assert!(!is_ssh_env(|_| None));
        assert!(!is_ssh_env(|k| (k == "SSH_CONNECTION").then(|| "".into())));
        assert!(is_ssh_env(
            |k| (k == "SSH_CONNECTION").then(|| "1 2 3 4".into())
        ));
        assert!(is_ssh_env(|k| (k == "SSH_CLIENT").then(|| "1 2 3".into())));
        assert!(is_ssh_env(|k| (k == "SSH_TTY").then(|| "/dev/pts/1".into())));
    }
}
