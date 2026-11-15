//! What a template reads from the running instance: `states` and the
//! functions on it (`helpers/template/states.py`, `extensions/state.py`),
//! the time zone, and HA's `RenderInfo`: which entities, domains and
//! whether all states or the time were read, which `render_template`
//! reports as `listeners`.
//!
//! A render runs on its own thread (never the async runtime), so what it
//! reads is held in a thread local for the length of one render: the
//! [`Home`], the time zone, what was read, and a sorted snapshot of the
//! state machine taken the first time a template iterates or counts states
//! (one `Arc` per state, no copies). Single lookups read the state machine
//! itself. A state's attributes are never copied as a whole: each value is
//! copied when it is read, and paid for (its size) before it is.
//!
//! Deliberate differences: iterating `states` (or a domain) goes in
//! entity id order (HA: the order entities were added); `context` reads
//! only `id`, `user_id` and `parent_id`; `as_dict` and the other methods of
//! HA's `State` aren't served (an error, not a value).

use super::Charged;
use super::jinja::{py_eq, py_error, type_name};
use crate::{EntityId, Hass, State as HaState};
use minijinja::value::{BinOp, Enumerator, Object, ObjectExt, ObjectRepr, Value, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State};
use smol_str::SmolStr;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

/// What templates read: the instance, and the entity registry's sensor
/// display precision (for `states(…, rounded=True)` and
/// `state_with_unit`), which `ha-core` can't see itself.
#[derive(Clone)]
pub struct Home {
    pub hass: Hass,
    pub display_precision: Option<DisplayPrecision>,
}

/// An entity's `sensor` display precision (`display_precision`, else
/// `suggested_display_precision`), from the entity registry.
pub type DisplayPrecision = Arc<dyn Fn(&str) -> Option<i64> + Send + Sync>;

impl Home {
    pub fn new(hass: Hass) -> Home {
        Home {
            hass,
            display_precision: None,
        }
    }
}

/// HA's `RenderInfo`, as far as `render_template` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderInfo {
    /// Every state was iterated (`all_states`) or counted
    /// (`all_states_lifecycle`).
    pub all_states: bool,
    /// Entities read, as asked for (`states('LIGHT.X')` collects
    /// `light.x` if it exists, else `LIGHT.X`).
    pub entities: BTreeSet<SmolStr>,
    /// Domains iterated or counted.
    pub domains: BTreeSet<SmolStr>,
    /// `now()`, `utcnow()`, `today_at()` or a relative time was used.
    pub has_time: bool,
}

impl RenderInfo {
    /// `_render_infos_to_track_states` with `has_time`: the `listeners` of
    /// a `render_template` event.
    pub fn listeners(&self) -> serde_json::Value {
        if self.all_states {
            return serde_json::json!({
                "all": true, "entities": [], "domains": [], "time": self.has_time,
            });
        }
        serde_json::json!({
            "all": false,
            "entities": self.entities.iter().map(SmolStr::as_str).collect::<Vec<_>>(),
            "domains": self.domains.iter().map(SmolStr::as_str).collect::<Vec<_>>(),
            "time": self.has_time,
        })
    }
}

/// One render's view of the instance.
#[derive(Default)]
struct Ctx {
    home: Option<Home>,
    tz: Option<super::pytime::Tz>,
    source: Arc<str>,
    info: RenderInfo,
    /// The first error raised where minijinja can't return one (an
    /// attribute lookup, a comparison), and the log's length then.
    poison: Option<(String, usize)>,
    snapshot: Option<Arc<[Arc<HaState>]>>,
}

thread_local! {
    static CTX: RefCell<Ctx> = RefCell::default();
}

/// Starts a render on this thread.
pub(super) fn begin(home: Option<&Home>, source: &Arc<str>) {
    let tz = home.map(|h| {
        let name = h.hass.config().time_zone.clone();
        match jiff::tz::TimeZone::get(&name) {
            Ok(tz) => super::pytime::Tz::Zone(tz, name.into()),
            Err(_) => super::pytime::Tz::Utc,
        }
    });
    CTX.with_borrow_mut(|c| {
        *c = Ctx {
            home: home.cloned(),
            tz,
            source: source.clone(),
            ..Ctx::default()
        }
    });
}

