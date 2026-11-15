//! Python's `datetime` module as templates see it: `datetime`, `date`,
//! `time` and `timedelta` values, their time zones, printing (`str`,
//! `repr`, `isoformat`, `strftime`), arithmetic and comparison, and the
//! parsers HA's template functions use (`dt_util.parse_datetime` with
//! ciso8601, `parse_duration`, `datetime.strptime`).
//!
//! What follows Python exactly, because templates show it:
//!
//! - **Wall-clock arithmetic.** An aware datetime plus a `timedelta` moves
//!   its wall time and keeps its zone, so `now() - timedelta(hours=1)`
//!   across a DST change lands on a different offset; two datetimes with
//!   the same zone subtract and compare by wall time, with different
//!   zones by instant (CPython's `datetime_subtract`, `datetime_richcompare`).
//! - **`fold`.** A wall time in a gap or a repeat takes the offset before
//!   the change unless `fold=1` (PEP 495); converting an instant into a zone
//!   sets `fold` on the second of two repeated wall times.
//! - **The zones.** `timezone.utc` (`utcnow()`, states' timestamps),
//!   `zoneinfo.ZoneInfo` (`now()`, `as_local`), ciso8601's `FixedOffset`
//!   and `datetime.timezone` for parsed offsets: they print differently.
//! - **`strftime`** is glibc's in the C locale, as HA runs on Linux.
//!
//! A naive datetime's `timestamp()` uses the configured time zone (Python
//! uses the process's, which an HA install sets to the same, see
//! `scripts/vectors/template_functions.py`).

use super::home;
use super::jinja::{py_error, type_name};
use minijinja::value::{BinOp, DynObject, Object, ObjectRepr, Value, ValueKind};
use minijinja::{Error, State};
use std::cmp::Ordering;
use std::fmt::{self, Write as _};
use std::sync::Arc;

const US_PER_SEC: i64 = 1_000_000;
const US_PER_DAY: i64 = 86_400 * US_PER_SEC;
/// `timedelta.max.days`.
const MAX_DAYS: i64 = 999_999_999;

// --- calendar ------------------------------------------------------------

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
pub(super) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01.
pub(super) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(super) fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

pub(super) fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A wall time with no zone, to the microsecond, in Python's range
/// (years 1 to 9999).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Naive {
    pub y: i32,
    pub mo: u8,
    pub d: u8,
    pub h: u8,
    pub mi: u8,
    pub s: u8,
    pub us: u32,
}

impl Naive {
    /// Checked as `datetime(...)` checks, with its messages.
    pub fn new(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64, us: i64) -> Result<Naive, String> {
        if !(1..=9999).contains(&y) {
            return Err(format!("year {y} is out of range"));
        }
        if !(1..=12).contains(&mo) {
            return Err("month must be in 1..12".into());
        }
        let dim = i64::from(days_in_month(y, mo as u32));
        if !(1..=dim).contains(&d) {
            return Err(format!(
                "day {d} must be in range 1..{dim} for month {mo} in year {y}"
            ));
        }
        if !(0..=23).contains(&h) {
            return Err("hour must be in 0..23".into());
        }
        if !(0..=59).contains(&mi) {
            return Err("minute must be in 0..59".into());
        }
        if !(0..=59).contains(&s) {
            return Err("second must be in 0..59".into());
        }
        if !(0..=999_999).contains(&us) {
            return Err("microsecond must be in 0..999999".into());
        }
        Ok(Naive {
            y: y as i32,
            mo: mo as u8,
            d: d as u8,
            h: h as u8,
            mi: mi as u8,
            s: s as u8,
            us: us as u32,
        })
    }

    pub fn midnight(y: i32, mo: u8, d: u8) -> Naive {
        Naive {
            y,
            mo,
            d,
            h: 0,
            mi: 0,
            s: 0,
            us: 0,
        }
    }

    pub fn days(&self) -> i64 {
        days_from_civil(i64::from(self.y), u32::from(self.mo), u32::from(self.d))
    }

    /// Microseconds since 1970-01-01T00:00 of the same (naive) clock.
    pub fn micros(&self) -> i64 {
        self.days() * US_PER_DAY
            + ((i64::from(self.h) * 60 + i64::from(self.mi)) * 60 + i64::from(self.s)) * US_PER_SEC
            + i64::from(self.us)
    }

    /// The wall time `us` microseconds after 1970-01-01T00:00, or Python's
    /// `OverflowError` outside years 1 to 9999.
    pub fn from_micros(us: i64) -> Result<Naive, Error> {
        let days = us.div_euclid(US_PER_DAY);
        let rest = us.rem_euclid(US_PER_DAY);
        let (y, mo, d) = civil_from_days(days);
        if !(1..=9999).contains(&y) {
            return Err(py_error("OverflowError: date value out of range".into()));
        }
        let secs = rest / US_PER_SEC;
        Ok(Naive {
            y: y as i32,
            mo: mo as u8,
            d: d as u8,
            h: (secs / 3600) as u8,
            mi: (secs / 60 % 60) as u8,
            s: (secs % 60) as u8,
            us: (rest % US_PER_SEC) as u32,
        })
    }

    fn jiff(&self) -> jiff::civil::DateTime {
        jiff::civil::DateTime::constant(
            self.y as i16,
            self.mo as i8,
            self.d as i8,
            self.h as i8,
            self.mi as i8,
            self.s as i8,
            (self.us * 1000) as i32,
        )
    }

    /// Monday is 0.
    pub fn weekday(&self) -> u32 {
        (self.days() + 3).rem_euclid(7) as u32
    }

    /// Day of the year, from 1.
    pub fn yday(&self) -> u32 {
        (self.days() - days_from_civil(i64::from(self.y), 1, 1) + 1) as u32
    }
}

// --- time zones ----------------------------------------------------------

/// A datetime's `tzinfo`.
#[derive(Debug, Clone)]
pub(super) enum Tz {
    /// `datetime.timezone.utc` (also what `timezone(timedelta(0))` gives).
    Utc,
    /// `zoneinfo.ZoneInfo(key)`: one object per key.
    Zone(jiff::tz::TimeZone, Arc<str>),
    /// ciso8601's `FixedOffset`, seconds east.
    Ciso(i32),
    /// `datetime.timezone(timedelta(seconds=…))`, never zero.
    Fixed(i32),
}

impl Tz {
    /// Python's `datetime.timezone(offset)`: zero is the UTC singleton.
    pub fn fixed(seconds: i32) -> Tz {
        if seconds == 0 {
            Tz::Utc
        } else {
            Tz::Fixed(seconds)
        }
    }

    /// The same object: subtraction and comparison then use wall times.
    /// Two fixed offsets are compared by value, which gives the same
    /// answers as Python's identity test.
    fn same(&self, other: &Tz) -> bool {
        match (self, other) {
            (Tz::Utc, Tz::Utc) => true,
            (Tz::Zone(_, a), Tz::Zone(_, b)) => a == b,
            (Tz::Ciso(a), Tz::Ciso(b)) | (Tz::Fixed(a), Tz::Fixed(b)) => a == b,
            _ => false,
        }
    }

    /// `utcoffset()` of a wall time, in seconds (PEP 495's `fold`).
    pub fn offset(&self, at: &Naive, fold: bool) -> i32 {
        match self {
            Tz::Utc => 0,
            Tz::Ciso(o) | Tz::Fixed(o) => *o,
            Tz::Zone(tz, _) => match tz.to_ambiguous_timestamp(at.jiff()).offset() {
                jiff::tz::AmbiguousOffset::Unambiguous { offset } => offset.seconds(),
                jiff::tz::AmbiguousOffset::Gap { before, after }
                | jiff::tz::AmbiguousOffset::Fold { before, after } => {
                    if fold { after } else { before }.seconds()
                }
            },
        }
    }

    /// `tzname()` of a wall time.
    pub fn name(&self, at: &Naive, fold: bool) -> String {
        match self {
            Tz::Utc => "UTC".into(),
            Tz::Ciso(o) | Tz::Fixed(o) => {
                let mut s = String::from("UTC");
                push_offset(&mut s, i64::from(*o) * US_PER_SEC, true);
                s
            }
            Tz::Zone(tz, _) => {
                let ambiguous = tz.to_ambiguous_timestamp(at.jiff()).offset();
                // The instant whose abbreviation it is: in a gap, the wall
                // time read with the other offset is on the chosen side.
                let offset = match ambiguous {
                    jiff::tz::AmbiguousOffset::Unambiguous { offset } => offset,
                    jiff::tz::AmbiguousOffset::Fold { before, after } => {
                        if fold {
                            after
                        } else {
                            before
                        }
                    }
                    jiff::tz::AmbiguousOffset::Gap { before, after } => {
                        if fold {
                            before
                        } else {
                            after
                        }
                    }
                };
                let at = at.micros() - i64::from(offset.seconds()) * US_PER_SEC;
                match jiff::Timestamp::from_microsecond(at) {
                    Ok(ts) => tz.to_offset_info(ts).abbreviation().to_owned(),
                    Err(_) => String::new(),
                }
            }
        }
    }

    /// The wall time of an instant (microseconds since the epoch, UTC) in
    /// this zone, with `fold` set on the second of two repeated times.
    pub fn wall_of(&self, us: i64) -> Result<(Naive, bool), Error> {
        let off = match self {
            Tz::Utc => 0,
            Tz::Ciso(o) | Tz::Fixed(o) => *o,
            Tz::Zone(tz, _) => match jiff::Timestamp::from_microsecond(us) {
                Ok(ts) => tz.to_offset(ts).seconds(),
                Err(_) => return Err(py_error("OverflowError: date value out of range".into())),
            },
        };
        let wall = Naive::from_micros(us + i64::from(off) * US_PER_SEC)?;
        let fold = match self {
            Tz::Zone(tz, _) => matches!(
                tz.to_ambiguous_timestamp(wall.jiff()).offset(),
                jiff::tz::AmbiguousOffset::Fold { before, after }
                    if after.seconds() == off && before != after
            ),
            _ => false,
        };
        Ok((wall, fold))
    }

    fn str(&self) -> String {
        match self {
            Tz::Zone(_, key) => key.to_string(),
            _ => self.name(&Naive::midnight(1970, 1, 1), false),
        }
    }

    fn repr(&self, out: &mut String) {
        match self {
            Tz::Utc => out.push_str("datetime.timezone.utc"),
            Tz::Zone(_, key) => {
                out.push_str("zoneinfo.ZoneInfo(key=");
                crate::py::repr_str(out, key);
                out.push(')');
            }
            Tz::Ciso(_) => out.push_str(&self.str()),
            Tz::Fixed(o) => {
                out.push_str("datetime.timezone(");
                TimeDelta(i128::from(*o) * i128::from(US_PER_SEC)).repr(out);
                out.push(')');
            }
        }
    }
}

/// `+HH:MM[:SS[.ffffff]]` (or without colons).
fn push_offset(out: &mut String, us: i64, colon: bool) {
    let sign = if us < 0 { '-' } else { '+' };
    let us = us.unsigned_abs();
    let (h, m, s, f) = (
        us / 3_600_000_000,
        us / 60_000_000 % 60,
        us / 1_000_000 % 60,
        us % 1_000_000,
    );
    let sep = if colon { ":" } else { "" };
    let _ = write!(out, "{sign}{h:02}{sep}{m:02}");
    if s != 0 || f != 0 {
        let _ = write!(out, "{sep}{s:02}");
        if f != 0 {
            let _ = write!(out, ".{f:06}");
        }
    }
}

