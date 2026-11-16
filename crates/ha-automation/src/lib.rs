//! Home Assistant's automations (`components/automation`,
//! `helpers/script.py`, `helpers/trigger.py`, `helpers/condition.py`), for
//! the config subset in [`config`].
//!
//! How it runs, as HA 2026.6.1 does:
//! - Triggers attach at `homeassistant_started` ([`Automations::started`]),
//!   or at once when turned on later. State changes reach them from the
//!   lossless state feed through listeners ([`listen`]); conditions read
//!   the live state machine, so a batch an integration writes without
//!   yielding is seen whole, as HA's `call_soon` dispatch sees it.
//! - A trigger whose conditions pass starts a run (one task) at once:
//!   `automation_triggered`, `last_triggered`, one state write; `single`
//!   drops a trigger while running (with HA's warning), `restart` stops the
//!   old run first.
//! - Time triggers sleep on the monotonic clock and re-check the wall clock
//!   on waking: a clock stepped back never fires early, one stepped forward
//!   fires late, as HA's timers do. Nothing polls.
//!
//! Templates, as HA's scripts use them (checked by `vectors/templates.json`,
//! made by `scripts/vectors/automation_templates.py`):
//! - **Variables.** A run's templates see `this` (the automation's state
//!   as `State.as_dict` when it triggered), `trigger` (what the trigger
//!   handed on: HA's `State` objects for `from_state`/`to_state`, a
//!   `timedelta` for `for`, a local `datetime` for a time trigger's `now`),
//!   the `variables` and `trigger_variables` rendered in order for the run
//!   (an error stops it: `Error rendering variables`), the variables
//!   `automation.trigger` passes, and `wait` after a `wait_for_trigger`.
//!   `trigger_variables` render limited (no states, no clock) when the
//!   triggers attach, for the triggers' own templates. An automation with
//!   no template and no variable makes none of these.
//! - **Service calls** render the action name, the target and the data
//!   with `render_complex` and `_parse_result` (`{{ 5 }}` is the number 5),
//!   and fail with HA's errors (`Error rendering data template: …`).
//! - **Conditions** (`template`, its shorthand, `numeric_state`'s
//!   `value_template`) and **triggers** (`template`, `numeric_state`'s
//!   `value_template`, in [`track`]) render as HA's do.
//! - Templates compile once, when the config loads ([`config`]); every
//!   render runs on a render thread (`ha_core::template::run`, shared with
//!   the WebSocket's `render_template`), never on the async runtime. What a
//!   render logs is written back on the runtime, in order. A trigger whose
//!   variables or conditions render does so before its run starts, in
//!   trigger order; the state feed waits for that ([`listen`]), so the
//!   next change is handled after it, as in HA.
//!
//! Deliberate differences: a render runs a moment after the change that
//! caused it, so the states it reads may be newer than that change (as a
//! condition's already may: see above); dict variables iterate their keys
//! sorted (HA: in insertion order).

mod cond;
pub mod config;
mod listen;
mod time;
mod tpl;
mod track;

use cond::Out;
use config::{Action, Condition, Config, Mode, Trigger, TriggerKind};
use ha_core::template::{DisplayPrecision, Home, Var, Vars};
use ha_core::{
    Context, EntityId, Event, EventKind, Hass, Kind, Schema, ServiceError, State, SupportsResponse,
};
use ha_registry::Registries;
use listen::{Guard, Listeners, Listening};
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use tokio::task::JoinHandle;

pub const DOMAIN: &str = "automation";

/// HA's `CONTEXT_RECENT_TIME_SECONDS`: an entity keeps a call's context
/// for its writes this long.
const CONTEXT_RECENT: jiff::SignedDuration = jiff::SignedDuration::from_secs(5);

/// Python's `float(str)`: surrounding whitespace, `_` between digits,
/// exponents, `inf`/`infinity`/`nan` in any case.
pub fn py_float(s: &str) -> Option<f64> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let (sign, body) = match lower.strip_prefix('-') {
        Some(b) => (-1.0, b),
        None => (1.0, lower.strip_prefix('+').unwrap_or(&lower)),
    };
    match body {
        "inf" | "infinity" => return Some(sign * f64::INFINITY),
        "nan" => return Some(f64::NAN),
        _ => {}
    }
    // Underscores only between digits.
    let b = t.as_bytes();
    for (i, c) in b.iter().enumerate() {
        if *c == b'_'
            && !(i > 0 && i + 1 < b.len() && b[i - 1].is_ascii_digit() && b[i + 1].is_ascii_digit())
        {
            return None;
        }
    }
    let clean: String = t.chars().filter(|c| *c != '_').collect();
    if clean.is_empty()
        || !clean
            .bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return None;
    }
    clean.parse().ok()
}

/// A `state_changed` event's (old, new) states.
type Change<'a> = (Option<&'a Arc<State>>, Option<&'a Arc<State>>);

fn change(ev: &Event) -> Option<Change<'_>> {
    match &ev.kind {
        EventKind::StateChanged {
            old_state,
            new_state,
            ..
        } => Some((old_state.as_ref(), new_state.as_ref())),
        _ => None,
    }
}

// ----- Trigger variables -----

/// What a trigger hands the run: what the engine reads (`id`, the
/// description) and, when templates may read it, HA's `trigger` variable.
#[derive(Debug, Clone)]
pub struct TriggerVars {
    pub id: String,
    pub idx: String,
    pub alias: Option<String>,
    pub platform: Option<&'static str>,
    pub description: Option<String>,
    /// HA's `trigger` dict.
    pub var: Option<Var>,
    /// The variables `automation.trigger` was called with.
    pub extra: Option<Map<String, Value>>,
}

/// Where a fired trigger goes: an automation's run, or a waiting step.
pub(crate) type Sink = Arc<dyn Fn(TriggerVars, Option<Context>) + Send + Sync>;

/// `trigger`: the trigger's data (`id`, `idx`, `alias`), its platform, and
/// what the platform adds.
pub(crate) fn trigger_var(t: &Trigger, platform: &str, fields: Vec<(&str, Var)>) -> Var {
    let mut entries = vec![
        ("id".to_owned(), Var::str(&t.id)),
        ("idx".to_owned(), Var::str(&t.idx.to_string())),
        (
            "alias".to_owned(),
            t.alias.as_deref().map_or_else(Var::none, Var::str),
        ),
        ("platform".to_owned(), Var::str(platform)),
    ];
    entries.extend(fields.into_iter().map(|(k, v)| (k.to_owned(), v)));
    Var::dict(entries)
}

