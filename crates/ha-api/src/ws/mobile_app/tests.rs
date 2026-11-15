//! Push notifications against HA 2026.6.1: `mobile_app/vectors/notify.json`,
//! made by `scripts/vectors/mobile_app_notify.py`. Each scenario's steps
//! (service calls, the app's WebSocket commands, a closed connection, 10 s
//! passing) run on simulated time, with the relay answered from memory and
//! the WebSocket driven through `Conn` itself. What HA sent (WebSocket
//! messages per connection, relay requests byte for byte and where they
//! fell among the messages), the call's result, the notify entities' states
//! and the integration's warnings and errors must match.

use super::super::{Conn, Frame, Outbox};
use crate::Shared;
use ha_auth::{Permission, Principal};
use ha_core::{EntityId, Hass, ServiceError};
use ha_integrations::mobile_app::MobileApp;
use ha_integrations::mobile_app::notify::{Relay, RelayAnswer, RelayError, RelayFuture};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::sync::mpsc;

const USER: &str = "00112233445566778899aabbccddeeff";
const OTHER: &str = "ffeeddccbbaa99887766554433221100";
const RELAY: &str = "http://relay.test";
const CLOSED: &str = "http://closed.test";

fn vectors() -> Value {
    serde_json::from_str(include_str!("../../mobile_app/vectors/notify.json")).unwrap()
}

/// The vector's placeholders, filled in.
fn resolve(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(
            s.replace("<relay>", RELAY)
                .replace("<closed>", CLOSED)
                .replace("<user>", USER)
                .replace("<other>", OTHER),
        ),
        Value::Array(a) => Value::Array(a.iter().map(resolve).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), resolve(v))).collect()),
        other => other.clone(),
    }
}

/// A request the relay got, and how many messages each connection had
/// queued by then.
#[derive(Debug)]
struct Sent {
    url: String,
    body: String,
    before: Vec<usize>,
}

/// The relay: each scenario's canned answers, in order.
#[derive(Debug, Default)]
struct FakeRelay {
    replies: Mutex<VecDeque<Value>>,
    sent: Mutex<Vec<Sent>>,
    outs: Mutex<Vec<mpsc::Sender<Frame>>>,
}

impl Relay for FakeRelay {
    fn post(&self, url: &str, body: String) -> RelayFuture {
        // A refused connection never reaches the relay.
        if url.starts_with(CLOSED) {
            return Box::pin(async { Err(RelayError::Client("connection refused".into())) });
        }
        let before = self
            .outs
            .lock()
            .iter()
            .map(|tx| tx.max_capacity() - tx.capacity())
            .collect();
        self.sent.lock().push(Sent {
            url: url.to_owned(),
            body,
            before,
        });
        let reply = self.replies.lock().pop_front().expect("a canned reply");
        if reply.get("hang").is_some() {
            return Box::pin(std::future::pending());
        }
        let answer = RelayAnswer {
            status: reply["status"].as_u64().unwrap() as u16,
            content_type: reply["content_type"].as_str().unwrap().to_owned(),
            body: reply["body"].as_str().unwrap().as_bytes().to_vec(),
        };
        Box::pin(async move { Ok(answer) })
    }
}

/// The integration's warnings and errors.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<(String, String)>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Logs {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                if f.name() == "message" {
                    self.0 = format!("{v:?}");
                }
            }
        }
        let meta = event.metadata();
        let level = match *meta.level() {
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARNING",
            tracing::Level::DEBUG => "DEBUG",
            _ => return,
        };
        if !meta.target().starts_with("ha_integrations::mobile_app") {
            return;
        }
        let mut m = Message(String::new());
        event.record(&mut m);
        self.0.lock().push((level.into(), m.0));
    }
}

struct Run {
    hass: Hass,
    shared: Shared,
    relay: Arc<FakeRelay>,
    conns: Vec<(Conn, mpsc::Receiver<Frame>)>,
    /// hars's confirm ids to HA's.
    ids: HashMap<String, String>,
}

