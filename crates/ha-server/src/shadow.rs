//! Shadow mode's log: what `hars` would have done, for `scripts/shadow-diff.py`
//! to compare with what Home Assistant did.
//!
//! `HARS_SHADOW=1` turns shadow mode on (service handlers that act on the
//! outside world don't run: [`ha_core::shadow`]). Every such call and every
//! `automation_triggered` goes to a JSON Lines file, one object per line, in
//! the order the bus fired them:
//!
//! ```text
//! {"t":1768465800.123456,"type":"call_service","domain":"light","service":"turn_on",
//!  "service_data":{...},"context":{"id":"…","parent_id":"…","user_id":null}}
//! {"t":1768465800.001,"type":"automation_triggered","data":{...},"context":{...}}
//! ```
//!
//! `t` is the event's `time_fired` in epoch seconds. The file is
//! `HARS_SHADOW_LOG`, by default `<config>/.hars/shadow.jsonl`; it is only
//! ever appended to.

use ha_core::{Event, EventKind, Hass, shadow};
use serde_json::{Value, json};
use std::io::Write as _;
use std::path::PathBuf;

/// The events the log keeps, and `homeassistant_final_write`, where it stops.
fn logged(event: &Event) -> bool {
    match &event.kind {
        EventKind::CallService { domain, .. } => !shadow::runs(domain),
        EventKind::Custom { name, .. } => name == "automation_triggered",
        EventKind::HomeassistantFinalWrite {} => true,
        _ => false,
    }
}

/// A time as the log writes it: epoch seconds.
fn seconds(microseconds: i64) -> f64 {
    microseconds as f64 / 1e6
}

/// One line of the log (no newline), or `None` for an event it doesn't keep.
fn line(event: &Event) -> Option<String> {
    let t = seconds(event.time_fired.as_microsecond());
    let mut v = match &event.kind {
        EventKind::CallService {
            domain,
            service,
            service_data,
        } => json!({
            "t": t, "type": "call_service", "domain": domain, "service": service,
            "service_data": service_data,
        }),
        EventKind::Custom { name, data } if name == "automation_triggered" => {
            json!({"t": t, "type": "automation_triggered", "data": data})
        }
        _ => return None,
    };
    v["context"] = serde_json::to_value(&event.context).unwrap_or(Value::Null);
    Some(v.to_string())
}

/// Append `text` to the log. A `marker` first makes sure the file ends in a
/// newline: a crash can leave a torn last line, which must not swallow the
/// marker that follows it.
fn append(path: &std::path::Path, text: &str, marker: bool) -> std::io::Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    let before = f.metadata()?.len();
    let written = (|| {
        if marker && before > 0 {
            let mut last = [0u8];
            f.seek(SeekFrom::End(-1))?;
            f.read_exact(&mut last)?;
            if last[0] != b'\n' {
                f.write_all(b"\n")?;
            }
        }
        f.write_all(text.as_bytes())
    })();
    if written.is_err() {
        // Not a prefix of it: the retry writes the whole batch again.
        let _ = f.set_len(before);
    }
    written
}

/// How the writer behaves when the disk doesn't cooperate.
#[derive(Clone, Copy)]
struct Limits {
    /// Bytes waiting to be written, and again for a batch whose write
    /// failed: past this, decisions are dropped (and counted) rather than
    /// held without bound.
    max_pending: usize,
    /// The wait before a failed write is tried again.
    retry: std::time::Duration,
}

const LIMITS: Limits = Limits {
    max_pending: 4 << 20,
    retry: std::time::Duration::from_secs(1),
};

/// What is waiting to be written.
#[derive(Default)]
struct Pending {
    /// A batch whose write failed, kept to try again: it goes first.
    held: String,
    text: String,
    /// Decisions dropped for want of room, and the first one's time.
    dropped: usize,
    dropped_from: Option<f64>,
}

impl Pending {
    fn push(&mut self, line: String, t: f64, limits: Limits) {
        if self.text.len() + line.len() > limits.max_pending {
            self.dropped += 1;
            self.dropped_from.get_or_insert(t);
        } else {
            self.text.push_str(&line);
            self.text.push('\n');
        }
    }