/// Ends a render: what it read, and the error it raised where minijinja
/// couldn't (with the log's length when it did).
pub(super) fn end() -> (RenderInfo, Option<(String, usize)>) {
    CTX.with_borrow_mut(|c| {
        let c = std::mem::take(c);
        (c.info, c.poison)
    })
}

/// Records an error raised where minijinja can't return one: the render
/// fails with it (the first one) when it ends.
pub(super) fn poison(message: String) {
    let logged = super::SCRATCH.with_borrow(|s| s.log.len());
    CTX.with_borrow_mut(|c| {
        if c.poison.is_none() {
            c.poison = Some((message, logged));
        }
    });
}

/// `DEFAULT_TIME_ZONE`: the configured zone, or `timezone.utc` with no
/// instance (as HA's default).
pub(super) fn local_tz() -> super::pytime::Tz {
    CTX.with_borrow(|c| c.tz.clone())
        .unwrap_or(super::pytime::Tz::Utc)
}

/// The template being rendered (for HA's messages).
pub(super) fn source() -> Arc<str> {
    CTX.with_borrow(|c| c.source.clone())
}

/// The instance, or HA's answer without one: these functions aren't
/// there.
pub(super) fn hass(name: &str) -> Result<Hass, Error> {
    CTX.with_borrow(|c| c.home.as_ref().map(|h| h.hass.clone()))
        .ok_or_else(|| super::unsupported_error("function", name))
}

pub(super) fn note_time() {
    CTX.with_borrow_mut(|c| c.info.has_time = true);
}

fn collect_entity(entity_id: &str) {
    CTX.with_borrow_mut(|c| {
        if !c.info.entities.contains(entity_id) {
            c.info.entities.insert(SmolStr::new(entity_id));
        }
    });
}

fn collect_domain(domain: &str) {
    CTX.with_borrow_mut(|c| {
        if !c.info.domains.contains(domain) {
            c.info.domains.insert(SmolStr::new(domain));
        }
    });
}

fn collect_all() {
    CTX.with_borrow_mut(|c| c.info.all_states = true);
}

/// Every state, sorted by entity id, taken once per render.
fn snapshot() -> Arc<[Arc<HaState>]> {
    if let Some(s) = CTX.with_borrow(|c| c.snapshot.clone()) {
        return s;
    }
    let Ok(hass) = hass("states") else {
        poison(super::unsupported("function", "states").0);
        return Arc::from(Vec::new());
    };
    let mut all = hass.states().all();
    all.sort_unstable_by(|a, b| a.entity_id.as_str().cmp(b.entity_id.as_str()));
    let all: Arc<[Arc<HaState>]> = all.into();
    CTX.with_borrow_mut(|c| c.snapshot = Some(all.clone()));
    all
}

/// The states of one domain: a contiguous run of the sorted snapshot
/// (ids from `domain.` up to `domain/`, the next byte), found by bisection.
fn domain_states(domain: &str) -> (Arc<[Arc<HaState>]>, std::ops::Range<usize>) {
    let all = snapshot();
    let (from, to) = (format!("{domain}."), format!("{domain}/"));
    let start = all.partition_point(|s| s.entity_id.as_str() < from.as_str());
    let end = all.partition_point(|s| s.entity_id.as_str() < to.as_str());
    (all, start..end.max(start))
}

/// Template states for a run of the snapshot, made as they are read: a
/// loop that stops early makes no more.
fn states_iter(all: Arc<[Arc<HaState>]>, run: std::ops::Range<usize>) -> Enumerator {
    Enumerator::Iter(Box::new(
        run.map(move |i| TemplateState::value(all[i].clone(), false)),
    ))
}

/// `hass.states.get(entity_id)`: as given, then lower-cased.
fn get_state(hass: &Hass, entity_id: &str) -> Option<Arc<HaState>> {
    let find = |id: &str| EntityId::parse(id).ok().and_then(|e| hass.states().get(&e));
    find(entity_id).or_else(|| {
        let lower = entity_id.to_lowercase();
        (lower != entity_id).then(|| find(&lower)).flatten()
    })
}

/// `_get_state`: the state, collecting the id when there is none.
fn lookup(hass: &Hass, entity_id: &str) -> Option<Arc<HaState>> {
    let state = get_state(hass, entity_id);
    if state.is_none() {
        collect_entity(entity_id);
    }
    state
}