/// A `tzinfo` value (`now().tzinfo`).
#[derive(Debug)]
pub(super) struct TzInfo(pub Tz);

impl Object for TzInfo {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.str())
    }
}

// --- timedelta -----------------------------------------------------------

/// `datetime.timedelta`, as microseconds (within ±999999999 days).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct TimeDelta(pub i128);

fn overflow_days(days: i128) -> Error {
    py_error(format!(
        "OverflowError: days={days}; must have magnitude <= {MAX_DAYS}"
    ))
}

impl TimeDelta {
    /// Checked against `timedelta`'s range, with its message.
    pub fn new(us: i128) -> Result<TimeDelta, Error> {
        let days = us.div_euclid(i128::from(US_PER_DAY));
        // `microseconds_to_delta_ex`: the days must fit a C int first.
        if i32::try_from(days).is_err() {
            return Err(py_error(
                "OverflowError: Python int too large to convert to C int".into(),
            ));
        }
        if days.abs() > i128::from(MAX_DAYS) {
            return Err(overflow_days(days));
        }
        Ok(TimeDelta(us))
    }

    pub fn value(us: i128) -> Result<Value, Error> {
        TimeDelta::new(us).map(Value::from_object)
    }

    fn parts(&self) -> (i128, i128, i128) {
        let days = self.0.div_euclid(i128::from(US_PER_DAY));
        let rest = self.0.rem_euclid(i128::from(US_PER_DAY));
        (
            days,
            rest / i128::from(US_PER_SEC),
            rest % i128::from(US_PER_SEC),
        )
    }

    /// `str(timedelta)`.
    pub fn str(&self, out: &mut String) {
        let (days, secs, us) = self.parts();
        if days != 0 {
            let plural = if days.abs() != 1 { "s" } else { "" };
            let _ = write!(out, "{days} day{plural}, ");
        }
        let _ = write!(
            out,
            "{}:{:02}:{:02}",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        );
        if us != 0 {
            let _ = write!(out, ".{us:06}");
        }
    }

    pub fn repr(&self, out: &mut String) {
        let (days, secs, us) = self.parts();
        out.push_str("datetime.timedelta(");
        let mut parts = Vec::new();
        if days != 0 {
            parts.push(format!("days={days}"));
        }
        if secs != 0 {
            parts.push(format!("seconds={secs}"));
        }
        if us != 0 {
            parts.push(format!("microseconds={us}"));
        }
        if parts.is_empty() {
            out.push('0');
        }
        out.push_str(&parts.join(", "));
        out.push(')');
    }

    /// `total_seconds()`: microseconds / 10**6, correctly rounded.
    pub fn total_seconds(&self) -> f64 {
        int_true_div(self.0, 1_000_000)
    }
}

/// Python's `int / int`: the exact quotient, rounded once.
pub(super) fn int_true_div(a: i128, b: i128) -> f64 {
    if a.unsigned_abs() < (1 << 53) && b.unsigned_abs() < (1 << 53) {
        return a as f64 / b as f64;
    }
    // Beyond 2**53: the quotient's integer part and remainder, combined.
    let q = a / b;
    let r = a % b;
    q as f64 + r as f64 / b as f64
}

/// `_divide_and_round`: `a / b` rounded half to even.
fn div_round_half_even(a: i128, b: i128) -> Option<i128> {
    if b == 0 {
        return None;
    }
    // Floor division and rounding read the same with both signs flipped.
    let (a, b) = if b < 0 {
        (a.checked_neg()?, -b)
    } else {
        (a, b)
    };
    let q = a.div_euclid(b);
    let twice = a.rem_euclid(b).checked_mul(2)?;
    Some(if twice > b || (twice == b && q % 2 != 0) {
        q + 1
    } else {
        q
    })
}

/// A finite float as `m · 2**e`, `m` an odd (or zero) integer with its
/// sign: `float.as_integer_ratio()` without the big integers.
fn mantissa_exponent(x: f64) -> (i128, i32) {
    if x == 0.0 {
        return (0, 0);
    }
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1 << 52) - 1);
    let (mut m, mut e) = if exp == 0 {
        (i128::from(frac), -1074)
    } else {
        (i128::from(frac | (1 << 52)), exp - 1075)
    };
    while m % 2 == 0 {
        m /= 2;
        e += 1;
    }
    (if bits >> 63 == 1 { -m } else { m }, e)
}

/// `us * x` (or `us / x`) rounded half to even, exactly, as CPython does
/// with `x.as_integer_ratio()` and big integers: `None` when the result is
/// past what a timedelta can hold. `x` is finite (and not zero to divide).
fn scale(us: i128, x: f64, divide: bool) -> Option<i128> {
    let (m, e) = mantissa_exponent(x);
    if us == 0 || m == 0 {
        return Some(0);
    }
    // `|us| < 2**67` and `|m| < 2**53`: their product fits; a power of two
    // past 2**126 makes the result too large, or rounds it to zero.
    let pow = |k: i32| -> Option<i128> { (k <= 126).then(|| 1i128 << k) };
    let (num, den) = match (divide, e >= 0) {
        // us · m · 2**e
        (false, true) => (us.checked_mul(m)?.checked_mul(pow(e)?)?, 1),
        // us · m / 2**-e
        (false, false) => match pow(-e) {
            Some(d) => (us.checked_mul(m)?, d),
            None => return Some(0),
        },
        // us · 2**-e / m
        (true, false) => (us.checked_mul(pow(-e)?)?, m),
        // us / (m · 2**e)
        (true, true) => match pow(e).and_then(|p| p.checked_mul(m)) {
            Some(d) => (us, d),
            None => return Some(0),
        },
    };
    div_round_half_even(num, den)
}

impl Object for TimeDelta {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn is_true(self: &Arc<Self>) -> bool {
        self.0 != 0
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let (days, secs, us) = self.parts();
        Some(match key.as_str()? {
            "days" => Value::from(days),
            "seconds" => Value::from(secs),
            "microseconds" => Value::from(us),
            _ => return no_attribute(&**self, key),
        })
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        match method {
            "total_seconds" => {
                no_args("total_seconds", args)?;
                Ok(Value::from(self.total_seconds()))
            }
            _ => Err(Error::from(minijinja::ErrorKind::UnknownMethod)),
        }
    }

    fn custom_cmp(self: &Arc<Self>, other: &DynObject) -> Option<Ordering> {
        other.downcast_ref::<TimeDelta>().map(|o| self.0.cmp(&o.0))
    }

    fn binop(
        self: &Arc<Self>,
        op: BinOp,
        other: &Value,
        reflected: bool,
    ) -> Option<Result<Value, Error>> {
        Some(timedelta_op(self, op, other, reflected))
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.str(&mut s);
        f.write_str(&s)
    }
}

fn no_args(name: &str, args: &[Value]) -> Result<(), Error> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(py_error(format!(
            "TypeError: {name}() takes no arguments ({} given)",
            args.len()
        )))
    }
}

/// `'<' not supported …` or `unsupported operand type(s) …`.
fn op_error(op: BinOp, a: &Value, b: &Value) -> Error {
    let sym = op_symbol(op);
    match op {
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => py_error(format!(
            "TypeError: '{sym}' not supported between instances of '{}' and '{}'",
            type_name(a),
            type_name(b)
        )),
        BinOp::Neg => py_error(format!(
            "TypeError: bad operand type for unary -: '{}'",
            type_name(a)
        )),
        _ => py_error(format!(
            "TypeError: unsupported operand type(s) for {sym}: '{}' and '{}'",
            type_name(a),
            type_name(b)
        )),
    }
}

fn op_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub | BinOp::Neg => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::FloorDiv => "//",
        BinOp::Rem => "%",
        BinOp::Pow => "** or pow()",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        _ => ">=",
    }
}

/// `==` and `!=` between unrelated types: Python answers by identity.
fn unrelated(op: BinOp, a: &Value, b: &Value, reflected: bool) -> Result<Value, Error> {
    match op {
        BinOp::Eq => Ok(Value::from(false)),
        BinOp::Ne => Ok(Value::from(true)),
        _ if reflected => Err(op_error(op, b, a)),
        _ => Err(op_error(op, a, b)),
    }
}

fn compare(op: BinOp, ord: Ordering) -> Value {
    Value::from(match op {
        BinOp::Eq => ord == Ordering::Equal,
        BinOp::Ne => ord != Ordering::Equal,
        BinOp::Lt => ord == Ordering::Less,
        BinOp::Le => ord != Ordering::Greater,
        BinOp::Gt => ord == Ordering::Greater,
        _ => ord != Ordering::Less,
    })
}

fn is_comparison(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
    )
}

/// An `int` (or `bool`) operand.
fn as_int(v: &Value) -> Option<i128> {
    match v.kind() {
        ValueKind::Bool => Some(i128::from(v.is_true())),
        ValueKind::Number if v.is_integer() => i128::try_from(v.clone()).ok(),
        _ => None,
    }
}

fn as_float(v: &Value) -> Option<f64> {
    (v.kind() == ValueKind::Number && !v.is_integer())
        .then(|| f64::try_from(v.clone()).ok())
        .flatten()
}

