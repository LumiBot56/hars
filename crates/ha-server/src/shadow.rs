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

/// The events the log keeps.
fn logged(event: &Event) -> bool {
    match &event.kind {
        EventKind::CallService { domain, .. } => !shadow::runs(domain),
        EventKind::Custom { name, .. } => name == "automation_triggered",
        _ => false,
    }
}

/// One line of the log (no newline), or `None` for an event it doesn't keep.
fn line(event: &Event) -> Option<String> {
    let t = event.time_fired.as_microsecond() as f64 / 1e6;
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

/// Append `text` to the log.
fn append(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
}

/// Start shadow mode and its log. Call before anything can fire a service.
/// Each start is marked in the log, so the diff knows where it begins and
/// where hars was down.
pub async fn start(hass: &Hass, path: PathBuf) {
    hass.set_shadow();
    let mut feed = hass.bus().subscribe_lossless(logged);
    let t = hass.clock().now().as_microsecond() as f64 / 1e6;
    let marker = format!("{}\n", json!({"t": t, "type": "shadow_started"}));
    let marked = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || append(&path, &marker)).await
    };
    if let Ok(Err(e)) | Err(e) = marked.map_err(std::io::Error::other) {
        tracing::error!("the shadow log could not be written: {e}");
    }
    tracing::warn!(
        "SHADOW MODE: service calls that act on devices are not run; they are logged to {}",
        path.display()
    );
    tokio::spawn(async move {
        while let Some(first) = feed.recv().await {
            // What queued while the last write ran goes out in one.
            let mut text = String::new();
            let mut next = Some(first);
            while let Some(event) = next {
                if let Some(l) = line(&event) {
                    text.push_str(&l);
                    text.push('\n');
                }
                next = feed.try_recv();
            }
            let path = path.clone();
            let written = tokio::task::spawn_blocking(move || append(&path, &text)).await;
            if let Ok(Err(e)) | Err(e) = written.map_err(std::io::Error::other) {
                tracing::error!("the shadow log could not be written: {e}");
            }
        }
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
}
