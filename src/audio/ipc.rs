//! mpv's JSON control socket (`--input-ipc-server`): what is asked of it, and reading
//! what comes back. The other end is meant to be mpv, but what it says is checked all
//! the same: a line is at most [`MAX_LINE`] bytes (a longer one is skipped without
//! being kept), numbers must be sane, and whatever is not understood is ignored.

use std::time::Duration;

use serde_json::Value;

use super::{Control, MAX_DURATION};

/// The longest line taken; mpv's own are well under a kilobyte.
pub const MAX_LINE: usize = 4096;

/// Asks mpv to report these whenever they change.
pub const OBSERVE: &[u8] = b"{\"command\":[\"observe_property\",1,\"time-pos\"]}\n\
{\"command\":[\"observe_property\",2,\"pause\"]}\n\
{\"command\":[\"observe_property\",3,\"duration\"]}\n\
{\"command\":[\"observe_property\",4,\"eof-reached\"]}\n";

/// What a line from mpv said that matters here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Said {
    Position(Duration),
    Length(Duration),
    Paused(bool),
    /// The end of the file was reached (mpv ends right after).
    Ended,
}

/// What one line says, if it is a change of one of the observed properties with a
/// sane value. Anything else (answers to commands, other events, broken JSON, a line
/// too long, a negative or endless number) is `None`.
pub fn parse(line: &[u8]) -> Option<Said> {
    if line.len() > MAX_LINE {
        return None;
    }
    let value: Value = serde_json::from_slice(line).ok()?;
    if value.get("event")?.as_str()? != "property-change" {
        return None;
    }
    let data = value.get("data")?;
    match value.get("name")?.as_str()? {
        "time-pos" => seconds(data).map(Said::Position),
        "duration" => seconds(data).map(Said::Length),
        "pause" => data.as_bool().map(Said::Paused),
        "eof-reached" => data.as_bool().filter(|ended| *ended).map(|_| Said::Ended),
        _ => None,
    }
}

/// A number of seconds: finite, at most [`MAX_DURATION`]; a little below zero (mpv
/// says so at the start of some files) counts as zero.
fn seconds(data: &Value) -> Option<Duration> {
    let secs = data.as_f64()?;
    (secs.is_finite() && secs > -1.0 && secs <= MAX_DURATION.as_secs_f64())
        .then(|| Duration::from_secs_f64(secs.max(0.0)))
}

/// The line that asks mpv for `control`; `None` for stopping, which is done by the
/// process, not by asking.
pub fn command(control: Control) -> Option<Vec<u8>> {
    match control {
        Control::TogglePause => Some(b"{\"command\":[\"cycle\",\"pause\"]}\n".to_vec()),
        Control::Seek(secs) => {
            Some(format!("{{\"command\":[\"seek\",{secs},\"relative\"]}}\n").into_bytes())
        }
        Control::Stop => None,
    }
}

/// Cuts what is read into lines. A line longer than [`MAX_LINE`] is dropped as soon as
/// it passes the limit, and the rest of it as it comes: a peer that never ends a line
/// costs no memory.
#[derive(Debug, Default)]
pub struct Lines {
    held: Vec<u8>,
    /// Inside a line that was too long, until its end.
    skipping: bool,
}

impl Lines {
    /// Takes what was read and calls `each` for every whole line in it.
    pub fn push(&mut self, mut bytes: &[u8], mut each: impl FnMut(&[u8])) {
        while let Some(at) = bytes.iter().position(|byte| *byte == b'\n') {
            let part = &bytes[..at];
            if !self.skipping && self.held.len() + part.len() <= MAX_LINE {
                if self.held.is_empty() {
                    each(part);
                } else {
                    self.held.extend_from_slice(part);
                    each(&self.held);
                }
            }
            self.held.clear();
            self.skipping = false;
            bytes = &bytes[at + 1..];
        }
        if self.skipping {
            return;
        }
        if self.held.len() + bytes.len() > MAX_LINE {
            self.held = Vec::new();
            self.skipping = true;
        } else {
            self.held.extend_from_slice(bytes);
        }
    }