fn timedelta_op(td: &TimeDelta, op: BinOp, other: &Value, reflected: bool) -> Result<Value, Error> {
    let me = Value::from_object(*td);
    let (lhs, rhs) = if reflected {
        (other, &me)
    } else {
        (&me, other)
    };
    if op == BinOp::Neg {
        return TimeDelta::value(-td.0);
    }
    let o = other.downcast_object_ref::<TimeDelta>();
    if is_comparison(op) {
        return match o {
            Some(o) => {
                let ord = if reflected {
                    o.0.cmp(&td.0)
                } else {
                    td.0.cmp(&o.0)
                };
                Ok(compare(op, ord))
            }
            None => unrelated(op, lhs, rhs, false),
        };
    }
    match (op, o) {
        (BinOp::Add, Some(o)) => TimeDelta::value(td.0 + o.0),
        (BinOp::Sub, Some(o)) => {
            let (a, b) = if reflected { (o.0, td.0) } else { (td.0, o.0) };
            TimeDelta::value(a - b)
        }
        (BinOp::Div, Some(o)) => {
            let (a, b) = if reflected { (o.0, td.0) } else { (td.0, o.0) };
            if b == 0 {
                return Err(py_error("ZeroDivisionError: division by zero".into()));
            }
            Ok(Value::from(int_true_div(a, b)))
        }
        (BinOp::FloorDiv, Some(o)) => {
            let (a, b) = if reflected { (o.0, td.0) } else { (td.0, o.0) };
            if b == 0 {
                return Err(py_error(
                    "ZeroDivisionError: integer division or modulo by zero".into(),
                ));
            }
            Ok(Value::from(
                a.div_euclid(b) - i128::from(a.rem_euclid(b) != 0 && b < 0),
            ))
        }
        (BinOp::Rem, Some(o)) => {
            let (a, b) = if reflected { (o.0, td.0) } else { (td.0, o.0) };
            if b == 0 {
                return Err(py_error(
                    "ZeroDivisionError: integer division or modulo by zero".into(),
                ));
            }
            let r = a.rem_euclid(b);
            TimeDelta::value(if r != 0 && b < 0 { r + b } else { r })
        }
        (BinOp::Mul, None) => {
            if let Some(n) = as_int(other) {
                return match td.0.checked_mul(n) {
                    Some(us) => TimeDelta::value(us),
                    None => Err(overflow_days(i128::MAX / i128::from(US_PER_DAY))),
                };
            }
            if let Some(x) = as_float(other) {
                if !x.is_finite() {
                    return Err(py_error(if x.is_nan() {
                        "ValueError: cannot convert NaN to integer ratio".into()
                    } else {
                        "OverflowError: cannot convert Infinity to integer ratio".into()
                    }));
                }
                return match scale(td.0, x, false) {
                    Some(us) => TimeDelta::value(us),
                    None => Err(py_error(
                        "OverflowError: Python int too large to convert to C int".into(),
                    )),
                };
            }
            if matches!(other.kind(), ValueKind::String | ValueKind::Seq) {
                return Err(py_error(format!(
                    "TypeError: can't multiply sequence by non-int of type '{}'",
                    type_name(&me)
                )));
            }
            Err(op_error(op, lhs, rhs))
        }
        (BinOp::Div | BinOp::FloorDiv, None) if !reflected => {
            if let Some(n) = as_int(other) {
                if n == 0 {
                    return Err(py_error(if op == BinOp::Div {
                        "ZeroDivisionError: division by zero".into()
                    } else {
                        "ZeroDivisionError: integer division or modulo by zero".into()
                    }));
                }
                let us = if op == BinOp::Div {
                    div_round_half_even(td.0, n)
                } else {
                    Some(td.0.div_euclid(n) - i128::from(td.0.rem_euclid(n) != 0 && n < 0))
                };
                return TimeDelta::value(us.unwrap_or(0));
            }
            if let (Some(x), BinOp::Div) = (as_float(other), op) {
                if x == 0.0 {
                    return Err(py_error("ZeroDivisionError: division by zero".into()));
                }
                if !x.is_finite() {
                    return Err(py_error(if x.is_nan() {
                        "ValueError: cannot convert NaN to integer ratio".into()
                    } else {
                        "OverflowError: cannot convert Infinity to integer ratio".into()
                    }));
                }
                return match scale(td.0, x, true) {
                    Some(us) => TimeDelta::value(us),
                    None => Err(py_error(
                        "OverflowError: Python int too large to convert to C int".into(),
                    )),
                };
            }
            Err(op_error(op, lhs, rhs))
        }
        // `datetime`, `date` + `timedelta`: theirs to answer.
        (BinOp::Add | BinOp::Sub, None)
            if other.downcast_object_ref::<DateTime>().is_some()
                || other.downcast_object_ref::<Date>().is_some() =>
        {
            match (
                other.downcast_object_ref::<DateTime>(),
                other.downcast_object_ref::<Date>(),
            ) {
                (Some(dt), _) => datetime_op(dt, op, &me, !reflected),
                (_, Some(d)) => date_op(d, op, &me, !reflected),
                _ => Err(op_error(op, lhs, rhs)),
            }
        }
        _ => Err(op_error(op, lhs, rhs)),
    }
}

// --- date ----------------------------------------------------------------

/// `datetime.date`.
#[derive(Debug, Clone, Copy)]
pub(super) struct Date {
    pub y: i32,
    pub mo: u8,
    pub d: u8,
}

impl Date {
    fn naive(&self) -> Naive {
        Naive::midnight(self.y, self.mo, self.d)
    }

    fn str(&self, out: &mut String) {
        let _ = write!(out, "{:04}-{:02}-{:02}", self.y, self.mo, self.d);
    }
}

impl Object for Date {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        Some(match key.as_str()? {
            "year" => Value::from(self.y),
            "month" => Value::from(self.mo),
            "day" => Value::from(self.d),
            _ => return no_attribute(&**self, key),
        })
    }

    fn call_method(
        self: &Arc<Self>,
        state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let n = self.naive();
        match method {
            "isoformat" => {
                no_args(method, args)?;
                let mut s = String::new();
                self.str(&mut s);
                Ok(Value::from(s))
            }
            "weekday" => no_args(method, args).map(|()| Value::from(n.weekday())),
            "isoweekday" => no_args(method, args).map(|()| Value::from(n.weekday() + 1)),
            "strftime" => strftime_method(state, &n, None, false, args),
            _ => Err(Error::from(minijinja::ErrorKind::UnknownMethod)),
        }
    }

    fn custom_cmp(self: &Arc<Self>, other: &DynObject) -> Option<Ordering> {
        other
            .downcast_ref::<Date>()
            .map(|o| self.naive().cmp(&o.naive()))
    }

    fn binop(
        self: &Arc<Self>,
        op: BinOp,
        other: &Value,
        reflected: bool,
    ) -> Option<Result<Value, Error>> {
        Some(date_op(self, op, other, reflected))
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.str(&mut s);
        f.write_str(&s)
    }
}

fn date_op(d: &Date, op: BinOp, other: &Value, reflected: bool) -> Result<Value, Error> {
    let me = Value::from_object(*d);
    let (lhs, rhs) = if reflected {
        (other, &me)
    } else {
        (&me, other)
    };
    if is_comparison(op) {
        if let Some(o) = other.downcast_object_ref::<Date>() {
            let ord = if reflected {
                o.naive().cmp(&d.naive())
            } else {
                d.naive().cmp(&o.naive())
            };
            return Ok(compare(op, ord));
        }
        // A `datetime` included: `==` is false, ordering a `TypeError`.
        return unrelated(op, lhs, rhs, false);
    }
    match op {
        BinOp::Add | BinOp::Sub => {
            if let Some(td) = other.downcast_object_ref::<TimeDelta>() {
                if op == BinOp::Sub && reflected {
                    return Err(op_error(op, lhs, rhs));
                }
                // Only the days count.
                let days = td.0.div_euclid(i128::from(US_PER_DAY));
                let days = if op == BinOp::Sub { -days } else { days };
                let base = i128::from(d.naive().days()) + days;
                let n = i64::try_from(base)
                    .ok()
                    .and_then(|b| Naive::from_micros(b.checked_mul(US_PER_DAY)?).ok())
                    .ok_or_else(|| py_error("OverflowError: date value out of range".into()))?;
                return Ok(Value::from_object(Date {
                    y: n.y,
                    mo: n.mo,
                    d: n.d,
                }));
            }
            if let (BinOp::Sub, Some(o)) = (op, other.downcast_object_ref::<Date>()) {
                let (a, b) = if reflected { (o, d) } else { (d, o) };
                return TimeDelta::value(
                    i128::from(a.naive().days() - b.naive().days()) * i128::from(US_PER_DAY),
                );
            }
            Err(op_error(op, lhs, rhs))
        }
        _ => Err(op_error(op, lhs, rhs)),
    }
}

// --- time ----------------------------------------------------------------

/// `datetime.time` (naive: what `datetime.time()` gives).
#[derive(Debug, Clone, Copy)]
pub(super) struct Time {
    pub h: u8,
    pub mi: u8,
    pub s: u8,
    pub us: u32,
}

impl Time {
    fn naive(&self) -> Naive {
        Naive {
            y: 1900,
            mo: 1,
            d: 1,
            h: self.h,
            mi: self.mi,
            s: self.s,
            us: self.us,
        }
    }

    fn str(&self, out: &mut String) {
        let _ = write!(out, "{:02}:{:02}:{:02}", self.h, self.mi, self.s);
        if self.us != 0 {
            let _ = write!(out, ".{:06}", self.us);
        }
    }
}

impl Object for Time {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        Some(match key.as_str()? {
            "hour" => Value::from(self.h),
            "minute" => Value::from(self.mi),
            "second" => Value::from(self.s),
            "microsecond" => Value::from(self.us),
            "tzinfo" => Value::from(()),
            _ => return no_attribute(&**self, key),
        })
    }

    fn call_method(
        self: &Arc<Self>,
        state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        match method {
            "isoformat" => {
                no_args(method, args)?;
                let mut s = String::new();
                self.str(&mut s);
                Ok(Value::from(s))
            }
            "strftime" => strftime_method(state, &self.naive(), None, false, args),
            _ => Err(Error::from(minijinja::ErrorKind::UnknownMethod)),
        }
    }

    fn custom_cmp(self: &Arc<Self>, other: &DynObject) -> Option<Ordering> {
        other
            .downcast_ref::<Time>()
            .map(|o| self.naive().cmp(&o.naive()))
    }

    fn binop(
        self: &Arc<Self>,
        op: BinOp,
        other: &Value,
        reflected: bool,
    ) -> Option<Result<Value, Error>> {
        let me = Value::from_object(*self.as_ref());
        let (lhs, rhs) = if reflected {
            (other, &me)
        } else {
            (&me, other)
        };
        if is_comparison(op) {
            if let Some(o) = other.downcast_object_ref::<Time>() {
                let ord = if reflected {
                    o.naive().cmp(&self.naive())
                } else {
                    self.naive().cmp(&o.naive())
                };
                return Some(Ok(compare(op, ord)));
            }
            return Some(unrelated(op, lhs, rhs, false));
        }
        Some(Err(op_error(op, lhs, rhs)))
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.str(&mut s);
        f.write_str(&s)
    }
}

// --- datetime ------------------------------------------------------------

/// `datetime.datetime`.
#[derive(Debug, Clone)]
pub(super) struct DateTime {
    pub wall: Naive,
    pub fold: bool,
    pub tz: Option<Tz>,
}

impl DateTime {
    pub fn naive(wall: Naive) -> DateTime {
        DateTime {
            wall,
            fold: false,
            tz: None,
        }
    }

    /// An instant in a zone (`fromtimestamp`, `astimezone`).
    pub fn at(us: i64, tz: Tz) -> Result<DateTime, Error> {
        let (wall, fold) = tz.wall_of(us)?;
        Ok(DateTime {
            wall,
            fold,
            tz: Some(tz),
        })
    }

    pub fn value(self) -> Value {
        Value::from_object(self)
    }

    /// `utcoffset()` in seconds; `None` when naive.
    pub fn offset(&self) -> Option<i32> {
        self.tz.as_ref().map(|tz| tz.offset(&self.wall, self.fold))
    }

    /// Microseconds since the epoch: aware by its offset, naive in the
    /// configured time zone.
    pub fn instant(&self) -> i64 {
        let off = match &self.tz {
            Some(tz) => tz.offset(&self.wall, self.fold),
            None => home::local_tz().offset(&self.wall, self.fold),
        };
        self.wall.micros() - i64::from(off) * US_PER_SEC
    }

    /// `timestamp()`.
    pub fn timestamp(&self) -> f64 {
        int_true_div(i128::from(self.instant()), i128::from(US_PER_SEC))
    }

    /// `astimezone(tz)`.
    pub fn to_zone(&self, tz: Tz) -> Result<DateTime, Error> {
        DateTime::at(self.instant(), tz)
    }

    /// `isoformat(sep)`.
    pub fn iso(&self, out: &mut String, sep: char) {
        let w = &self.wall;
        let _ = write!(
            out,
            "{:04}-{:02}-{:02}{sep}{:02}:{:02}:{:02}",
            w.y, w.mo, w.d, w.h, w.mi, w.s
        );
        if w.us != 0 {
            let _ = write!(out, ".{:06}", w.us);
        }
        if let Some(off) = self.offset() {
            push_offset(out, i64::from(off) * US_PER_SEC, true);
        }
    }