/// A state's attributes (`ReadOnlyDict`), sharing the state: a value is
/// made from the state's JSON when it is read, paid for first.
#[derive(Debug)]
pub(super) struct Attributes(Arc<HaState>);

const READ_ONLY_DICT: &str = "homeassistant.util.read_only_dict.ReadOnlyDict object";

/// The fuel a copy of `v` costs: one per value and per 64 bytes of
/// strings, as the engine charges what it makes. Stops counting past a
/// render's whole budget.
fn json_cost(v: &serde_json::Value) -> u64 {
    let mut cost = 0u64;
    let mut pending = vec![v];
    while let Some(v) = pending.pop() {
        cost += 1;
        if cost > super::FUEL {
            break;
        }
        match v {
            serde_json::Value::String(s) => cost += s.len() as u64 / 64,
            serde_json::Value::Array(items) => pending.extend(items),
            serde_json::Value::Object(m) => {
                for (k, item) in m {
                    cost += k.len() as u64 / 64;
                    pending.push(item);
                }
            }
            _ => {}
        }
    }
    cost
}

impl Attributes {
    /// The value at `key`, if there is one (and its cost).
    fn find(&self, key: &Value) -> Option<&serde_json::Value> {
        key.as_str()
            .filter(|_| key.kind() == ValueKind::String)
            .and_then(|k| self.0.attributes.get(k))
    }

    fn missing(key: &Value) {
        let mut message = String::new();
        match key.as_str().filter(|_| key.kind() == ValueKind::String) {
            Some(name) => {
                message.push('\'');
                message.push_str(READ_ONLY_DICT);
                message.push_str("' has no attribute ");
                crate::py::repr_str(&mut message, name);
            }
            None => {
                message.push_str(READ_ONLY_DICT);
                message.push_str(" has no element ");
                message.push_str(&key.to_string());
            }
        }
        super::note_miss(message);
    }
}

impl Object for Attributes {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        match self.find(key) {
            // Paid to the running render before the copy, however it is
            // read (`x.y`, a filter's `attribute=`, a comparison); past the
            // budget nothing is copied and the render stops.
            Some(v) => match minijinja::value::consume_fuel_now(json_cost(v)) {
                Ok(()) => Some(super::convert(v)),
                Err(_) => {
                    poison(super::OUT_OF_BUDGET.clone());
                    None
                }
            },
            None => {
                Attributes::missing(key);
                None
            }
        }
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        self.mapped_enumerator(|this| {
            Box::new(this.0.attributes.keys().map(|k| Value::from(k.as_str())))
        })
    }

    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        Some(self.0.attributes.len())
    }
}

/// `valid_domain`.
fn valid_domain(s: &str) -> bool {
    let s = s.strip_suffix('\n').unwrap_or(s);
    !s.is_empty()
        && !s.starts_with('_')
        && !s.ends_with('_')
        && !s.contains("__")
        && s.chars().all(|c| {
            c.is_ascii_lowercase() || c == '_' || crate::entity_id::decimal_value(c).is_some()
        })
}

/// The id a function was given: Python's `entity_id.lower()` fails on
/// anything but a string.
fn entity_arg(state: &State, v: &Value) -> Result<String, Error> {
    match v.kind() {
        ValueKind::String => {
            state.consume_fuel(v.as_str().map_or(0, str::len) as u64 / 64)?;
            Ok(v.as_str().unwrap_or_default().to_owned())
        }
        // `LoggingUndefined.__getattr__('lower')`.
        ValueKind::Undefined => Err(Error::from(ErrorKind::UndefinedError)),
        _ => Err(py_error(format!(
            "AttributeError: '{}' object has no attribute 'lower'",
            type_name(v)
        ))),
    }
}

// --- the objects ---------------------------------------------------------

/// `AllStates`: `states`.
#[derive(Debug)]
pub(super) struct AllStates;

/// Names Jinja asks objects for that HA answers with `None`.
const RESERVED: &[&str] = &[
    "contextfunction",
    "evalcontextfunction",
    "environmentfunction",
    "jinja_pass_arg",
];

