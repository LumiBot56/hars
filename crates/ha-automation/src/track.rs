//! Triggers that render: the `template` trigger
//! (`components/template/trigger.py` on `helpers/event.py`'s
//! `async_track_template_result`) and a `numeric_state` trigger with a
//! `value_template`.
//!
//! Each is one task, idle until something it depends on happens. Changes
//! reach it through listeners, each carrying a [`Guard`] so the state feed
//! waits until the change is handled (HA handles it in one go, in its event
//! loop). Renders run on a render thread.
//!
//! The template trigger, as HA:
//! - When it attaches, the template is rendered twice: once to arm the
//!   trigger (armed if false; an error is a warning), once to set up the
//!   tracking (an error is logged as HA's `Error while processing
//!   template`). It then listens to what that render read: the entities,
//!   the domains (any change of an entity in one), or every change, and
//!   re-renders each minute (at second 0, plus a fixed random 50–500 ms)
//!   while the render reads the clock. Nothing else wakes it.
//! - A change re-renders it if the last render read it (or, for an
//!   entity added or removed, counted its domain or every state), or if
//!   the last render failed. A change of an entity read by name renders at
//!   once; one picked up through a domain at most once a second, through
//!   every state (or after an error) at most once a minute: a change inside
//!   the limit is replayed when it ends, and until then only the entities
//!   read by name are listened to.
//! - A new result is handed on (the first one after attaching always is,
//!   and an error only if the last wasn't one): an error is a warning; a
//!   true result fires if armed, after `for` if it has one (any new result
//!   cancels a pending `for`); a false one arms.
//!
//! Deliberate differences: HA's change listeners see a change at once;
//! these see it in feed order after the work of earlier changes is done,
//! so the states a render reads may already be newer than the change, as
//! with conditions ([`crate`]). A trigger attached while the feed is
//! handing out a change may miss that one change if its attachment has to
//! render `trigger_variables` first.

use crate::cond::{self, CondError, Out};
use crate::config::{Tpl, Trigger};
use crate::listen::{Guard, Listening};
use crate::{Shared, Sink, TriggerVars, change, time};
use ha_core::template::{Mode, RenderInfo, Rendered, TemplateError, Var, Vars};
use ha_core::{Context, Event};
use smol_str::SmolStr;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// A change for a tracker, and the guard the feed waits on.
type Msg = (u64, Arc<Event>, Guard);

/// `ALL_STATES_RATE_LIMIT`, `DOMAIN_STATES_RATE_LIMIT`.
const ALL_STATES_RATE_LIMIT: jiff::SignedDuration = jiff::SignedDuration::from_secs(60);
const DOMAIN_STATES_RATE_LIMIT: jiff::SignedDuration = jiff::SignedDuration::from_secs(1);

/// A listener that forwards changes to a tracker's inbox.
fn forward(shared: &Arc<Shared>, tx: &mpsc::UnboundedSender<Msg>) -> crate::listen::Callback {
    let (tx, listeners) = (tx.clone(), Arc::downgrade(&shared.listeners));
    Arc::new(move |seq, ev| {
        if let Some(l) = listeners.upgrade() {
            let _ = tx.send((seq, ev.clone(), l.guard()));
        }
    })
}

/// `RenderInfo` as HA freezes it, with whether the render failed.
#[derive(Default)]
struct Info {
    read: RenderInfo,
    failed: bool,
}

impl Info {
    /// `RenderInfo.filter`.
    fn filter(&self, entity_id: &str, domain: &str) -> bool {
        if self.failed || self.read.all_states {
            return true;
        }
        if !self.read.domains.is_empty() {
            return self.read.domains.contains(domain) || self.read.entities.contains(entity_id);
        }
        self.read.entities.contains(entity_id)
    }

    /// `RenderInfo.filter_lifecycle`.
    fn filter_lifecycle(&self, domain: &str) -> bool {
        self.failed
            || self.read.all_states_lifecycle
            || self.read.domains_lifecycle.contains(domain)
    }