    fn repr(&self, out: &mut String) {
        let w = &self.wall;
        let _ = write!(
            out,
            "datetime.datetime({}, {}, {}, {}, {}",
            w.y, w.mo, w.d, w.h, w.mi
        );
        if w.us != 0 {
            let _ = write!(out, ", {}, {}", w.s, w.us);
        } else if w.s != 0 {
            let _ = write!(out, ", {}", w.s);
        }
        if self.fold {
            out.push_str(", fold=1");
        }
        if let Some(tz) = &self.tz {
            out.push_str(", tzinfo=");
            tz.repr(out);
        }
        out.push(')');
    }

    fn replace(&self, args: &[Value]) -> Result<Value, Error> {
        let (kwargs, positional) = split_kwargs(args);
        if !positional.is_empty() {
            return Err(py_error(
                "NotImplementedError: hars does not support datetime.replace() with positional arguments yet".into(),
            ));
        }
        let w = &self.wall;
        let mut f = [
            i64::from(w.y),
            i64::from(w.mo),
            i64::from(w.d),
            i64::from(w.h),
            i64::from(w.mi),
            i64::from(w.s),
            i64::from(w.us),
        ];
        let mut tz = self.tz.clone();
        let mut fold = self.fold;
        const NAMES: [&str; 7] = [
            "year",
            "month",
            "day",
            "hour",
            "minute",
            "second",
            "microsecond",
        ];
        for (k, v) in kwargs {
            if let Some(i) = NAMES.iter().position(|n| *n == k) {
                f[i] = py_index(&v)?;
            } else if k == "tzinfo" {
                tz = if v.is_none() {
                    None
                } else if let Some(t) = v.downcast_object_ref::<TzInfo>() {
                    Some(t.0.clone())
                } else {
                    return Err(py_error(format!(
                        "TypeError: tzinfo argument must be None or of a tzinfo subclass, not type '{}'",
                        type_name(&v)
                    )));
                };
            } else if k == "fold" {
                let n = py_index(&v)?;
                if !(0..=1).contains(&n) {
                    return Err(py_error("ValueError: fold must be either 0 or 1".into()));
                }
                fold = n == 1;
            } else {
                return Err(py_error(format!(
                    "TypeError: replace() got an unexpected keyword argument '{k}'"
                )));
            }
        }
        let wall = Naive::new(f[0], f[1], f[2], f[3], f[4], f[5], f[6])
            .map_err(|m| py_error(format!("ValueError: {m}")))?;
        Ok(DateTime { wall, fold, tz }.value())
    }
}

/// Keyword arguments (in order) and the positional ones.
pub(super) fn split_kwargs(args: &[Value]) -> (Vec<(String, Value)>, &[Value]) {
    match args.last() {
        Some(last) if last.is_kwargs() => {
            let mut out = Vec::new();
            if let Ok(keys) = last.try_iter() {
                for k in keys {
                    let v = last.get_item(&k).unwrap_or_default();
                    out.push((k.as_str().unwrap_or_default().to_owned(), v));
                }
            }
            (out, &args[..args.len() - 1])
        }
        _ => (Vec::new(), args),
    }
}

/// `operator.index`: an `int` (or `bool`).
pub(super) fn py_index(v: &Value) -> Result<i64, Error> {
    match as_int(v) {
        Some(i) => i64::try_from(i).map_err(|_| {
            py_error("OverflowError: Python int too large to convert to C int".into())
        }),
        None => Err(py_error(format!(
            "TypeError: '{}' object cannot be interpreted as an integer",
            type_name(v)
        ))),
    }
}

impl Object for DateTime {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let w = &self.wall;
        Some(match key.as_str()? {
            "year" => Value::from(w.y),
            "month" => Value::from(w.mo),
            "day" => Value::from(w.d),
            "hour" => Value::from(w.h),
            "minute" => Value::from(w.mi),
            "second" => Value::from(w.s),
            "microsecond" => Value::from(w.us),
            "fold" => Value::from(u8::from(self.fold)),
            "tzinfo" => match &self.tz {
                Some(tz) => Value::from_object(TzInfo(tz.clone())),
                None => Value::from(()),
            },
            _ => return no_attribute(&**self, key),
        })
    }

    fn call_method(
        self: &Arc<Self>,
        state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let w = &self.wall;
        match method {
            "isoformat" => {
                let sep = match args {
                    [] => 'T',
                    [s] => s
                        .as_str()
                        .filter(|s| s.chars().count() == 1)
                        .and_then(|s| s.chars().next())
                        .ok_or_else(|| {
                            py_error(
                                "TypeError: isoformat() argument 1 must be a unicode character"
                                    .into(),
                            )
                        })?,
                    _ => {
                        return Err(py_error(
                            "NotImplementedError: hars does not support isoformat() with these arguments yet".into(),
                        ));
                    }
                };
                let mut s = String::new();
                self.iso(&mut s, sep);
                Ok(Value::from(s))
            }
            "timestamp" => no_args(method, args).map(|()| Value::from(self.timestamp())),
            "date" => no_args(method, args).map(|()| {
                Value::from_object(Date {
                    y: w.y,
                    mo: w.mo,
                    d: w.d,
                })
            }),
            "time" => no_args(method, args).map(|()| {
                Value::from_object(Time {
                    h: w.h,
                    mi: w.mi,
                    s: w.s,
                    us: w.us,
                })
            }),
            "weekday" => no_args(method, args).map(|()| Value::from(w.weekday())),
            "isoweekday" => no_args(method, args).map(|()| Value::from(w.weekday() + 1)),
            "utcoffset" => no_args(method, args).and_then(|()| match self.offset() {
                Some(o) => TimeDelta::value(i128::from(o) * i128::from(US_PER_SEC)),
                None => Ok(Value::from(())),
            }),
            "tzname" => no_args(method, args).map(|()| match &self.tz {
                Some(tz) => Value::from(tz.name(w, self.fold)),
                None => Value::from(()),
            }),
            "replace" => self.replace(args),
            "strftime" => strftime_method(state, w, self.tz.as_ref(), self.fold, args),
            _ => Err(Error::from(minijinja::ErrorKind::UnknownMethod)),
        }
    }

    fn custom_cmp(self: &Arc<Self>, other: &DynObject) -> Option<Ordering> {
        let other = other.downcast_ref::<DateTime>()?;
        match datetime_order(self, other) {
            Ok(ord) => Some(ord),
            Err(e) => {
                home::poison(e);
                Some(Ordering::Equal)
            }
        }
    }

    fn binop(
        self: &Arc<Self>,
        op: BinOp,
        other: &Value,
        reflected: bool,
    ) -> Option<Result<Value, Error>> {
        Some(datetime_op(self, op, other, reflected))
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.iso(&mut s, ' ');
        f.write_str(&s)
    }
}

/// Both aware with the same `tzinfo` (Python's identity test).
fn same_zone(a: &DateTime, b: &DateTime) -> bool {
    match (&a.tz, &b.tz) {
        (Some(x), Some(y)) => x.same(y),
        _ => true,
    }
}

/// Whether flipping `fold` would change the UTC offset.
fn fold_matters(dt: &DateTime) -> bool {
    dt.tz
        .as_ref()
        .is_some_and(|tz| tz.offset(&dt.wall, dt.fold) != tz.offset(&dt.wall, !dt.fold))
}

/// Python's ordering of two datetimes; naive against aware raises.
fn datetime_order(a: &DateTime, b: &DateTime) -> Result<Ordering, String> {
    match (&a.tz, &b.tz) {
        (None, None) => Ok(a.wall.cmp(&b.wall)),
        (Some(x), Some(y)) if x.same(y) => Ok(a.wall.cmp(&b.wall)),
        (Some(_), Some(_)) => Ok(a.instant().cmp(&b.instant())),
        _ => Err("TypeError: can't compare offset-naive and offset-aware datetimes".into()),
    }
}

fn datetime_op(dt: &DateTime, op: BinOp, other: &Value, reflected: bool) -> Result<Value, Error> {
    let me = Value::from_object(dt.clone());
    let (lhs, rhs) = if reflected {
        (other, &me)
    } else {
        (&me, other)
    };
    if is_comparison(op) {
        if let Some(o) = other.downcast_object_ref::<DateTime>() {
            let (a, b) = if reflected { (o, dt) } else { (dt, o) };
            return match datetime_order(a, b) {
                // PEP 495: between zones, a time whose offset depends on
                // `fold` equals nothing (`pep495_eq_exception`).
                Ok(Ordering::Equal)
                    if matches!(op, BinOp::Eq | BinOp::Ne)
                        && !same_zone(a, b)
                        && (fold_matters(a) || fold_matters(b)) =>
                {
                    Ok(Value::from(op == BinOp::Ne))
                }
                Ok(ord) => Ok(compare(op, ord)),
                Err(_) if matches!(op, BinOp::Eq | BinOp::Ne) => Ok(Value::from(op == BinOp::Ne)),
                Err(e) => Err(py_error(e)),
            };
        }
        // A `date` included: `==` is false, ordering a `TypeError`.
        return unrelated(op, lhs, rhs, false);
    }
    match op {
        BinOp::Add | BinOp::Sub => {
            if let Some(td) = other.downcast_object_ref::<TimeDelta>() {
                if op == BinOp::Sub && reflected {
                    return Err(op_error(op, lhs, rhs));
                }
                let delta = if op == BinOp::Sub { -td.0 } else { td.0 };
                let us = i64::try_from(i128::from(dt.wall.micros()) + delta)
                    .map_err(|_| py_error("OverflowError: date value out of range".into()))?;
                let wall = Naive::from_micros(us)?;
                return Ok(DateTime {
                    wall,
                    fold: false,
                    tz: dt.tz.clone(),
                }
                .value());
            }
            if let (BinOp::Sub, Some(o)) = (op, other.downcast_object_ref::<DateTime>()) {
                let (a, b) = if reflected { (o, dt) } else { (dt, o) };
                let us = match (&a.tz, &b.tz) {
                    (None, None) => a.wall.micros() - b.wall.micros(),
                    (Some(x), Some(y)) if x.same(y) => a.wall.micros() - b.wall.micros(),
                    (Some(_), Some(_)) => a.instant() - b.instant(),
                    _ => {
                        return Err(py_error(
                            "TypeError: can't subtract offset-naive and offset-aware datetimes"
                                .into(),
                        ));
                    }
                };
                return TimeDelta::value(i128::from(us));
            }
            Err(op_error(op, lhs, rhs))
        }
        _ => Err(op_error(op, lhs, rhs)),
    }
}

/// An attribute Python doesn't have: Jinja's undefined, with its hint.
fn no_attribute<T: Object + 'static>(this: &T, key: &Value) -> Option<Value> {
    let _ = this;
    let name = std::any::type_name::<T>();
    let class = match name.rsplit("::").next() {
        Some("DateTime") => "datetime.datetime",
        Some("Date") => "datetime.date",
        Some("Time") => "datetime.time",
        _ => "datetime.timedelta",
    };
    let mut message = format!("'{class}' object has no attribute ");
    crate::py::repr_str(&mut message, key.as_str().unwrap_or_default());
    super::note_miss(message);
    None
}