/// A `numeric_state` trigger's firing.
pub(crate) fn numeric_vars(
    t: &Trigger,
    entity: &str,
    below: Option<f64>,
    above: Option<f64>,
    old: Option<&Arc<State>>,
    new: &Arc<State>,
    templated: bool,
) -> TriggerVars {
    let description = format!("numeric state of {entity}");
    let float = |f: Option<f64>| f.map_or_else(Var::none, Var::float);
    let var = templated.then(|| {
        trigger_var(
            t,
            "numeric_state",
            vec![
                ("entity_id", Var::str(entity)),
                ("below", float(below)),
                ("above", float(above)),
                ("from_state", Var::opt_state(old)),
                ("to_state", Var::state(new.clone())),
                ("for", Var::none()),
                ("description", Var::str(&description)),
            ],
        )
    });
    TriggerVars {
        id: t.id.clone(),
        idx: t.idx.to_string(),
        alias: t.alias.clone(),
        platform: Some("numeric_state"),
        description: Some(description),
        var,
        extra: None,
    }
}

fn logger(entity_id: &EntityId) -> String {
    format!(
        "homeassistant.components.automation.{}",
        entity_id.as_str().trim_start_matches("automation.")
    )
}

/// `this`: the automation's state as `State.as_dict` (its
/// `last_triggered` a `datetime`, as HA keeps it).
fn this_var(hass: &Hass, entity_id: &EntityId) -> Var {
    let Some(state) = hass.states().get(entity_id) else {
        return Var::none();
    };
    let Ok(Value::Object(dict)) = serde_json::to_value(&*state) else {
        return Var::none();
    };
    Var::dict(dict.into_iter().map(|(k, v)| {
        let value = match (k.as_str(), v) {
            ("attributes", Value::Object(attrs)) => Var::dict(attrs.into_iter().map(|(a, x)| {
                let x = match (a.as_str(), &x) {
                    ("last_triggered", Value::String(t)) => t
                        .parse::<jiff::Timestamp>()
                        .map_or_else(|_| Var::json(&x), |t| Var::datetime(t, "UTC")),
                    _ => Var::json(&x),
                };
                (a, x)
            })),
            (_, v) => Var::json(&v),
        };
        (k, value)
    }))
}

// ----- Automations -----

struct Run {
    id: u64,
    handle: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Live {
    enabled: bool,
    last_triggered: Option<jiff::Timestamp>,
    runs: Vec<Run>,
    next_run: u64,
    /// Listeners and timers of the attached triggers.
    attached: Vec<Attached>,
    /// Counts detachments: an attachment still rendering its
    /// `trigger_variables` attaches only if none happened since.
    generation: u64,
    /// An attachment is rendering its `trigger_variables`.
    attaching: bool,
    /// The context the entity writes with, and when it was set.
    context: Option<(Context, jiff::Timestamp)>,
}

enum Attached {
    Listening(#[allow(dead_code)] Listening),
    Task(JoinHandle<()>),
    /// A state trigger's pending `for` timer.
    Pending(PendingFor),
}

impl Drop for Attached {
    fn drop(&mut self) {
        match self {
            Attached::Task(h) => h.abort(),
            Attached::Pending(p) => {
                for (_, _, h) in p.lock().drain(..) {
                    h.abort();
                }
            }
            Attached::Listening(_) => {}
        }
    }
}

/// A state trigger's pending `for`s: each one's id, the value it waits on,
/// and its timer.
type PendingFor = Arc<Mutex<Vec<(u64, String, JoinHandle<()>)>>>;

/// How a run is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fire {
    /// An attached trigger: only while enabled, conditions checked.
    Trigger,
    /// `automation.trigger`: whether enabled or not.
    Service { skip_conditions: bool },
}

pub struct Automation {
    shared: Weak<Shared>,
    pub entity_id: EntityId,
    pub name: String,
    pub unique_id: Option<String>,
    /// The config as loaded, as JSON text in its YAML key order (`None`
    /// if HA couldn't encode it either).
    pub raw: Option<String>,
    /// The validated config, or why it isn't valid (`unavailable`).
    config: Result<Arc<Config>, String>,
    live: Mutex<Live>,
    /// Triggers waiting for their variables or conditions to render go
    /// through one at a time, in order.
    gate: tokio::sync::Mutex<()>,
}

struct Shared {
    hass: Hass,
    listeners: Arc<Listeners>,
    automations: Mutex<Vec<Arc<Automation>>>,
    /// What templates read.
    home: RwLock<Home>,
    /// The last attachment that renders when it attaches: the next one
    /// waits for it, so they set up (and log) in the order they attached,
    /// as HA's do.
    attach_chain: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

/// A rendering attachment's place in [`Shared::attach_chain`]: the one
/// before it, and its own (dropped when it has set up).
pub(crate) type Turn = (
    Option<tokio::sync::oneshot::Receiver<()>>,
    tokio::sync::oneshot::Sender<()>,
);

impl Shared {
    fn home(&self) -> Home {
        self.home.read().clone()
    }

