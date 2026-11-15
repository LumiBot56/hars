//! Python's number rules that HA's template functions rely on: `float()`
//! and `int()` of strings, `round()`, `math.fsum`, and fixed-point
//! formatting. Each is CPython's algorithm (`Objects/floatobject.c`,
//! `Objects/longobject.c`, `Modules/mathmodule.c`), checked by the
//! template function vectors.

use crate::entity_id::decimal_value;

/// Python's `str.strip()` whitespace.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// `float(s)` for a string: surrounding whitespace, Unicode decimal
/// digits, underscores between digits, `inf`/`infinity`/`nan` in any case.
pub(super) fn py_float(s: &str) -> Option<f64> {
    let s = s.trim_matches(is_py_space);
    // `_PyUnicode_TransformDecimalAndSpaceToASCII`.
    let mut ascii = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            ascii.push(c);
        } else {
            ascii.push(char::from_digit(decimal_value(c)?, 10)?);
        }
    }
    let body = ascii.strip_prefix(['+', '-']).unwrap_or(&ascii);
    let lower = body.to_ascii_lowercase();
    if matches!(lower.as_str(), "inf" | "infinity" | "nan") {
        let x = if lower == "nan" {
            f64::NAN
        } else {
            f64::INFINITY
        };
        return Some(if ascii.starts_with('-') { -x } else { x });
    }
    // Underscores only between digits.
    let b = ascii.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'_'
            && !(i > 0 && b[i - 1].is_ascii_digit() && b.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            return None;
        }
    }
    let clean: String = ascii.chars().filter(|c| *c != '_').collect();
    // `[sign] (digits [. digits*] | . digits) [e [sign] digits]`.
    let mut it = clean.as_bytes();
    if let [b'+' | b'-', rest @ ..] = it {
        it = rest;
    }
    let int_len = it.iter().take_while(|c| c.is_ascii_digit()).count();
    let mut rest = &it[int_len..];
    let mut frac_len = 0;
    if let [b'.', r @ ..] = rest {
        frac_len = r.iter().take_while(|c| c.is_ascii_digit()).count();
        rest = &r[frac_len..];
    }
    if int_len + frac_len == 0 {
        return None;
    }
    if let [b'e' | b'E', r @ ..] = rest {
        let r = match r {
            [b'+' | b'-', r @ ..] => r,
            r => r,
        };
        let exp_len = r.iter().take_while(|c| c.is_ascii_digit()).count();
        if exp_len == 0 {
            return None;
        }
        rest = &r[exp_len..];
    }
    if !rest.is_empty() {
        return None;
    }
    clean.parse().ok()
}

/// Why `int(s, base)` failed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum IntError {
    /// Python's `ValueError`.
    Invalid,
    /// The base isn't 0 or 2 to 36.
    Base,
    /// Valid, but past `i128` (Python has no limit).
    TooBig,
}

/// `int(s, base)`.
pub(super) fn py_int(s: &str, base: u32) -> Result<i128, IntError> {
    if base == 1 || base > 36 {
        return Err(IntError::Base);
    }
    let asked_zero = base == 0;
    let s = s.trim_matches(is_py_space);
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let lower = s.to_ascii_lowercase();
    let prefixed = |p: &str| lower.starts_with(p);
    let (base, body, had_prefix) = match base {
        0 if prefixed("0x") => (16, &s[2..], true),
        0 if prefixed("0o") => (8, &s[2..], true),
        0 if prefixed("0b") => (2, &s[2..], true),
        0 => (10, s, false),
        16 if prefixed("0x") => (16, &s[2..], true),
        8 if prefixed("0o") => (8, &s[2..], true),
        2 if prefixed("0b") => (2, &s[2..], true),
        b => (b, s, false),
    };
    let body = if had_prefix {
        body.strip_prefix('_').unwrap_or(body)
    } else {
        body
    };
    if body.is_empty() {
        return Err(IntError::Invalid);
    }
    // Base 0 refuses a decimal with a leading zero ("010", not "000").
    if base == 10 && asked_zero && base0_leading_zero(body) {
        return Err(IntError::Invalid);
    }
    let mut value: i128 = 0;
    let mut prev_digit = false;
    let mut too_big = false;
    for c in body.chars() {
        if c == '_' {
            if !prev_digit {
                return Err(IntError::Invalid);
            }
            prev_digit = false;
            continue;
        }
        let d = match decimal_value(c) {
            Some(d) => d,
            None => c.to_digit(36).ok_or(IntError::Invalid)?,
        };
        if d >= base {
            return Err(IntError::Invalid);
        }
        prev_digit = true;
        match value
            .checked_mul(i128::from(base))
            .and_then(|v| v.checked_add(i128::from(d)))
        {
            Some(v) => value = v,
            None => too_big = true,
        }
    }
    if !prev_digit {
        return Err(IntError::Invalid);
    }
    // `sys.get_int_max_str_digits()`: past 4300 digits, a base that isn't
    // a power of two is a `ValueError`.
    if !base.is_power_of_two() && body.chars().filter(|c| *c != '_').count() > 4300 {
        return Err(IntError::Invalid);
    }
    if too_big {
        return Err(IntError::TooBig);
    }
    Ok(if neg { -value } else { value })
}

