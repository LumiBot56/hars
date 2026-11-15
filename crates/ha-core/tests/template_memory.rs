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

#[test]
fn reading_large_attributes_stays_within_the_budget() {
    // 64 sensors holding a 1 MB string each: 64 MB of attributes.
    let hass = Hass::new();
    for i in 0..64 {
        let id = EntityId::parse(&format!("sensor.big{i}")).unwrap();
        let attributes = serde_json::json!({"data": "x".repeat(1_000_000), "n": i});
        hass.set_state(id, "on", attributes.as_object().unwrap().clone());
    }
    let home = Home::new(hass);
    // Build the environments first.
    let _ = Template::new("{{ states | count }}").render_in(Some(&home), None, false);
    for source in [
        "{% for s in states.sensor %}{% if s.attributes.data is defined %}x{% endif %}{% endfor %}",
        "{% for s in states.sensor %}{{ s.attributes['data'] | length }}{% endfor %}",
        "{{ states.sensor | map(attribute='attributes.data') | count }}",
        "{{ states.sensor | map(attribute='attributes.data') | list | count }}",
        "{{ states.sensor | map(attribute='attributes.data') | join | length }}",
        "{{ states.sensor | selectattr('attributes.data', 'defined') | list | count }}",
        "{{ states.sensor | selectattr('attributes.data.0', 'defined') | count }}",
        "{{ states.sensor | rejectattr('attributes.data', 'undefined') | list | count }}",
        "{{ states.sensor | sort(attribute='attributes.data') | count }}",
        "{{ states.sensor | groupby('attributes.data') | count }}",
        "{{ states.sensor | unique(attribute='attributes.data') | list | count }}",
        "{{ (states.sensor | max(attribute='attributes.data')).entity_id }}",
        "{{ states.sensor | sum(attribute='attributes.n') }}{{ states.sensor | map(attribute='attributes') | map(attribute='data') | list | count }}",
        "{{ states.sensor | map(attribute='attributes') | list == [] }}",
    ] {
        let before = CURRENT.load(Ordering::Relaxed);
        PEAK.store(before, Ordering::Relaxed);
        let result = Template::new(source)
            .render_in(Some(&home), None, false)
            .result;
        let peak = PEAK.load(Ordering::Relaxed) - before;
        let err = result.expect_err(source);
        assert!(err.0.contains("budget"), "{source}: {err}");
        // The budget is about 32 MB of data; the attributes are 64 MB.
        assert!(peak < 40 * MIB, "{source}: peak {} MiB", peak / MIB);
    }
}