    /// The next place in [`Shared::attach_chain`].
    pub(crate) fn turn(&self) -> Turn {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (self.attach_chain.lock().replace(rx), tx)
    }
}

/// Every automation, its listeners and its services.
#[derive(Clone)]
pub struct Automations {
    shared: Arc<Shared>,
}

/// Attach one trigger: its listeners or timers go in `out`, and its
/// firings to `sink`. `vars` are what its templates render with (`this`,
/// `trigger_variables`, or a waiting run's variables); `templated` says
/// whether to make the `trigger` variable; `since` is the first change it
/// hears.
#[allow(clippy::too_many_arguments)]
fn attach_trigger(
    shared: &Arc<Shared>,
    name: &str,
    t: &Trigger,
    vars: &Arc<Vars>,
    templated: bool,
    sink: Sink,
    since: u64,
    out: &mut Vec<Attached>,
) {
    let make = |description: String, platform: &'static str, var: Option<Var>| TriggerVars {
        id: t.id.clone(),
        idx: t.idx.to_string(),
        alias: t.alias.clone(),
        platform: Some(platform),
        description: Some(description),
        var,
        extra: None,
    };
    match &t.kind {
        TriggerKind::State {
            entity_ids,
            to,
            for_,
            match_all,
        } => {
            for e in entity_ids {
                let (to, for_, match_all, sink) = (to.clone(), *for_, *match_all, sink.clone());
                let description = format!("state of {e}");
                let t = t.clone();
                let fire = move |old: Option<&Arc<State>>, new: &Arc<State>| {
                    let var = templated.then(|| {
                        trigger_var(
                            &t,
                            "state",
                            vec![
                                ("entity_id", Var::str(new.entity_id.as_str())),
                                ("from_state", Var::opt_state(old)),
                                ("to_state", Var::state(new.clone())),
                                ("for", for_.map_or_else(Var::none, Var::timedelta)),
                                ("attribute", Var::none()),
                                ("description", Var::str(&description)),
                            ],
                        )
                    });
                    TriggerVars {
                        id: t.id.clone(),
                        idx: t.idx.to_string(),
                        alias: t.alias.clone(),
                        platform: Some("state"),
                        description: Some(description.clone()),
                        var,
                        extra: None,
                    }
                };
                // With `for`: the pending firings (id, value, timer).
                let pending: PendingFor = Arc::default();
                let next_pending = Arc::new(AtomicU64::new(0));
                out.push(Attached::Pending(pending.clone()));
                out.push(Attached::Listening(shared.listeners.listen(
                    e,
                    since,
                    Arc::new(move |_, ev| {
                        let Some((old, new)) = change(ev) else { return };
                        // `async_track_same_state`: another value (or
                        // removal) cancels each pending `for`.
                        let new_s = new.map(|n| n.state.as_str());
                        pending.lock().retain(|(_, v, h)| {
                            let same = Some(v.as_str()) == new_s;
                            if !same {
                                h.abort();
                            }
                            same
                        });
                        let Some(new) = new else { return };
                        let new_s = new.state.as_str();
                        let to_ok = to.as_ref().is_none_or(|t| t.iter().any(|x| x == new_s));
                        // With no `to`, any change fires (an attribute's
                        // too); with one, only a new state.
                        let same = old.map(|s| s.state.as_str()) == Some(new_s);
                        if !to_ok || (!match_all && same) {
                            return;
                        }
                        let vars = fire(old, new);
                        match for_ {
                            None => sink(vars, Some(new.context.clone())),
                            Some(d) => {
                                let (sink, ctx) = (sink.clone(), new.context.clone());
                                let p2 = pending.clone();
                                // One timer per change: a pending one for
                                // the same value (an attribute change with
                                // no `to`) runs on, as HA's does.
                                let mut p = pending.lock();
                                let id = next_pending.fetch_add(1, Ordering::Relaxed);
                                let h = tokio::spawn(async move {
                                    tokio::time::sleep(d).await;
                                    p2.lock().retain(|(i, _, _)| *i != id);
                                    sink(vars, Some(ctx));
                                });
                                p.push((id, new_s.to_owned(), h));
                            }
                        }
                    }),
                )));
            }
        }
        TriggerKind::NumericState {
            entity_ids,
            below,
            above,
            value_template: Some(template),
        } => {
            let (listening, task) = track::NumericTemplate {
                trigger: t.clone(),
                entity_ids: entity_ids.clone(),
                below: *below,
                above: *above,
                template: template.clone(),
                name: name.to_owned(),
                vars: vars.clone(),
                templated,
            }
            .spawn(shared, sink, since);
            out.extend(listening.into_iter().map(Attached::Listening));
            out.push(Attached::Task(task));
        }
        TriggerKind::NumericState {
            entity_ids,
            below,
            above,
            value_template: None,
        } => {
            let hass = &shared.hass;
            let armed: Arc<Mutex<HashSet<String>>> = Arc::default();
            for e in entity_ids {
                // At attach: armed unless it matches already; an error
                // (absent, not a number) leaves it unarmed.
                match cond::numeric(hass, e, *below, *above, None) {
                    Ok(false) => {
                        armed.lock().insert(e.clone());
                    }
                    Ok(true) => {}
                    Err(err) => tracing::warn!(
                        logger = "homeassistant.components.homeassistant.triggers.numeric_state",
                        "Error initializing '{name}' trigger: {err}"
                    ),
                }
                let (armed, e2, sink) = (armed.clone(), e.clone(), sink.clone());
                let (below, above) = (*below, *above);
                let (name, t) = (name.to_owned(), t.clone());
                out.push(Attached::Listening(shared.listeners.listen(
                    e,
                    since,
                    Arc::new(move |_, ev| {
                        let Some((old, Some(new))) = change(ev) else {
                            return;
                        };
                        match cond::numeric_of(new, below, above, None) {
                            Err(err) => tracing::warn!(
                                logger =
                                    "homeassistant.components.homeassistant.triggers.numeric_state",
                                "Error in '{name}' trigger: {err}"
                            ),
                            Ok(false) => {
                                armed.lock().insert(e2.clone());
                            }
                            Ok(true) => {
                                if armed.lock().remove(&e2) {
                                    let vars =
                                        numeric_vars(&t, &e2, below, above, old, new, templated);
                                    sink(vars, Some(new.context.clone()));
                                }
                            }
                        }
                    }),
                )));
            }
        }
        TriggerKind::Time { at } => {
            for at in at {
                let (at, sink) = (*at, sink.clone());
                let hass = shared.hass.clone();
                let t = t.clone();
                out.push(Attached::Task(tokio::spawn(async move {
                    // HA's tracker: a random 50-500 ms past the second,
                    // fixed for the tracker's life.
                    let micro = rand::random_range(50_000..=500_000);
                    let mut from = hass.clock().now();
                    loop {
                        let Some(next) = time::next_local(&hass, from, at, micro) else {
                            return;
                        };
                        time::sleep_until_wall(&hass, next).await;
                        let var = templated.then(|| {
                            let tz = hass.config().time_zone.clone();
                            trigger_var(
                                &t,
                                "time",
                                vec![
                                    ("now", Var::datetime(hass.clock().now(), &tz)),
                                    ("description", Var::str("time")),
                                    ("entity_id", Var::none()),
                                ],
                            )
                        });
                        sink(make_time(&t, var), None);
                        from = hass.clock().now() + jiff::SignedDuration::from_secs(1);
                    }
                })));
            }
        }
        TriggerKind::Template {
            value_template,
            for_,
            renders,
        } => {
            let (listening, task) = track::TemplateTrigger {
                trigger: t.clone(),
                template: value_template.clone(),
                for_: *for_,
                name: name.to_owned(),
                vars: vars.clone(),
                templated,
                renders: renders.0.clone(),
            }
            .spawn(shared, sink, since);
            out.push(Attached::Listening(listening));
            out.push(Attached::Task(task));
        }
    }
    let _ = make;
}

/// A time trigger's firing.
fn make_time(t: &Trigger, var: Option<Var>) -> TriggerVars {
    TriggerVars {
        id: t.id.clone(),
        idx: t.idx.to_string(),
        alias: t.alias.clone(),
        platform: Some("time"),
        description: Some("time".into()),
        var,
        extra: None,
    }
}

impl Automation {
    fn hass(&self) -> Option<Hass> {
        self.shared.upgrade().map(|s| s.hass.clone())
    }