    /// `RenderInfo.rate_limit`.
    fn rate_limit(&self) -> Option<jiff::SignedDuration> {
        if self.read.all_states || self.failed {
            Some(ALL_STATES_RATE_LIMIT)
        } else if !self.read.domains.is_empty() || !self.read.domains_lifecycle.is_empty() {
            Some(DOMAIN_STATES_RATE_LIMIT)
        } else {
            None
        }
    }
}

/// What the tracker listens to (`TrackStates`).
#[derive(PartialEq, Eq, Default)]
struct Tracking {
    all: bool,
    entities: BTreeSet<SmolStr>,
    domains: BTreeSet<SmolStr>,
}

impl Tracking {
    /// `_render_infos_to_track_states`, with domains and every state left
    /// out while rate limited (`_suppress_domain_all_in_render_info`).
    fn of(info: &Info, limited: bool) -> Tracking {
        let r = &info.read;
        if limited {
            return Tracking {
                entities: r.entities.clone(),
                ..Tracking::default()
            };
        }
        if r.needs_all() {
            return Tracking {
                all: true,
                ..Tracking::default()
            };
        }
        Tracking {
            all: false,
            entities: r.entities.clone(),
            domains: r.tracked_domains().cloned().collect(),
        }
    }
}

/// `TrackTemplateResult.result`: a result or an error, compared as HA
/// compares them.
#[derive(Clone)]
enum Outcome {
    Ok(Rendered),
    Err(TemplateError),
}

impl Outcome {
    fn same(&self, other: &Outcome) -> bool {
        match (self, other) {
            (Outcome::Ok(a), Outcome::Ok(b)) => {
                let (a, b) = (Var::rendered(a).to_json(), Var::rendered(b).to_json());
                py_eq(&a, &b)
            }
            // Two errors are "the same": only the first is handed on.
            (Outcome::Err(_), Outcome::Err(_)) => true,
            _ => false,
        }
    }
}

/// Python's `==` on parsed results: numbers and booleans by value.
fn py_eq(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value as J;
    let num = |v: &J| match v {
        J::Number(n) => n.as_f64(),
        J::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    };
    match (a, b) {
        (J::Array(x), J::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q))
        }
        (J::Object(x), J::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w)))
        }
        _ => match (num(a), num(b)) {
            (Some(p), Some(q)) => p == q,
            _ => a == b,
        },
    }
}

/// `result_as_boolean`: `cv.boolean` on the result, else false.
fn as_boolean(r: &Rendered) -> bool {
    let v = Var::rendered(r);
    if let Some(s) = v.as_str() {
        return matches!(
            s.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on" | "enable"
        );
    }
    match v.to_json() {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => false,
    }
}

/// A template trigger's attachment.
pub(crate) struct TemplateTrigger {
    pub(crate) trigger: Trigger,
    pub(crate) template: Tpl,
    pub(crate) for_: Option<Duration>,
    /// The automation's name (HA's `trigger_info["name"]`).
    pub(crate) name: String,
    /// The variables the triggers attach with (`this`, `trigger_variables`).
    pub(crate) vars: Arc<Vars>,
    /// Whether the run's templates read `trigger` (else it isn't made).
    pub(crate) templated: bool,
    /// The template's render count, for HA's `Template<…renders=N>`.
    pub(crate) renders: Arc<AtomicU64>,
}

impl TemplateTrigger {
    /// Attach: listen to every change from now (queued until the first
    /// render says what to listen to), and start the task.
    pub(crate) fn spawn(
        self,
        shared: &Arc<Shared>,
        sink: Sink,
        since: u64,
    ) -> tokio::task::JoinHandle<()> {
        let (tx, rx) = mpsc::unbounded_channel();
        // Every change from now is queued until the first render says what
        // to listen to; this listener is then dropped (`State::listen`).
        let first = shared.listeners.listen_all(since, forward(shared, &tx));
        let weak = Arc::downgrade(shared);
        tokio::spawn(self.run(weak, tx, rx, (since, first), sink, shared.turn()))
    }