    /// Something to write: lines, or a gap to report.
    fn is_empty(&self) -> bool {
        self.held.is_empty() && self.text.is_empty() && self.dropped_from.is_none()
    }

    /// The next write: a batch that failed before, else what has queued
    /// (never both, so a failing disk doesn't grow one batch without bound:
    /// at most `max_pending` is held and `max_pending` queues). With nothing
    /// but a drop to report, the gap line.
    fn take(&mut self, now: f64) -> String {
        if !self.held.is_empty() {
            return std::mem::take(&mut self.held);
        }
        if self.text.is_empty() {
            self.gap(now);
        }
        std::mem::take(&mut self.text)
    }

    /// A write succeeded. If decisions were dropped meanwhile, a `shadow_gap`
    /// line says so, so the comparison knows it can't vouch for that time.
    fn wrote(&mut self, now: f64) {
        self.gap(now);
    }

    fn gap(&mut self, now: f64) {
        if let Some(from) = self.dropped_from.take() {
            let gap = json!({
                "t": from, "until": now, "type": "shadow_gap", "dropped": self.dropped
            });
            self.text.push_str(&format!("{gap}\n"));
            self.dropped = 0;
        }
    }
}

/// Start shadow mode and its log. Call before anything can fire a service.
/// Each start is marked in the log, so the diff knows where it begins and
/// where hars was down. At shutdown the final-write stage waits for what is
/// queued to be written.
pub async fn start(hass: &Hass, path: PathBuf) {
    start_with(hass, path, LIMITS).await;
}