impl Object for AllStates {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Iterable
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let Some(name) = key.as_str() else {
            poison(format!(
                "TypeError: argument of type '{}' is not iterable",
                type_name(key)
            ));
            return None;
        };
        if name.contains('.') {
            return state_if_valid(name);
        }
        if RESERVED.contains(&name) {
            return Some(Value::from(()));
        }
        if !valid_domain(name) {
            poison(format!("TemplateError: Invalid domain name '{name}'"));
            return None;
        }
        Some(Value::from_object(DomainStates(SmolStr::new(name))))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        collect_all();
        let all = snapshot();
        let len = all.len();
        states_iter(all, 0..len)
    }

    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        collect_all();
        Some(snapshot().len())
    }

    fn call(self: &Arc<Self>, state: &mut State<'_, '_>, args: &[Value]) -> Result<Value, Error> {
        states_call(state, args)
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<template AllStates>")
    }
}

/// `_get_state_if_valid`.
fn state_if_valid(entity_id: &str) -> Option<Value> {
    let Ok(hass) = hass("states") else {
        poison(super::unsupported("function", "states").0);
        return None;
    };
    match lookup(&hass, entity_id) {
        Some(s) => Some(TemplateState::value(s, true)),
        None if !crate::valid_entity_id(entity_id) => {
            poison(format!("TemplateError: Invalid entity ID '{entity_id}'"));
            None
        }
        None => Some(Value::from(())),
    }
}

/// `DomainStates`: `states.light`.
#[derive(Debug)]
pub(super) struct DomainStates(SmolStr);

impl Object for DomainStates {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Iterable
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let mut id = String::from(self.0.as_str());
        id.push('.');
        match key.as_str() {
            Some(name) => id.push_str(name),
            None => id.push_str(&key.to_string()),
        }
        state_if_valid(&id)
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        collect_domain(&self.0);
        let (all, run) = domain_states(&self.0);
        states_iter(all, run)
    }

    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        collect_domain(&self.0);
        Some(domain_states(&self.0).1.len())
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<template DomainStates('{}')>", self.0)
    }
}

/// `TemplateState`: one state. Reading it collects its entity, unless it
/// came from iterating (`collect=False`).
#[derive(Debug)]
pub(super) struct TemplateState {
    state: Arc<HaState>,
    collect: bool,
}

/// `State` names that aren't served.
const STATE_METHODS: &[&str] = &[
    "as_compressed_state",
    "as_compressed_state_json",
    "as_dict",
    "as_dict_json",
    "expire",
    "from_dict",
    "json_fragment",
];

impl TemplateState {
    fn value(state: Arc<HaState>, collect: bool) -> Value {
        Value::from_object(TemplateState { state, collect })
    }

    fn collect(&self) {
        if self.collect {
            collect_entity(self.state.entity_id.as_str());
        }
    }

    /// `State.name`.
    fn name(&self) -> String {
        match self.state.attributes.get("friendly_name") {
            Some(n) if py_truthy(n) => {
                let mut s = String::new();
                json_str(&mut s, n);
                s
            }
            _ => self.state.entity_id.object_id().replace('_', " "),
        }
    }

    fn timestamp(t: jiff::Timestamp) -> Value {
        utc_datetime(t)
    }
}

/// A UTC `datetime` (states' timestamps).
pub(super) fn utc_datetime(t: jiff::Timestamp) -> Value {
    match super::pytime::DateTime::at(t.as_microsecond(), super::pytime::Tz::Utc) {
        Ok(dt) => dt.value(),
        Err(_) => Value::from(()),
    }
}

/// Python's truth of a JSON value.
fn py_truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64() != Some(0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

/// `str()` of a JSON value.
fn json_str(out: &mut String, v: &serde_json::Value) {
    match v {
        serde_json::Value::String(s) => out.push_str(s),
        other => json_repr(out, other),
    }
}

/// `repr()` of a JSON value, as the Python value HA would hold.
pub(super) fn json_repr(out: &mut String, v: &serde_json::Value) {
    match v {
        serde_json::Value::Null => out.push_str("None"),
        serde_json::Value::Bool(true) => out.push_str("True"),
        serde_json::Value::Bool(false) => out.push_str("False"),
        serde_json::Value::Number(n) => match n.as_f64() {
            Some(f) if !(n.is_i64() || n.is_u64()) => out.push_str(&crate::py::repr_float(f)),
            _ => out.push_str(&n.to_string()),
        },
        serde_json::Value::String(s) => crate::py::repr_str(out, s),
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                json_repr(out, item);
            }
            out.push(']');
        }
        serde_json::Value::Object(m) => {
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                crate::py::repr_str(out, k);
                out.push_str(": ");
                json_repr(out, item);
            }
            out.push('}');
        }
    }
}

