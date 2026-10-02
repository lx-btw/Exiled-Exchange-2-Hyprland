//! `CLIENT->MAIN::write-data` sink. Ports `main/src/host-files/FileWriter.ts`:
//! library item log (`<name>.csv`, appended if it already exists) and the
//! client-log event stream (`client-log.ndjson`, always truncated on open).
//! Default directory is upstream's `<userData>/apt-data/csv-data`; the
//! renderer's `libraryOutputPath` overrides it.
//!
//! Writes go through `spawn_blocking` on an `Arc`-shared channel state with
//! its own `std` mutex, never under `Overlay`'s lock: `Overlay` only clones
//! the `Arc<FileWriter>` and hands the job off.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::config::config_dir;
use crate::logger::ee_log;

#[derive(Default)]
struct Inner {
    enabled: bool,
    uploads_path: Option<PathBuf>,
    item_file: Option<File>,
    client_log_file: Option<File>,
    current_date_time: i64,
}

#[derive(Default)]
pub struct FileWriter {
    inner: Mutex<Inner>,
}

fn default_path() -> PathBuf {
    config_dir().join("csv-data")
}

impl FileWriter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// `HostConfig.libraryAlpha` / `libraryOutputPath`.
    pub fn restart(&self, enabled: bool, output_path: Option<&str>) {
        let mut inner = self.inner.lock().unwrap();
        inner.enabled = enabled;
        inner.uploads_path = output_path.filter(|p| !p.is_empty()).map(PathBuf::from);
    }

    /// Upstream's `flushClientLogFile` (before a log re-parse).
    pub fn flush_client_log_file(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.client_log_file.take().is_some() {
            ee_log!("info [FileWriter] Channel client-log closed.");
        }
    }

    /// Handles one `write-data` payload on a blocking thread.
    pub fn handle(self: &Arc<Self>, payload: Value) {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.handle_blocking(&payload));
    }

    fn handle_blocking(&self, payload: &Value) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.enabled {
            return;
        }
        let dir = inner.uploads_path.clone().unwrap_or_else(default_path);
        match payload.get("action").and_then(Value::as_str) {
            Some("client-log-event") => {
                if inner.client_log_file.is_none() {
                    // upstream doesn't mkdir here (only on session start); a
                    // missing directory would just fail the open, so create it
                    let _ = std::fs::create_dir_all(&dir);
                    match File::create(dir.join("client-log.ndjson")) {
                        Ok(f) => inner.client_log_file = Some(f),
                        Err(err) => {
                            ee_log!("error [FileWriter] Failed to open client-log: {err}");
                            return;
                        }
                    }
                }
                let Some(data) = payload.get("data") else {
                    return;
                };
                let ts = data.get("ts").and_then(Value::as_i64).unwrap_or(0);
                if ts < inner.current_date_time {
                    ee_log!(
                        "warn [FileWriter] time just went backwards: {ts} < {}",
                        inner.current_date_time
                    );
                }
                inner.current_date_time = ts;
                write_line(&mut inner.client_log_file, &data.to_string(), "client-log");
                if payload.get("close").and_then(Value::as_bool) == Some(true) {
                    inner.client_log_file = None;
                    ee_log!("info [FileWriter] Channel client-log closed.");
                }
            }
            Some("log-item") => {
                let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
                write_line(&mut inner.item_file, text, "item");
            }
            Some("session") => {
                if payload.get("start").and_then(Value::as_bool) == Some(true) {
                    let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
                    let header = payload.get("header").and_then(Value::as_str).unwrap_or("");
                    if name.is_empty() || header.is_empty() {
                        ee_log!("error [FileWriter] Invalid session start event.");
                        return;
                    }
                    if let Err(err) = start_session(&mut inner, &dir, name, header) {
                        ee_log!("error [FileWriter] Failed to create session file: {err}");
                    }
                } else if inner.item_file.take().is_some() {
                    ee_log!("info [FileWriter] Channel item closed.");
                } else {
                    ee_log!("error [FileWriter] Channel item already closed.");
                }
            }
            _ => {}
        }
    }
}

fn start_session(
    inner: &mut Inner,
    dir: &std::path::Path,
    name: &str,
    header: &str,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // `name` is renderer-chosen; keep it a plain file name inside `dir`
    let file_name = std::path::Path::new(name)
        .file_name()
        .ok_or_else(|| std::io::Error::other("invalid session name"))?;
    let mut path = dir.join(file_name);
    path.as_mut_os_string().push(".csv");
    let existed = path.exists();
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(existed)
        .truncate(!existed)
        .open(&path)?;
    inner.item_file = Some(file);
    // header only for a freshly-created file (upstream: `mode === "w"`)
    if !existed {
        write_line(&mut inner.item_file, header, "item");
    }
    Ok(())
}

fn write_line(file: &mut Option<File>, line: &str, channel: &str) {
    let Some(f) = file else {
        ee_log!("error [FileWriter] Unable to write to channel {channel}, channel closed.");
        return;
    };
    if let Err(err) = writeln!(f, "{line}") {
        ee_log!("error [FileWriter] write to channel {channel} failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(fw: &FileWriter, payload: Value) {
        fw.handle_blocking(&payload);
    }

    #[test]
    fn session_item_log_and_client_log() {
        let dir = tempfile::tempdir().unwrap();
        let fw = FileWriter::default();

        // disabled: nothing written
        fw.restart(false, Some(dir.path().to_str().unwrap()));
        run(
            &fw,
            json!({"action": "session", "start": true, "name": "s", "header": "h"}),
        );
        assert!(!dir.path().join("s.csv").exists());

        fw.restart(true, Some(dir.path().to_str().unwrap()));
        run(
            &fw,
            json!({"action": "session", "start": true, "name": "s", "header": "a,b"}),
        );
        run(&fw, json!({"action": "log-item", "text": "1,2"}));
        run(&fw, json!({"action": "session", "start": false}));
        // resuming appends and does not repeat the header
        run(
            &fw,
            json!({"action": "session", "start": true, "name": "s", "header": "a,b"}),
        );
        run(&fw, json!({"action": "log-item", "text": "3,4"}));
        run(&fw, json!({"action": "session", "start": false}));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("s.csv")).unwrap(),
            "a,b\n1,2\n3,4\n"
        );

        run(
            &fw,
            json!({"action": "client-log-event", "data": {"ts": 5, "type": 1}, "close": false}),
        );
        run(
            &fw,
            json!({"action": "client-log-event", "data": {"ts": 6, "type": 2}, "close": true}),
        );
        let log = std::fs::read_to_string(dir.path().join("client-log.ndjson")).unwrap();
        assert_eq!(log.lines().count(), 2);
        // reopening truncates
        run(
            &fw,
            json!({"action": "client-log-event", "data": {"ts": 7, "type": 3}, "close": true}),
        );
        let log = std::fs::read_to_string(dir.path().join("client-log.ndjson")).unwrap();
        assert_eq!(log.lines().count(), 1);

        // path traversal in the session name stays inside the directory
        run(
            &fw,
            json!({"action": "session", "start": true, "name": "../evil", "header": "h"}),
        );
        assert!(dir.path().join("evil.csv").exists());
    }
}
