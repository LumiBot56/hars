//! HA's type casts, maths and time functions
//! (`helpers/template/extensions/type_cast.py`, `math.py`, `datetime.py`),
//! registered as globals, filters and tests as HA registers them.
//!
//! Each follows its Python source line by line: which exceptions it catches
//! (a `default` answers for those, else `raise_no_default`'s `ValueError`),
//! and which escape (`OverflowError`, `ZeroDivisionError`, an undefined
//! value's error). Arguments bind as Python binds them, by position or by
//! name.

use super::home;
use super::jinja::{py_error, quiet_undefined, text, type_name};
use super::num::{self, IntError};
use super::pytime::{self, Date, DateTime, Naive, TimeDelta, Tz, Unparsed};
use minijinja::value::{Rest, Value, ValueKind, ValueOrKwargs};
use minijinja::{Environment, Error, ErrorKind, State};
use std::cmp::Ordering;

/// Arguments as minijinja passes them: positional, then keywords.
fn values(args: Rest<ValueOrKwargs>) -> Vec<Value> {
    args.into_values()
}

/// Python's binding of `args` to parameters `names`, the first `required`
/// of them required: by position, then by keyword.
pub(super) fn bind<const N: usize>(
    qual: &str,
    names: [&str; N],
    required: usize,
    args: &[Value],
) -> Result<[Option<Value>; N], Error> {
    let (kwargs, positional) = pytime::split_kwargs(args);
    if positional.len() > N {
        let takes = if required == N {
            format!("{N}")
        } else {
            format!("from {required} to {N}")
        };
        let s = if N == 1 { "" } else { "s" };
        return Err(py_error(format!(
            "TypeError: {qual}() takes {takes} positional argument{s} but {} were given",
            positional.len()
        )));
    }
    let mut out: [Option<Value>; N] = std::array::from_fn(|_| None);
    for (slot, v) in out.iter_mut().zip(positional) {
        *slot = Some(v.clone());
    }
    for (k, v) in kwargs {
        let Some(i) = names.iter().position(|n| *n == k) else {
            return Err(py_error(format!(
                "TypeError: {qual}() got an unexpected keyword argument '{k}'"
            )));
        };
        if out[i].is_some() {
            return Err(py_error(format!(
                "TypeError: {qual}() got multiple values for argument '{k}'"
            )));
        }
        out[i] = Some(v);
    }
    let missing: Vec<&str> = (0..required)
        .filter(|i| out[*i].is_none())
        .map(|i| names[i])
        .collect();
    if !missing.is_empty() {
        let s = if missing.len() == 1 { "" } else { "s" };
        let list = missing
            .iter()
            .map(|n| format!("'{n}'"))
            .collect::<Vec<_>>()
            .join(" and ");
        return Err(py_error(format!(
            "TypeError: {qual}() missing {} required positional argument{s}: {list}",
            missing.len()
        )));
    }
    Ok(out)
}

/// `raise_no_default(function, value)`: HA's `ValueError`, with
/// `str(value)` (which, for an undefined value, warns or raises first).
fn raise_no_default(state: &State, function: &str, value: &Value) -> Error {
    let shown = match text_again(state, value) {
        Ok(s) => s,
        Err(e) => return e,
    };
    py_error(format!(
        "ValueError: Template error: {function} got invalid input '{shown}' when rendering template '{}' but no default was specified",
        home::source()
    ))
}

/// `str(value)`, which for an undefined value warns each time it is
/// printed (minijinja's printer warns once).
fn text_again(state: &State, value: &Value) -> Result<String, Error> {
    let miss = super::SCRATCH.with_borrow(|s| s.miss.clone());
    let shown = text(state, value);
    if value.is_undefined() {
        super::SCRATCH.with_borrow_mut(|s| s.miss = miss);
    }
    shown
}

/// The `default` if one was given, else `raise_no_default`.
fn or_default(
    state: &State,
    default: Option<Value>,
    function: &str,
    value: &Value,
) -> Result<Value, Error> {
    default.ok_or_else(|| raise_no_default(state, function, value))
}

/// `float(value)`: `None` for Python's `ValueError` or `TypeError`; an
/// undefined value raises (quietly: `Undefined.__float__`).
fn to_float(state: &State, v: &Value) -> Result<Option<f64>, Error> {
    Ok(match v.kind() {
        ValueKind::Undefined => return Err(quiet_undefined()),
        ValueKind::Bool => Some(if v.is_true() { 1.0 } else { 0.0 }),
        ValueKind::Number => f64::try_from(v.clone()).ok(),
        ValueKind::String => {
            let s = v.as_str().unwrap_or_default();
            state.consume_fuel(s.len() as u64 / 64)?;
            num::py_float(s)
        }
        _ => None,
    })
}

/// `int(x)` of a float.
fn int_of_float(x: f64) -> Result<Result<Value, ()>, Error> {
    if x.is_nan() {
        return Ok(Err(()));
    }
    if x.is_infinite() {
        return Err(py_error(
            "OverflowError: cannot convert float infinity to integer".into(),
        ));
    }
    let t = x.trunc();
    if t.abs() < 1.7e38 {
        return Ok(Ok(Value::from(t as i128)));
    }
    Err(py_error(
        "NotImplementedError: hars does not support integers this large in templates yet".into(),
    ))
}

/// Python's `repr()` (for `{value=}` in HA's messages).
fn repr(state: &State, v: &Value) -> Result<String, Error> {
    let mut s = String::new();
    super::py_repr(&mut super::Charged::new(&mut s, state), v, 0)?;
    Ok(s)
}

/// Whether Python sees `v` as iterable (`isinstance(v, Iterable)`).
fn is_iterable(v: &Value) -> bool {
    matches!(
        v.kind(),
        ValueKind::String
            | ValueKind::Seq
            | ValueKind::Map
            | ValueKind::Iterable
            | ValueKind::Undefined
    )
}

