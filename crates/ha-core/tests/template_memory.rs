//! Peak memory of templates that read large attributes: a state's
//! attributes are copied value by value as they are read, each paid for
//! before the copy, so a filter that reads many of them (inside one
//! instruction) stops at the budget instead of copying them all.
//!
//! Its own test binary, so its counting allocator sees only this test.

// The counting allocator must implement `GlobalAlloc`, an unsafe trait; it
// only forwards to the system allocator and counts. Test code only.
#![allow(unsafe_code)]

use ha_core::template::{Home, Template};
use ha_core::{EntityId, Hass};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged to the system allocator.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(p, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const MIB: usize = 1024 * 1024;

/// 64 sensors, each with `attributes` (made per sensor) and state "on".
fn home(attributes: impl Fn(usize) -> serde_json::Value) -> Home {
    let hass = Hass::new();
    for i in 0..64 {
        let id = EntityId::parse(&format!("sensor.big{i}")).unwrap();
        hass.set_state(id, "on", attributes(i).as_object().unwrap().clone());
    }
    Home::new(hass)
}

/// Each template fails with an error containing `want` (or, for
/// `=text`, renders `text`), allocating less than `limit` bytes over what
/// was there before it ran.
fn check(home: &Home, limit: usize, cases: &[(&str, &str)]) {
    // Build the environments first.
    let _ = Template::new("{{ states | count }}").render_in(Some(home), None, false);
    for (source, want) in cases {
        let before = CURRENT.load(Ordering::Relaxed);
        PEAK.store(before, Ordering::Relaxed);
        let result = Template::new(source)
            .render_in(Some(home), None, false)
            .result;
        let peak = PEAK.load(Ordering::Relaxed) - before;
        match want.strip_prefix('=') {
            Some(text) => {
                let rendered = result.expect(source);
                assert_eq!(rendered.as_text(), Some(text), "{source}");
            }
            None => {
                let err = result.expect_err(source);
                assert!(err.0.contains(want), "{source}: {err}");
            }
        }
        assert!(peak < limit, "{source}: peak {} KiB", peak / 1024);
    }
}

const BUDGET: &str = "budget";

#[test]
fn reading_large_state_data_stays_within_the_budget() {
    // 64 sensors holding a 1 MB string each: 64 MB of attributes. The
    // budget is about 32 MB of data.
    let big = home(|i| serde_json::json!({"data": "x".repeat(1_000_000), "n": i}));
    check(
        &big,
        40 * MIB,
        &[
            (
                "{% for s in states.sensor %}{% if s.attributes.data is defined %}x{% endif %}{% endfor %}",
                BUDGET,
            ),
            (
                "{% for s in states.sensor %}{{ s.attributes['data'] | length }}{% endfor %}",
                BUDGET,
            ),
            (
                "{{ states.sensor | map(attribute='attributes.data') | count }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | map(attribute='attributes.data') | list | count }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | map(attribute='attributes.data') | join | length }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | selectattr('attributes.data', 'defined') | list | count }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | selectattr('attributes.data.0', 'defined') | count }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | rejectattr('attributes.data', 'undefined') | list | count }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | sort(attribute='attributes.data') | count }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | groupby('attributes.data') | count }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | unique(attribute='attributes.data') | list | count }}",
                BUDGET,
            ),
            (
                "{{ (states.sensor | max(attribute='attributes.data')).entity_id }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | sum(attribute='attributes.n') }}{{ states.sensor | map(attribute='attributes') | map(attribute='data') | list | count }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | map(attribute='attributes') | list == [] }}",
                BUDGET,
            ),
            // A state printed, however it is asked for.
            (
                "{% for s in states.sensor %}{{ (s ~ '') | length }}{% endfor %}",
                BUDGET,
            ),
            (
                "{% for s in states.sensor %}{{ s | string | length }}{% endfor %}",
                BUDGET,
            ),
            ("{{ states.sensor | list | string | length }}", BUDGET),
            (
                "{{ states.sensor | map(attribute='attributes') | map('string') | join | length }}",
                BUDGET,
            ),
            // States aren't hashable or ordered (Python's `TypeError`), and
            // never compared by what they print.
            (
                "{{ ([states.sensor.big0, states.sensor.big1] * 1000) | unique | count }}",
                "unhashable type: 'TemplateState'",
            ),
            (
                "{{ ([states.sensor.big0, states.sensor.big1] * 1000) | sort | count }}",
                "TypeError",
            ),
            (
                "{{ ([states.sensor.big0, states.sensor.big1] * 1000) | max }}",
                "TypeError",
            ),
            (
                "{{ [states.sensor.big0.attributes, states.sensor.big1.attributes] | unique | count }}",
                "unhashable type: 'ReadOnlyDict'",
            ),
        ],
    );
    drop(big);

    // Derived from attributes: `name` (`friendly_name`), `state_with_unit`.
    let named = home(|_| serde_json::json!({"friendly_name": "n".repeat(1_000_000)}));
    check(
        &named,
        40 * MIB,
        &[
            (
                "{{ states.sensor | map(attribute='name') | count }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | map(attribute='name') | list | count }}",
                BUDGET,
            ),
            (
                "{{ states.sensor | sort(attribute='name') | count }}",
                BUDGET,
            ),
        ],
    );
    drop(named);
    let units = home(|_| serde_json::json!({"unit_of_measurement": "u".repeat(1_000_000)}));
    check(
        &units,
        40 * MIB,
        &[
            (
                "{{ states.sensor | map(attribute='state_with_unit') | count }}",
                BUDGET,
            ),
            (
                "{% for s in states.sensor %}{{ states(s.entity_id, with_unit=True) | length }}{% endfor %}",
                BUDGET,
            ),
            (
                "{% for s in states.sensor %}{{ states(s.entity_id, rounded=True, with_unit=True) | length }}{% endfor %}",
                BUDGET,
            ),
        ],
    );
    drop(units);

    // What an attribute costs is counted without listing its parts:
    // 1,000,000 nulls cost more than the budget, found with no more than
    // a stack as deep as the value.
    let nulls = home(|i| {
        if i == 0 {
            serde_json::json!({"data": vec![serde_json::Value::Null; 1_000_000]})
        } else {
            serde_json::json!({})
        }
    });
    check(
        &nulls,
        MIB,
        &[
            ("{{ states.sensor.big0.attributes.data | count }}", BUDGET),
            ("{{ state_attr('sensor.big0', 'data') | count }}", BUDGET),
            ("{{ states.sensor.big0 | string | length }}", BUDGET),
        ],
    );
    drop(nulls);

    // Messages and parsing driven by long strings from a state: a missing
    // key's warning, and `strptime`'s format and whitespace.
    let long = home(|i| {
        if i == 0 {
            serde_json::json!({
                "key": "x".repeat(1_000_000),
                "fmt": "x".repeat(1_000_000),
                "spaces": " ".repeat(1_000_000),
            })
        } else {
            serde_json::json!({})
        }
    });
    let key = "{% set k = state_attr('sensor.big0', 'key') %}";
    let missing = [
        "states.sensor.big0.attributes[k]",
        "{}[k]",
        "none[k]",
        "(1)[k]",
        "[1][k]",
        "now()[k]",
        "states.sensor[k]",
        "states.sensor.big0[k]",
        "states.sensor.big0[k ~ '_']",
        "x | default(k) | float(none) | string ~ (k | float(0) | string)",
    ]
    .map(|m| format!("{key}{{% for i in range(100) %}}{{{{ {m} }}}}{{% endfor %}}"));
    let mut cases: Vec<(&str, &str)> = missing.iter().map(|m| (m.as_str(), BUDGET)).collect();
    // The first error raised where minijinja can't return one is the
    // render's; the loop goes on paying for each message until the budget.
    let domain = format!("{key}{{% for i in range(100) %}}{{{{ states[k ~ 'X'] }}}}{{% endfor %}}");
    cases.push((&domain, "Invalid domain name"));
    cases.extend([
        // Python: more format than string is a `ValueError`, so the default.
        (
            "{{ strptime('', state_attr('sensor.big0', 'fmt'), 'fallback') }}",
            "=fallback",
        ),
        (
            "{{ strptime('2026', '%Y' ~ state_attr('sensor.big0', 'fmt'), 'fallback') }}",
            "=fallback",
        ),
        // A million ways to read the spaces: each paid as it is tried.
        (
            "{{ strptime(state_attr('sensor.big0', 'spaces'), ' %Y', 'fallback') }}",
            BUDGET,
        ),
    ]);
    // The warnings are kept (HA's log gets each one); their total is the
    // budget's.
    check(&long, 40 * MIB, &cases);
}