    fn mode(&self) -> Mode {
        self.config.as_ref().map_or(Mode::Single, |c| c.mode)
    }

    fn templated(&self) -> bool {
        self.config.as_ref().is_ok_and(|c| c.templated)
    }

    fn attributes(&self, live: &Live) -> Map<String, Value> {
        let mut a = Map::new();
        if let Some(id) = &self.unique_id {
            a.insert("id".into(), json!(id));
        }
        if self.config.is_ok() {
            a.insert(
                "last_triggered".into(),
                json!(live.last_triggered.map(ha_core::isoformat)),
            );
            a.insert("mode".into(), json!(self.mode().as_str()));
            a.insert("current".into(), json!(live.runs.len()));
        }
        a.insert("friendly_name".into(), json!(self.name));
        a
    }

    /// Write the entity's state, with `context` or the entity's recent one.
    fn write(&self, live: &mut Live, context: Option<Context>) {
        let Some(hass) = self.hass() else { return };
        let state = match (&self.config, live.enabled) {
            (Err(_), _) => "unavailable",
            (Ok(_), true) => "on",
            (Ok(_), false) => "off",
        };
        // `async_set_context`'s expiry, on the wall clock.
        let now = hass.clock().now();
        let context = context.or_else(|| {
            live.context
                .as_ref()
                .filter(|(_, at)| now.duration_since(*at) <= CONTEXT_RECENT)
                .map(|(c, _)| c.clone())
        });
        hass.set_state_with_context(
            self.entity_id.clone(),
            state,
            self.attributes(live),
            context,
        );
    }

    fn set_context(live: &mut Live, ctx: &Context, now: jiff::Timestamp) {
        live.context = Some((ctx.clone(), now));
    }

    /// Where this automation's triggers fire.
    fn sink(self: &Arc<Self>) -> Sink {
        let me = Arc::downgrade(self);
        Arc::new(move |vars, ctx| {
            if let Some(a) = me.upgrade() {
                a.fired(vars, ctx.as_ref(), Fire::Trigger);
            }
        })
    }

    /// Attach the triggers (enabled, valid, not yet attached), with `this`
    /// and the `trigger_variables` (`_async_attach_triggers`).
    fn attach(self: &Arc<Self>) {
        let (Ok(cfg), Some(shared)) = (&self.config, self.shared.upgrade()) else {
            return;
        };
        let mut live = self.live.lock();
        if !live.enabled || !live.attached.is_empty() || live.attaching {
            return;
        }
        let since = shared.hass.states().seq();
        let mut vars = Vars::new();
        if cfg.templated {
            vars.insert("this".into(), this_var(&shared.hass, &self.entity_id));
        }
        if cfg.trigger_variables.iter().all(|v| v.value.is_static()) {
            tpl::static_variables(&cfg.trigger_variables, &mut vars);
            self.attach_with(&shared, cfg, &Arc::new(vars), since, &mut live);
            return;
        }
        // The variables render first (limited), on a render thread; the
        // state feed waits.
        live.attaching = true;
        let generation = live.generation;
        drop(live);
        let guard = shared.listeners.guard();
        let (me, cfg, home) = (self.clone(), cfg.clone(), shared.home());
        tokio::spawn(async move {
            let defs = cfg.trigger_variables.clone();
            let rendered = ha_core::template::run::run(move || {
                let mut out = Out::default();
                (tpl::variables(&defs, vars, &home, &mut out), out)
            })
            .await;
            let Some(shared) = me.shared.upgrade() else {
                return;
            };
            let mut live = me.live.lock();
            live.attaching = false;
            if live.generation != generation {
                // Turned off (and maybe on again) meanwhile: start over.
                drop(live);
                me.attach();
                return;
            }
            let Some((rendered, mut out)) = rendered else {
                return;
            };
            out.emit();
            match rendered {
                Err(e) => tracing::error!(
                    logger = logger(&me.entity_id),
                    "Error rendering trigger variables: {e}"
                ),
                Ok(vars) => {
                    if live.enabled && live.attached.is_empty() {
                        me.attach_with(&shared, &cfg, &Arc::new(vars), since, &mut live);
                    }
                }
            }
            drop(live);
            drop(guard);
        });
    }

    fn attach_with(
        self: &Arc<Self>,
        shared: &Arc<Shared>,
        cfg: &Config,
        vars: &Arc<Vars>,
        since: u64,
        live: &mut Live,
    ) {
        let sink = self.sink();
        for t in &cfg.triggers {
            attach_trigger(
                shared,
                &self.name,
                t,
                vars,
                cfg.templated,
                sink.clone(),
                since,
                &mut live.attached,
            );
        }
        tracing::info!(
            logger = logger(&self.entity_id),
            "Initialized trigger {}",
            self.name
        );
    }