    async fn run(
        self,
        shared: Weak<Shared>,
        tx: mpsc::UnboundedSender<Msg>,
        mut rx: mpsc::UnboundedReceiver<Msg>,
        (since, queued): (u64, Listening),
        sink: Sink,
        (before, turn): crate::Turn,
    ) {
        if let Some(before) = before {
            let _ = before.await;
        }
        let Some(home) = shared.upgrade().map(|s| s.home()) else {
            return;
        };
        let source = self.template.source().to_owned();
        // The trigger's own first render, then the tracker's.
        let (t, h, v) = (self.template.clone(), home.clone(), self.vars.clone());
        let Some((first, setup, mut out)) = ha_core::template::run::run(move || {
            let mut out = Out::default();
            let first = out.render(&t, &h, &v, true);
            let setup = t.render_vars(Some(&h), &v, Mode::Lenient, true);
            out.note(&t, setup.log);
            (first, (setup.result, setup.info), out)
        })
        .await
        else {
            return;
        };
        self.renders.fetch_add(3, Ordering::Relaxed);
        out.emit();
        let mut armed = false;
        match &first {
            Err(e) => tracing::warn!(
                logger = "homeassistant.components.template.trigger",
                "Error initializing 'template' trigger for '{}': {e}",
                self.name
            ),
            Ok(r) => armed = !as_boolean(r),
        }
        let (result, read) = setup;
        let failed = result.is_err();
        if failed {
            tracing::error!(
                logger = "homeassistant.helpers.event",
                "Error while processing template: Template<template=({source}) renders={}>",
                self.renders.load(Ordering::Relaxed)
            );
        }
        // Set up: the next attachment may render.
        drop(turn);
        let mut st = State {
            info: Info { read, failed },
            last: None,
            rate_last: None,
            rate_timer: None,
            tracking: None,
            listening: vec![queued],
            minute: None,
            armed,
            pending: None,
            next_seq: since,
        };
        // The changes heard so far are kept: the next listeners start
        // where the first left off.
        let Some(sh) = shared.upgrade() else { return };
        st.listen(&sh, &tx);
        st.time_listener(&sh);
        drop(sh);
        loop {
            let rate_at = st.rate_timer.as_ref().map(|(at, _)| *at);
            let pending_at = st.pending.as_ref().map(|(at, _, _)| *at);
            let minute_at = st.minute;
            tokio::select! {
                msg = rx.recv() => {
                    let Some((seq, ev, guard)) = msg else { return };
                    if seq < st.next_seq {
                        continue;
                    }
                    st.next_seq = seq + 1;
                    self.refresh(&shared, &tx, &mut st, Some(ev), false, &sink).await;
                    drop(guard);
                }
                () = sleep_until(rate_at) => {
                    let Some((_, ev)) = st.rate_timer.clone() else { continue };
                    self.refresh(&shared, &tx, &mut st, Some(ev), true, &sink).await;
                }
                () = wall_sleep(&shared, minute_at) => {
                    // The next tick, with the same offset.
                    if let (Some((_, micro)), Some(sh)) = (minute_at, shared.upgrade()) {
                        let from = sh.hass.clock().now() + jiff::SignedDuration::from_secs(1);
                        st.minute = Some((time::next_minute(from, micro), micro));
                    }
                    self.refresh(&shared, &tx, &mut st, None, false, &sink).await;
                }
                () = sleep_until(pending_at) => {
                    if let Some((_, vars, ctx)) = st.pending.take() {
                        sink(vars, ctx);
                    }
                }
            }
        }
    }

