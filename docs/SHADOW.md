# Shadow mode

Shadow mode runs `hars` beside the Home Assistant it will replace, with HA
still in charge of the house. For a week it does what it would do (the same
triggers, the same automations) while nothing it decides reaches a device,
and a diff tool compares its decisions with HA's. It is the ROADMAP's gate
before cutover (M7): **one week with no differences.**

## What shadow mode does

`HARS_SHADOW=1` changes one thing: a service call is validated and its
`call_service` event fired as usual, so automations, the logbook and the
recorder see it, but a handler that acts on the outside world doesn't run.
No command goes to a Hue light, a LIFX bulb, a SwitchBot, a phone. Only
`automation`, `homeassistant` (which forwards to the target's domain, checked
in its turn), `system_log`, `persistent_notification`, `frontend`, `weather`
and `labs` run (`ha_core::shadow`); an unknown domain is held back.

The states `hars` shows still follow the real devices: Hue reports over its
event stream, LIFX and SwitchBot are polled, and they report what HA's
commands did. So `hars`'s automations see what HA's saw, and a
`wait_for_trigger` that waits for the light it "turned on" completes when HA's
command arrives.

Every held-back call and every `automation_triggered` goes to a JSON Lines
file (`HARS_SHADOW_LOG`, default `<config>/.hars/shadow.jsonl`), appended to,
with a `shadow_started` line each time `hars` starts.

Entities whose state only changes when *commanded* are the exception: the
SwitchBot IR air conditioner (`assumed_state`) and its switch, and the Bot's
button (its state is when it was last pressed). Nothing reports their change,
so in shadow mode `hars` never updates them. An automation that reads one will
see a stale value; read the diff with that in mind.

## What it does not check

Shadow mode compares **decisions**. It does not show that the dashboards look
right, that the phone keeps its session and reports, that history graphs and
the Map trail render, or that device commands are accepted (the held-back
handlers are the code that talks to the device). Those are the other M5, M6
and M7 acceptance items. A passing week says the automations match; it does
not say `hars` can replace HA.

## Setting it up

`hars` must have **its own config directory**. Never point `HARS_CONFIG` at
HA's: both would write `.storage`.

1. Take a copy of HA's config directory (a backup, or a copy with HA
   stopped) and import it:

   ```sh
   docker run --rm -v hars-shadow:/config -v /path/to/ha-config-copy:/ha:ro hars import /ha --dry-run
   docker run --rm -v hars-shadow:/config -v /path/to/ha-config-copy:/ha:ro hars import /ha
   ```

   Read `import-report.json`: an automation reported `unavailable` will show
   up as a *trigger missing* in the diff.
2. Run it on another port, on the LAN so it can reach the devices:

   ```sh
   docker run -d --name hars-shadow --network host \
     -e HARS_SHADOW=1 -e HARS_BIND=0.0.0.0:8124 -v hars-shadow:/config hars
   docker logs hars-shadow | grep "SHADOW MODE"
   ```

   Look at it at `http://host:8124`. Services called from its UI go through the
   same hold-back, so it cannot control a device.
3. Leave the phone on HA. Do not log the companion app in to `hars`: its
   webhooks would feed `hars` state that HA doesn't have.

Side effects on the house are reads only, but they are more reads: SwitchBot's
cloud API is polled by both (about 290 more requests a day, against its 10,000
daily limit), the Hue bridge has a second event-stream client, and LIFX bulbs
get a second poller.

## Reading the result

```sh
scripts/shadow-diff.py --ha-db /path/to/home-assistant_v2.db \
                       --hars-log /path/to/shadow.jsonl
```

(Copy the database first if HA is running; the tool opens it read-only.) It
compares, over the window from `hars`'s first start to now:

- **calls**: the same service, targets and data within 1 s. Different data on
  the same service and targets is a *mismatch*; HA's call with no match is
  *missing*; `hars`'s with no match is *extra*. Only calls made on behalf of an
  automation are compared (`--all-contexts` adds the UI's and the apps').
- **triggers**: HA's `automation_triggered` against `hars`'s, by automation and
  time, with a per-automation count.

It exits 0 when nothing differs, 1 otherwise, so it can gate a script. When
`hars` was down (it prints each start it saw), leave the gap out with
`--exclude START,END`; otherwise HA's calls in it count as missing. If you
change an automation in HA, import again (`--force`) and start the week over.

`python3 -m unittest discover -s scripts` runs its tests.