    /// `async_trigger`: `this`, the variables and the conditions (unless
    /// skipped), then the mode, the event, `last_triggered` and a write,
    /// then the run. Returns the run (to await, for `automation.trigger`).
    fn fired(
        self: &Arc<Self>,
        vars: TriggerVars,
        parent: Option<&Context>,
        how: Fire,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let (Ok(cfg), Some(shared)) = (&self.config, self.shared.upgrade()) else {
            return None;
        };
        let hass = shared.hass.clone();
        // `_async_trigger_if_enabled`: a trigger that raced `turn_off`.
        if how == Fire::Trigger && !self.live.lock().enabled {
            return None;
        }
        let skip_conditions = how
            == (Fire::Service {
                skip_conditions: true,
            });
        let log = logger(&self.entity_id);
        if let Some(d) = &vars.description {
            tracing::debug!(logger = log, "Automation trigger 'None' triggered by {d}");
        }
        let now = hass.clock().now();
        let mut ctx = Context::at(now);
        ctx.parent_id = parent.map(|p| p.id);
        // `{"this": this, **run_variables}`.
        let mut run_vars = Vars::new();
        if cfg.templated {
            run_vars.insert("this".into(), this_var(&hass, &self.entity_id));
            for (k, v) in vars.extra.iter().flatten() {
                run_vars.insert(k.clone(), Var::json(v));
            }
            let trigger = vars
                .var
                .clone()
                .unwrap_or_else(|| Var::dict([("platform".to_owned(), Var::none())]));
            run_vars.insert("trigger".into(), trigger);
        }
        // Made when the run ends (or never runs), which is what
        // `automation.trigger` waits for.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let renders = cfg.variables.iter().any(|v| !v.value.is_static())
            || (!skip_conditions && cfg.conditions.iter().any(Condition::has_template));
        if !renders {
            tpl::static_variables(&cfg.variables, &mut run_vars);
            if !skip_conditions {
                let (pass, warning) = cond::all(&hass, &cfg.conditions, &vars.id, None);
                if let Some(w) = warning {
                    self.condition_warning(&w);
                }
                if !pass {
                    tracing::debug!(logger = log, "Conditions not met, aborting automation.");
                    return None;
                }
            }
            self.start(vars, run_vars, ctx, done_tx, None);
            return Some(done_rx);
        }
        // Rendered on a render thread, one trigger at a time; the state
        // feed waits.
        let guard = shared.listeners.guard();
        let (me, cfg, home) = (self.clone(), cfg.clone(), shared.home());
        tokio::spawn(async move {
            let _gate = me.gate.lock().await;
            let id = vars.id.clone();
            let job_cfg = cfg.clone();
            let job = ha_core::template::run::run(move || {
                let mut out = Out::default();
                let rendered = tpl::variables(&job_cfg.variables, run_vars, &home, &mut out);
                let checked = match &rendered {
                    Ok(v) if !skip_conditions => Some(cond::all(
                        &hass,
                        &job_cfg.conditions,
                        &id,
                        Some((&home, v, &mut out)),
                    )),
                    _ => None,
                };
                (rendered, checked, out)
            })
            .await;
            let Some((rendered, checked, mut out)) = job else {
                return;
            };
            out.emit();
            let run_vars = match rendered {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(logger = log, "Error rendering variables: {e}");
                    return;
                }
            };
            if let Some((pass, warning)) = checked {
                if let Some(w) = warning {
                    me.condition_warning(&w);
                }
                if !pass {
                    tracing::debug!(logger = log, "Conditions not met, aborting automation.");
                    return;
                }
            }
            // Turned off while it rendered.
            if how == Fire::Trigger && !me.live.lock().enabled {
                return;
            }
            me.start(vars, run_vars, ctx, done_tx, Some(guard));
        });
        Some(done_rx)
    }

    /// `ConditionsChecker`'s warning.
    fn condition_warning(&self, errors: &str) {
        tracing::warn!(
            logger = "homeassistant.components.automation",
            "Error evaluating condition in '{}':\n{errors}",
            self.name
        );
    }