    /// `TrackTemplateResultInfo._refresh` for its one template, then
    /// `template_listener` with a new result.
    async fn refresh(
        &self,
        shared: &Weak<Shared>,
        tx: &mpsc::UnboundedSender<Msg>,
        st: &mut State,
        event: Option<Arc<Event>>,
        replayed: bool,
        sink: &Sink,
    ) {
        let Some(sh) = shared.upgrade() else { return };
        let now = match &event {
            Some(ev) if !replayed => ev.time_fired,
            _ => sh.hass.clock().now(),
        };
        // `_render_template_if_ready`.
        if let Some(ev) = &event {
            let Some((old, new)) = change(ev) else { return };
            let state = new.or(old).expect("a change has a state");
            let (eid, domain) = (state.entity_id.as_str(), state.entity_id.domain());
            let rerender = st.info.filter(eid, domain)
                || ((new.is_none() || old.is_none()) && st.info.filter_lifecycle(domain));
            if !rerender {
                return;
            }
            let limit = if st.info.read.entities.contains(eid) {
                None
            } else {
                st.info.rate_limit()
            };
            let had_timer = st.rate_timer.is_some();
            if let (Some(limit), Some(last)) = (limit, st.rate_last) {
                let next = last + limit;
                if next > now {
                    if st.rate_timer.is_none() {
                        let wait = Duration::try_from(next.duration_since(now)).unwrap_or_default();
                        st.rate_timer = Some((Instant::now() + wait, ev.clone()));
                    }
                    if !had_timer {
                        // Rate limited: only the entities read by name.
                        st.listen(&sh, tx);
                    }
                    return;
                }
                st.rate_timer = None;
            }
        }
        st.rate_timer = None;
        st.rate_last = Some(now);
        let home = sh.home();
        drop(sh);
        let (t, v) = (self.template.clone(), self.vars.clone());
        let Some((result, read, mut out)) = ha_core::template::run::run(move || {
            let mut out = Out::default();
            let r = t.render_vars(Some(&home), &v, Mode::Lenient, true);
            out.note(&t, r.log);
            (r.result, r.info, out)
        })
        .await
        else {
            return;
        };
        self.renders.fetch_add(2, Ordering::Relaxed);
        out.emit();
        let outcome = match result {
            Ok(r) => Outcome::Ok(r),
            Err(e) => Outcome::Err(e),
        };
        st.info = Info {
            failed: matches!(outcome, Outcome::Err(_)),
            read,
        };
        let Some(sh) = shared.upgrade() else { return };
        st.time_listener(&sh);
        st.listen(&sh, tx);
        drop(sh);
        if st.last.as_ref().is_some_and(|last| last.same(&outcome)) {
            return;
        }
        st.last = Some(outcome.clone());
        self.listener(st, event.as_deref(), &outcome, sink);
    }

    /// The trigger's `template_listener`.
    fn listener(&self, st: &mut State, event: Option<&Event>, outcome: &Outcome, sink: &Sink) {
        let result = match outcome {
            Outcome::Err(e) => {
                tracing::warn!(
                    logger = "homeassistant.components.template.trigger",
                    "Error evaluating 'template' trigger for '{}': {e}",
                    self.name
                );
                return;
            }
            Outcome::Ok(r) => r,
        };
        st.pending = None;
        if !as_boolean(result) {
            st.armed = true;
            return;
        }
        if !st.armed {
            return;
        }
        st.armed = false;
        let (entity_id, from, to) = match event.and_then(|e| change(e).map(|c| (e, c))) {
            Some((_, (old, new))) => {
                let state = new.or(old).expect("a change has a state");
                (
                    Some(state.entity_id.as_str().to_owned()),
                    old.cloned(),
                    new.cloned(),
                )
            }
            None => (None, None, None),
        };
        let description = match &entity_id {
            Some(e) => format!("{e} via template"),
            None => "time change or manual update via template".to_owned(),
        };
        let var = self.templated.then(|| {
            crate::trigger_var(
                &self.trigger,
                "template",
                vec![
                    (
                        "entity_id",
                        entity_id.as_deref().map_or_else(Var::none, Var::str),
                    ),
                    ("from_state", Var::opt_state(from.as_ref())),
                    ("to_state", Var::opt_state(to.as_ref())),
                    ("for", self.for_.map_or_else(Var::none, Var::timedelta)),
                    ("description", Var::str(&description)),
                ],
            )
        });
        let vars = TriggerVars {
            id: self.trigger.id.clone(),
            idx: self.trigger.idx.to_string(),
            alias: self.trigger.alias.clone(),
            platform: Some("template"),
            description: Some(description),
            var,
            extra: None,
        };
        let ctx = to.map(|s| s.context.clone());
        match self.for_ {
            None => sink(vars, ctx),
            Some(d) => st.pending = Some((Instant::now() + d, vars, ctx)),
        }
    }
}