/// Python's type name of one of these values.
pub(super) fn py_type_name(v: &Value) -> Option<&'static str> {
    let o = v.as_object()?;
    Some(if o.downcast_ref::<DateTime>().is_some() {
        "datetime.datetime"
    } else if o.downcast_ref::<Date>().is_some() {
        "datetime.date"
    } else if o.downcast_ref::<Time>().is_some() {
        "datetime.time"
    } else if o.downcast_ref::<TimeDelta>().is_some() {
        "datetime.timedelta"
    } else {
        match o.downcast_ref::<TzInfo>()?.0 {
            Tz::Zone(..) => "zoneinfo.ZoneInfo",
            Tz::Ciso(_) => "FixedOffset",
            _ => "datetime.timezone",
        }
    })
}

/// Python's `repr()` of one of these values.
pub(super) fn py_repr(v: &Value, out: &mut String) -> bool {
    let Some(o) = v.as_object() else {
        return false;
    };
    if let Some(dt) = o.downcast_ref::<DateTime>() {
        dt.repr(out);
    } else if let Some(d) = o.downcast_ref::<Date>() {
        let _ = write!(out, "datetime.date({}, {}, {})", d.y, d.mo, d.d);
    } else if let Some(t) = o.downcast_ref::<Time>() {
        let _ = write!(out, "datetime.time({}, {}", t.h, t.mi);
        if t.us != 0 {
            let _ = write!(out, ", {}, {}", t.s, t.us);
        } else if t.s != 0 {
            let _ = write!(out, ", {}", t.s);
        }
        out.push(')');
    } else if let Some(td) = o.downcast_ref::<TimeDelta>() {
        td.repr(out);
    } else if let Some(tz) = o.downcast_ref::<TzInfo>() {
        tz.0.repr(out);
    } else {
        return false;
    }
    true
}

// --- strftime ------------------------------------------------------------

const DAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn strftime_method(
    state: &State,
    w: &Naive,
    tz: Option<&Tz>,
    fold: bool,
    args: &[Value],
) -> Result<Value, Error> {
    let fmt = match args {
        [f] if f.kind() == ValueKind::String => f.as_str().unwrap_or_default(),
        [f] => {
            return Err(py_error(format!(
                "TypeError: strftime() argument 1 must be str, not {}",
                type_name(f)
            )));
        }
        _ => {
            return Err(py_error(format!(
                "TypeError: strftime() takes exactly 1 argument ({} given)",
                args.len()
            )));
        }
    };
    strftime(state, w, tz, fold, fmt).map(Value::from)
}

/// ISO 8601 week-based year and week.
fn iso_week(w: &Naive) -> (i64, u32) {
    let wd = i64::from(w.weekday());
    let yday = i64::from(w.yday()) - 1;
    let thursday = yday - wd + 3;
    let y = i64::from(w.y);
    if thursday < 0 {
        let prev = if is_leap(y - 1) { 366 } else { 365 };
        return (y - 1, ((thursday + prev) / 7 + 1) as u32);
    }
    let len = if is_leap(y) { 366 } else { 365 };
    if thursday >= len {
        return (y + 1, 1);
    }
    (y, (thursday / 7 + 1) as u32)
}

/// `datetime.strftime`: `%f`, `%z`, `%:z` and `%Z` by Python, the rest as
/// glibc's in the C locale (with its `-`, `_`, `0` and `^` flags). Paid for
/// before it is built: no directive writes more than 26 characters.
pub(super) fn strftime(
    state: &State,
    w: &Naive,
    tz: Option<&Tz>,
    fold: bool,
    fmt: &str,
) -> Result<String, Error> {
    state.consume_fuel((fmt.len() as u64).saturating_mul(13) / 64)?;
    let mut out = String::with_capacity(fmt.len() * 2);
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut flag = None;
        while let Some(&f) = chars.peek() {
            if matches!(f, '-' | '_' | '0' | '^' | '#') {
                flag = Some(f);
                chars.next();
            } else {
                break;
            }
        }
        let Some(d) = chars.next() else {
            out.push('%');
            if let Some(f) = flag {
                out.push(f);
            }
            break;
        };
        // A number padded to `width` (glibc's flags).
        let num = |out: &mut String, n: i64, width: usize, space: bool| {
            let pad = match flag {
                Some('-') => 0,
                Some('_') => width,
                Some('0') => width,
                _ => width,
            };
            let space = match flag {
                Some('_') => true,
                Some('0') => false,
                _ => space,
            };
            let s = n.to_string();
            if pad > s.len() {
                for _ in 0..pad - s.len() {
                    out.push(if space { ' ' } else { '0' });
                }
            }
            out.push_str(&s);
        };
        let text = |out: &mut String, s: &str| {
            if flag == Some('^') {
                out.push_str(&s.to_uppercase());
            } else {
                out.push_str(s);
            }
        };
        let hour12 = if w.h % 12 == 0 { 12 } else { w.h % 12 };
        let weekday = w.weekday() as usize;
        match d {
            'a' => text(&mut out, &DAYS[weekday][..3]),
            'A' => text(&mut out, DAYS[weekday]),
            'b' | 'h' => text(&mut out, &MONTHS[w.mo as usize - 1][..3]),
            'B' => text(&mut out, MONTHS[w.mo as usize - 1]),
            'c' => {
                let _ = write!(
                    out,
                    "{} {} {:>2} {:02}:{:02}:{:02} {}",
                    &DAYS[weekday][..3],
                    &MONTHS[w.mo as usize - 1][..3],
                    w.d,
                    w.h,
                    w.mi,
                    w.s,
                    w.y
                );
            }
            'C' => num(&mut out, i64::from(w.y) / 100, 2, false),
            'd' => num(&mut out, i64::from(w.d), 2, false),
            'D' | 'x' => {
                let _ = write!(out, "{:02}/{:02}/{:02}", w.mo, w.d, w.y % 100);
            }
            'e' => num(&mut out, i64::from(w.d), 2, true),
            'F' => {
                let _ = write!(out, "{}-{:02}-{:02}", w.y, w.mo, w.d);
            }
            'G' => num(&mut out, iso_week(w).0, 1, false),
            'g' => num(&mut out, iso_week(w).0 % 100, 2, false),
            'H' => num(&mut out, i64::from(w.h), 2, false),
            'I' => num(&mut out, i64::from(hour12), 2, false),
            'j' => num(&mut out, i64::from(w.yday()), 3, false),
            'k' => num(&mut out, i64::from(w.h), 2, true),
            'l' => num(&mut out, i64::from(hour12), 2, true),
            'm' => num(&mut out, i64::from(w.mo), 2, false),
            'M' => num(&mut out, i64::from(w.mi), 2, false),
            'n' => out.push('\n'),
            'p' => text(&mut out, if w.h < 12 { "AM" } else { "PM" }),
            'P' => out.push_str(if w.h < 12 { "am" } else { "pm" }),
            'r' => {
                let _ = write!(
                    out,
                    "{:02}:{:02}:{:02} {}",
                    hour12,
                    w.mi,
                    w.s,
                    if w.h < 12 { "AM" } else { "PM" }
                );
            }
            'R' => {
                let _ = write!(out, "{:02}:{:02}", w.h, w.mi);
            }
            'S' => num(&mut out, i64::from(w.s), 2, false),
            't' => out.push('\t'),
            'T' | 'X' => {
                let _ = write!(out, "{:02}:{:02}:{:02}", w.h, w.mi, w.s);
            }
            'u' => num(&mut out, weekday as i64 + 1, 1, false),
            'U' => {
                let sunday = (weekday + 1) % 7;
                num(
                    &mut out,
                    (i64::from(w.yday()) - 1 + 7 - sunday as i64) / 7,
                    2,
                    false,
                );
            }
            'V' => num(&mut out, i64::from(iso_week(w).1), 2, false),
            'w' => num(&mut out, ((weekday + 1) % 7) as i64, 1, false),
            'W' => {
                num(
                    &mut out,
                    (i64::from(w.yday()) - 1 + 7 - weekday as i64) / 7,
                    2,
                    false,
                );
            }
            'y' => num(&mut out, i64::from(w.y) % 100, 2, false),
            'Y' => num(&mut out, i64::from(w.y), 4, false),
            'f' => {
                let _ = write!(out, "{:06}", w.us);
            }
            'z' => {
                if let Some(tz) = tz {
                    push_offset(&mut out, i64::from(tz.offset(w, fold)) * US_PER_SEC, false);
                }
            }
            ':' if chars.peek() == Some(&'z') => {
                chars.next();
                if let Some(tz) = tz {
                    push_offset(&mut out, i64::from(tz.offset(w, fold)) * US_PER_SEC, true);
                }
            }
            'Z' => {
                if let Some(tz) = tz {
                    out.push_str(&tz.name(w, fold));
                }
            }
            '%' => out.push('%'),
            other => {
                return Err(py_error(format!(
                    "NotImplementedError: hars does not support the strftime directive '%{other}' yet"
                )));
            }
        }
    }
    Ok(out)
}

// --- parsers -------------------------------------------------------------

/// A decimal digit (any Unicode `Nd`, as Python's `\d` and `int()`).
fn digit(c: char) -> Option<u32> {
    crate::entity_id::decimal_value(c)
}

/// Why a string isn't parsed.
#[derive(Debug, PartialEq)]
pub(super) enum Unparsed {
    /// Neither ciso8601 nor HA's regex reads it: `None`.
    Invalid,
    /// A form ciso8601 reads that hars doesn't yet (week and ordinal dates).
    Unsupported,
}

/// `dt_util.parse_datetime`: ciso8601 first, then HA's `DATETIME_RE`.
pub(super) fn parse_datetime(s: &str) -> Result<DateTime, Unparsed> {
    match ciso8601(s) {
        Ok(dt) => Ok(dt),
        Err(Unparsed::Unsupported) => Err(Unparsed::Unsupported),
        Err(Unparsed::Invalid) => datetime_re(s).ok_or(Unparsed::Invalid),
    }
}

/// ASCII digits read from a cursor.
struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn digits(&mut self, n: usize) -> Option<u32> {
        let end = self.i.checked_add(n)?;
        let part = self.b.get(self.i..end)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.i = end;
        Some(part.iter().fold(0, |a, d| a * 10 + u32::from(d - b'0')))
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn done(&self) -> bool {
        self.i >= self.b.len()
    }
}