    /// The mode, the event, `last_triggered` and a write, then the run.
    fn start(
        self: &Arc<Self>,
        vars: TriggerVars,
        mut run_vars: Vars,
        ctx: Context,
        done_tx: tokio::sync::oneshot::Sender<()>,
        guard: Option<Guard>,
    ) {
        let (Ok(cfg), Some(hass)) = (&self.config, self.hass()) else {
            return;
        };
        let log = logger(&self.entity_id);
        let now = hass.clock().now();
        let mut live = self.live.lock();
        Self::set_context(&mut live, &ctx, now);
        if !live.runs.is_empty() {
            match cfg.mode {
                Mode::Single => {
                    tracing::warn!(logger = log, "{}: Already running", self.name);
                    return;
                }
                Mode::Restart => {
                    tracing::info!(logger = log, "{}: Restarting", self.name);
                    for r in live.runs.drain(..) {
                        if let Some(h) = r.handle {
                            h.abort();
                        }
                    }
                }
            }
        }
        let mut data = json!({"name": self.name, "entity_id": self.entity_id.as_str()});
        if let Some(d) = &vars.description {
            data["source"] = json!(d);
        }
        hass.bus().fire(Event::new(
            EventKind::Custom {
                name: "automation_triggered".into(),
                data,
            },
            now,
            ctx.clone(),
        ));
        live.last_triggered = Some(hass.clock().now());
        live.next_run += 1;
        let run_id = live.next_run;
        live.runs.push(Run {
            id: run_id,
            handle: None,
        });
        self.write(&mut live, Some(ctx.clone()));
        tracing::info!(logger = log, "{}: Running automation actions", self.name);
        let me = self.clone();
        let handle = tokio::spawn(async move {
            let _done = done_tx;
            let mut guard = guard;
            let Ok(cfg) = &me.config else { return };
            if let Err(e) = me
                .run_actions(&cfg.actions, &vars, &ctx, None, &mut run_vars, &mut guard)
                .await
            {
                tracing::error!(
                    logger = logger(&me.entity_id),
                    "Error while executing automation {}: {e}",
                    me.entity_id
                );
            }
            drop(guard);
            let mut live = me.live.lock();
            live.runs.retain(|r| r.id != run_id);
            me.write(&mut live, None);
        });
        if let Some(r) = live.runs.iter_mut().find(|r| r.id == run_id) {
            r.handle = Some(handle);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_actions<'a>(
        self: &'a Arc<Self>,
        actions: &'a [Action],
        tv: &'a TriggerVars,
        ctx: &'a Context,
        choose: Option<String>,
        vars: &'a mut Vars,
        guard: &'a mut Option<Guard>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let Some(shared) = self.shared.upgrade() else {
                return Ok(());
            };
            let hass = shared.hass.clone();
            let log = logger(&self.entity_id);
            let prefix = choose.unwrap_or_default();
            for (step, action) in actions.iter().enumerate() {
                match action {
                    Action::Call {
                        service,
                        data,
                        target,
                    } => {
                        tracing::info!(
                            logger = log,
                            "{}: {prefix}Executing step call service",
                            self.name
                        );
                        let prepared = if action.is_static_call() {
                            let config::Service::Static { domain, service } = service else {
                                unreachable!("a static call")
                            };
                            let mut d = Map::new();
                            for c in data {
                                if let config::Complex::Static(Value::Object(m)) = c {
                                    d.extend(m.clone());
                                }
                            }
                            let target = match target {
                                Some(config::Complex::Static(Value::Object(t))) => Some(t.clone()),
                                _ => None,
                            };
                            Ok(tpl::Prepared {
                                domain: domain.clone(),
                                service: service.clone(),
                                data: d,
                                target,
                            })
                        } else {
                            let (s, d, t, v) =
                                (service.clone(), data.clone(), target.clone(), vars.clone());
                            let home = shared.home();
                            match ha_core::template::run::run(move || {
                                let mut out = Out::default();
                                (tpl::prepare(&s, &d, t.as_ref(), &home, &v, &mut out), out)
                            })
                            .await
                            {
                                Some((prepared, mut out)) => {
                                    out.emit();
                                    prepared
                                }
                                None => Err("the template stopped unexpectedly".into()),
                            }
                        };
                        // The call is made: the next change may be handled.
                        guard.take();
                        let result = match prepared {
                            Err(e) => Err(e),
                            Ok(p) => hass
                                .call_service(
                                    &p.domain,
                                    &p.service,
                                    p.data,
                                    p.target.filter(|t| !t.is_empty()),
                                    false,
                                    Some(ctx.clone()),
                                )
                                .await
                                .map(|_| ())
                                .map_err(|e| match &e {
                                    ServiceError::Invalid(m) => m.clone(),
                                    e => e.to_string(),
                                }),
                        };
                        if let Err(msg) = result {
                            tracing::error!(
                                logger = log,
                                "{}: {prefix}Error executing script. Error for call_service at pos {}: {msg}",
                                self.name,
                                step + 1
                            );
                            return Err(msg);
                        }
                    }
                    Action::Choose { options, default } => {
                        let chosen = self.choose(&shared, options, tv, vars).await;
                        match chosen {
                            Some(i) => {
                                let p = format!("Choose at step {}: choice {}: ", step + 1, i + 1);
                                let seq = &options[i].1;
                                self.run_actions(seq, tv, ctx, Some(p), vars, guard).await?;
                            }
                            None if !default.is_empty() => {
                                let p = format!("Choose at step {}: default: ", step + 1);
                                self.run_actions(default, tv, ctx, Some(p), vars, guard)
                                    .await?;
                            }
                            None => {}
                        }
                    }
                    Action::WaitForTrigger { triggers } => {
                        tracing::info!(
                            logger = log,
                            "{}: {prefix}Executing step wait for trigger",
                            self.name
                        );
                        guard.take();
                        let matched = self.wait_for(&shared, triggers, vars).await;
                        if self.templated() {
                            vars.insert(
                                "wait".into(),
                                Var::dict([
                                    ("remaining".to_owned(), Var::none()),
                                    ("completed".to_owned(), Var::bool(matched.is_some())),
                                    (
                                        "trigger".to_owned(),
                                        matched.and_then(|m| m.var).unwrap_or_else(Var::none),
                                    ),
                                ]),
                            );
                        }
                    }
                }
            }
            guard.take();
            Ok(())
        })
    }

    /// `_async_step_choose`: the first option whose conditions pass
    /// (`_test_conditions`: an error is logged and the option isn't taken).
    async fn choose(
        &self,
        shared: &Arc<Shared>,
        options: &[(Vec<Condition>, Vec<Action>)],
        tv: &TriggerVars,
        vars: &Vars,
    ) -> Option<usize> {
        let log = logger(&self.entity_id);
        let rendered = options
            .iter()
            .any(|(c, _)| c.iter().any(Condition::has_template));
        let (chosen, warnings, mut out) = if rendered {
            let (hass, home, opts, v, id) = (
                shared.hass.clone(),
                shared.home(),
                options.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>(),
                vars.clone(),
                tv.id.clone(),
            );
            ha_core::template::run::run(move || {
                let mut out = Out::default();
                let mut warnings = Vec::new();
                let chosen = opts.iter().position(|conds| {
                    let (pass, w) =
                        cond::test(&hass, conds, &id, "choose", Some((&home, &v, &mut out)));
                    warnings.extend(w);
                    pass
                });
                (chosen, warnings, out)
            })
            .await?
        } else {
            let mut warnings = Vec::new();
            let chosen = options.iter().position(|(conds, _)| {
                let (pass, w) = cond::test(&shared.hass, conds, &tv.id, "choose", None);
                warnings.extend(w);
                pass
            });
            (chosen, warnings, Out::default())
        };
        out.emit();
        for w in warnings {
            tracing::warn!(logger = log, "{}: {w}", self.name);
        }
        chosen
    }

    /// `wait_for_trigger`: its triggers attach now (a state already
    /// matching doesn't count; templates render with the run's variables);
    /// done when one fires.
    async fn wait_for(
        &self,
        shared: &Arc<Shared>,
        triggers: &[Trigger],
        vars: &Vars,
    ) -> Option<TriggerVars> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TriggerVars>();
        let sink: Sink = Arc::new(move |v, _| {
            let _ = tx.send(v);
        });
        let since = shared.hass.states().seq();
        let vars = Arc::new(vars.clone());
        let mut attached = Vec::new();
        for t in triggers {
            attach_trigger(
                shared,
                &self.name,
                t,
                &vars,
                self.templated(),
                sink.clone(),
                since,
                &mut attached,
            );
        }
        drop(sink);
        tracing::info!(
            logger = logger(&self.entity_id),
            "{}: Initialized trigger",
            self.name
        );
        let matched = rx.recv().await;
        drop(attached);
        matched
    }

    fn stop_runs(&self, live: &mut Live) {
        for r in live.runs.drain(..) {
            if let Some(h) = r.handle {
                h.abort();
            }
        }
    }
}

/// The registry's entity id for `unique_id`, else `slugify(name)` made
/// unique; `None` if the registry has it disabled.
fn entity_id_for(
    hass: &Hass,
    registries: &Registries,
    unique_id: Option<&str>,
    name: &str,
    taken: &HashSet<String>,
) -> Option<(EntityId, Option<String>)> {
    let entry = unique_id.and_then(|u| {
        registries.entities.iter().find(|e| {
            e.platform == DOMAIN && e.unique_id.as_ref().and_then(Value::as_str) == Some(u)
        })
    });
    if let Some(e) = entry {
        if e.disabled_by.is_some() {
            return None;
        }
        return EntityId::parse(&e.entity_id)
            .ok()
            .map(|id| (id, e.name.clone()));
    }
    let base = format!("{DOMAIN}.{}", ha_components::slugify(name));
    let used = |id: &str| {
        taken.contains(id)
            || registries.entities.iter().any(|e| e.entity_id == id)
            || EntityId::parse(id).is_ok_and(|i| hass.states().get(&i).is_some())
    };
    let id = (1..)
        .map(|n| {
            if n == 1 {
                base.clone()
            } else {
                format!("{base}_{n}")
            }
        })
        .find(|id| !used(id))?;
    EntityId::parse(&id).ok().map(|id| (id, None))
}

/// The automations in a loaded `configuration.yaml`: every `automation`
/// or `automation <anything>` key (`config_per_platform`), a list of them or
/// one; a falsy value is none.
pub fn from_config(config: &ha_core::yaml::Yaml) -> Vec<Value> {
    from_config_yaml(config)
        .iter()
        .map(|y| y.to_json())
        .collect()
}