/// `int(s, 0)`'s extra rule: a decimal with a leading zero is refused.
fn base0_leading_zero(s: &str) -> bool {
    let s = s.trim_matches(is_py_space);
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("0x") || lower.starts_with("0o") || lower.starts_with("0b") {
        return false;
    }
    s.starts_with('0') && s.chars().any(|c| c != '0' && c != '_')
}

/// `f"{x:.{p}f}"`, with `z` turning a negative zero positive.
pub(super) fn format_fixed(x: f64, p: usize, z: bool) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let s = format!("{x:.p$}");
    if z && s.starts_with('-') && s[1..].chars().all(|c| c == '0' || c == '.') {
        return s[1..].to_owned();
    }
    s
}

/// `round(x, ndigits)` for a float: the exact decimal value rounded half
/// to even (CPython's `double_round`).
pub(super) fn py_round(x: f64, ndigits: i64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    if ndigits > 323 {
        return x;
    }
    if ndigits < -308 {
        return 0.0 * x;
    }
    if ndigits >= 0 {
        let p = ndigits as usize;
        return format!("{x:.p$}").parse().unwrap_or(x);
    }
    // Round the integer part at 10**-ndigits; the fraction only breaks
    // ties.
    let k = (-ndigits) as usize;
    let int = format!("{:.0}", x.abs().trunc());
    let has_fraction = x.fract() != 0.0;
    let rounded = if int.len() < k {
        // Below 10**(k-1): under half of 10**k.
        "0".to_owned()
    } else {
        let (head, tail) = int.split_at(int.len() - k);
        let first = tail.as_bytes()[0];
        let rest_zero = tail[1..].bytes().all(|b| b == b'0');
        let head_odd = head.as_bytes().last().is_some_and(|d| (d - b'0') % 2 == 1);
        let up = first > b'5' || (first == b'5' && (!rest_zero || has_fraction || head_odd));
        let mut digits: Vec<u8> = head.bytes().collect();
        if digits.is_empty() {
            digits.push(b'0');
        }
        if up {
            let mut i = digits.len();
            loop {
                if i == 0 {
                    digits.insert(0, b'1');
                    break;
                }
                i -= 1;
                if digits[i] == b'9' {
                    digits[i] = b'0';
                } else {
                    digits[i] += 1;
                    break;
                }
            }
        }
        let mut s = String::from_utf8(digits).unwrap_or_default();
        s.push_str(&"0".repeat(k));
        s
    };
    let v: f64 = rounded.parse().unwrap_or(0.0);
    if x < 0.0 { -v } else { v }
}

/// `round(x)` for a float: an integer value, ties to even.
pub(super) fn round_half_even(x: f64) -> f64 {
    x.round_ties_even()
}