impl Run {
    fn new(scenario: &Value, now: jiff::Timestamp) -> Self {
        // The vectors' `utcnow`, for the rate limit line.
        let hass = Hass::with_clock(ha_core::SimulatedClock::new(now).clock());
        hass.start();
        let regs = scenario["registrations"].as_array().unwrap();
        let entries: Vec<Map<String, Value>> = regs
            .iter()
            .enumerate()
            .map(|(i, data)| {
                let data = resolve(data);
                json!({"domain": "mobile_app", "entry_id": format!("entry{i}"),
                       "title": data["device_name"], "disabled_by": null, "data": data})
                .as_object()
                .unwrap()
                .clone()
            })
            .collect();
        let registry = ha_registry::Registry::in_memory(&hass, Default::default());
        // The rows HA's notify platform makes.
        for e in &entries {
            registry.get_or_create_entity(
                "notify",
                "mobile_app",
                e["data"]["device_id"].as_str().unwrap(),
                None,
                ha_registry::EntityPatch {
                    object_id_base: Some(Some(e["title"].as_str().unwrap().into())),
                    ..Default::default()
                },
            );
        }
        let app = Arc::new(MobileApp::new(&entries, vec![]));
        let relay = Arc::new(FakeRelay::default());
        relay
            .replies
            .lock()
            .extend(scenario["relay"].as_array().unwrap().iter().cloned());
        app.set_relay(relay.clone());
        let config_entries =
            ha_integrations::config_entries::ConfigEntries::in_memory(&hass, entries);
        app.setup(&hass, registry.clone(), config_entries, None);
        let shared = Shared {
            hass: hass.clone(),
            auth: crate::test_util::auth(true),
            config: Arc::new(crate::ApiConfig {
                registry: Some(registry),
                mobile_app: Some(app),
                ..Default::default()
            }),
            templates: Default::default(),
            core_config: Default::default(),
        };
        Self {
            hass,
            shared,
            relay,
            conns: Vec::new(),
            ids: HashMap::new(),
        }
    }

    fn conn(&mut self, n: usize) -> &mut Conn {
        while self.conns.len() <= n {
            let (tx, rx) = mpsc::channel(super::super::MAX_PENDING);
            self.relay.outs.lock().push(tx.clone());
            let conn = Conn {
                shared: self.shared.clone(),
                principal: Principal {
                    user_id: USER.into(),
                    refresh_token_id: "token".into(),
                    permission: Permission::Admin,
                },
                user: serde_json::from_value(json!({"id": USER, "name": "Ann"})).unwrap(),
                out: Outbox {
                    tx,
                    overflow: Default::default(),
                },
                coalesce: Arc::new(AtomicBool::new(false)),
                last_id: 0,
                subs: HashMap::new(),
                events: Default::default(),
                event_listener: None,
                busy: None,
                queue: Default::default(),
                queued_bytes: 0,
            };
            self.conns.push((conn, rx));
        }
        &mut self.conns[n].0
    }

    /// HA's confirm id in `text` replaced by the one hars sent.
    fn to_hars(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (ours, theirs) in &self.ids {
            text = text.replace(theirs, ours);
        }
        text
    }

    fn to_ha(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (ours, theirs) in &self.ids {
            text = text.replace(ours, theirs);
        }
        text
    }

    /// Let everything that can run, run (without moving the clock).
    async fn settle() {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
    }

    async fn step(&mut self, step: &Value) -> Option<Result<(), ServiceError>> {
        let mut result = None;
        if let Some(c) = step.get("call") {
            let data = resolve(&c[2]).as_object().unwrap().clone();
            let r = self
                .hass
                .call_service(
                    c[0].as_str().unwrap(),
                    c[1].as_str().unwrap(),
                    data,
                    None,
                    false,
                    None,
                )
                .await;
            result = Some(r.map(|_| ()));
        } else if let Some(w) = step.get("ws") {
            let msg = self.to_hars(&resolve(&w[1]).to_string());
            let conn = self.conn(w[0].as_u64().unwrap() as usize);
            assert!(conn.enqueue(&msg));
            conn.drain();
        } else if let Some(n) = step.get("close") {
            self.conn(n.as_u64().unwrap() as usize).close();
        } else if step.get("advance").is_some() {
            tokio::time::advance(Duration::from_secs(10)).await;
        }
        Self::settle().await;
        result
    }