/// HA's `raw_config` shows its validators' changes to nested mappings:
/// an action's legacy `service` becomes `action`, moved last. Top-level
/// keys, triggers and conditions are as written.
pub fn migrate_actions(item: &ha_core::yaml::Yaml) -> ha_core::yaml::Yaml {
    use ha_core::yaml::Yaml;
    fn action(a: &Yaml) -> Yaml {
        let Yaml::Map(pairs) = a else {
            return a.clone();
        };
        let mut out: Vec<(Yaml, Yaml)> = Vec::new();
        let mut service = None;
        let has_action = pairs.iter().any(|(k, _)| k.as_str() == Some("action"));
        for (k, v) in pairs {
            match k.as_str() {
                Some("service") if !has_action => service = Some(v.clone()),
                Some("sequence" | "default" | "then" | "else") => out.push((k.clone(), actions(v))),
                Some("choose") => out.push((k.clone(), choose(v))),
                _ => out.push((k.clone(), v.clone())),
            }
        }
        if let Some(s) = service {
            out.push((Yaml::Str("action".into()), s));
        }
        Yaml::Map(out)
    }
    fn actions(v: &Yaml) -> Yaml {
        match v {
            Yaml::Seq(items) => Yaml::Seq(items.iter().map(action).collect()),
            single => action(single),
        }
    }
    fn choose(v: &Yaml) -> Yaml {
        let option = |o: &Yaml| match o {
            Yaml::Map(pairs) => Yaml::Map(
                pairs
                    .iter()
                    .map(|(k, v)| match k.as_str() {
                        Some("sequence") => (k.clone(), actions(v)),
                        _ => (k.clone(), v.clone()),
                    })
                    .collect(),
            ),
            o => o.clone(),
        };
        match v {
            Yaml::Seq(items) => Yaml::Seq(items.iter().map(option).collect()),
            single => option(single),
        }
    }
    let Yaml::Map(pairs) = item else {
        return item.clone();
    };
    Yaml::Map(
        pairs
            .iter()
            .map(|(k, v)| match k.as_str() {
                Some("actions" | "action") => (k.clone(), actions(v)),
                _ => (k.clone(), v.clone()),
            })
            .collect(),
    )
}

/// [`from_config`], as loaded (for the raw configs the editor shows).
pub fn from_config_yaml(config: &ha_core::yaml::Yaml) -> Vec<ha_core::yaml::Yaml> {
    use ha_core::yaml::Yaml;
    let Yaml::Map(pairs) = config else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, v) in pairs {
        let Some(k) = k.as_str() else { continue };
        if k != DOMAIN && !k.starts_with("automation ") {
            continue;
        }
        if !v.is_truthy() {
            continue;
        }
        match v {
            Yaml::Seq(items) => out.extend(items.iter().cloned()),
            item => out.push(item.clone()),
        }
    }
    out
}

/// Whether a config has text that could be a template (compiling it, in
/// [`config::parse`], needs a render thread).
fn has_template_text(v: &Value) -> bool {
    match v {
        Value::String(s) => config::is_template_string(s),
        Value::Array(a) => a.iter().any(has_template_text),
        Value::Object(o) => o
            .iter()
            .any(|(k, v)| config::is_template_string(k) || has_template_text(v)),
        _ => false,
    }
}

/// [`config::parse_ordered`] for each item; on a render thread if any has
/// a template to compile.
async fn parse_all(items: &[Value], orders: Vec<config::Order>) -> Vec<Result<Config, String>> {
    let parse = |items: &[Value], orders: &[config::Order]| -> Vec<Result<Config, String>> {
        items
            .iter()
            .zip(orders)
            .map(|(item, order)| config::parse_ordered(item, order))
            .collect()
    };
    if !items.iter().any(has_template_text) {
        return parse(items, &orders);
    }
    let owned = items.to_vec();
    match ha_core::template::run::run(move || parse(&owned, &orders)).await {
        Some(parsed) => parsed,
        None => items
            .iter()
            .map(|_| Err("its templates could not be compiled".to_owned()))
            .collect(),
    }
}

impl Automations {
    /// Make the automations (the items of HA's `automation:` config) and
    /// write their states, restored as HA restores them. Triggers attach at
    /// [`Automations::started`]. Variables render in sorted order: use
    /// [`Automations::setup_yaml`] for the order written.
    pub async fn setup(hass: &Hass, registries: &Registries, items: &[Value]) -> Self {
        let raw: Vec<Option<String>> = items.iter().map(|v| Some(v.to_string())).collect();
        let orders = vec![config::Order::default(); items.len()];
        Self::setup_raw(hass, registries, items, &raw, orders).await
    }

    /// [`Automations::setup`] from YAML as loaded, keeping each item's raw
    /// config in its YAML key order for `automation/config`.
    pub async fn setup_yaml(
        hass: &Hass,
        registries: &Registries,
        items: &[ha_core::yaml::Yaml],
    ) -> Self {
        let json: Vec<Value> = items.iter().map(|y| y.to_json()).collect();
        let raw: Vec<Option<String>> = items
            .iter()
            .map(|y| migrate_actions(y).to_json_text())
            .collect();
        let orders = items.iter().map(config::Order::of).collect();
        Self::setup_raw(hass, registries, &json, &raw, orders).await
    }