/// ciso8601 2.3.3's `parse_datetime`, for calendar dates.
fn ciso8601(s: &str) -> Result<DateTime, Unparsed> {
    use Unparsed::{Invalid, Unsupported};
    let mut c = Cursor {
        b: s.as_bytes(),
        i: 0,
    };
    let year = c.digits(4).ok_or(Invalid)?;
    let extended = c.eat(b'-');
    if c.peek() == Some(b'W') {
        return Err(Unsupported);
    }
    // `YYYY-DDD` / `YYYYDDD`: an ordinal date.
    let rest = &c.b[c.i..];
    let ordinal = rest.len() >= 3
        && rest[..3].iter().all(u8::is_ascii_digit)
        && rest.get(3).is_none_or(|b| !b.is_ascii_digit());
    if ordinal && (extended || rest.len() == 3 || matches!(rest.get(3), Some(b'T' | b't' | b' '))) {
        return Err(Unsupported);
    }
    let month = c.digits(2).ok_or(Invalid)?;
    let day = if extended {
        if c.eat(b'-') {
            c.digits(2).ok_or(Invalid)?
        } else {
            1
        }
    } else {
        c.digits(2).ok_or(Invalid)?
    };
    let (mut h, mut mi, mut sec, mut us) = (0, 0, 0, 0);
    let mut tz = None;
    if !c.done() {
        if !matches!(c.peek(), Some(b'T' | b't' | b' ')) {
            return Err(Invalid);
        }
        c.i += 1;
        h = c.digits(2).ok_or(Invalid)?;
        let colon = |c: &mut Cursor| -> Option<bool> {
            if extended {
                if c.eat(b':') { Some(true) } else { Some(false) }
            } else {
                Some(c.peek().is_some_and(|b| b.is_ascii_digit()))
            }
        };
        if extended && c.peek().is_some_and(|b| b.is_ascii_digit()) {
            // Basic time after an extended date.
            return Err(Invalid);
        }
        if colon(&mut c) == Some(true) {
            mi = c.digits(2).ok_or(Invalid)?;
            if colon(&mut c) == Some(true) {
                sec = c.digits(2).ok_or(Invalid)?;
                if c.eat(b'.') || c.eat(b',') {
                    let start = c.i;
                    while c.peek().is_some_and(|b| b.is_ascii_digit()) {
                        c.i += 1;
                    }
                    let frac = &c.b[start..c.i];
                    if frac.is_empty() {
                        return Err(Invalid);
                    }
                    us = frac
                        .iter()
                        .chain(std::iter::repeat(&b'0'))
                        .take(6)
                        .fold(0, |a, d| a * 10 + u32::from(d - b'0'));
                }
            }
        }
        if !c.done() {
            match c.peek() {
                Some(b'Z' | b'z') => {
                    c.i += 1;
                    tz = Some(Tz::Utc);
                }
                Some(sign @ (b'+' | b'-')) => {
                    c.i += 1;
                    let oh = c.digits(2).ok_or(Invalid)?;
                    let om = if c.eat(b':') || c.peek().is_some_and(|b| b.is_ascii_digit()) {
                        c.digits(2).ok_or(Invalid)?
                    } else {
                        0
                    };
                    if oh > 23 || om > 59 {
                        return Err(Invalid);
                    }
                    let secs = ((oh * 60 + om) * 60) as i32;
                    let secs = if sign == b'-' { -secs } else { secs };
                    tz = Some(if secs == 0 { Tz::Utc } else { Tz::Ciso(secs) });
                }
                _ => return Err(Invalid),
            }
            if !c.done() {
                return Err(Invalid);
            }
        }
    }
    let (mut y, mut mo, mut d) = (i64::from(year), i64::from(month), i64::from(day));
    // `24:00` is midnight at the end of the day.
    let next_day = h == 24 && mi == 0 && sec == 0 && us == 0;
    if next_day {
        h = 0;
        if Naive::new(y, mo, d, 0, 0, 0, 0).is_err() {
            return Err(Invalid);
        }
        let (ny, nm, nd) = civil_from_days(days_from_civil(y, mo as u32, d as u32) + 1);
        (y, mo, d) = (ny, i64::from(nm), i64::from(nd));
    }
    let wall = Naive::new(
        y,
        mo,
        d,
        i64::from(h),
        i64::from(mi),
        i64::from(sec),
        i64::from(us),
    )
    .map_err(|_| Invalid)?;
    Ok(DateTime {
        wall,
        fold: false,
        tz,
    })
}

/// HA's `DATETIME_RE` fallback, with `re.match` and `$`.
fn datetime_re(s: &str) -> Option<DateTime> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let num = |i: &mut usize, min: usize, max: usize| -> Option<i64> {
        let start = *i;
        let mut v: i64 = 0;
        while *i < chars.len() && *i - start < max {
            match digit(chars[*i]) {
                Some(d) => v = v * 10 + i64::from(d),
                None => break,
            }
            *i += 1;
        }
        (*i - start >= min).then_some(v)
    };
    let lit = |i: &mut usize, c: char| -> bool {
        if chars.get(*i) == Some(&c) {
            *i += 1;
            true
        } else {
            false
        }
    };
    let y = num(&mut i, 4, 4)?;
    if !lit(&mut i, '-') {
        return None;
    }
    let mo = num(&mut i, 1, 2)?;
    if !lit(&mut i, '-') {
        return None;
    }
    let d = num(&mut i, 1, 2)?;
    if !(lit(&mut i, 'T') || lit(&mut i, ' ')) {
        return None;
    }
    let h = num(&mut i, 1, 2)?;
    if !lit(&mut i, ':') {
        return None;
    }
    let mi = num(&mut i, 1, 2)?;
    let (mut sec, mut us) = (0, 0);
    // `(?::second(?:\.us\d{0,6})?)?`: backtracks to nothing if it fails.
    let save = i;
    if lit(&mut i, ':') {
        match num(&mut i, 1, 2) {
            Some(s) => {
                sec = s;
                let save2 = i;
                if lit(&mut i, '.') {
                    let start = i;
                    match num(&mut i, 1, 6) {
                        Some(_) => {
                            let digits: String = chars[start..i]
                                .iter()
                                .map(|c| {
                                    char::from_digit(digit(*c).unwrap_or(0), 10).unwrap_or('0')
                                })
                                .collect();
                            us = format!("{digits:0<6}").parse().unwrap_or(0);
                            let _ = num(&mut i, 0, 6);
                        }
                        None => i = save2,
                    }
                }
            }
            None => i = save,
        }
    }
    let mut tz = None;
    if i < chars.len() {
        if lit(&mut i, 'Z') {
            tz = Some(Tz::Utc);
        } else if matches!(chars[i], '+' | '-') {
            let neg = chars[i] == '-';
            i += 1;
            let start = i;
            let oh = num(&mut i, 2, 2)?;
            let _ = start;
            let mut om = 0;
            let save = i;
            let colon = lit(&mut i, ':');
            match num(&mut i, 2, 2) {
                Some(m) => om = m,
                None => i = if colon { save } else { i },
            }
            let off = ((oh * 60 + om) * 60) as i32;
            tz = Some(Tz::fixed(if neg { -off } else { off }));
        }
    }
    if i != chars.len() {
        return None;
    }
    let wall = Naive::new(y, mo, d, h, mi, sec, us).ok()?;
    Some(DateTime {
        wall,
        fold: false,
        tz,
    })
}

/// `dt_util.parse_time`: `H:M[:S]` with Python's `int()`.
pub(super) fn parse_time(s: &str) -> Option<Time> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 {
        return None;
    }
    let h = super::num::py_int(parts[0], 10).ok()?;
    let m = super::num::py_int(parts[1], 10).ok()?;
    let sec = match parts.get(2) {
        Some(p) => super::num::py_int(p, 10).ok()?,
        None => 0,
    };
    ((0..=23).contains(&h) && (0..=59).contains(&m) && (0..=59).contains(&sec)).then_some(Time {
        h: h as u8,
        mi: m as u8,
        s: sec as u8,
        us: 0,
    })
}

/// `timedelta(days, seconds, microseconds, milliseconds, minutes, hours,
/// weeks)`: CPython's `delta_new`, accumulating whole units exactly and
/// the fractions of floats as it does, rounded half to even at the end.
pub(super) fn timedelta(parts: [(&'static str, Option<Value>); 7]) -> Result<TimeDelta, Error> {
    // Microseconds per unit, in `delta_new`'s order.
    const ORDER: [(usize, i128); 7] = [
        (2, 1),
        (3, 1000),
        (1, 1_000_000),
        (4, 60_000_000),
        (5, 3_600_000_000),
        (0, 86_400_000_000),
        (6, 604_800_000_000),
    ];
    let too_big = || py_error("OverflowError: Python int too large to convert to C int".into());
    let mut total: i128 = 0;
    let mut leftover: f64 = 0.0;
    for (i, factor) in ORDER {
        let (name, Some(v)) = &parts[i] else {
            continue;
        };
        if let Some(n) = as_int(v) {
            total = n
                .checked_mul(factor)
                .and_then(|p| total.checked_add(p))
                .ok_or_else(too_big)?;
        } else if let Some(x) = as_float(v) {
            if !x.is_finite() {
                return Err(py_error(if x.is_nan() {
                    "ValueError: cannot convert float NaN to integer".into()
                } else {
                    "OverflowError: cannot convert float infinity to integer".into()
                }));
            }
            let int = x.trunc();
            let frac = x - int;
            if int.abs() >= 1e30 {
                return Err(too_big());
            }
            total = (int as i128)
                .checked_mul(factor)
                .and_then(|p| total.checked_add(p))
                .ok_or_else(too_big)?;
            if frac != 0.0 {
                let scaled = factor as f64 * frac;
                let whole = scaled.trunc();
                total = total.checked_add(whole as i128).ok_or_else(too_big)?;
                leftover += scaled - whole;
            }
        } else {
            return Err(py_error(format!(
                "TypeError: unsupported type for timedelta {name} component: {}",
                type_name(v)
            )));
        }
    }
    if leftover != 0.0 {
        let mut whole = leftover.round();
        if (whole - leftover).abs() == 0.5 {
            let odd = total.rem_euclid(2) as f64;
            whole = 2.0 * ((leftover + odd) * 0.5).round() - odd;
        }
        total = total.checked_add(whole as i128).ok_or_else(too_big)?;
    }
    TimeDelta::new(total)
}

/// `dt_util.parse_duration`: Django's standard, ISO 8601 and PostgreSQL
/// forms.
pub(super) fn parse_duration(s: &str) -> Option<Result<TimeDelta, Error>> {
    let f = |v: Option<&str>| -> f64 {
        v.map(|v| v.replace(',', ".").parse().unwrap_or(0.0))
            .unwrap_or(0.0)
    };
    let build = |days: f64, iso: bool, sign: f64, h: f64, m: f64, sec: f64, us: f64| {
        let num = |x: f64| Some(Value::from(x));
        let days_td = timedelta([
            ("days", num(days)),
            ("seconds", None),
            ("microseconds", None),
            ("milliseconds", None),
            ("minutes", None),
            ("hours", None),
            ("weeks", None),
        ])?;
        let days_td = if iso && sign < 0.0 {
            TimeDelta::new(-days_td.0)?
        } else {
            days_td
        };
        let rest = timedelta([
            ("days", None),
            ("seconds", num(sec)),
            ("microseconds", num(us)),
            ("milliseconds", None),
            ("minutes", num(m)),
            ("hours", num(h)),
            ("weeks", None),
        ])?;
        let rest = if sign < 0.0 { -rest.0 } else { rest.0 };
        TimeDelta::new(days_td.0 + rest)
    };
    if let Some(c) = standard_duration(s) {
        let sign = if c.sign { -1.0 } else { 1.0 };
        let us = c.us.map(|u| format!("{u:0<6}"));
        return Some(build(
            f(c.days),
            false,
            sign,
            f(c.hours),
            f(c.minutes),
            f(Some(c.seconds)),
            f(us.as_deref()),
        ));
    }
    if let Some(c) = iso8601_duration(s) {
        let sign = if c.0 { -1.0 } else { 1.0 };
        return Some(build(f(c.1), true, sign, f(c.2), f(c.3), f(c.4), 0.0));
    }
    if let Some(c) = postgres_interval(s) {
        let sign = if c.sign { -1.0 } else { 1.0 };
        let us = c.us.map(|u| format!("{u:0<6}"));
        return Some(build(
            f(c.days),
            false,
            sign,
            f(c.hours),
            f(c.minutes),
            f(c.seconds),
            f(us.as_deref()),
        ));
    }
    None
}

struct Standard<'a> {
    days: Option<&'a str>,
    sign: bool,
    hours: Option<&'a str>,
    minutes: Option<&'a str>,
    seconds: &'a str,
    us: Option<&'a str>,
}

/// Leading ASCII-or-Unicode digits of `s` (Python's `\d+`), as a slice.
fn digits_prefix(s: &str) -> &str {
    let end = s
        .char_indices()
        .find(|(_, c)| digit(*c).is_none())
        .map_or(s.len(), |(i, _)| i);
    &s[..end]
}

/// `STANDARD_DURATION_RE`.
fn standard_duration(s: &str) -> Option<Standard<'_>> {
    // `^(?:(?P<days>-?\d+) (days?, )?)?` — try with the days part, then
    // without.
    fn try_from<'a>(days: Option<&'a str>, rest: &'a str) -> Option<Standard<'a>> {
        let (sign, rest) = match rest.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, rest),
        };
        // `((?:(?P<hours>\d+):)(?=\d+:\d+))?(?:(?P<minutes>\d+):)?(?P<seconds>\d+)`
        let a = digits_prefix(rest);
        if a.is_empty() {
            return None;
        }
        let after_a = &rest[a.len()..];
        let numbers: Vec<&str> = {
            let mut v = vec![a];
            let mut r = after_a;
            while let Some(x) = r.strip_prefix(':') {
                let n = digits_prefix(x);
                if n.is_empty() {
                    break;
                }
                v.push(n);
                r = &x[n.len()..];
            }
            v
        };
        let consumed: usize = numbers.iter().map(|n| n.len()).sum::<usize>() + numbers.len() - 1;
        if numbers.len() > 3 {
            return None;
        }
        let tail = &rest[consumed..];
        let (hours, minutes, seconds) = match numbers.len() {
            3 => (Some(numbers[0]), Some(numbers[1]), numbers[2]),
            2 => (None, Some(numbers[0]), numbers[1]),
            _ => (None, None, numbers[0]),
        };
        // `(?:[\.,](?P<microseconds>\d{1,6})\d{0,6})?$`
        let us = if tail.is_empty() {
            None
        } else {
            let t = tail.strip_prefix('.').or_else(|| tail.strip_prefix(','))?;
            let d = digits_prefix(t);
            if d.is_empty() || d.chars().count() > 12 || d.len() != t.len() {
                return None;
            }
            let cut = d.char_indices().nth(6).map_or(d.len(), |(i, _)| i);
            Some(&d[..cut])
        };
        Some(Standard {
            days,
            sign,
            hours,
            minutes,
            seconds,
            us,
        })
    }
    let s = s.strip_suffix('\n').unwrap_or(s);
    // With days: `-?\d+ ` then optional `days?, `.
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s),
    };
    let d = digits_prefix(body);
    if !d.is_empty() {
        if let Some(after) = body[d.len()..].strip_prefix(' ') {
            let days = &s[..d.len() + usize::from(neg)];
            for prefix in ["days, ", "day, "] {
                if let Some(r) = after.strip_prefix(prefix) {
                    if let Some(found) = try_from(Some(days), r) {
                        return Some(found);
                    }
                }
            }
            if let Some(found) = try_from(Some(days), after) {
                return Some(found);
            }
        }
    }
    try_from(None, s)
}