impl Object for TemplateState {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let s = &self.state;
        let Some(name) = key.as_str() else {
            return state_no_attribute(key);
        };
        let v = match name {
            "entity_id" => return Some(Value::from(s.entity_id.as_str())),
            "state" => Value::from(s.state.as_str()),
            "attributes" => Value::from_object(Attributes(s.clone())),
            "last_changed" => TemplateState::timestamp(s.last_changed),
            "last_updated" => TemplateState::timestamp(s.last_updated),
            "last_reported" => TemplateState::timestamp(s.last_reported.get()),
            "last_changed_timestamp" => Value::from(micros_to_secs(s.last_changed)),
            "last_reported_timestamp" => Value::from(micros_to_secs(s.last_reported.get())),
            "context" => Value::from_object(Context(s.clone())),
            "domain" => Value::from(s.entity_id.domain()),
            "object_id" => Value::from(s.entity_id.object_id()),
            "name" => Value::from(self.name()),
            "state_with_unit" => Value::from(format_state(s, true, true)),
            _ if STATE_METHODS.contains(&name) => {
                poison(format!(
                    "NotImplementedError: hars does not support the state's '{name}' in templates yet"
                ));
                return None;
            }
            _ if name.starts_with('_') => {
                super::note_miss(format!(
                    "access to attribute '{name}' of 'TemplateState' object is unsafe."
                ));
                return None;
            }
            _ => return state_no_attribute(key),
        };
        self.collect();
        Some(v)
    }

    /// `TemplateStateBase.__eq__` reads the state (collecting it) and asks
    /// `State`, which has no `__eq__`: Python falls back to identity, and
    /// HA hands out one cached object per state (one for iterating, one
    /// for lookups).
    fn binop(
        self: &Arc<Self>,
        op: BinOp,
        other: &Value,
        _reflected: bool,
    ) -> Option<Result<Value, Error>> {
        if !matches!(op, BinOp::Eq | BinOp::Ne | BinOp::EqItem) {
            return None;
        }
        let other_state = other.downcast_object_ref::<TemplateState>();
        let same = other_state
            .is_some_and(|o| Arc::ptr_eq(&o.state, &self.state) && o.collect == self.collect);
        // In a container or `in`, Python finds the same object without
        // asking it (and so without reading the state).
        if op == BinOp::EqItem && same {
            return Some(Ok(Value::from(true)));
        }
        self.collect();
        if let Some(o) = other_state {
            o.collect();
        }
        Some(Ok(Value::from(same == (op != BinOp::Ne))))
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        state_repr(&mut s, &self.state);
        f.write_str(&s)
    }
}

fn state_no_attribute(key: &Value) -> Option<Value> {
    let mut message = String::from(
        "'homeassistant.helpers.template.states.TemplateState object' has no attribute ",
    );
    match key.as_str() {
        Some(k) => crate::py::repr_str(&mut message, k),
        None => message.push_str(&key.to_string()),
    }
    super::note_miss(message);
    None
}

fn micros_to_secs(t: jiff::Timestamp) -> f64 {
    super::pytime::int_true_div(i128::from(t.as_microsecond()), 1_000_000)
}

/// `TemplateState.__repr__` with `State.__repr__` inside it.
fn state_repr(out: &mut String, s: &HaState) {
    out.push_str("<template TemplateState(<state ");
    out.push_str(s.entity_id.as_str());
    out.push('=');
    out.push_str(&s.state);
    if !s.attributes.is_empty() {
        out.push_str("; ");
        repr_helper_map(out, &s.attributes);
    }
    out.push_str(" @ ");
    let local = super::pytime::DateTime::at(s.last_changed.as_microsecond(), local_tz());
    if let Ok(dt) = local {
        dt.iso(out, 'T');
    }
    out.push_str(">)>");
}