    async fn setup_raw(
        hass: &Hass,
        registries: &Registries,
        items: &[Value],
        raw: &[Option<String>],
        orders: Vec<config::Order>,
    ) -> Self {
        let parsed = parse_all(items, orders).await;
        let shared = Arc::new(Shared {
            hass: hass.clone(),
            listeners: Arc::default(),
            automations: Mutex::default(),
            home: RwLock::new(Home::new(hass.clone())),
            attach_chain: Mutex::default(),
        });
        // The state feed, to the listeners, one change at a time.
        {
            let mut feed = hass.subscribe_state_changes();
            let listeners = Arc::downgrade(&shared.listeners);
            tokio::spawn(async move {
                while let Some((seq, ev)) = feed.recv_seq().await {
                    let Some(l) = listeners.upgrade() else { return };
                    if l.dispatch(seq, &ev) {
                        drop(l);
                        // A run started here gets to its first step (HA
                        // starts it as an eager task) before the next
                        // change is seen; work that renders, until it is
                        // done.
                        tokio::task::yield_now().await;
                        let Some(l) = listeners.upgrade() else { return };
                        l.idle().await;
                    }
                }
            });
        }
        let restore = hass.restore();
        let mut taken = HashSet::new();
        let mut seen_ids = HashSet::new();
        for (n, (item, config)) in items.iter().zip(parsed).enumerate() {
            let alias = item.get("alias").and_then(Value::as_str).map(str::to_owned);
            let name = alias.unwrap_or_else(|| format!("automation {n}"));
            let unique_id = match item.get("id") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Number(num)) => Some(num.to_string()),
                _ => None,
            };
            if let Some(u) = &unique_id
                && !seen_ids.insert(u.clone())
            {
                tracing::error!(
                    logger = "homeassistant.components.automation",
                    "Platform automation does not generate unique IDs. ID {u} already exists - ignoring"
                );
                continue;
            }
            let Some((entity_id, registry_name)) =
                entity_id_for(hass, registries, unique_id.as_deref(), &name, &taken)
            else {
                continue;
            };
            taken.insert(entity_id.as_str().to_owned());
            let name = registry_name.unwrap_or(name);
            if let Err(e) = &config {
                tracing::error!(
                    logger = "homeassistant.components.automation",
                    "Automation with alias '{name}' could not be validated and has been disabled: {e}"
                );
            }
            // Restore: on/off and `last_triggered`; `initial_state` wins.
            let mut live = Live::default();
            if let Ok(c) = &config {
                let last = restore.last_state(entity_id.as_str());
                live.enabled = match &last {
                    Some(s) => s["state"] == "on",
                    None => true,
                };
                if let Some(i) = c.initial_state {
                    live.enabled = i;
                }
                if last.is_none() {
                    tracing::debug!(
                        logger = logger(&entity_id),
                        "Automation {entity_id} not in state storage, state True from default is used"
                    );
                }
                live.last_triggered = last
                    .as_ref()
                    .and_then(|s| s["attributes"]["last_triggered"].as_str())
                    .and_then(|t| t.parse::<jiff::Timestamp>().ok());
                restore.register(entity_id.as_str(), None);
            }
            let a = Arc::new(Automation {
                shared: Arc::downgrade(&shared),
                entity_id,
                name,
                unique_id,
                raw: raw[n].clone(),
                config: config.map(Arc::new),
                live: Mutex::new(live),
                gate: tokio::sync::Mutex::new(()),
            });
            {
                let mut live = a.live.lock();
                a.write(&mut live, None);
            }
            shared.automations.lock().push(a);
        }
        let me = Self { shared };
        me.register_services();
        me
    }

    /// The entity registry's sensor display precision, for templates'
    /// `states(…, rounded=True)`.
    pub fn set_display_precision(&self, precision: DisplayPrecision) {
        self.shared.home.write().display_precision = Some(precision);
    }

    /// The automation with this entity id.
    pub fn by_entity(&self, entity_id: &str) -> Option<Arc<Automation>> {
        self.list()
            .into_iter()
            .find(|a| a.entity_id.as_str() == entity_id)
    }

    pub fn list(&self) -> Vec<Arc<Automation>> {
        self.shared.automations.lock().clone()
    }

    /// `homeassistant_started`: attach every enabled automation's
    /// triggers, then write its state again.
    pub fn started(&self) {
        for a in self.list() {
            a.attach();
            let mut live = a.live.lock();
            a.write(&mut live, None);
        }
    }

    /// `homeassistant_stop`: stop every run (each then reads `current: 0`).
    pub fn stop(&self) {
        for a in self.list() {
            let mut live = a.live.lock();
            if !live.runs.is_empty() {
                a.stop_runs(&mut live);
                a.write(&mut live, None);
            }
        }
    }

    fn targets(&self, data: &Map<String, Value>) -> Vec<Arc<Automation>> {
        let ids = ha_core::ServiceCall::entity_ids_of(data.get("entity_id"));
        self.list()
            .into_iter()
            .filter(|a| a.config.is_ok())
            .filter(|a| match &ids {
                Ok(ha_core::EntityTarget::All) => true,
                Ok(ha_core::EntityTarget::Ids(ids)) => ids.contains(&a.entity_id),
                Err(_) => false,
            })
            .collect()
    }

    fn register_services(&self) {
        let hass = &self.shared.hass;
        let weak = Arc::downgrade(&self.shared);
        let get = move || weak.upgrade().map(|shared| Automations { shared });
        for name in ["turn_on", "turn_off", "toggle", "trigger"] {
            let schema = match name {
                "turn_off" => Schema::entity_service().optional("stop_actions", Kind::Boolean),
                "trigger" => Schema::entity_service()
                    .optional("skip_condition", Kind::StrictBool)
                    .optional("variables", Kind::Dict),
                _ => Schema::entity_service(),
            };
            let get = get.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let me = get();
                    async move {
                        let Some(me) = me else { return Ok(None) };
                        let mut runs = Vec::new();
                        for a in me.targets(&call.data) {
                            match name {
                                "trigger" => {
                                    let skip = call
                                        .data
                                        .get("skip_condition")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(true);
                                    // `{**variables, "trigger": {"platform": None}}`.
                                    let vars = TriggerVars {
                                        id: String::new(),
                                        idx: String::new(),
                                        alias: None,
                                        platform: None,
                                        description: None,
                                        var: None,
                                        extra: call
                                            .data
                                            .get("variables")
                                            .and_then(Value::as_object)
                                            .cloned(),
                                    };
                                    runs.extend(a.fired(
                                        vars,
                                        Some(&call.context),
                                        Fire::Service {
                                            skip_conditions: skip,
                                        },
                                    ));
                                }
                                _ => {
                                    let on = match name {
                                        "turn_on" => true,
                                        "turn_off" => false,
                                        _ => !a.live.lock().enabled,
                                    };
                                    let stop = name != "turn_off"
                                        || call.data.get("stop_actions").and_then(Value::as_bool)
                                            != Some(false);
                                    me.set_enabled(&a, on, stop, &call.context);
                                }
                            }
                        }
                        for r in runs {
                            let _ = r.await;
                        }
                        Ok(None)
                    }
                },
            );
        }
    }

    fn set_enabled(&self, a: &Arc<Automation>, on: bool, stop_actions: bool, ctx: &Context) {
        {
            let mut live = a.live.lock();
            live.enabled = on;
            if !on {
                live.attached.clear();
                live.generation += 1;
                if stop_actions {
                    a.stop_runs(&mut live);
                }
            }
            Automation::set_context(&mut live, ctx, self.shared.hass.clock().now());
        }
        if on && self.shared.hass.core_state() == ha_core::CoreState::Running {
            a.attach();
        }
        let mut live = a.live.lock();
        a.write(&mut live, Some(ctx.clone()));
    }
}

#[cfg(test)]
mod template_tests;
#[cfg(test)]
mod tests;