/// `ISO8601_DURATION_RE`: (negative, days, hours, minutes, seconds).
#[allow(clippy::type_complexity)]
fn iso8601_duration(
    s: &str,
) -> Option<(bool, Option<&str>, Option<&str>, Option<&str>, Option<&str>)> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let (neg, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut rest = rest.strip_prefix('P')?;
    // `\d+([\.,]\d+)?` then the unit.
    fn number(r: &str) -> Option<usize> {
        let a = digits_prefix(r);
        if a.is_empty() {
            return None;
        }
        let mut n = a.len();
        if let Some(f) = r[n..]
            .strip_prefix('.')
            .or_else(|| r[n..].strip_prefix(','))
        {
            let b = digits_prefix(f);
            if !b.is_empty() {
                n += 1 + b.len();
            }
        }
        Some(n)
    }
    fn take<'a>(r: &mut &'a str, unit: char) -> Option<&'a str> {
        let n = number(r)?;
        if r[n..].starts_with(unit) {
            let v = &r[..n];
            *r = &r[n + 1..];
            Some(v)
        } else {
            None
        }
    }
    let s2 = rest;
    let days = take(&mut rest, 'D');
    let _ = s2;
    let (mut h, mut m, mut sec) = (None, None, None);
    if let Some(t) = rest.strip_prefix('T') {
        let mut t = t;
        h = take(&mut t, 'H');
        m = take(&mut t, 'M');
        sec = take(&mut t, 'S');
        rest = t;
    }
    rest.is_empty().then_some((neg, days, h, m, sec))
}

struct Postgres<'a> {
    days: Option<&'a str>,
    sign: bool,
    hours: Option<&'a str>,
    minutes: Option<&'a str>,
    seconds: Option<&'a str>,
    us: Option<&'a str>,
}

/// `POSTGRES_INTERVAL_RE`.
fn postgres_interval(s: &str) -> Option<Postgres<'_>> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    #[allow(clippy::type_complexity)]
    fn time(r: &str) -> Option<(bool, &str, &str, &str, Option<&str>)> {
        let (sign, r) = match r.as_bytes().first() {
            Some(b'-') => (true, &r[1..]),
            Some(b'+') => (false, &r[1..]),
            _ => (false, r),
        };
        let h = digits_prefix(r);
        if h.is_empty() {
            return None;
        }
        let r = r[h.len()..].strip_prefix(':')?;
        let two = |r: &str| -> Option<usize> {
            let mut it = r.char_indices();
            for _ in 0..2 {
                let (_, c) = it.next()?;
                digit(c)?;
            }
            Some(it.next().map_or(r.len(), |(i, _)| i))
        };
        let mn = two(r)?;
        let (m, r) = r.split_at(mn);
        let r = r.strip_prefix(':')?;
        let sn = two(r)?;
        let (sec, r) = r.split_at(sn);
        let us = if r.is_empty() {
            None
        } else {
            let f = r.strip_prefix('.')?;
            let d = digits_prefix(f);
            if d.is_empty() || d.len() != f.len() || d.chars().count() > 6 {
                return None;
            }
            Some(d)
        };
        Some((sign, h, m, sec, us))
    }
    // `(?:(?P<days>-?\d+) (days? ?))?` then the optional time.
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s),
    };
    let d = digits_prefix(body);
    if !d.is_empty() {
        if let Some(after) = body[d.len()..].strip_prefix(' ') {
            let days = &s[..d.len() + usize::from(neg)];
            for unit in ["days ", "day ", "days", "day"] {
                if let Some(r) = after.strip_prefix(unit) {
                    if r.is_empty() {
                        return Some(Postgres {
                            days: Some(days),
                            sign: false,
                            hours: None,
                            minutes: None,
                            seconds: None,
                            us: None,
                        });
                    }
                    if let Some((sign, h, m, sec, us)) = time(r) {
                        return Some(Postgres {
                            days: Some(days),
                            sign,
                            hours: Some(h),
                            minutes: Some(m),
                            seconds: Some(sec),
                            us,
                        });
                    }
                }
            }
        }
    }
    if s.is_empty() {
        return Some(Postgres {
            days: None,
            sign: false,
            hours: None,
            minutes: None,
            seconds: None,
            us: None,
        });
    }
    let (sign, h, m, sec, us) = time(s)?;
    Some(Postgres {
        days: None,
        sign,
        hours: Some(h),
        minutes: Some(m),
        seconds: Some(sec),
        us,
    })
}

// --- strptime ------------------------------------------------------------

/// One piece of a `strptime` format.
enum Piece {
    /// A literal character (matched ignoring case).
    Lit(char),
    /// Whitespace in the format: `\s+`.
    Space,
    /// A directive's alternatives, tried in order.
    Field(char, &'static [&'static str]),
    /// `%f`: one to six digits.
    Fraction,
    /// `%z`.
    Offset,
    /// Names (`%a`, `%b`, `%p`), longest first.
    Names(char, Vec<String>),
}

/// What the parts of `_strptime`'s regexes match: `d` a digit, ` ` a space,
/// `0-5` a digit range, else the character itself.
const D: &[&str] = &["3[0-1]", "[1-2]d", "0[1-9]", "[1-9]", " [1-9]"];
const H: &[&str] = &["2[0-3]", "[0-1]d", "d", " d"];
const I: &[&str] = &["1[0-2]", "0[1-9]", "[1-9]", " [1-9]"];
const J: &[&str] = &[
    "36[0-6]", "3[0-5]d", "[1-2]dd", "0[1-9]d", "00[1-9]", "[1-9]d", "0[1-9]", "[1-9]",
];
const MO: &[&str] = &["1[0-2]", "0[1-9]", "[1-9]"];
const MI: &[&str] = &["[0-5]d", "d"];
const S: &[&str] = &["6[0-1]", "[0-5]d", "d"];
const YY: &[&str] = &["dd"];
const YYYY: &[&str] = &["dddd"];

/// The format as pieces, or Python's `ValueError`, or `None` for a
/// directive hars doesn't parse yet.
fn strptime_pieces(fmt: &str) -> Result<Option<Vec<Piece>>, String> {
    let mut out = Vec::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            out.push(Piece::Space);
            continue;
        }
        if c != '%' {
            out.push(Piece::Lit(c));
            continue;
        }
        let Some(d) = chars.next() else {
            return Err(format!("stray % in format '{fmt}'"));
        };
        let names = |v: &[&str]| {
            let mut v: Vec<String> = v.iter().map(|s| s.to_lowercase()).collect();
            v.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
            v
        };
        let piece = match d {
            'd' | 'e' => Piece::Field('d', D),
            'H' | 'k' => Piece::Field('H', H),
            'I' | 'l' => Piece::Field('I', I),
            'j' => Piece::Field('j', J),
            'm' => Piece::Field('m', MO),
            'M' => Piece::Field('M', MI),
            'S' => Piece::Field('S', S),
            'y' => Piece::Field('y', YY),
            'Y' => Piece::Field('Y', YYYY),
            'f' => Piece::Fraction,
            'z' => Piece::Offset,
            'a' => Piece::Names('a', names(&DAYS.map(|d| &d[..3]))),
            'A' => Piece::Names('A', names(&DAYS)),
            'b' => Piece::Names('b', names(&MONTHS.map(|m| &m[..3]))),
            'B' => Piece::Names('B', names(&MONTHS)),
            'p' => Piece::Names('p', names(&["am", "pm"])),
            '%' => Piece::Lit('%'),
            'T' => {
                out.extend([
                    Piece::Field('H', H),
                    Piece::Lit(':'),
                    Piece::Field('M', MI),
                    Piece::Lit(':'),
                ]);
                Piece::Field('S', S)
            }
            'R' => {
                out.extend([Piece::Field('H', H), Piece::Lit(':')]);
                Piece::Field('M', MI)
            }
            _ => return Ok(None),
        };
        out.push(piece);
    }
    Ok(Some(out))
}