    /// The messages each connection was sent since the last look.
    fn frames(&mut self) -> Vec<Vec<Value>> {
        self.conns
            .iter_mut()
            .map(|(_, rx)| {
                std::iter::from_fn(|| rx.try_recv().ok())
                    .map(|f| match f {
                        Frame::Json(s) => serde_json::from_str(&s).unwrap(),
                        Frame::Ping => panic!("a ping"),
                    })
                    .collect()
            })
            .collect()
    }
}

fn check_result(name: &str, got: &Result<(), ServiceError>, want: &Value) {
    if want == "ok" {
        assert!(got.is_ok(), "{name}: {got:?}");
        return;
    }
    let want = &want["error"];
    let message = want["message"].as_str().unwrap();
    let err = got.as_ref().expect_err(name);
    match (want["type"].as_str().unwrap(), err) {
        ("HomeAssistantError", ServiceError::Translated(r)) => {
            assert_eq!(r.message, message, "{name}");
            assert_eq!(r.key, want["translation_key"].as_str().unwrap(), "{name}");
            assert_eq!(
                r.domain,
                want["translation_domain"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                json!(r.placeholders),
                want["translation_placeholders"],
                "{name}"
            );
        }
        ("HomeAssistantError", ServiceError::Failed(m)) => {
            assert!(want.get("translation_key").is_none(), "{name}");
            assert_eq!(m, message, "{name}");
        }
        ("MultipleInvalid", ServiceError::Invalid(m)) => assert_eq!(m, message, "{name}"),
        // Python's JSON parser and serde's word their errors differently.
        ("JSONDecodeError", ServiceError::Unknown(_)) => {}
        (_, ServiceError::Unknown(m)) => assert_eq!(m, message, "{name}"),
        (ty, other) => panic!("{name}: want {ty} {message:?}, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn push_notifications_as_ha() {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::layer::SubscriberExt::with(
        tracing_subscriber::registry(),
        logs.clone(),
    ));
    let v = vectors();
    let now: jiff::Timestamp = v["rate_limits"]["now"].as_str().unwrap().parse().unwrap();
    let mut steps = 0;
    for scenario in v["scenarios"].as_array().unwrap() {
        let mut run = Run::new(scenario, now);
        for (i, recorded) in scenario["steps"].as_array().unwrap().iter().enumerate() {
            let name = format!("{} #{i} {}", scenario["name"], recorded["step"]);
            logs.0.lock().clear();
            run.relay.sent.lock().clear();
            let step = &recorded["step"];
            let result = run.step(step).await;
            if let Some(want) = recorded.get("result") {
                check_result(&name, result.as_ref().expect("a call"), want);
            }
            if let Some(want) = recorded.get("services") {
                let got: Vec<String> = run
                    .hass
                    .services()
                    .services()
                    .into_iter()
                    .filter(|(d, _)| d == "notify")
                    .flat_map(|(_, s)| s.into_iter().map(|(n, _)| n.to_string()))
                    .collect();
                // hars has no `notify.persistent_notification`.
                let want: Vec<&str> = want
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|s| *s != "persistent_notification")
                    .collect();
                assert_eq!(got, want, "{name}");
            }

            // What went out: each connection's messages, and the relay's
            // requests with where they fell among them.
            let frames = run.frames();
            let mut want_ws: Vec<Vec<Value>> = vec![Vec::new(); frames.len()];
            let mut want_relay = Vec::new();
            for sent in recorded["sent"].as_array().unwrap() {
                if let Some(w) = sent.get("ws") {
                    let c = w[0].as_u64().unwrap() as usize;
                    want_ws[c].push(resolve(&w[1]));
                } else {
                    let before: Vec<usize> = want_ws.iter().map(Vec::len).collect();
                    want_relay.push((resolve(&sent["relay"]), before));
                }
            }
            for (c, (got, want)) in frames.iter().zip(&want_ws).enumerate() {
                assert_eq!(got.len(), want.len(), "{name}: conn {c}: {got:?}");
                for (got, want) in got.iter().zip(want) {
                    // Learn hars's confirm id for HA's.
                    let ours = got
                        .pointer("/event/hass_confirm_id")
                        .and_then(Value::as_str);
                    let theirs = want
                        .pointer("/event/hass_confirm_id")
                        .and_then(Value::as_str);
                    if let (Some(o), Some(t)) = (ours, theirs) {
                        assert!(run.ids.insert(o.into(), t.into()).is_none(), "{name}");
                    }
                    let got: Value = serde_json::from_str(&run.to_ha(&got.to_string())).unwrap();
                    assert_eq!(&got, want, "{name}: conn {c}");
                }
            }
            let sent = std::mem::take(&mut *run.relay.sent.lock());
            assert_eq!(sent.len(), want_relay.len(), "{name}: {sent:?}");
            for (got, (want, before)) in sent.iter().zip(&want_relay) {
                assert_eq!(
                    format!("{}{}", RELAY, want["path"].as_str().unwrap()),
                    got.url,
                    "{name}"
                );
                assert_eq!(
                    run.to_ha(&got.body),
                    want["body"].as_str().unwrap(),
                    "{name}"
                );
                let mut got_before = got.before.clone();
                got_before.resize(before.len(), 0);
                assert_eq!(&got_before, before, "{name}: where the request fell");
            }

            for (entity, want) in recorded["states"].as_object().unwrap() {
                let state = run
                    .hass
                    .states()
                    .get(&EntityId::parse(entity).unwrap())
                    .unwrap_or_else(|| panic!("{name}: no {entity}"));
                if want == "<time>" {
                    assert!(
                        state.state.ends_with("+00:00"),
                        "{name}: {entity} {}",
                        state.state
                    );
                } else {
                    assert_eq!(state.state, want.as_str().unwrap(), "{name}: {entity}");
                }
            }

            // HA's log lines; an error a confirm timer raised is logged by
            // hars as a failed fallback.
            let mut want_logs: Vec<(String, String)> = recorded["log"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| {
                    let line = resolve(&l[1]).as_str().unwrap().to_owned();
                    (l[0].as_str().unwrap().into(), line)
                })
                .collect();
            for e in recorded
                .get("timer_errors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                want_logs.push((
                    "ERROR".into(),
                    format!(
                        "mobile_app push notification fallback failed: {}",
                        e["message"].as_str().unwrap()
                    ),
                ));
            }
            assert_eq!(*logs.0.lock(), want_logs, "{name}");
            steps += 1;
        }
    }
    assert_eq!(steps, 126);
}

/// `log_rate_limits`'s line, or what HA raised, for every answer.
#[test]
fn rate_limit_lines_as_ha() {
    let v = vectors();
    let now: jiff::Timestamp = v["rate_limits"]["now"].as_str().unwrap().parse().unwrap();
    let cases = v["rate_limits"]["cases"].as_array().unwrap();
    for case in cases {
        let got =
            ha_integrations::mobile_app::notify::rate_limit_line("Phone", &case["rateLimits"], now);
        match (&got, case.get("line")) {
            (Ok(line), Some(want)) => assert_eq!(line, want.as_str().unwrap()),
            (Err(ServiceError::Unknown(m)), None) => {
                assert_eq!(m, case["error"]["message"].as_str().unwrap(), "{case}");
            }
            _ => panic!("{case}: {got:?}"),
        }
    }
    assert_eq!(cases.len(), 20);
}