/// `math.fsum`: the correctly rounded sum (CPython's `math_fsum`), or
/// Python's exception text.
pub(super) fn fsum(items: impl IntoIterator<Item = f64>) -> Result<f64, String> {
    let mut p: Vec<f64> = Vec::new();
    let mut special_sum = 0.0;
    let mut inf_sum = 0.0;
    for item in items {
        let xsave = item;
        let mut x = item;
        let mut i = 0;
        for j in 0..p.len() {
            let mut y = p[j];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                p[i] = lo;
                i += 1;
            }
            x = hi;
        }
        p.truncate(i);
        if x != 0.0 {
            if !x.is_finite() {
                if xsave.is_finite() {
                    return Err("OverflowError: intermediate overflow in fsum".into());
                }
                if xsave.is_infinite() {
                    inf_sum += xsave;
                }
                special_sum += xsave;
                p.clear();
            } else {
                p.push(x);
            }
        }
    }
    if special_sum != 0.0 || special_sum.is_nan() {
        if inf_sum.is_nan() {
            return Err("ValueError: -inf + inf in fsum".into());
        }
        return Ok(special_sum);
    }
    let mut n = p.len();
    let mut hi = 0.0;
    if n > 0 {
        n -= 1;
        hi = p[n];
        let mut lo = 0.0;
        while n > 0 {
            let x = hi;
            n -= 1;
            let y = p[n];
            hi = x + y;
            let yr = hi - x;
            lo = y - yr;
            if lo != 0.0 {
                break;
            }
        }
        if n > 0 && ((lo < 0.0 && p[n - 1] < 0.0) || (lo > 0.0 && p[n - 1] > 0.0)) {
            let y = lo * 2.0;
            let x = hi + y;
            let yr = x - hi;
            if y == yr {
                hi = x;
            }
        }
    }
    Ok(hi)
}

/// Python's float `%`: the divisor's sign. `None` for a zero divisor.
pub(super) fn py_fmod(x: f64, m: f64) -> Option<f64> {
    if m == 0.0 {
        return None;
    }
    let r = x % m;
    Some(if r != 0.0 {
        if (m < 0.0) != (r < 0.0) { r + m } else { r }
    } else {
        0.0f64.copysign(m)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_parse_as_python() {
        for (s, want) in [
            ("1.5", Some(1.5)),
            (" 2 ", Some(2.0)),
            ("1_000.5", Some(1000.5)),
            ("1__0", None),
            ("_1", None),
            ("1_", None),
            ("1_.5", None),
            ("1e1_0", Some(1e10)),
            (".5", Some(0.5)),
            ("5.", Some(5.0)),
            ("+5", Some(5.0)),
            (".", None),
            ("e5", None),
            ("1e", None),
            ("", None),
            ("-Infinity", Some(f64::NEG_INFINITY)),
            ("١٢.٥", Some(12.5)),
            ("0x10", None),
        ] {
            assert_eq!(py_float(s), want, "{s:?}");
        }
        assert!(py_float("NaN").unwrap().is_nan());
    }

    #[test]
    fn ints_parse_as_python() {
        assert_eq!(py_int(" 12 ", 10), Ok(12));
        assert_eq!(py_int("1_000", 10), Ok(1000));
        assert_eq!(py_int("0x1F", 16), Ok(31));
        assert_eq!(py_int("0x_1F", 0), Ok(31));
        assert_eq!(py_int("1F", 16), Ok(31));
        assert_eq!(py_int("0b11", 0), Ok(3));
        assert_eq!(py_int("z", 36), Ok(35));
        assert_eq!(py_int("12", 1), Err(IntError::Base));
        assert_eq!(py_int("1.5", 10), Err(IntError::Invalid));
        assert_eq!(py_int("9".repeat(50).as_str(), 10), Err(IntError::TooBig));
        assert_eq!(py_int("010", 0), Err(IntError::Invalid));
        assert_eq!(py_int("000", 0), Ok(0));
        assert_eq!(py_int("010", 10), Ok(10));
    }

    #[test]
    fn rounding_is_pythons() {
        assert_eq!(py_round(2.675, 2), 2.67);
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(15.0, -1), 20.0);
        assert_eq!(py_round(25.0, -1), 20.0);
        assert_eq!(py_round(25.5, -1), 30.0);
        assert_eq!(py_round(1234.5, -2), 1200.0);
        assert_eq!(py_round(4.0, -1), 0.0);
        assert_eq!(py_round(5.0, -1), 0.0);
        assert_eq!(py_round(5.5, -1), 10.0);
        assert_eq!(py_round(-15.0, -1), -20.0);
        assert_eq!(round_half_even(2.5), 2.0);
        assert_eq!(round_half_even(-2.5), -2.0);
    }

    #[test]
    fn fsum_is_exact() {
        assert_eq!(fsum([0.1; 10]), Ok(1.0));
        assert_eq!(
            fsum([1e308, 1e308]),
            Err("OverflowError: intermediate overflow in fsum".into())
        );
        assert_eq!(
            fsum([f64::INFINITY, f64::NEG_INFINITY]),
            Err("ValueError: -inf + inf in fsum".into())
        );
        assert_eq!(fsum([1e100, 1.0, -1e100, 1e-100]), Ok(1.0));
    }
}
