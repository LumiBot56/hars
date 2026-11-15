//! Shadow mode: `hars` runs beside the Home Assistant it will replace, with
//! HA still in charge of the house.
//!
//! In shadow mode a service call is processed as always (validated, the
//! `call_service` event fired, so automations, the logbook and the recorder
//! see it) but a handler that would act on the outside world is not run: no
//! command goes to a light, a switch, a phone. The states `hars` shows still
//! follow the real devices, because they report what HA's commands did.
//! Calls that only touch `hars` itself (its automations, its own
//! notifications) run.

/// Domains whose services run in shadow mode: they act on `hars`'s own
/// state, never on a device or another service. Every other domain is
/// held back, so a service nobody thought of is safe by default.
///
/// `homeassistant` is here because `homeassistant.turn_on` and friends only
/// forward to the target's own domain, which is checked in its turn.
const RUNS: [&str; 7] = [
    "automation",
    "homeassistant",
    "system_log",
    "persistent_notification",
    "frontend",
    "weather",
    "labs",
];

/// Whether `domain`'s services run in shadow mode.
pub fn runs(domain: &str) -> bool {
    RUNS.contains(&domain)
}

#[cfg(test)]
mod tests {
    use super::runs;

    #[test]
    fn only_hars_own_domains_run() {
        for d in ["automation", "homeassistant", "weather"] {
            assert!(runs(d), "{d}");
        }
        for d in [
            "light", "switch", "climate", "button", "scene", "hue", "lifx", "notify",
        ] {
            assert!(!runs(d), "{d}");
        }
        // An unknown domain is held back.
        assert!(!runs("garage_door"));
    }
}
