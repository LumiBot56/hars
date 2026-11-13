# Roadmap

Ordered by "what unlocks a usable hub soonest".

## M0 — Core (in progress)
- [x] Cargo workspace, CI, perf-tuned release profile
- [x] `EntityId`, `State`, `StateMachine`, `EventBus`, `Hass`
- [x] REST: `/api/`, `/api/states`, `/api/states/{entity_id}`
- [ ] Service registry (`hass.services.call(domain, service, data)`)
- [ ] `criterion` benchmarks for state writes + event fan-out
- [ ] Graceful shutdown + lifecycle events (`homeassistant_start`/`stop`)

## M1 — Talk to the existing ecosystem
- [ ] WebSocket API (`auth`, `subscribe_events`, `get_states`, `call_service`)
- [ ] Long-lived access tokens / auth
- [ ] Serve the official HA frontend against `hars`
- [ ] `configuration.yaml` loading (`!include`, `!secret`)

## M2 — Persistence & automation
- [ ] Recorder: SQLite history with batched writes
- [ ] Restore state on restart
- [ ] Automations: state/time/event triggers, conditions, service-call actions
- [ ] Jinja-compatible templates (`minijinja`)

## M3 — First real integrations
- [ ] MQTT (client + discovery)
- [ ] ESPHome native API
- [ ] Zigbee (via zigbee2mqtt first, native later)
- [ ] `sun`, `time_date`, `template`, `input_*` helpers

## M4 — Benchmarks vs Home Assistant
- [ ] Reproducible harness: same config + entity count on HA and `hars`
- [ ] Publish RSS / CPU / startup / event-throughput comparisons