/// `util.repr_helper` of a mapping.
fn repr_helper_map(out: &mut String, m: &serde_json::Map<String, serde_json::Value>) {
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(k);
        out.push('=');
        match v {
            serde_json::Value::Object(inner) => repr_helper_map(out, inner),
            other => json_str(out, other),
        }
    }
}

/// `State.context`: its ids.
#[derive(Debug)]
pub(super) struct Context(Arc<HaState>);

impl Object for Context {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let c = &self.0.context;
        Some(match key.as_str()? {
            "id" => Value::from(c.id.to_string()),
            "user_id" => c.user_id.as_deref().map_or(Value::from(()), Value::from),
            "parent_id" => c
                .parent_id
                .map_or(Value::from(()), |p| Value::from(p.to_string())),
            _ => return None,
        })
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<homeassistant.core.Context object>")
    }
}

/// `str()`/`repr()` of these objects, paid for as it is written.
pub(super) fn py_repr<W: fmt::Write>(
    out: &mut Charged<'_, W>,
    v: &Value,
) -> Option<Result<(), Error>> {
    let o = v.as_object()?;
    if o.downcast_ref::<AllStates>().is_some() {
        return Some(out.put("<template AllStates>"));
    }
    if let Some(d) = o.downcast_ref::<DomainStates>() {
        return Some(out.put(&format!("<template DomainStates('{}')>", d.0)));
    }
    if let Some(t) = o.downcast_ref::<TemplateState>() {
        // Its size is the state's: paid before it is built.
        let size = t.state.attributes.values().map(json_cost).sum::<u64>()
            + t.state.state.len() as u64 / 64
            + 1;
        if let Err(e) = out.state.consume_fuel(size) {
            return Some(Err(e));
        }
        let mut s = String::new();
        state_repr(&mut s, &t.state);
        return Some(out.put(&s));
    }
    if o.downcast_ref::<Context>().is_some() {
        return Some(out.put("<homeassistant.core.Context object>"));
    }
    None
}

// --- formatting ----------------------------------------------------------

/// `TemplateStateBase.format_state`.
fn format_state(s: &HaState, rounded: bool, with_unit: bool) -> String {
    let mut out = if rounded && s.entity_id.domain() == "sensor" {
        rounded_state(s)
    } else {
        s.state.to_string()
    };
    if with_unit {
        if let Some(unit) = s.attributes.get("unit_of_measurement") {
            if py_truthy(unit) {
                out.push(' ');
                json_str(&mut out, unit);
            }
        }
    }
    out
}

/// `sensor.async_rounded_state`: `f"{float(state):z.{precision}f}"`.
fn rounded_state(s: &HaState) -> String {
    let precision = CTX.with_borrow(|c| {
        c.home
            .as_ref()
            .and_then(|h| h.display_precision.as_ref())
            .and_then(|p| p(s.entity_id.as_str()))
    });
    let (Some(p), Some(x)) = (precision, super::num::py_float(&s.state)) else {
        return s.state.to_string();
    };
    let Ok(p) = usize::try_from(p) else {
        return s.state.to_string();
    };
    super::num::format_fixed(x, p.min(1000), true)
}

// --- functions -----------------------------------------------------------

/// `AllStates.__call__(entity_id, rounded, with_unit)`.
fn states_call(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [entity_id, rounded, with_unit] = super::funcs::bind(
        "AllStates.__call__",
        ["entity_id", "rounded", "with_unit"],
        1,
        args,
    )?;
    let hass = hass("states")?;
    let entity_id = entity_arg(state, entity_id.as_ref().expect("required"))?;
    let Some(s) = lookup(&hass, &entity_id) else {
        return Ok(Value::from("unknown"));
    };
    collect_entity(s.entity_id.as_str());
    let with_unit = with_unit.is_some_and(|v| v.is_true());
    let rounded = rounded.map_or(with_unit, |v| v.is_true());
    if rounded || with_unit {
        return Ok(Value::from(format_state(&s, rounded, with_unit)));
    }
    Ok(Value::from(s.state.as_str()))
}

