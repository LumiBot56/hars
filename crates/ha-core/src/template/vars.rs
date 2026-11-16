//! Template variables made in Rust: the Python values HA's scripts hand
//! their templates (`trigger`, `this`, `wait`, rendered `variables`), and
//! back again to JSON for a service call's data.
//!
//! A [`Var`] is a template value, cheap to clone (it shares what it holds)
//! and safe to send to a render thread. What HA passes as Python objects
//! keeps its type: a state is HA's `State` (printed as `State` prints,
//! never collected into a render's `RenderInfo`), `trigger.for` a
//! `timedelta`, `trigger.now` a local `datetime`, and a rendered result
//! the value `_parse_result` made of it (a tuple stays a tuple).
//!
//! Deliberate differences: a dict's keys iterate sorted (HA: in the order
//! they were added), and a rendered dict's keys that aren't strings become
//! their Python `str()` (HA keeps them as they are).

use super::literal::{Int, Parsed, Py};
use super::{Dict, Rendered, home, pytime};
use crate::State;
use minijinja::value::{Tuple, Value, ValueKind};
use serde_json::Map;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// One template variable.
#[derive(Clone)]
pub struct Var(pub(super) Value);

/// A render's variables, by name.
pub type Vars = BTreeMap<String, Var>;

impl fmt::Debug for Var {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Var({:?})", self.0.kind())
    }
}

impl Var {
    /// `None`.
    pub fn none() -> Var {
        Var(Value::from(()))
    }

    pub fn str(s: &str) -> Var {
        Var(Value::from(s))
    }

    pub fn bool(b: bool) -> Var {
        Var(Value::from(b))
    }

    pub fn int(i: i64) -> Var {
        Var(Value::from(i))
    }

    pub fn float(f: f64) -> Var {
        Var(Value::from(f))
    }

    /// A JSON value as the Python value it decodes to.
    pub fn json(v: &serde_json::Value) -> Var {
        Var(super::convert(v))
    }

    /// HA's `State`.
    pub fn state(state: Arc<State>) -> Var {
        Var(home::TemplateState::raw(state))
    }

    /// HA's `State`, or `None`.
    pub fn opt_state(state: Option<&Arc<State>>) -> Var {
        state.map_or_else(Var::none, |s| Var::state(s.clone()))
    }

    /// A `timedelta`.
    pub fn timedelta(d: std::time::Duration) -> Var {
        Var(pytime::TimeDelta::value(d.as_micros() as i128).unwrap_or_else(|_| Value::from(())))
    }

    /// An aware `datetime` at `t`, in the time zone named (`UTC` if it
    /// isn't known).
    pub fn datetime(t: jiff::Timestamp, time_zone: &str) -> Var {
        let tz = match jiff::tz::TimeZone::get(time_zone) {
            Ok(tz) => pytime::Tz::Zone(tz, time_zone.into()),
            Err(_) => pytime::Tz::Utc,
        };
        Var(pytime::DateTime::at(t.as_microsecond(), tz)
            .map_or_else(|_| Value::from(()), pytime::DateTime::value))
    }

    /// A `dict`.
    pub fn dict(entries: impl IntoIterator<Item = (String, Var)>) -> Var {
        Var(Value::from_object(Dict {
            entries: entries.into_iter().map(|(k, v)| (k, v.0)).collect(),
            root: false,
            type_name: "dict object",
        }))
    }

    /// A `list`.
    pub fn list(items: Vec<Var>) -> Var {
        Var(Value::from(
            items.into_iter().map(|v| v.0).collect::<Vec<_>>(),
        ))
    }

    /// What a template rendered: the string, or the value `_parse_result`
    /// made of it.
    pub fn rendered(r: &Rendered) -> Var {
        match &r.0 {
            Parsed::Text(s) => Var::str(s),
            Parsed::Value(v) => Var(py_value(v)),
        }
    }

    /// Whether it is a `dict`.
    pub fn is_dict(&self) -> bool {
        self.0.kind() == ValueKind::Map
    }

    /// Whether it is `None`.
    pub fn is_none(&self) -> bool {
        self.0.is_none()
    }

    /// A dict's entries (`None` for anything else).
    pub fn entries(&self) -> Option<Vec<(String, Var)>> {
        if !self.is_dict() {
            return None;
        }
        let mut out = Vec::new();
        for k in self.0.try_iter().ok()? {
            let v = self.0.get_item(&k).unwrap_or_default();
            out.push((py_str(&k), Var(v)));
        }
        Some(out)
    }

    /// A dict's value at `key`.
    pub fn get(&self, key: &str) -> Option<Var> {
        if !self.is_dict() {
            return None;
        }
        self.0
            .get_attr(key)
            .ok()
            .filter(|v| !v.is_undefined())
            .map(Var)
    }

