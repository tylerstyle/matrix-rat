//! Brings the terminal window matrix-rat runs in to the front, after a click on a
//! desktop notification.
//!
//! A program inside a terminal does not own a window, so it cannot raise one itself;
//! only the compositor can, and each has its own way. What is tried, in this order:
//!
//! 1. the user's own command (`notification_click_command` in the config file), for
//!    any desktop;
//! 2. niri, which lists its windows with the process they belong to: the window of
//!    the terminal we descend from is focused.
//!
//! Everything here is best effort and runs off the UI thread: a desktop without a way
//! simply keeps the window where it is, and the room is opened all the same.

use std::process::{Command, Stdio};
use std::time::Duration;

/// The longest a helper program may take before it is given up on.
const PATIENCE: Duration = Duration::from_secs(3);

/// Asks the desktop to show our window. Returns at once.
pub fn raise_window(command: &str) {
    let command = command.trim().to_owned();
    // A notification clicked on this desktop is about a window on this desktop;
    // over SSH the terminal is somewhere else.
    if command.is_empty() && crate::portal::in_ssh_session() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("raise-window".to_owned())
        .spawn(move || {
            if !command.is_empty() {
                run_user_command(&command);
            } else if std::env::var_os("NIRI_SOCKET").is_some() {
                raise_in_niri();
            }
        });
    if let Err(err) = spawned {
        tracing::debug!(%err, "could not start the window helper");
    }
}

/// Runs a program to its end or until `PATIENCE` runs out, and returns what it wrote
/// (if `capture`; else nothing) if it succeeded. Nothing of ours is passed to it on
/// standard input.
fn run(mut command: Command, capture: bool) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut child = command
        .stdin(Stdio::null())
        // Only read where the answer is wanted: a program that leaves something
        // running with our pipe still open would otherwise keep us waiting for its end.
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read on a thread of its own, so that a program which never ends cannot hold
    // this one for longer than our patience.
    let reader = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            // The lists we read are small; a program that writes without end is cut off.
            let _ = (&mut stdout).take(4 * 1024 * 1024).read_to_end(&mut out);
            out
        })
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < PATIENCE => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let status = status.filter(|status| status.success())?;
    let _ = status;
    match reader {
        Some(reader) => reader.join().ok(),
        None => Some(Vec::new()),
    }
}

fn run_user_command(command: &str) {
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(command);
    if run(shell, false).is_none() {
        tracing::debug!("the notification click command failed or took too long");
    }
}

fn raise_in_niri() {
    let mut list = Command::new("niri");
    list.args(["msg", "--json", "windows"]);
    let Some(out) = run(list, true) else {
        return;
    };
    let Some(id) = niri_window(&out, &ancestors(std::process::id())) else {
        tracing::debug!("no single niri window belongs to this terminal");
        return;
    };
    let mut focus = Command::new("niri");
    focus.args(["msg", "action", "focus-window", "--id", &id.to_string()]);
    let _ = run(focus, false);
}

/// The window among niri's that belongs to one of these processes (ours first, then
/// its parents: the nearest one wins). A terminal that keeps several windows in one
/// process leaves no way to tell them apart by process; then the one that names us
/// in its title is taken, and failing that none: the wrong window in front would be
/// worse than none.
fn niri_window(json: &[u8], ancestors: &[u32]) -> Option<u64> {
    let windows: serde_json::Value = serde_json::from_slice(json).ok()?;
    let windows = windows.as_array()?;
    for pid in ancestors {
        let mine: Vec<&serde_json::Value> = windows
            .iter()
            .filter(|window| window["pid"].as_u64() == Some(u64::from(*pid)))
            .collect();
        match mine.as_slice() {
            [] => continue,
            [only] => return only["id"].as_u64(),
            several => {
                let mut named = several.iter().filter(|window| {
                    window["title"]
                        .as_str()
                        .is_some_and(|title| title.contains("matrix-rat"))
                });
                return match (named.next(), named.next()) {
                    (Some(one), None) => one["id"].as_u64(),
                    _ => None,
                };
            }
        }
    }
    None
}