/// Matches one alternative of a field at `i`; its length.
fn match_alt(s: &[char], i: usize, alt: &str) -> Option<usize> {
    let mut n = 0;
    let mut p = alt.chars().peekable();
    while let Some(c) = p.next() {
        let got = *s.get(i + n)?;
        let ok = match c {
            'd' => digit(got).is_some(),
            '[' => {
                let lo = p.next()?;
                p.next();
                let hi = p.next()?;
                p.next();
                got.is_ascii_digit() && (lo..=hi).contains(&got)
            }
            other => got == other,
        };
        if !ok {
            return None;
        }
        n += 1;
    }
    Some(n)
}

/// Fields found: the directive and its text.
type Found = Vec<(char, String)>;

/// `re.match` of the pieces against `s`, with backtracking as Python's
/// regex does; each step paid for.
fn strptime_match(
    state: &State,
    pieces: &[Piece],
    s: &[char],
    i: usize,
    found: &mut Found,
) -> Result<Option<usize>, Error> {
    state.consume_fuel(1)?;
    let Some((piece, rest)) = pieces.split_first() else {
        return Ok(Some(i));
    };
    let mut tries: Vec<(usize, Option<(char, String)>)> = Vec::new();
    match piece {
        Piece::Lit(c) => {
            if s.get(i)
                .is_some_and(|g| g.to_lowercase().eq(c.to_lowercase()))
            {
                tries.push((i + 1, None));
            }
        }
        Piece::Space => {
            let mut n = 0;
            while s.get(i + n).is_some_and(|c| c.is_whitespace()) {
                n += 1;
            }
            for k in (1..=n).rev() {
                tries.push((i + k, None));
            }
        }
        Piece::Field(name, alts) => {
            for alt in alts.iter() {
                if let Some(n) = match_alt(s, i, alt) {
                    tries.push((i + n, Some((*name, s[i..i + n].iter().collect()))));
                }
            }
        }
        Piece::Fraction => {
            let mut n = 0;
            while n < 6 && s.get(i + n).is_some_and(char::is_ascii_digit) {
                n += 1;
            }
            for k in (1..=n).rev() {
                tries.push((i + k, Some(('f', s[i..i + k].iter().collect()))));
            }
        }
        Piece::Offset => {
            // `[+-]\d\d:?[0-5]\d(:?[0-5]\d(\.\d{1,6})?)?|(?-i:Z)`
            if s.get(i) == Some(&'Z') {
                tries.push((i + 1, Some(('z', "Z".into()))));
            } else if matches!(s.get(i), Some('+' | '-')) {
                let dig = |k: usize| s.get(k).is_some_and(|c| digit(*c).is_some());
                let d05 = |k: usize| s.get(k).is_some_and(|c| ('0'..='5').contains(c));
                let mut j = i + 1;
                if dig(j) && dig(j + 1) {
                    j += 2;
                    let colon = s.get(j) == Some(&':');
                    let k = j + usize::from(colon);
                    if d05(k) && dig(k + 1) {
                        let base = k + 2;
                        let mut ends = vec![base];
                        let colon2 = s.get(base) == Some(&':');
                        let k2 = base + usize::from(colon2);
                        if d05(k2) && dig(k2 + 1) {
                            let sec_end = k2 + 2;
                            if s.get(sec_end) == Some(&'.') {
                                let mut n = 0;
                                while n < 6 && dig(sec_end + 1 + n) {
                                    n += 1;
                                }
                                for f in (1..=n).rev() {
                                    ends.insert(0, sec_end + 1 + f);
                                }
                            }
                            ends.insert(ends.len() - 1, sec_end);
                        }
                        for e in ends {
                            tries.push((e, Some(('z', s[i..e].iter().collect()))));
                        }
                    }
                }
            }
        }
        Piece::Names(name, names) => {
            for n in names {
                let len = n.chars().count();
                if s.len() >= i + len
                    && s[i..i + len]
                        .iter()
                        .zip(n.chars())
                        .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
                {
                    tries.push((i + len, Some((*name, n.clone()))));
                }
            }
        }
    }
    for (next, field) in tries {
        let pushed = field.is_some();
        if let Some(f) = field {
            found.push(f);
        }
        if let Some(end) = strptime_match(state, rest, s, next, found)? {
            return Ok(Some(end));
        }
        if pushed {
            found.pop();
        }
    }
    Ok(None)
}

/// `datetime.strptime(string, fmt)`: `Ok(Err(message))` for Python's
/// `ValueError`.
pub(super) fn strptime(
    state: &State,
    s: &str,
    fmt: &str,
) -> Result<Result<DateTime, String>, Error> {
    let pieces = match strptime_pieces(fmt) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(py_error(
                "NotImplementedError: hars does not support this strptime format yet".into(),
            ));
        }
        Err(e) => return Ok(Err(e)),
    };
    // Recursion is one level per piece.
    if pieces.len() > 256 {
        return Err(py_error(
            "NotImplementedError: hars does not support strptime formats this long".into(),
        ));
    }
    state.consume_fuel((s.len() as u64 + fmt.len() as u64) / 64)?;
    let chars: Vec<char> = s.chars().collect();
    let mut found = Found::new();
    let Some(end) = strptime_match(state, &pieces, &chars, 0, &mut found)? else {
        let mut m = String::from("time data ");
        crate::py::repr_str(&mut m, s);
        m.push_str(" does not match format ");
        crate::py::repr_str(&mut m, fmt);
        return Ok(Err(m));
    };
    if end != chars.len() {
        let rest: String = chars[end..].iter().collect();
        return Ok(Err(format!("unconverted data remains: {rest}")));
    }
    let int = |t: &str| -> i64 {
        t.trim().chars().fold(0i64, |a, c| {
            a.saturating_mul(10)
                .saturating_add(i64::from(digit(c).unwrap_or(0)))
        })
    };
    // Python's `\d` takes any decimal digit; read them as ASCII.
    let found: Found = found
        .into_iter()
        .map(|(k, v)| {
            let ascii = v
                .chars()
                .map(|c| digit(c).and_then(|d| char::from_digit(d, 10)).unwrap_or(c))
                .collect();
            (k, ascii)
        })
        .collect();
    let (mut year, mut month, mut day) = (None, 1, 1);
    let (mut hour, mut minute, mut second, mut us) = (0, 0, 0, 0);
    let mut julian = None;
    let mut gmtoff: Option<(i64, i64)> = None;
    let ampm = found
        .iter()
        .find(|(k, _)| *k == 'p')
        .map(|(_, v)| v.to_lowercase());
    for (k, v) in &found {
        match k {
            'y' => {
                let y = int(v);
                year = Some(if y <= 68 { y + 2000 } else { y + 1900 });
            }
            'Y' => year = Some(int(v)),
            'm' => month = int(v),
            'B' => {
                month = MONTHS
                    .iter()
                    .position(|m| m.eq_ignore_ascii_case(v))
                    .unwrap_or(0) as i64
                    + 1
            }
            'b' => {
                month = MONTHS
                    .iter()
                    .position(|m| m[..3].eq_ignore_ascii_case(v))
                    .unwrap_or(0) as i64
                    + 1;
            }
            'd' => day = int(v),
            'H' => hour = int(v),
            'I' => {
                hour = int(v);
                match ampm.as_deref() {
                    None | Some("am") => {
                        if hour == 12 {
                            hour = 0;
                        }
                    }
                    _ => {
                        if hour != 12 {
                            hour += 12;
                        }
                    }
                }
            }
            'M' => minute = int(v),
            'S' => second = int(v),
            'f' => us = int(&format!("{v:0<6}")),
            'j' => julian = Some(int(v)),
            'z' => {
                if v == "Z" {
                    gmtoff = Some((0, 0));
                } else {
                    let mut z: String = v.clone();
                    if z.as_bytes().get(3) == Some(&b':') {
                        z.remove(3);
                        if z.len() > 5 {
                            if z.as_bytes()[5] != b':' {
                                return Ok(Err(format!("Inconsistent use of : in {v}")));
                            }
                            z.remove(5);
                        }
                    }
                    let h = int(&z[1..3]);
                    let m = int(&z[3..5]);
                    let sec = z.get(5..7).map_or(0, int);
                    let frac = z.get(8..).map_or(0, |f| int(&format!("{f:0<6}")));
                    let off = h * 3600 + m * 60 + sec;
                    gmtoff = Some(if z.starts_with('-') {
                        (-off, -frac)
                    } else {
                        (off, frac)
                    });
                }
            }
            _ => {}
        }
    }
    let mut year = year.unwrap_or(if month == 2 && day == 29 { 1904 } else { 1900 });
    let leap_fix = found.iter().all(|(k, _)| !matches!(k, 'y' | 'Y')) && month == 2 && day == 29;
    if let Some(j) = julian {
        // `date.fromordinal`: day 366 of a common year is next year's first.
        let (y, m, d) = civil_from_days(days_from_civil(year, 1, 1) + j - 1);
        year = y;
        month = i64::from(m);
        day = i64::from(d);
    } else if Naive::new(year, month, day, 0, 0, 0, 0).is_err() {
        return Ok(Err("day is out of range for month".into()));
    }
    let year = if leap_fix { 1900 } else { year };
    let wall = match Naive::new(year, month, day, hour, minute, second, us) {
        Ok(w) => w,
        Err(e) => return Ok(Err(e)),
    };
    let tz = match gmtoff {
        None => None,
        Some((secs, frac)) => {
            if frac != 0 {
                return Err(py_error(
                    "NotImplementedError: hars does not support offsets with fractions of a second yet"
                        .into(),
                ));
            }
            Some(Tz::fixed(secs as i32))
        }
    };
    Ok(Ok(DateTime {
        wall,
        fold: false,
        tz,
    }))
}

/// `datetime.fromtimestamp(value, tz)`: Python's checks and messages; the
/// fraction rounded half to even to the microsecond.
pub(super) fn from_timestamp(value: &Value, tz: Tz) -> Result<Result<DateTime, String>, Error> {
    let (secs, us): (i64, i64) = match value.kind() {
        ValueKind::Bool => (i64::from(value.is_true()), 0),
        ValueKind::Number if value.is_integer() => match i64::try_from(value.clone()) {
            Ok(s) => (s, 0),
            Err(_) => {
                return Err(py_error(
                    "OverflowError: timestamp out of range for platform time_t".into(),
                ));
            }
        },
        ValueKind::Number => {
            let x = f64::try_from(value.clone()).unwrap_or(f64::NAN);
            if x.is_nan() {
                return Ok(Err("Invalid value NaN (not a number)".into()));
            }
            if x.is_infinite() {
                return Err(py_error(
                    "OverflowError: cannot convert float infinity to integer".into(),
                ));
            }
            let int = x.trunc();
            let mut frac = ((x - int) * 1e6).round_ties_even();
            let mut int = int;
            if frac >= 1e6 {
                frac -= 1e6;
                int += 1.0;
            } else if frac < 0.0 {
                frac += 1e6;
                int -= 1.0;
            }
            if !(-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&int) {
                return Err(py_error(
                    "OverflowError: timestamp out of range for platform time_t".into(),
                ));
            }
            (int as i64, frac as i64)
        }
        _ => {
            return Ok(Err(format!(
                "'{}' object cannot be interpreted as an integer",
                type_name(value)
            )));
        }
    };
    let days = secs.div_euclid(86_400);
    let (y, _, _) = civil_from_days(days);
    if !(1..=9999).contains(&y) {
        return Ok(Err(format!("year {y} is out of range")));
    }
    let instant = secs * US_PER_SEC + us;
    DateTime::at(instant, tz).map(Ok)
}