    /// The string, if it is one.
    pub fn as_str(&self) -> Option<&str> {
        (self.0.kind() == ValueKind::String)
            .then(|| self.0.as_str())
            .flatten()
    }

    /// The number, if it is one (`bool` counts, as in Python).
    pub fn as_f64(&self) -> Option<f64> {
        match self.0.kind() {
            ValueKind::Bool => Some(if self.0.is_true() { 1.0 } else { 0.0 }),
            ValueKind::Number => f64::try_from(self.0.clone())
                .ok()
                .or_else(|| i128::try_from(self.0.clone()).ok().map(|i| i as f64)),
            _ => None,
        }
    }

    /// Python's `str()` of it.
    pub fn to_py_str(&self) -> String {
        py_str(&self.0)
    }

    /// As JSON, for a service call's data: tuples and sets as lists, as
    /// HA's JSON encoder writes them; integers past 64 bits as floats; any
    /// other object as its `str()`.
    pub fn to_json(&self) -> serde_json::Value {
        to_json(&self.0, 0)
    }
}

/// `str()` of a scalar (a dict key, a value HA's scripts made); objects
/// print as they do in a template.
fn py_str(v: &Value) -> String {
    match v.kind() {
        ValueKind::String => v.as_str().unwrap_or_default().to_owned(),
        ValueKind::Undefined | ValueKind::None => "None".into(),
        ValueKind::Bool if v.is_true() => "True".into(),
        ValueKind::Bool => "False".into(),
        ValueKind::Number if !v.is_integer() => {
            f64::try_from(v.clone()).map_or_else(|_| v.to_string(), crate::py::repr_float)
        }
        ValueKind::Seq | ValueKind::Map
            if v.as_object()
                .is_none_or(|o| o.downcast_ref::<home::TemplateState>().is_none()) =>
        {
            let mut s = String::new();
            home::json_repr(&mut s, &to_json(v, 0));
            s
        }
        _ => v.to_string(),
    }
}

fn to_json(v: &Value, depth: usize) -> serde_json::Value {
    use serde_json::Value as J;
    if depth > 100 {
        return J::Null;
    }
    match v.kind() {
        ValueKind::Undefined | ValueKind::None => J::Null,
        ValueKind::Bool => J::Bool(v.is_true()),
        ValueKind::Number if v.is_integer() => match i128::try_from(v.clone()) {
            Ok(i) => i64::try_from(i)
                .map(J::from)
                .or_else(|_| u64::try_from(i).map(J::from))
                .unwrap_or_else(|_| float_json(i as f64)),
            Err(_) => J::Null,
        },
        ValueKind::Number => float_json(f64::try_from(v.clone()).unwrap_or(f64::NAN)),
        ValueKind::String => J::String(v.as_str().unwrap_or_default().to_owned()),
        ValueKind::Seq | ValueKind::Iterable => J::Array(
            v.try_iter()
                .map(|items| items.map(|i| to_json(&i, depth + 1)).collect())
                .unwrap_or_default(),
        ),
        ValueKind::Map => {
            let mut m = Map::new();
            if let Ok(keys) = v.try_iter() {
                for k in keys {
                    let item = v.get_item(&k).unwrap_or_default();
                    m.insert(py_str(&k), to_json(&item, depth + 1));
                }
            }
            J::Object(m)
        }
        _ => J::String(py_str(v)),
    }
}

/// orjson writes NaN and the infinities as `null`.
fn float_json(f: f64) -> serde_json::Value {
    serde_json::Number::from_f64(f).map_or(serde_json::Value::Null, serde_json::Value::Number)
}

/// A literal `_parse_result` made, as a template value.
fn py_value(v: &Py) -> Value {
    match v {
        Py::None | Py::Ellipsis | Py::Int(Int::Huge) | Py::Str(None) => Value::from(()),
        Py::Bool(b) => Value::from(*b),
        Py::Int(Int::Small(i)) => Value::from(*i),
        Py::Float(f) => Value::from(*f),
        // `_parse_result` keeps complex numbers as text: never here.
        Py::Complex(re, _) => Value::from(*re),
        Py::Str(Some(s)) => Value::from(s.as_str()),
        Py::Bytes(b) => Value::from_bytes(b.clone()),
        Py::List(items) | Py::Set(items) => {
            Value::from(items.iter().map(py_value).collect::<Vec<_>>())
        }
        Py::Tuple(items) => {
            Value::from(Tuple::from(items.iter().map(py_value).collect::<Vec<_>>()))
        }
        Py::Dict(pairs) => Value::from_object(Dict {
            entries: pairs
                .iter()
                .map(|(k, v)| (py_str(&py_value(k)), py_value(v)))
                .collect::<BTreeMap<_, _>>(),
            root: false,
            type_name: "dict object",
        }),
    }
}