/// This process and its parents, nearest first, as far as `/proc` tells.
fn ancestors(mut pid: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    // A process tree is not deep; the bound is against a loop in bad data.
    while pid > 1 && chain.len() < 64 {
        chain.push(pid);
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            break;
        };
        match parent_of(&stat) {
            Some(parent) => pid = parent,
            None => break,
        }
    }
    chain
}

/// The parent's process id from a `/proc/<pid>/stat` line. The program's name comes
/// second, in brackets, and may itself hold spaces and brackets: the fields are
/// counted from its last closing bracket.
fn parent_of(stat: &str) -> Option<u32> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    // After the name: the state, then the parent.
    after_name.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOWS: &str = r#"[
        {"id": 5, "pid": 100, "app_id": "kitty", "title": "matrix-rat", "is_focused": false},
        {"id": 7, "pid": 300, "app_id": "firefox", "title": "a page", "is_focused": true},
        {"id": 8, "pid": 400, "app_id": "kitty", "title": "shell", "is_focused": false},
        {"id": 9, "pid": 400, "app_id": "kitty", "title": "matrix-rat", "is_focused": false},
        {"id": 10, "pid": 500, "app_id": "foot", "title": "one", "is_focused": false},
        {"id": 11, "pid": 500, "app_id": "foot", "title": "two", "is_focused": false},
        {"id": 12, "pid": null, "app_id": null, "title": null, "is_focused": false}
    ]"#;

    #[test]
    fn the_window_of_the_terminal_we_run_in_is_found() {
        let find = |ancestors: &[u32]| niri_window(WINDOWS.as_bytes(), ancestors);
        // Ourselves, a shell, then the terminal.
        assert_eq!(find(&[900, 800, 100, 1]), Some(5));
        // Nothing of ours has a window (a multiplexer's server, say).
        assert_eq!(find(&[900, 800]), None);
        // The nearest parent with a window wins.
        assert_eq!(find(&[900, 100, 300]), Some(5));
        // One process, several windows: the one that names us.
        assert_eq!(find(&[900, 400]), Some(9));
        // No telling which: none.
        assert_eq!(find(&[900, 500]), None);
    }

    #[test]
    fn bad_answers_from_the_compositor_find_nothing() {
        assert_eq!(niri_window(b"", &[1]), None);
        assert_eq!(niri_window(b"{}", &[1]), None);
        assert_eq!(niri_window(b"[{\"pid\": 1}]", &[1]), None);
        assert_eq!(niri_window(b"[{\"id\": \"x\", \"pid\": 1}]", &[1]), None);
    }

    #[test]
    fn the_parent_is_read_past_a_name_with_spaces_and_brackets() {
        assert_eq!(parent_of("42 (bash) S 7 42 42 0"), Some(7));
        assert_eq!(parent_of("42 (a (odd) name) R 13 1 1"), Some(13));
        assert_eq!(parent_of("42 bash S 7"), None);
        assert_eq!(parent_of("42 (bash)"), None);
    }

    #[test]
    fn our_own_ancestors_start_with_us_and_end_before_init() {
        let chain = ancestors(std::process::id());
        assert_eq!(chain.first(), Some(&std::process::id()));
        assert!(chain.len() >= 2, "{chain:?}");
        assert!(!chain.contains(&1));
    }

    #[test]
    fn a_program_that_never_ends_is_given_up_on() {
        let mut slow = Command::new("sleep");
        slow.arg("30");
        let started = std::time::Instant::now();
        assert!(run(slow, true).is_none());
        assert!(started.elapsed() < PATIENCE + Duration::from_secs(2));
        let mut quick = Command::new("sh");
        quick.args(["-c", "printf hello"]);
        assert_eq!(run(quick, true).as_deref(), Some(&b"hello"[..]));
        let mut failing = Command::new("sh");
        failing.args(["-c", "exit 3"]);
        assert!(run(failing, true).is_none());
        // Something left running with the output still open does not hold us when
        // the output is not wanted.
        let mut lingering = Command::new("sh");
        lingering.args(["-c", "sleep 30 & exit 0"]);
        let started = std::time::Instant::now();
        assert_eq!(run(lingering, false), Some(Vec::new()));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