    /// Bytes kept of a line not ended yet.
    #[cfg(test)]
    pub fn held(&self) -> usize {
        self.held.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(line: &str) -> Option<Said> {
        parse(line.as_bytes())
    }

    #[test]
    fn what_mpv_says_is_read() {
        let secs = Duration::from_secs_f64;
        assert_eq!(
            said(r#"{"event":"property-change","id":1,"name":"time-pos","data":5.02}"#),
            Some(Said::Position(secs(5.02)))
        );
        assert_eq!(
            said(r#"{"event":"property-change","id":3,"name":"duration","data":12}"#),
            Some(Said::Length(secs(12.0)))
        );
        assert_eq!(
            said(r#"{"event":"property-change","id":2,"name":"pause","data":true}"#),
            Some(Said::Paused(true))
        );
        assert_eq!(
            said(r#"{"event":"property-change","id":4,"name":"eof-reached","data":true}"#),
            Some(Said::Ended)
        );
        // A hair below zero at the start of a file is the start.
        assert_eq!(
            said(r#"{"event":"property-change","id":1,"name":"time-pos","data":-0.02}"#),
            Some(Said::Position(Duration::ZERO))
        );
    }

    #[test]
    fn anything_else_is_ignored() {
        for line in [
            // Answers, other events, properties not asked about.
            r#"{"data":null,"request_id":0,"error":"success"}"#,
            r#"{"event":"playback-restart"}"#,
            r#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#,
            r#"{"event":"property-change","id":9,"name":"volume","data":100}"#,
            // Not known yet, or not true.
            r#"{"event":"property-change","id":1,"name":"time-pos"}"#,
            r#"{"event":"property-change","id":1,"name":"time-pos","data":null}"#,
            r#"{"event":"property-change","id":4,"name":"eof-reached","data":false}"#,
            // Values no sound has.
            r#"{"event":"property-change","id":1,"name":"time-pos","data":-50}"#,
            r#"{"event":"property-change","id":3,"name":"duration","data":1e300}"#,
            r#"{"event":"property-change","id":2,"name":"pause","data":"yes"}"#,
            r#"{"event":"property-change","id":1,"name":"time-pos","data":"5"}"#,
            // Broken.
            "",
            "not json",
            r#"{"event":"property-change","name":"time-pos","data":5"#,
            "[1,2,3]",
            "\u{0}\u{1b}[31m",
            r#"{"event":42,"name":"pause","data":true}"#,
        ] {
            assert_eq!(said(line), None, "{line}");
        }
        // Deeply nested: refused by the JSON reader's own limit, not a crash.
        let deep = format!("{}{}", "[".repeat(2000), "]".repeat(2000));
        assert_eq!(parse(&deep.as_bytes()[..MAX_LINE.min(deep.len())]), None);
        // Too long, even if it would make sense.
        let padded = format!(
            r#"{{"event":"property-change","name":"pause","data":true,"x":"{}"}}"#,
            "a".repeat(MAX_LINE)
        );
        assert_eq!(said(&padded), None);
    }

    #[test]
    fn commands() {
        assert_eq!(
            command(Control::TogglePause).unwrap(),
            b"{\"command\":[\"cycle\",\"pause\"]}\n"
        );
        assert_eq!(
            command(Control::Seek(-5)).unwrap(),
            b"{\"command\":[\"seek\",-5,\"relative\"]}\n"
        );
        assert_eq!(command(Control::Stop), None);
        let observed: Vec<Value> = OBSERVE
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        let names: Vec<&str> = observed
            .iter()
            .map(|value| value["command"][2].as_str().unwrap())
            .collect();
        assert_eq!(names, ["time-pos", "pause", "duration", "eof-reached"]);
    }

    #[test]
    fn lines_are_cut_and_bounded() {
        let mut lines = Lines::default();
        let mut seen: Vec<String> = Vec::new();
        let take = |lines: &mut Lines, bytes: &[u8], seen: &mut Vec<String>| {
            lines.push(bytes, |line| {
                seen.push(String::from_utf8_lossy(line).into_owned())
            });
        };
        take(&mut lines, b"one\ntw", &mut seen);
        take(&mut lines, b"o\nthr", &mut seen);
        take(&mut lines, b"ee\n\n", &mut seen);
        assert_eq!(seen, ["one", "two", "three", ""]);
        assert_eq!(lines.held(), 0);

        // A peer that never ends its line: what is kept stays under the limit however
        // much comes, and the line is dropped whole once it ends.
        seen.clear();
        let block = vec![b'a'; 1000];
        for _ in 0..10_000 {
            take(&mut lines, &block, &mut seen);
            assert!(lines.held() <= MAX_LINE);
        }
        assert!(seen.is_empty());
        take(&mut lines, b"aaa\nafter\n", &mut seen);
        assert_eq!(seen, ["after"]);

        // Exactly at the limit is a line; one byte more is not.
        seen.clear();
        let mut exact = vec![b'b'; MAX_LINE];
        exact.push(b'\n');
        take(&mut lines, &exact[..10], &mut seen);
        take(&mut lines, &exact[10..], &mut seen);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].len(), MAX_LINE);
        let mut over = vec![b'c'; MAX_LINE + 1];
        over.push(b'\n');
        take(&mut lines, &over, &mut seen);
        take(&mut lines, b"next\n", &mut seen);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], "next");
    }
}