/// The attribute `name` of a state, if it has it (`attributes.get`),
/// paid for before it is copied.
fn attribute(state: &State, s: &Arc<HaState>, name: &Value) -> Result<Option<Value>, Error> {
    match name.kind() {
        ValueKind::String => match s.attributes.get(name.as_str().unwrap_or_default()) {
            Some(v) => {
                state.consume_fuel(json_cost(v))?;
                Ok(Some(super::convert(v)))
            }
            None => Ok(None),
        },
        ValueKind::Seq | ValueKind::Map => Err(py_error(format!(
            "TypeError: unhashable type: '{}'",
            type_name(name)
        ))),
        _ => Ok(None),
    }
}

/// Registers `states`, `is_state`, `state_attr`, `is_state_attr` and
/// `has_value` as HA does (`StateExtension`).
pub(super) fn add(env: &mut Environment<'static>) {
    use minijinja::value::{Rest, ValueOrKwargs};
    fn values(args: Rest<ValueOrKwargs>) -> Vec<Value> {
        args.into_values()
    }
    env.add_global("states", Value::from_object(AllStates));
    env.add_filter("states", |state: &State, args: Rest<ValueOrKwargs>| {
        states_call(state, &values(args))
    });
    let is_state = |state: &State, args: Rest<ValueOrKwargs>| -> Result<bool, Error> {
        let [entity_id, want] = super::funcs::bind(
            "StateExtension.is_state",
            ["entity_id", "state"],
            2,
            &values(args),
        )?;
        let (entity_id, want) = (entity_id.expect("required"), want.expect("required"));
        let hass = hass("is_state")?;
        let Some(s) = lookup(&hass, &entity_arg(state, &entity_id)?) else {
            return Ok(false);
        };
        collect_entity(s.entity_id.as_str());
        let current = Value::from(s.state.as_str());
        if py_eq(state, &current, &want)? {
            return Ok(true);
        }
        // `isinstance(state, list) and state_obj.state in state`.
        if want.kind() == ValueKind::Seq && type_name(&want) == "list" {
            for item in want.try_iter()? {
                if py_eq(state, &item, &current)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    };
    env.add_function("is_state", is_state);
    env.add_test("is_state", is_state);
    let state_attr = |state: &State, args: Rest<ValueOrKwargs>| -> Result<Value, Error> {
        let [entity_id, name] = super::funcs::bind(
            "StateExtension.state_attr",
            ["entity_id", "name"],
            2,
            &values(args),
        )?;
        let (entity_id, name) = (entity_id.expect("required"), name.expect("required"));
        let hass = hass("state_attr")?;
        let Some(s) = lookup(&hass, &entity_arg(state, &entity_id)?) else {
            return Ok(Value::from(()));
        };
        collect_entity(s.entity_id.as_str());
        Ok(attribute(state, &s, &name)?.unwrap_or_else(|| Value::from(())))
    };
    env.add_function("state_attr", state_attr);
    env.add_filter("state_attr", state_attr);
    let is_state_attr = |state: &State, args: Rest<ValueOrKwargs>| -> Result<bool, Error> {
        let [entity_id, name, value] = super::funcs::bind(
            "StateExtension.is_state_attr",
            ["entity_id", "name", "value"],
            3,
            &values(args),
        )?;
        let (entity_id, name, value) = (
            entity_id.expect("required"),
            name.expect("required"),
            value.expect("required"),
        );
        let hass = hass("is_state_attr")?;
        let Some(s) = lookup(&hass, &entity_arg(state, &entity_id)?) else {
            return Ok(false);
        };
        collect_entity(s.entity_id.as_str());
        match attribute(state, &s, &name)? {
            Some(attr) => py_eq(state, &attr, &value),
            None => Ok(false),
        }
    };
    env.add_function("is_state_attr", is_state_attr);
    env.add_test("is_state_attr", is_state_attr);
    let has_value = |state: &State, args: Rest<ValueOrKwargs>| -> Result<bool, Error> {
        let [entity_id] =
            super::funcs::bind("StateExtension.has_value", ["entity_id"], 1, &values(args))?;
        let hass = hass("has_value")?;
        let Some(s) = lookup(&hass, &entity_arg(state, &entity_id.expect("required"))?) else {
            return Ok(false);
        };
        collect_entity(s.entity_id.as_str());
        Ok(!matches!(s.state.as_str(), "unavailable" | "unknown"))
    };
    env.add_function("has_value", has_value);
    env.add_filter("has_value", has_value);
    env.add_test("has_value", has_value);
}