async fn start_with(hass: &Hass, path: PathBuf, limits: Limits) {
    hass.set_shadow();
    let mut feed = hass.bus().subscribe_lossless(logged);
    let clock = hass.clock().clone();
    let marker = format!(
        "{}\n",
        json!({"t": seconds(clock.now().as_microsecond()), "type": "shadow_started"})
    );
    let mut pending = Pending::default();
    let marked = {
        let (path, marker) = (path.clone(), marker.clone());
        tokio::task::spawn_blocking(move || append(&path, &marker, true)).await
    };
    // A marker that couldn't be written goes out with the first write that can.
    let mut repair = false;
    if let Ok(Err(e)) | Err(e) = marked.map_err(std::io::Error::other) {
        tracing::error!("the shadow log could not be written: {e}");
        pending.held = marker;
        repair = true;
    }
    tracing::warn!(
        "SHADOW MODE: service calls that act on devices are not run; they are logged to {}",
        path.display()
    );
    let (drained, drained_rx) = tokio::sync::oneshot::channel::<()>();
    hass.on_final_write("shadow log", move || async move {
        let _ = drained_rx.await;
    });
    tokio::spawn(async move {
        type Write = tokio::task::JoinHandle<(String, std::io::Result<()>)>;
        let mut writing: Option<Write> = None;
        let mut retry_at: Option<tokio::time::Instant> = None;
        let mut closing = false;
        // At shutdown, one last try, not a retry loop.
        let mut last_try = false;
        loop {
            let ready = retry_at.is_none_or(|at| tokio::time::Instant::now() >= at);
            if writing.is_none() && !pending.is_empty() && (ready || (closing && !last_try)) {
                last_try = closing;
                let text = pending.take(seconds(clock.now().as_microsecond()));
                let path = path.clone();
                let marker = std::mem::take(&mut repair);
                writing = Some(tokio::task::spawn_blocking(move || {
                    let r = append(&path, &text, marker);
                    (text, r)
                }));
                retry_at = None;
            }
            if closing && writing.is_none() && (pending.is_empty() || last_try) {
                if !pending.is_empty() {
                    tracing::error!("the shadow log could not be written at shutdown");
                }
                break;
            }
            tokio::select! {
                event = feed.recv(), if !closing => match event {
                    Some(e) if matches!(e.kind, EventKind::HomeassistantFinalWrite {}) => closing = true,
                    Some(e) => {
                        if let Some(l) = line(&e) {
                            pending.push(l, seconds(e.time_fired.as_microsecond()), limits);
                        }
                    }
                    None => closing = true,
                },
                done = async { writing.as_mut().unwrap().await }, if writing.is_some() => {
                    writing = None;
                    match done {
                        Ok((_, Ok(()))) => pending.wrote(seconds(clock.now().as_microsecond())),
                        Ok((text, Err(e))) => {
                            tracing::error!("the shadow log could not be written, will retry: {e}");
                            pending.held = text;
                            // The retry checks the file's end again.
                            repair = true;
                            retry_at = Some(tokio::time::Instant::now() + limits.retry);
                        }
                        Err(e) => tracing::error!("the shadow log writer failed: {e}"),
                    }
                }
                () = async { tokio::time::sleep_until(retry_at.unwrap()).await }, if retry_at.is_some() && writing.is_none() => {}
            }
        }
        let _ = drained.send(());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ha_core::{ServiceData, SupportsResponse};

    /// Held-back calls and triggers are logged in order; calls that run
    /// aren't, and the held-back handler never runs.
    #[tokio::test]
    async fn logs_held_back_calls_and_triggers() {
        let path = std::env::temp_dir().join(format!("hars-shadow-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let hass = Hass::new();
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        hass.services()
            .register("light", "turn_on", SupportsResponse::None, move |_| {
                let flag = flag.clone();
                async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(None)
                }
            });
        hass.services()
            .register("automation", "trigger", SupportsResponse::None, |_| async {
                Ok(None)
            });
        start(&hass, path.clone()).await;

        let data = json!({"entity_id": "light.a", "brightness_pct": 40});
        let data: ServiceData = data.as_object().unwrap().clone();
        hass.call_service("light", "turn_on", data, None, false, None)
            .await
            .unwrap();
        hass.call_service(
            "automation",
            "trigger",
            ServiceData::new(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.bus().fire(Event::new(
            EventKind::Custom {
                name: "automation_triggered".into(),
                data: json!({"name": "Hall", "entity_id": "automation.hall", "source": "state"}),
            },
            hass.clock().now(),
            ha_core::Context::new(),
        ));
        let mut lines = Vec::new();
        for _ in 0..100 {
            lines = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .collect();
            if lines.len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_file(&path);
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0]["type"], "shadow_started");
        let lines = &lines[1..];
        assert_eq!(lines[0]["type"], "call_service");
        assert_eq!(lines[0]["domain"], "light");
        assert_eq!(lines[0]["service"], "turn_on");
        assert_eq!(
            lines[0]["service_data"],
            json!({"entity_id": "light.a", "brightness_pct": 40})
        );
        assert!(lines[0]["t"].as_f64().unwrap() > 1.7e9);
        assert!(lines[0]["context"]["id"].is_string());
        assert_eq!(lines[1]["type"], "automation_triggered");
        assert_eq!(lines[1]["data"]["entity_id"], "automation.hall");
    }

    fn read_lines(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn trigger(hass: &Hass, name: &str) {
        hass.bus().fire(Event::new(
            EventKind::Custom {
                name: "automation_triggered".into(),
                data: json!({"name": name, "entity_id": format!("automation.{name}")}),
            },
            hass.clock().now(),
            ha_core::Context::new(),
        ));
    }

    /// A write that fails is kept and retried; what doesn't fit while it
    /// fails is counted, and a `shadow_gap` line says so once the disk works.
    #[tokio::test]
    async fn a_failing_disk_loses_nothing_silently() {
        let path = std::env::temp_dir().join(format!("hars-shadow-gap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(&path);
        let hass = Hass::new();
        let limits = Limits {
            max_pending: 400,
            retry: std::time::Duration::from_millis(20),
        };
        // The marker can't be written: the log's path is a directory.
        std::fs::create_dir_all(&path).unwrap();
        start_with(&hass, path.clone(), limits).await;
        for i in 0..20 {
            trigger(&hass, &format!("a{i}"));
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // The disk comes back.
        std::fs::remove_dir_all(&path).unwrap();
        let mut lines = Vec::new();
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            lines = read_lines(&path);
            if lines.iter().any(|l| l["type"] == "shadow_gap") {
                break;
            }
        }
        let _ = std::fs::remove_file(&path);
        let kept = lines
            .iter()
            .filter(|l| l["type"] == "automation_triggered")
            .count();
        let gap = lines
            .iter()
            .find(|l| l["type"] == "shadow_gap")
            .expect("a gap line");
        let dropped = usize::try_from(gap["dropped"].as_u64().unwrap()).unwrap();
        assert!(kept > 0 && dropped > 0, "{kept} kept, {dropped} dropped");
        assert_eq!(kept + dropped, 20, "every decision is kept or counted");
        assert!(gap["until"].as_f64().unwrap() >= gap["t"].as_f64().unwrap());
    }

    /// A torn last line (a crash) doesn't swallow the next start's marker.
    #[tokio::test]
    async fn a_torn_line_does_not_swallow_the_next_marker() {
        let path = std::env::temp_dir().join(format!("hars-shadow-torn-{}", std::process::id()));
        std::fs::write(&path, "{\"t\": 1.0, \"type\": \"call_ser").unwrap();
        start(&Hass::new(), path.clone()).await;
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "shadow_started");
        assert_eq!(text.lines().count(), 2);
    }

    /// The final-write stage waits for the queue to be written.
    #[tokio::test]
    async fn shutdown_writes_what_is_queued() {
        let path = std::env::temp_dir().join(format!("hars-shadow-stop-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let hass = Hass::new();
        hass.start();
        start(&hass, path.clone()).await;
        for i in 0..200 {
            trigger(&hass, &format!("a{i}"));
        }
        hass.begin_stop();
        hass.finish_stop().await;
        let n = read_lines(&path)
            .iter()
            .filter(|l| l["type"] == "automation_triggered")
            .count();
        let _ = std::fs::remove_file(&path);
        assert_eq!(n, 200);
    }

    /// A decision too big to queue is counted and reported even when nothing
    /// else is waiting.
    #[tokio::test]
    async fn an_oversized_decision_still_leaves_a_gap() {
        let path = std::env::temp_dir().join(format!("hars-shadow-big-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let hass = Hass::new();
        let limits = Limits {
            max_pending: 100,
            retry: std::time::Duration::from_millis(20),
        };
        start_with(&hass, path.clone(), limits).await;
        trigger(&hass, &"x".repeat(500));
        let mut gap = None;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            gap = read_lines(&path)
                .into_iter()
                .find(|l| l["type"] == "shadow_gap");
            if gap.is_some() {
                break;
            }
        }
        let _ = std::fs::remove_file(&path);
        assert_eq!(gap.expect("a gap line")["dropped"], 1);
    }

    /// A failed batch is held as it was, never merged with what has queued,
    /// so a disk that stays full can't grow it.
    #[test]
    fn a_held_batch_is_not_merged_with_new_lines() {
        let limits = Limits {
            max_pending: 50,
            retry: std::time::Duration::ZERO,
        };
        let mut p = Pending::default();
        p.push("a".repeat(40), 1.0, limits);
        let first = p.take(2.0);
        p.held = first.clone(); // the write failed
        p.push("b".repeat(40), 3.0, limits);
        assert_eq!(p.take(4.0), first, "the held batch goes alone");
        p.held = first.clone(); // and fails again
        assert_eq!(p.held.len() + p.text.len(), 41 + 41);
        // Nothing queued but a drop: the gap is the write.
        let mut p = Pending::default();
        p.push("c".repeat(500), 5.0, limits);
        assert!(!p.is_empty());
        let gap: Value = serde_json::from_str(p.take(6.0).trim()).unwrap();
        assert_eq!(
            (gap["type"].as_str(), gap["t"].as_f64()),
            (Some("shadow_gap"), Some(5.0))
        );
        assert!(p.is_empty());
    }

    /// `append` adds to what is there, and with `marker` first ends a torn
    /// last line. (Undoing a write that fails halfway is not tested: it needs
    /// a disk that fills mid-write.)
    #[test]
    fn append_adds_and_a_marker_ends_a_torn_line() {
        let path = std::env::temp_dir().join(format!("hars-shadow-trunc-{}", std::process::id()));
        std::fs::write(&path, "keep\n").unwrap();
        append(&path, "one\n", false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep\none\n");
        std::fs::write(&path, "torn").unwrap();
        append(&path, "marker\n", true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(text, "torn\nmarker\n");
    }
}