/// A template trigger's tracking state.
struct State {
    info: Info,
    /// The last result handed on (none until the first refresh).
    last: Option<Outcome>,
    /// When it last rendered (`KeyedRateLimit._last_triggered`).
    rate_last: Option<jiff::Timestamp>,
    /// A change replayed when the rate limit ends.
    rate_timer: Option<(Instant, Arc<Event>)>,
    tracking: Option<Tracking>,
    listening: Vec<Listening>,
    /// The next minute tick (wall time), while the template reads the clock.
    minute: Option<(jiff::Timestamp, i64)>,
    armed: bool,
    /// A firing waiting out `for`.
    pending: Option<(Instant, TriggerVars, Option<Context>)>,
    /// The first change not yet handled.
    next_seq: u64,
}

impl State {
    /// `async_update_listeners`: listen to what the last render read
    /// (unchanged listeners stay as they are).
    fn listen(&mut self, sh: &Arc<Shared>, tx: &mpsc::UnboundedSender<Msg>) {
        let want = Tracking::of(&self.info, self.rate_timer.is_some());
        if self.tracking.as_ref() == Some(&want) {
            return;
        }
        let mut listening = Vec::new();
        if want.all {
            listening.push(sh.listeners.listen_all(self.next_seq, forward(sh, tx)));
        } else {
            for d in &want.domains {
                listening.push(
                    sh.listeners
                        .listen_domain(d, self.next_seq, forward(sh, tx)),
                );
            }
            for e in &want.entities {
                // Read by a name its domain doesn't cover already.
                let covered = e
                    .split_once('.')
                    .is_some_and(|(d, _)| want.domains.contains(d));
                if !covered {
                    listening.push(sh.listeners.listen(e, self.next_seq, forward(sh, tx)));
                }
            }
        }
        self.listening = listening;
        self.tracking = Some(want);
    }