/// The items of an iterable, paid for one by one; iterating an undefined
/// value warns (lenient) or raises.
fn items(state: &State, v: &Value) -> Result<Vec<Value>, Error> {
    if v.is_undefined() {
        if state.undefined_behavior() == minijinja::UndefinedBehavior::Strict {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        super::warn_undefined();
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for item in v.try_iter()? {
        state.consume_fuel(1)?;
        out.push(item);
    }
    Ok(out)
}

/// A Python number: `bool` is an `int`.
#[derive(Clone, Copy)]
enum Num {
    Int(i128),
    Float(f64),
}

fn num_of(v: &Value) -> Option<Num> {
    match v.kind() {
        ValueKind::Bool => Some(Num::Int(i128::from(v.is_true()))),
        ValueKind::Number if v.is_integer() => i128::try_from(v.clone()).ok().map(Num::Int),
        ValueKind::Number => f64::try_from(v.clone()).ok().map(Num::Float),
        _ => None,
    }
}

impl Num {
    fn f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
}

fn int_arg(v: Option<&Value>, default: i64) -> Result<i64, Error> {
    match v {
        Some(v) => pytime::py_index(v),
        None => Ok(default),
    }
}

/// Registers HA's type casts, maths and time functions.
pub(super) fn add(env: &mut Environment<'static>) {
    add_type_casts(env);
    add_math(env);
    add_time(env);
}

// --- type casts ----------------------------------------------------------

fn forgiving_float(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [value, default] = bind(
        "TypeCastExtension.forgiving_float",
        ["value", "default"],
        1,
        args,
    )?;
    let value = value.expect("required");
    match to_float(state, &value)? {
        Some(x) => Ok(Value::from(x)),
        None => or_default(state, default, "float", &value),
    }
}

/// `jinja2.filters.do_int` with HA's sentinel default.
fn forgiving_int(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [value, default, base] = bind(
        "TypeCastExtension.forgiving_int",
        ["value", "default", "base"],
        1,
        args,
    )?;
    let value = value.expect("required");
    let fallback = |state: &State| -> Result<Value, Error> {
        // `int(float(value))`, else the default (an `OverflowError` too).
        match to_float(state, &value)? {
            Some(x) if x.is_finite() => {
                int_of_float(x)?.or_else(|()| or_default(state, default.clone(), "int", &value))
            }
            _ => or_default(state, default.clone(), "int", &value),
        }
    };
    match value.kind() {
        ValueKind::Undefined => Err(quiet_undefined()),
        ValueKind::Bool => Ok(Value::from(i64::from(value.is_true()))),
        ValueKind::Number if value.is_integer() => Ok(value.clone()),
        ValueKind::Number => {
            let x = f64::try_from(value.clone()).unwrap_or(f64::NAN);
            match int_of_float(x)? {
                Ok(v) => Ok(v),
                // `int(nan)` is a `ValueError`: then `int(float(nan))`.
                Err(()) => fallback(state),
            }
        }
        ValueKind::String => {
            let base = match &base {
                None => Some(10),
                Some(b) => match num_of(b) {
                    Some(Num::Int(b)) => u32::try_from(b).ok().or(Some(1)),
                    _ => None,
                },
            };
            let s = value.as_str().unwrap_or_default();
            state.consume_fuel(s.len() as u64 / 64)?;
            match base.map(|b| num::py_int(s, b)) {
                Some(Ok(i)) => Ok(Value::from(i)),
                Some(Err(IntError::TooBig)) => Err(py_error(
                    "NotImplementedError: hars does not support integers this large in templates yet"
                        .into(),
                )),
                _ => fallback(state),
            }
        }
        _ => fallback(state),
    }
}

/// `cv.boolean`, or `None` for its `vol.Invalid`.
fn cv_boolean(state: &State, v: &Value) -> Result<Option<bool>, Error> {
    Ok(match v.kind() {
        ValueKind::Bool => Some(v.is_true()),
        ValueKind::String => {
            let s = v.as_str().unwrap_or_default();
            state.consume_fuel(s.len() as u64 / 64)?;
            let lower = s.to_lowercase();
            match crate::py::strip(&lower) {
                "1" | "true" | "yes" | "on" | "enable" => Some(true),
                "0" | "false" | "no" | "off" | "disable" => Some(false),
                _ => None,
            }
        }
        ValueKind::Number => num_of(v).map(|n| match n {
            Num::Int(i) => i != 0,
            Num::Float(f) => f != 0.0,
        }),
        _ => {
            // `f"invalid boolean value {value}"` prints it.
            text_again(state, v)?;
            None
        }
    })
}

fn forgiving_boolean(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [value, default] = bind("forgiving_boolean", ["value", "default"], 1, args)?;
    let value = value.expect("required");
    match cv_boolean(state, &value)? {
        Some(b) => Ok(Value::from(b)),
        None => or_default(state, default, "bool", &value),
    }
}

fn is_number(state: &State, args: &[Value]) -> Result<bool, Error> {
    let [value] = bind("TypeCastExtension.is_number", ["value"], 1, args)?;
    Ok(to_float(state, &value.expect("required"))?.is_some_and(f64::is_finite))
}

fn add_type_casts(env: &mut Environment<'static>) {
    let float = |state: &State, args: Rest<ValueOrKwargs>| forgiving_float(state, &values(args));
    env.add_function("float", float);
    env.add_filter("float", float);
    let int = |state: &State, args: Rest<ValueOrKwargs>| forgiving_int(state, &values(args));
    env.add_function("int", int);
    env.add_filter("int", int);
    let boolean =
        |state: &State, args: Rest<ValueOrKwargs>| forgiving_boolean(state, &values(args));
    env.add_function("bool", boolean);
    env.add_filter("bool", boolean);
    let number = |state: &State, args: Rest<ValueOrKwargs>| is_number(state, &values(args));
    env.add_function("is_number", number);
    env.add_filter("is_number", number);
    env.add_test("is_number", number);
    env.add_test("string_like", |v: &Value| v.kind() == ValueKind::String);
}

// --- maths ---------------------------------------------------------------

/// A one-argument `math` function: `None` for its `ValueError`.
type MathFn = fn(f64) -> Option<f64>;

fn domain(ok: bool, x: f64) -> Option<f64> {
    ok.then_some(x)
}

fn unary(state: &State, name: &'static str, f: MathFn, args: &[Value]) -> Result<Value, Error> {
    let [value, default] = bind(name, ["value", "default"], 1, args)?;
    let value = value.expect("required");
    match to_float(state, &value)?.and_then(f) {
        Some(x) => Ok(Value::from(x)),
        None => or_default(state, default, name, &value),
    }
}

fn atan2(state: &State, args: &[Value]) -> Result<Value, Error> {
    let (kwargs, positional) = pytime::split_kwargs(args);
    let mut default = None;
    for (k, v) in kwargs {
        if k != "default" {
            return Err(py_error(format!(
                "TypeError: MathExtension.arc_tangent2() got an unexpected keyword argument '{k}'"
            )));
        }
        default = Some(v);
    }
    let mut args: Vec<Value> = positional.to_vec();
    if (1..=2).contains(&args.len())
        && args[0].kind() == ValueKind::Seq
        && matches!(type_name(&args[0]), "list" | "tuple")
    {
        if args.len() == 2 && default.is_none() {
            default = Some(args[1].clone());
        }
        args = items(state, &args[0])?;
    } else if args.len() == 3 && default.is_none() {
        default = Some(args[2].clone());
    }
    let shown = || Value::from(minijinja::value::Tuple::from(args.clone()));
    let (Some(y), Some(x)) = (args.first(), args.get(1)) else {
        // `args[0]` of too few: an `IndexError`, which isn't caught.
        return Err(py_error("IndexError: tuple index out of range".into()));
    };
    match (to_float(state, y)?, to_float(state, x)?) {
        (Some(y), Some(x)) => Ok(Value::from(y.atan2(x))),
        _ => or_default(state, default, "atan2", &shown()),
    }
}

fn logarithm(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [value, base, default] = bind(
        "MathExtension.logarithm",
        ["value", "base", "default"],
        1,
        args,
    )?;
    let value = value.expect("required");
    let base_value = base.unwrap_or(Value::from(std::f64::consts::E));
    let Some(b) = to_float(state, &base_value)? else {
        return or_default(state, default, "log", &base_value);
    };
    let Some(x) = to_float(state, &value)? else {
        return or_default(state, default, "log", &value);
    };
    // `loghelper` of each: positive only.
    let log = |v: f64| -> Option<f64> { (v.is_nan() || v > 0.0).then(|| v.ln()) };
    let (Some(num), Some(den)) = (log(x), log(b)) else {
        return or_default(state, default, "log", &value);
    };
    if den == 0.0 {
        return Err(py_error("ZeroDivisionError: division by zero".into()));
    }
    Ok(Value::from(num / den))
}

/// The list `average`, `median` and `statistical_mode` work on, and the
/// default (from a second positional argument), as HA picks them.
fn stat_args(
    state: &State,
    name: &str,
    args: &[Value],
    mode: bool,
) -> Result<(Value, Vec<Value>, Option<Value>), Error> {
    let (kwargs, positional) = pytime::split_kwargs(args);
    let mut default = None;
    for (k, v) in kwargs {
        if k != "default" {
            return Err(py_error(format!(
                "TypeError: {name}() got an unexpected keyword argument '{k}'"
            )));
        }
        default = Some(v);
    }
    let Some(first) = positional.first() else {
        return Err(py_error(format!(
            "TypeError: {name} expected at least 1 argument, got 0"
        )));
    };
    let shown = Value::from(minijinja::value::Tuple::from(positional.to_vec()));
    let list_or_tuple =
        first.kind() == ValueKind::Seq && matches!(type_name(first), "list" | "tuple");
    let takes_first = if mode {
        (positional.len() == 1 && is_iterable(first)) || list_or_tuple
    } else {
        is_iterable(first)
    };
    if takes_first {
        if positional.len() > 1 && default.is_none() {
            default = Some(positional[1].clone());
        }
        return Ok((shown, items(state, first)?, default));
    }
    if positional.len() == 1 {
        return Err(py_error(format!(
            "TypeError: '{}' object is not iterable",
            type_name(first)
        )));
    }
    Ok((shown, positional.to_vec(), default))
}

fn average(state: &State, args: &[Value]) -> Result<Value, Error> {
    let (shown, list, default) = stat_args(state, "average", args, false)?;
    let mut floats = Vec::with_capacity(list.len());
    for v in &list {
        match v.kind() {
            ValueKind::Undefined => return Err(quiet_undefined()),
            _ => match num_of(v) {
                Some(n) => floats.push(n.f64()),
                // `fsum`'s `TypeError`.
                None => return or_default(state, default, "average", &shown),
            },
        }
    }
    if floats.is_empty() {
        return or_default(state, default, "average", &shown);
    }
    let total = num::fsum(floats.iter().copied()).map_err(py_error)?;
    Ok(Value::from(total / floats.len() as f64))
}

/// Python's sort of `list` (by `<`); `None` for its `TypeError`.
fn py_sorted(state: &State, mut list: Vec<Value>) -> Result<Option<Vec<Value>>, Error> {
    if let Some(first) = list.first().cloned() {
        for v in &list[1..] {
            if python_lt(state, &first, v).is_err() || python_lt(state, v, &first).is_err() {
                return Ok(None);
            }
        }
    }
    let n = list.len() as u64;
    state.consume_fuel(n.saturating_mul(u64::from(64 - n.leading_zeros())))?;
    list.sort_by(|a, b| match python_lt(state, a, b) {
        Ok(true) => Ordering::Less,
        _ => match python_lt(state, b, a) {
            Ok(true) => Ordering::Greater,
            _ => Ordering::Equal,
        },
    });
    Ok(Some(list))
}

fn python_lt(state: &State, a: &Value, b: &Value) -> Result<bool, Error> {
    if let Some(rv) = minijinja::value::object_binop(minijinja::value::BinOp::Lt, a, b) {
        return rv.map(|v| v.is_true());
    }
    super::jinja::py_less(state, a, b)
}

fn median(state: &State, args: &[Value]) -> Result<Value, Error> {
    let (shown, list, default) = stat_args(state, "median", args, false)?;
    if list.iter().any(Value::is_undefined) {
        return Err(quiet_undefined());
    }
    let Some(data) = py_sorted(state, list)? else {
        return or_default(state, default, "median", &shown);
    };
    let n = data.len();
    if n == 0 {
        return or_default(state, default, "median", &shown);
    }
    if n % 2 == 1 {
        return Ok(data[n / 2].clone());
    }
    let (a, b) = (&data[n / 2 - 1], &data[n / 2]);
    match (num_of(a), num_of(b)) {
        (Some(Num::Int(x)), Some(Num::Int(y))) => match x.checked_add(y) {
            Some(s) => Ok(Value::from(pytime::int_true_div(s, 2))),
            None => Ok(Value::from((x as f64 + y as f64) / 2.0)),
        },
        (Some(x), Some(y)) => Ok(Value::from((x.f64() + y.f64()) / 2.0)),
        // `(a + b) / 2` with the values' own operators (timedeltas); a
        // `TypeError` (two datetimes, two strings) is caught.
        _ => {
            use minijinja::value::{BinOp, object_binop};
            let halved = object_binop(BinOp::Add, a, b).map(|sum| {
                sum.and_then(|s| {
                    object_binop(BinOp::Div, &s, &Value::from(2)).unwrap_or(Err(type_error()))
                })
            });
            match halved {
                Some(Ok(v)) => Ok(v),
                Some(Err(e)) if !e.detail().is_some_and(|d| d.starts_with("TypeError:")) => Err(e),
                _ => or_default(state, default, "median", &shown),
            }
        }
    }
}

/// A placeholder `TypeError` (its text isn't shown: it is caught).
fn type_error() -> Error {
    py_error("TypeError: unsupported operand".into())
}

/// A key equal values share (Python's hash and `==`): `1`, `1.0` and
/// `True` are one key.
#[derive(PartialEq, Eq, Hash)]
enum Key {
    Int(i128),
    Float(u64),
    Str(String),
    None,
}

fn statistical_mode(state: &State, args: &[Value]) -> Result<Value, Error> {
    let (shown, list, default) = stat_args(state, "statistical_mode", args, true)?;
    // Each distinct value with its count, in first-seen order.
    let mut counts: Vec<(Value, usize)> = Vec::new();
    let mut index: std::collections::HashMap<Key, usize> = std::collections::HashMap::new();
    for v in list {
        state.consume_fuel(1)?;
        let key = match v.kind() {
            ValueKind::Undefined => return Err(quiet_undefined()),
            ValueKind::None => Some(Key::None),
            ValueKind::String => Some(Key::Str(v.as_str().unwrap_or_default().to_owned())),
            // `Counter`'s `TypeError: unhashable type`.
            ValueKind::Map => return or_default(state, default, "statistical_mode", &shown),
            ValueKind::Seq if type_name(&v) == "list" => {
                return or_default(state, default, "statistical_mode", &shown);
            }
            _ => match num_of(&v) {
                Some(Num::Int(i)) => Some(Key::Int(i)),
                Some(Num::Float(f)) if f.fract() == 0.0 && f.abs() < 1e38 => {
                    Some(Key::Int(f as i128))
                }
                Some(Num::Float(f)) => Some(Key::Float(f.to_bits())),
                None => None,
            },
        };
        let found = match key {
            Some(k) => match index.get(&k) {
                Some(i) => Some(*i),
                None => {
                    index.insert(k, counts.len());
                    None
                }
            },
            // Tuples, datetimes, …: compared with each kept value (paid
            // for by `py_eq`).
            None => {
                let mut at = None;
                for (i, (kept, _)) in counts.iter().enumerate() {
                    if super::jinja::py_eq(state, kept, &v)? {
                        at = Some(i);
                        break;
                    }
                }
                at
            }
        };
        match found {
            Some(i) => counts[i].1 += 1,
            None => counts.push((v, 1)),
        }
    }
    // `Counter.most_common(1)`: the first of the most frequent.
    let mut best: Option<(usize, &Value)> = None;
    for (v, n) in &counts {
        if best.is_none_or(|(m, _)| *n > m) {
            best = Some((*n, v));
        }
    }
    match best {
        Some((_, v)) => Ok(v.clone()),
        None => or_default(state, default, "statistical_mode", &shown),
    }
}

/// `min`/`max` as globals: Jinja's filter over the arguments.
fn min_max(state: &mut State, name: &str, args: &[Value]) -> Result<Value, Error> {
    let (kwargs, positional) = match args.last() {
        Some(last) if last.is_kwargs() => (Some(last.clone()), &args[..args.len() - 1]),
        _ => (None, args),
    };
    let target = match positional {
        [] => {
            return Err(py_error(format!(
                "TypeError: {name} expected at least 1 argument, got 0"
            )));
        }
        [one] if is_iterable(one) => one.clone(),
        [one] => {
            return Err(py_error(format!(
                "TypeError: '{}' object is not iterable",
                type_name(one)
            )));
        }
        many => Value::from(minijinja::value::Tuple::from(many.to_vec())),
    };
    let mut call = vec![target];
    call.extend(kwargs);
    state.apply_filter(name, &call)
}

fn bitwise(op: &'static str, a: &Value, b: &Value) -> Result<Value, Error> {
    let both_bool = a.kind() == ValueKind::Bool && b.kind() == ValueKind::Bool;
    match (num_of(a), num_of(b)) {
        (Some(Num::Int(x)), Some(Num::Int(y))) => {
            let r = match op {
                "&" => x & y,
                "|" => x | y,
                _ => x ^ y,
            };
            Ok(if both_bool {
                Value::from(r != 0)
            } else {
                Value::from(r)
            })
        }
        _ => Err(py_error(format!(
            "TypeError: unsupported operand type(s) for {op}: '{}' and '{}'",
            type_name(a),
            type_name(b)
        ))),
    }
}

/// `float(value) + amount` / `* amount`.
fn arith(state: &State, name: &'static str, args: &[Value]) -> Result<Value, Error> {
    let [value, amount, default] = bind(name, ["value", "amount", "default"], 2, args)?;
    let (value, amount) = (value.expect("required"), amount.expect("required"));
    let Some(x) = to_float(state, &value)? else {
        return or_default(state, default, name, &value);
    };
    if amount.is_undefined() {
        return Err(quiet_undefined());
    }
    if let (Some(td), "multiply") = (amount.downcast_object_ref::<TimeDelta>(), name) {
        let td = Value::from_object(*td);
        return minijinja::value::object_binop(minijinja::value::BinOp::Mul, &td, &Value::from(x))
            .unwrap_or_else(|| Ok(Value::from(())));
    }
    match num_of(&amount) {
        Some(n) => Ok(Value::from(if name == "add" {
            x + n.f64()
        } else {
            x * n.f64()
        })),
        None => or_default(state, default, name, &value),
    }
}

/// `MathExtension.forgiving_round`.
fn forgiving_round(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [value, precision, method, default] = bind(
        "MathExtension.forgiving_round",
        ["value", "precision", "method", "default"],
        1,
        args,
    )?;
    let mut value = value.expect("required");
    let precision = precision.unwrap_or(Value::from(0));
    let method = method.unwrap_or(Value::from("common"));
    let fail = |state: &State, value: &Value| or_default(state, default.clone(), "round", value);
    // `float(10**precision)`.
    let multiplier = match num_of(&precision) {
        // `float(10**precision)` of an int past a float's range.
        Some(Num::Int(p)) if p > 308 => {
            return Err(py_error(
                "OverflowError: int too large to convert to float".into(),
            ));
        }
        Some(Num::Int(p)) => 10f64.powi(p.max(-400) as i32),
        Some(Num::Float(p)) if 10f64.powf(p).is_infinite() => {
            return Err(py_error("OverflowError: (34, 'Result too large')".into()));
        }
        Some(Num::Float(p)) => 10f64.powf(p),
        None if precision.is_undefined() => return Err(quiet_undefined()),
        None => return fail(state, &value),
    };
    let int_precision = match num_of(&precision) {
        Some(Num::Int(p)) => Some(p),
        _ => None,
    };
    let method = method.as_str().unwrap_or("");
    let Some(x) = to_float(state, &value)? else {
        return fail(state, &value);
    };
    let rounded = match method {
        "ceil" | "floor" => {
            let scaled = x * multiplier;
            let whole = if method == "ceil" {
                scaled.ceil()
            } else {
                scaled.floor()
            };
            match int_of_float(whole)? {
                Ok(_) => whole / multiplier,
                Err(()) => return fail(state, &value),
            }
        }
        "half" => {
            let whole = num::round_half_even(x * 2.0);
            match int_of_float(whole)? {
                Ok(_) => whole / 2.0,
                Err(()) => return fail(state, &value),
            }
        }
        _ => match int_precision {
            Some(p) => num::py_round(x, i64::try_from(p).unwrap_or(i64::MAX)),
            // `round(x, 1.5)`: a `TypeError`.
            None => return fail(state, &value),
        },
    };
    value = Value::from(rounded);
    if int_precision == Some(0) {
        return match int_of_float(rounded)? {
            Ok(v) => Ok(v),
            Err(()) => fail(state, &value),
        };
    }
    Ok(value)
}

/// `float()` of each argument, or HA's message naming them all.
fn numeric_args(state: &State, named: &[(&str, &Value)]) -> Result<Vec<f64>, Error> {
    let mut out = Vec::with_capacity(named.len());
    let mut bad = false;
    for (_, v) in named {
        match to_float(state, v)? {
            Some(x) => out.push(x),
            None => bad = true,
        }
    }
    if bad {
        let mut parts = Vec::new();
        for (n, v) in named {
            parts.push(format!("{n}={}", repr(state, v)?));
        }
        return Err(py_error(format!(
            "ValueError: function requires numeric arguments, got {}",
            parts.join(", ")
        )));
    }
    Ok(out)
}

fn clamp(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [v, lo, hi] = bind(
        "MathExtension.clamp",
        ["value", "min_value", "max_value"],
        3,
        args,
    )?;
    let (v, lo, hi) = (
        v.expect("required"),
        lo.expect("required"),
        hi.expect("required"),
    );
    let n = numeric_args(
        state,
        &[("value", &v), ("min_value", &lo), ("max_value", &hi)],
    )?;
    // `max(min, min(max, value))`, Python's left-to-right picks.
    let inner = if n[0] < n[2] { n[0] } else { n[2] };
    Ok(Value::from(if inner > n[1] { inner } else { n[1] }))
}

fn wrap(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [v, lo, hi] = bind(
        "MathExtension.wrap",
        ["value", "min_value", "max_value"],
        3,
        args,
    )?;
    let (v, lo, hi) = (
        v.expect("required"),
        lo.expect("required"),
        hi.expect("required"),
    );
    let n = numeric_args(
        state,
        &[("value", &v), ("min_value", &lo), ("max_value", &hi)],
    )?;
    Ok(Value::from(match num::py_fmod(n[0] - n[1], n[2] - n[1]) {
        Some(r) => r + n[1],
        None => n[1],
    }))
}

fn remap(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [v, in_min, in_max, out_min, out_max, steps, edges] = bind(
        "MathExtension.remap",
        [
            "value", "in_min", "in_max", "out_min", "out_max", "steps", "edges",
        ],
        5,
        args,
    )?;
    let (v, in_min, in_max, out_min, out_max) = (
        v.expect("required"),
        in_min.expect("required"),
        in_max.expect("required"),
        out_min.expect("required"),
        out_max.expect("required"),
    );
    let n = numeric_args(
        state,
        &[
            ("value", &v),
            ("in_min", &in_min),
            ("in_max", &in_max),
            ("out_min", &out_min),
            ("out_max", &out_max),
        ],
    )?;
    let (mut x, a, b, c, d) = (n[0], n[1], n[2], n[3], n[4]);
    let edges = edges.as_ref().and_then(Value::as_str).unwrap_or("none");
    let not_equal = || -> Result<(), Error> {
        if a == b {
            return Err(py_error(format!(
                "ValueError: in_min={} must not equal in_max={}",
                repr(state, &in_min)?,
                repr(state, &in_max)?
            )));
        }
        Ok(())
    };
    match edges {
        "clamp" => {
            let inner = if x < b { x } else { b };
            x = if inner > a { inner } else { a };
        }
        "wrap" => {
            not_equal()?;
            x = num::py_fmod(x - a, b - a).unwrap_or(0.0) + a;
        }
        "mirror" => {
            not_equal()?;
            let range = b - a;
            let offset = x - a;
            let period = (offset / range).floor();
            if !period.is_finite() {
                return Err(py_error(if period.is_nan() {
                    "ValueError: cannot convert float NaN to integer".into()
                } else {
                    "OverflowError: cannot convert float infinity to integer".into()
                }));
            }
            let mut pos = offset - period * range;
            if period < 0.0 || period % 2.0 != 0.0 {
                pos = range - pos;
            }
            x = a + pos;
        }
        _ => {}
    }
    let steps = match &steps {
        None => Num::Int(0),
        Some(s) => num_of(s).ok_or_else(|| {
            py_error(format!(
                "TypeError: '>' not supported between instances of 'int' and '{}'",
                type_name(s)
            ))
        })?,
    };
    let steps = match steps {
        Num::Int(i) => Num::Int(i.max(0)),
        Num::Float(f) => Num::Float(if 0.0 > f { 0.0 } else { f }),
    };
    let no_steps = steps.f64() == 0.0;
    if no_steps && a == c && b == d {
        return Ok(Value::from(x));
    }
    if b - a == 0.0 {
        return Err(py_error("ZeroDivisionError: division by zero".into()));
    }
    let mut normalized = (x - a) / (b - a);
    if !no_steps {
        let r = num::round_half_even(normalized * steps.f64());
        if int_of_float(r)?.is_err() {
            return Err(py_error(
                "ValueError: cannot convert float NaN to integer".into(),
            ));
        }
        normalized = r / steps.f64();
    }
    Ok(Value::from(c + normalized * (d - c)))
}

fn add_math(env: &mut Environment<'static>) {
    env.add_global("e", Value::from(std::f64::consts::E));
    env.add_global("pi", Value::from(std::f64::consts::PI));
    env.add_global("tau", Value::from(std::f64::consts::TAU));
    macro_rules! unary_fn {
        ($name:literal, $f:expr) => {{
            let f =
                |state: &State, args: Rest<ValueOrKwargs>| unary(state, $name, $f, &values(args));
            env.add_function($name, f);
            env.add_filter($name, f);
        }};
    }
    unary_fn!("sin", |x| domain(x.is_finite() || x.is_nan(), x.sin()));
    unary_fn!("cos", |x| domain(x.is_finite() || x.is_nan(), x.cos()));
    unary_fn!("tan", |x| domain(x.is_finite() || x.is_nan(), x.tan()));
    unary_fn!("asin", |x| domain(
        x.is_nan() || (-1.0..=1.0).contains(&x),
        x.asin()
    ));
    unary_fn!("acos", |x| domain(
        x.is_nan() || (-1.0..=1.0).contains(&x),
        x.acos()
    ));
    unary_fn!("atan", |x| Some(x.atan()));
    unary_fn!("sqrt", |x| domain(x.is_nan() || x >= 0.0, x.sqrt()));
    let f = |state: &State, args: Rest<ValueOrKwargs>| atan2(state, &values(args));
    env.add_function("atan2", f);
    env.add_filter("atan2", f);
    let f = |state: &State, args: Rest<ValueOrKwargs>| logarithm(state, &values(args));
    env.add_function("log", f);
    env.add_filter("log", f);
    let f = |state: &State, args: Rest<ValueOrKwargs>| average(state, &values(args));
    env.add_function("average", f);
    env.add_filter("average", f);
    let f = |state: &State, args: Rest<ValueOrKwargs>| median(state, &values(args));
    env.add_function("median", f);
    env.add_filter("median", f);
    let f = |state: &State, args: Rest<ValueOrKwargs>| statistical_mode(state, &values(args));
    env.add_function("statistical_mode", f);
    env.add_filter("statistical_mode", f);
    env.add_function("min", |state: &mut State, args: Rest<ValueOrKwargs>| {
        min_max(state, "min", &values(args))
    });
    env.add_function("max", |state: &mut State, args: Rest<ValueOrKwargs>| {
        min_max(state, "max", &values(args))
    });
    for (name, op) in [
        ("bitwise_and", "&"),
        ("bitwise_or", "|"),
        ("bitwise_xor", "^"),
    ] {
        let f = move |args: Rest<ValueOrKwargs>| -> Result<Value, Error> {
            let [a, b] = bind(name, ["first_value", "second_value"], 2, &values(args))?;
            let (a, b) = (a.expect("required"), b.expect("required"));
            if a.is_undefined() || b.is_undefined() {
                return Err(quiet_undefined());
            }
            bitwise(op, &a, &b)
        };
        env.add_function(name, f);
        env.add_filter(name, f);
    }
    env.add_filter("add", |state: &State, args: Rest<ValueOrKwargs>| {
        arith(state, "add", &values(args))
    });
    env.add_filter("multiply", |state: &State, args: Rest<ValueOrKwargs>| {
        arith(state, "multiply", &values(args))
    });
    env.add_filter("round", |state: &State, args: Rest<ValueOrKwargs>| {
        forgiving_round(state, &values(args))
    });
    let f = |state: &State, args: Rest<ValueOrKwargs>| clamp(state, &values(args));
    env.add_function("clamp", f);
    env.add_filter("clamp", f);
    let f = |state: &State, args: Rest<ValueOrKwargs>| wrap(state, &values(args));
    env.add_function("wrap", f);
    env.add_filter("wrap", f);
    let f = |state: &State, args: Rest<ValueOrKwargs>| remap(state, &values(args));
    env.add_function("remap", f);
    env.add_filter("remap", f);
}

// --- time ----------------------------------------------------------------

/// `dt_util.now()`: the instance's clock in the configured zone.
fn now(name: &str, tz: Tz) -> Result<DateTime, Error> {
    let hass = home::hass(name)?;
    DateTime::at(hass.clock().now().as_microsecond(), tz)
}

/// `dt_util.as_local`.
fn as_local(v: &DateTime) -> Result<DateTime, Error> {
    let local = home::local_tz();
    match &v.tz {
        Some(tz) if same_zone(tz, &local) => Ok(v.clone()),
        None => Ok(DateTime {
            wall: v.wall,
            fold: v.fold,
            tz: Some(local),
        }),
        Some(_) => v.to_zone(local),
    }
}

/// `tzinfo == DEFAULT_TIME_ZONE`.
fn same_zone(a: &Tz, b: &Tz) -> bool {
    match (a, b) {
        (Tz::Utc, Tz::Utc) => true,
        (Tz::Zone(_, x), Tz::Zone(_, y)) => x == y,
        (Tz::Fixed(x), Tz::Fixed(y)) => x == y,
        _ => false,
    }
}

fn datetime_arg(v: &Value) -> Option<DateTime> {
    v.downcast_object_ref::<DateTime>().cloned()
}

/// `dt_util.utc_from_timestamp`: `Ok(Err(()))` for Python's `ValueError`
/// or `TypeError`.
fn utc_from_timestamp(v: &Value) -> Result<Result<DateTime, ()>, Error> {
    Ok(pytime::from_timestamp(v, Tz::Utc)?.map_err(|_| ()))
}

/// `dt_util.parse_datetime(value)` for a string value, paid for.
fn parse(state: &State, s: &str) -> Result<Option<DateTime>, Error> {
    state.consume_fuel(s.len() as u64 / 64)?;
    match pytime::parse_datetime(s) {
        Ok(dt) => Ok(Some(dt)),
        Err(Unparsed::Invalid) => Ok(None),
        Err(Unparsed::Unsupported) => Err(py_error(
            "NotImplementedError: hars does not support week or ordinal ISO dates yet".into(),
        )),
    }
}

fn as_datetime(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [value, default] = bind(
        "DateTimeExtension.as_datetime",
        ["value", "default"],
        1,
        args,
    )?;
    let value = value.expect("required");
    if value.downcast_object_ref::<DateTime>().is_some() {
        return Ok(value);
    }
    if let Some(d) = value.downcast_object_ref::<Date>() {
        return Ok(DateTime::naive(Naive::midnight(d.y, d.mo, d.d)).value());
    }
    if let Some(x) = to_float(state, &value)? {
        if let Ok(dt) = utc_from_timestamp(&Value::from(x))? {
            return Ok(dt.value());
        }
    }
    if let Some(s) = value.as_str().filter(|_| value.kind() == ValueKind::String) {
        if let Some(dt) = parse(state, s)? {
            return Ok(dt.value());
        }
        return Ok(default.unwrap_or(Value::from(())));
    }
    or_default(state, default, "as_datetime", &value)
}

fn as_timestamp(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [value, default] = bind(
        "DateTimeExtension.as_timestamp",
        ["value", "default"],
        1,
        args,
    )?;
    let value = value.expect("required");
    if let Some(dt) = datetime_arg(&value) {
        return Ok(Value::from(dt.timestamp()));
    }
    let s = text_again(state, &value)?;
    match parse(state, &s)? {
        Some(dt) => Ok(Value::from(dt.timestamp())),
        None => or_default(state, default, "as_timestamp", &value),
    }
}

fn as_local_fn(args: &[Value]) -> Result<Value, Error> {
    let [value] = bind("DateTimeExtension.as_local", ["value"], 1, args)?;
    let value = value.expect("required");
    match datetime_arg(&value) {
        Some(dt) => as_local(&dt).map(DateTime::value),
        None if value.is_undefined() => Err(Error::from(ErrorKind::UndefinedError)),
        None => Err(py_error(format!(
            "AttributeError: '{}' object has no attribute 'tzinfo'",
            type_name(&value)
        ))),
    }
}

fn timestamp_fn(state: &State, which: &'static str, args: &[Value]) -> Result<Value, Error> {
    let (value, format, local, default) = if which == "timestamp_custom" {
        let [value, format, local, default] = bind(
            "DateTimeExtension.timestamp_custom",
            ["value", "date_format", "local", "default"],
            1,
            args,
        )?;
        (value, format, local, default)
    } else {
        let [value, default] = bind(which, ["value", "default"], 1, args)?;
        (value, None, None, default)
    };
    let value = value.expect("required");
    if value.is_undefined() {
        return Err(quiet_undefined());
    }
    let Ok(dt) = utc_from_timestamp(&value)? else {
        return or_default(state, default, which, &value);
    };
    match which {
        "timestamp_utc" => {
            let mut s = String::new();
            dt.iso(&mut s, 'T');
            Ok(Value::from(s))
        }
        "timestamp_local" => {
            let mut s = String::new();
            as_local(&dt)?.iso(&mut s, 'T');
            Ok(Value::from(s))
        }
        _ => {
            let dt = if local.as_ref().is_none_or(Value::is_true) {
                as_local(&dt)?
            } else {
                dt
            };
            let format = format.unwrap_or(Value::from("%Y-%m-%d %H:%M:%S"));
            match format
                .as_str()
                .filter(|_| format.kind() == ValueKind::String)
            {
                Some(f) => {
                    pytime::strftime(state, &dt.wall, dt.tz.as_ref(), dt.fold, f).map(Value::from)
                }
                None => or_default(state, default, which, &value),
            }
        }
    }
}

fn strptime_fn(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [string, format, default] = bind(
        "DateTimeExtension.strptime",
        ["string", "fmt", "default"],
        2,
        args,
    )?;
    let (string, format) = (string.expect("required"), format.expect("required"));
    let (Some(s), Some(f)) = (
        string
            .as_str()
            .filter(|_| string.kind() == ValueKind::String),
        format
            .as_str()
            .filter(|_| format.kind() == ValueKind::String),
    ) else {
        if string.is_undefined() || format.is_undefined() {
            return Err(quiet_undefined());
        }
        return or_default(state, default, "strptime", &string);
    };
    match pytime::strptime(state, s, f)? {
        Ok(dt) => Ok(dt.value()),
        Err(_) => or_default(state, default, "strptime", &string),
    }
}

fn timedelta_fn(args: &[Value]) -> Result<Value, Error> {
    let [days, seconds, us, ms, minutes, hours, weeks] = bind(
        "timedelta",
        [
            "days",
            "seconds",
            "microseconds",
            "milliseconds",
            "minutes",
            "hours",
            "weeks",
        ],
        0,
        args,
    )?;
    for v in [&days, &seconds, &us, &ms, &minutes, &hours, &weeks]
        .into_iter()
        .flatten()
    {
        if v.is_undefined() {
            return Err(quiet_undefined());
        }
    }
    pytime::timedelta([
        ("days", days),
        ("seconds", seconds),
        ("microseconds", us),
        ("milliseconds", ms),
        ("minutes", minutes),
        ("hours", hours),
        ("weeks", weeks),
    ])
    .map(Value::from_object)
}

fn as_timedelta(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [value] = bind("DateTimeExtension.as_timedelta", ["value"], 1, args)?;
    let value = value.expect("required");
    let Some(s) = value.as_str().filter(|_| value.kind() == ValueKind::String) else {
        let shown = if value.is_undefined() {
            if state.undefined_behavior() == minijinja::UndefinedBehavior::Strict {
                "StrictUndefined"
            } else {
                "LoggingUndefined"
            }
        } else {
            type_name(&value)
        };
        return Err(py_error(format!(
            "TypeError: expected string or bytes-like object, got '{shown}'"
        )));
    };
    state.consume_fuel(s.len() as u64 / 64)?;
    match pytime::parse_duration(s) {
        Some(td) => td.map(Value::from_object),
        None => Ok(Value::from(())),
    }
}

/// `dt_util.start_of_local_day()`.
fn start_of_local_day() -> Result<DateTime, Error> {
    let today = now("today_at", home::local_tz())?;
    Ok(DateTime {
        wall: Naive::midnight(today.wall.y, today.wall.mo, today.wall.d),
        fold: false,
        tz: Some(home::local_tz()),
    })
}

fn today_at(state: &State, args: &[Value]) -> Result<Value, Error> {
    let [time_str] = bind("DateTimeExtension.today_at", ["time_str"], 0, args)?;
    home::note_time();
    let today = start_of_local_day()?;
    let Some(time_str) = time_str else {
        return Ok(today.value());
    };
    // `if not time_str` (an undefined value warns when tested).
    if time_str.is_undefined() {
        super::jinja::truth(state, &time_str)?;
        return Ok(today.value());
    }
    if !time_str.is_true() {
        return Ok(today.value());
    }
    let s = text(state, &time_str)?;
    match pytime::parse_time(&s) {
        Some(t) => Ok(DateTime {
            wall: Naive {
                h: t.h,
                mi: t.mi,
                s: t.s,
                us: 0,
                ..today.wall
            },
            fold: false,
            tz: today.tz,
        }
        .value()),
        None => Err(py_error(format!(
            "ValueError: could not convert {} to datetime: '{s}'",
            type_name(&time_str)
        ))),
    }
}

/// `dt_util._get_timestring`.
fn timestring(mut diff: i128, precision: i64) -> String {
    if diff == 0 {
        return "0 seconds".into();
    }
    const UNITS: [(&str, i128); 6] = [
        ("year", 365 * 24 * 3600),
        ("month", 30 * 24 * 3600),
        ("day", 24 * 3600),
        ("hour", 3600),
        ("minute", 60),
        ("second", 1),
    ];
    let formatn = |n: i128, unit: &str| {
        if n == 1 {
            format!("1 {unit} ")
        } else {
            format!("{n} {unit}s ")
        }
    };
    let mut out = String::new();
    let mut current = 0;
    for (unit, factor) in UNITS {
        if diff < factor {
            continue;
        }
        current += 1;
        if current == precision {
            let n = num::round_half_even(diff as f64 / factor as f64) as i128;
            out.push_str(&formatn(n, unit));
            return out.trim_end().to_owned();
        }
        let n = diff.div_euclid(factor);
        out.push_str(&formatn(n, unit));
        diff -= n * factor;
    }
    out.trim_end().to_owned()
}

/// `relative_time`, `time_since`, `time_until`.
fn relative(which: &'static str, args: &[Value]) -> Result<Value, Error> {
    let (value, precision) = if which == "relative_time" {
        let [value] = bind("DateTimeExtension.relative_time", ["value"], 1, args)?;
        (value, None)
    } else {
        let [value, precision] = bind(which, ["value", "precision"], 1, args)?;
        (value, precision)
    };
    home::note_time();
    let value = value.expect("required");
    // `current_precision == precision`: anything but an int never matches.
    let precision = int_arg(precision.as_ref(), 1).unwrap_or(i64::MIN);
    let Some(dt) = datetime_arg(&value) else {
        return Ok(value);
    };
    let dt = if dt.tz.is_none() { as_local(&dt)? } else { dt };
    let current = now(which, home::local_tz())?.value();
    let target = dt.clone().value();
    let future = which == "time_until";
    let (op, first, second) = if future {
        (minijinja::value::BinOp::Gt, &current, &target)
    } else {
        (minijinja::value::BinOp::Lt, &current, &target)
    };
    let skip = minijinja::value::object_binop(op, first, second)
        .unwrap_or(Ok(Value::from(false)))?
        .is_true();
    if skip {
        return Ok(target);
    }
    let (a, b) = if future {
        (&target, &current)
    } else {
        (&current, &target)
    };
    let delta = minijinja::value::object_binop(minijinja::value::BinOp::Sub, a, b)
        .unwrap_or(Ok(Value::from(())))?;
    let seconds = delta
        .downcast_object_ref::<TimeDelta>()
        .map_or(0.0, TimeDelta::total_seconds);
    let rounded = num::round_half_even(seconds) as i128;
    Ok(Value::from(timestring(rounded, precision)))
}

fn add_time(env: &mut Environment<'static>) {
    let f = |state: &State, args: Rest<ValueOrKwargs>| as_datetime(state, &values(args));
    env.add_function("as_datetime", f);
    env.add_filter("as_datetime", f);
    let f = |args: Rest<ValueOrKwargs>| as_local_fn(&values(args));
    env.add_function("as_local", f);
    env.add_filter("as_local", f);
    let f = |state: &State, args: Rest<ValueOrKwargs>| as_timedelta(state, &values(args));
    env.add_function("as_timedelta", f);
    env.add_filter("as_timedelta", f);
    let f = |state: &State, args: Rest<ValueOrKwargs>| as_timestamp(state, &values(args));
    env.add_function("as_timestamp", f);
    env.add_filter("as_timestamp", f);
    env.add_function("strptime", |state: &State, args: Rest<ValueOrKwargs>| {
        strptime_fn(state, &values(args))
    });
    env.add_function("timedelta", |args: Rest<ValueOrKwargs>| {
        timedelta_fn(&values(args))
    });
    for which in ["timestamp_custom", "timestamp_local", "timestamp_utc"] {
        env.add_filter(which, move |state: &State, args: Rest<ValueOrKwargs>| {
            timestamp_fn(state, which, &values(args))
        });
    }
    env.add_test("datetime", |v: &Value| {
        v.downcast_object_ref::<DateTime>().is_some()
    });
    env.add_function("now", |args: Rest<ValueOrKwargs>| -> Result<Value, Error> {
        bind::<0>("DateTimeExtension.now", [], 0, &values(args))?;
        home::hass("now")?;
        home::note_time();
        now("now", home::local_tz()).map(DateTime::value)
    });
    env.add_function(
        "utcnow",
        |args: Rest<ValueOrKwargs>| -> Result<Value, Error> {
            bind::<0>("DateTimeExtension.utcnow", [], 0, &values(args))?;
            home::hass("utcnow")?;
            home::note_time();
            now("utcnow", Tz::Utc).map(DateTime::value)
        },
    );
    for which in ["relative_time", "time_since", "time_until"] {
        let f = move |args: Rest<ValueOrKwargs>| {
            home::hass(which)?;
            relative(which, &values(args))
        };
        env.add_function(which, f);
        env.add_filter(which, f);
    }
    let f = |state: &State, args: Rest<ValueOrKwargs>| {
        home::hass("today_at")?;
        today_at(state, &values(args))
    };
    env.add_function("today_at", f);
    env.add_filter("today_at", f);
}

#[cfg(test)]
mod tests {
    use super::timestring;

    #[test]
    fn timestrings_are_has() {
        assert_eq!(timestring(0, 1), "0 seconds");
        assert_eq!(timestring(89, 2), "1 minute 29 seconds");
        assert_eq!(timestring(45 * 86400, 1), "2 months");
        assert_eq!(timestring(800 * 86400, 0), "2 years 2 months 10 days");
    }
}