    /// `_setup_time_listener`: a tick each minute while the template reads
    /// the clock, with its random offset fixed while it lasts.
    fn time_listener(&mut self, sh: &Arc<Shared>) {
        if !self.info.read.has_time {
            self.minute = None;
            return;
        }
        if self.minute.is_none() {
            // HA's tracker: a random 50-500 ms past the second, fixed for
            // its life.
            let micro = rand::random_range(50_000..=500_000);
            self.minute = Some((time::next_minute(sh.hass.clock().now(), micro), micro));
        }
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn wall_sleep(shared: &Weak<Shared>, at: Option<(jiff::Timestamp, i64)>) {
    let (Some((at, _)), Some(hass)) = (at, shared.upgrade().map(|s| s.hass.clone())) else {
        return std::future::pending().await;
    };
    time::sleep_until_wall(&hass, at).await;
}

/// A `numeric_state` trigger with a `value_template`: each change of its
/// entities is checked on a render thread, in order.
pub(crate) struct NumericTemplate {
    pub(crate) trigger: Trigger,
    pub(crate) entity_ids: Vec<String>,
    pub(crate) below: Option<f64>,
    pub(crate) above: Option<f64>,
    pub(crate) template: Tpl,
    pub(crate) name: String,
    pub(crate) vars: Arc<Vars>,
    pub(crate) templated: bool,
}

impl NumericTemplate {
    pub(crate) fn spawn(
        self,
        shared: &Arc<Shared>,
        sink: Sink,
        since: u64,
    ) -> (Vec<Listening>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let listening = self
            .entity_ids
            .iter()
            .map(|e| shared.listeners.listen(e, since, forward(shared, &tx)))
            .collect();
        let task = tokio::spawn(self.run(Arc::downgrade(shared), rx, sink, shared.turn()));
        (listening, task)
    }

    /// The variables the template renders with for an entity
    /// (`variables(entity_id)`).
    fn vars_for(&self, entity_id: &str) -> Vars {
        let mut vars = (*self.vars).clone();
        let float = |f: Option<f64>| f.map_or_else(Var::none, Var::float);
        vars.insert(
            "trigger".into(),
            Var::dict([
                ("platform".to_owned(), Var::str("numeric_state")),
                ("entity_id".to_owned(), Var::str(entity_id)),
                ("below".to_owned(), float(self.below)),
                ("above".to_owned(), float(self.above)),
                ("attribute".to_owned(), Var::none()),
            ]),
        );
        vars
    }

    async fn run(
        self,
        shared: Weak<Shared>,
        mut rx: mpsc::UnboundedReceiver<Msg>,
        sink: Sink,
        (before, turn): crate::Turn,
    ) {
        if let Some(before) = before {
            let _ = before.await;
        }
        let me = Arc::new(self);
        let Some(sh) = shared.upgrade() else { return };
        let (hass, home) = (sh.hass.clone(), sh.home());
        drop(sh);
        // Armed if it doesn't match when attached; an error leaves it not.
        let job = me.clone();
        let (h, hm) = (hass.clone(), home.clone());
        let Some((checks, mut out)) = ha_core::template::run::run(move || {
            let mut out = Out::default();
            let checks: Vec<(String, Result<bool, CondError>)> = job
                .entity_ids
                .iter()
                .map(|e| {
                    let vars = job.vars_for(e);
                    let r = cond::numeric(
                        &h,
                        e,
                        job.below,
                        job.above,
                        Some((&job.template, &hm, &vars, &mut out)),
                    );
                    (e.clone(), r)
                })
                .collect();
            (checks, out)
        })
        .await
        else {
            return;
        };
        out.emit();
        let mut armed = BTreeSet::new();
        for (e, r) in checks {
            match r {
                Ok(false) => {
                    armed.insert(e);
                }
                Ok(true) => {}
                Err(err) => tracing::warn!(
                    logger = "homeassistant.components.homeassistant.triggers.numeric_state",
                    "Error initializing '{}' trigger: {err}",
                    me.name
                ),
            }
        }
        // Armed: the next attachment may render.
        drop(turn);
        while let Some((_, ev, guard)) = rx.recv().await {
            let Some((_, Some(new))) = change(&ev) else {
                continue;
            };
            let new = new.clone();
            let entity = new.entity_id.as_str().to_owned();
            let (job, hm) = (me.clone(), home.clone());
            let state = new.clone();
            let e2 = entity.clone();
            let Some((r, mut out)) = ha_core::template::run::run(move || {
                let mut out = Out::default();
                let vars = job.vars_for(&e2);
                let r = cond::numeric_of(
                    &state,
                    job.below,
                    job.above,
                    Some((&job.template, &hm, &vars, &mut out)),
                );
                (r, out)
            })
            .await
            else {
                return;
            };
            out.emit();
            match r {
                Err(err) => tracing::warn!(
                    logger = "homeassistant.components.homeassistant.triggers.numeric_state",
                    "Error in '{}' trigger: {err}",
                    me.name
                ),
                Ok(false) => {
                    armed.insert(entity);
                }
                Ok(true) => {
                    if armed.remove(&entity) {
                        let old = change(&ev).and_then(|(o, _)| o.cloned());
                        let vars = crate::numeric_vars(
                            &me.trigger,
                            &entity,
                            me.below,
                            me.above,
                            old.as_ref(),
                            &new,
                            me.templated,
                        );
                        sink(vars, Some(new.context.clone()));
                    }
                }
            }
            drop(guard);
        }
    }
}
