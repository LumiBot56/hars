#!/usr/bin/env python3
"""Vectors for templates in YAML automations.

    .venv/bin/python -I scripts/vectors/automation_templates.py
    cargo test -p ha-automation templates

Runs Home Assistant 2026.6.1's `automation` (with `helpers/script.py`,
`helpers/condition.py`, `helpers/trigger.py`, `helpers/service.py` and
`helpers/event.py`), the `homeassistant` integration's `state`,
`numeric_state` and `time` triggers and the `template` integration's
trigger, on Jinja2 3.1.6, in Europe/London, and writes
crates/ha-automation/src/vectors/templates.json: for each scenario, the
automations (as `automations.yaml` would give them), the steps (states
written, time passing, `automation.trigger` calls) and what HA did, in
order: the service calls its actions made with their rendered data, the
`automation_triggered` events, the automations' validation errors and the
WARNING and ERROR log lines of the automation, script, condition,
template and trigger loggers.

What is faked, as only things outside HA may be:

- the clock: the event loop runs on virtual time, which moves only when a
  step says so (every timer due on the way runs, in order), and
  `time.time`, `homeassistant.util.dt.utcnow` and `now` follow it;
- randomness: `random` is seeded, so the time trackers' microsecond
  offsets are the same on every run;
- the services the actions call (`notify.mobile_app_phone`,
  `light.turn_on`, …) only record the data they get.

Run it twice: the output must not change.
"""

import asyncio
import datetime as dt
import importlib.metadata
import json
import logging
import os
import pathlib
import random
import sys
import tempfile
import time
import zoneinfo

PINNED = {"homeassistant": "2026.6.1", "jinja2": "3.1.6", "voluptuous": "0.15.2"}
OUT = pathlib.Path(__file__).resolve().parents[2] / "crates/ha-automation/src/vectors"
TZ = "Europe/London"
START = dt.datetime(2026, 11, 10, 16, 0, 0, tzinfo=zoneinfo.ZoneInfo(TZ))
LOGGERS = (
    "homeassistant.components.automation",
    "homeassistant.components.homeassistant.triggers",
    "homeassistant.components.template",
    "homeassistant.helpers.condition",
    "homeassistant.helpers.event",
    "homeassistant.helpers.script",
    "homeassistant.helpers.service",
    "homeassistant.helpers.template",
    "homeassistant.helpers.trigger",
)

TEMP = {"friendly_name": "Lounge Temperature", "unit_of_measurement": "°C"}
HUMID = {"friendly_name": "Lounge Humidity", "unit_of_measurement": "%"}
PHONE = "notify.mobile_app_phone"
# Which of HA's `trigger` keys a trigger has (`in` on the dict).
KEYS = (
    "{{ ['above', 'alias', 'attribute', 'below', 'description', 'entity_id', 'for', "
    "'from_state', 'id', 'idx', 'now', 'platform', 'to_state'] | select('in', trigger) | list }}"
)


def temp(value, **attrs):
    return {"op": "set", "entity_id": "sensor.lounge_temperature", "state": value,
            "attributes": {**TEMP, **attrs}}


def set_(entity_id, state, attributes=None):
    return {"op": "set", "entity_id": entity_id, "state": state, "attributes": attributes or {}}


def wait(s):
    return {"op": "wait", "s": s}


BASE_STATES = [
    set_("sensor.lounge_temperature", "22", TEMP),
    set_("sensor.lounge_humidity", "48", HUMID),
    set_("binary_sensor.lounge_window", "off", {"friendly_name": "Lounge Window"}),
    set_("light.lounge", "off", {"friendly_name": "Lounge"}),
    set_("light.hall", "off", {"friendly_name": "Hall"}),
]

# (name, doc, automations, steps[, initial states written over BASE_STATES[,
# entity ids to register, whose registry ids `<uuid:entity_id>` stands for]])
SCENARIOS = [
    (
        "alert_numeric_state",
        "The owner's alert: numeric_state above 27 on the lounge sensor sends a "
        "phone notification whose message reads the trigger, states() and "
        "state_attr(); it fires on each crossing only.",
        [
            {
                "id": "hot_lounge",
                "alias": "Hot lounge",
                "triggers": [
                    {"trigger": "numeric_state", "entity_id": "sensor.lounge_temperature",
                     "above": 27}
                ],
                "actions": [
                    {
                        "action": PHONE,
                        "data": {
                            "title": "{{ state_attr(trigger.entity_id, 'friendly_name') }}",
                            "message": "It's {{ trigger.to_state.state }}°C in the lounge "
                            "({{ states('sensor.lounge_humidity') }}% humidity, was "
                            "{{ trigger.from_state.state }})",
                            "data": {"tag": "lounge-temp", "push": {"sound": "default"}},
                        },
                    }
                ],
            }
        ],
        [temp("26"), wait(60), temp("28"), wait(60), temp("29"), wait(60), temp("25"),
         wait(60), set_("sensor.lounge_humidity", "51", HUMID), temp("30.5"), wait(10)],
    ),
    (
        "alert_state_template_condition",
        "A state trigger with no `to` (attribute changes fire too) gated by a "
        "template condition and a shorthand template condition.",
        [
            {
                "id": "hot_lounge_closed",
                "alias": "Hot lounge, window closed",
                "triggers": [{"trigger": "state", "entity_id": "sensor.lounge_temperature"}],
                "conditions": [
                    {"condition": "template",
                     "value_template": "{{ trigger.to_state.state | float(0) > 27 }}"},
                    "{{ is_state('binary_sensor.lounge_window', 'off') }}",
                ],
                "actions": [
                    {"action": PHONE,
                     "data": {"message": "{{ trigger.from_state.state }} -> "
                              "{{ trigger.to_state.state }} {{ trigger.to_state.attributes.unit_of_measurement }}"}}
                ],
            }
        ],
        [temp("26"), wait(5), temp("28"), wait(5), temp("28", source="attr"), wait(5),
         set_("binary_sensor.lounge_window", "on"), temp("29"), wait(5),
         set_("binary_sensor.lounge_window", "off"), temp("27"), wait(5), temp("31")],
    ),
    (
        "trigger_variables_state",
        "Every `trigger` variable of a state trigger with an id, an alias and "
        "`for`, and `this`, as the action's data sees them (native types).",
        [
            {
                "id": "window_open",
                "alias": "Window open",
                "triggers": [
                    {"trigger": "state", "entity_id": "binary_sensor.lounge_window",
                     "to": "on", "for": {"seconds": 5}, "id": "opened", "alias": "Opened"}
                ],
                "actions": [
                    {
                        "action": PHONE,
                        "data": {
                            "platform": "{{ trigger.platform }}",
                            "id": "{{ trigger.id }}",
                            "idx": "{{ trigger.idx }}",
                            "alias": "{{ trigger.alias }}",
                            "entity_id": "{{ trigger.entity_id }}",
                            "from": "{{ trigger.from_state.state }}",
                            "to": "{{ trigger.to_state.state }}",
                            "to_name": "{{ trigger.to_state.name }}",
                            "to_domain": "{{ trigger.to_state.domain }}",
                            "to_object_id": "{{ trigger.to_state.object_id }}",
                            "to_changed": "{{ trigger.to_state.last_changed }}",
                            "for": "{{ trigger.for }}",
                            "for_seconds": "{{ trigger.for.total_seconds() }}",
                            "attribute": "{{ trigger.attribute }}",
                            "description": "{{ trigger.description }}",
                            "keys": KEYS,
                            "this_entity": "{{ this.entity_id }}",
                            "this_state": "{{ this.state }}",
                            "this_current": "{{ this.attributes.current }}",
                            "this_mode": "{{ this.attributes.mode }}",
                            "this_last": "{{ this.attributes.last_triggered }}",
                            "this_id": "{{ this.attributes.id }}",
                        },
                    }
                ],
            }
        ],
        [set_("binary_sensor.lounge_window", "on"), wait(2),
         set_("binary_sensor.lounge_window", "off"), wait(2),
         set_("binary_sensor.lounge_window", "on"), wait(10),
         set_("binary_sensor.lounge_window", "off"), wait(1),
         set_("binary_sensor.lounge_window", "on"), wait(10)],
    ),
    (
        "trigger_variables_numeric_time",
        "The `trigger` variables of numeric_state and time triggers.",
        [
            {
                "id": "numeric_vars",
                "alias": "Numeric vars",
                "triggers": [
                    {"trigger": "numeric_state", "entity_id": "sensor.lounge_temperature",
                     "above": 20, "below": "30"}
                ],
                "actions": [
                    {"action": PHONE, "data": {
                        "platform": "{{ trigger.platform }}",
                        "entity_id": "{{ trigger.entity_id }}",
                        "below": "{{ trigger.below }}",
                        "above": "{{ trigger.above }}",
                        "for": "{{ trigger.for }}",
                        "attribute": "{{ trigger.attribute is defined }}",
                        "description": "{{ trigger.description }}",
                        "from": "{{ trigger.from_state.state }}",
                        "keys": KEYS,
                    }}
                ],
            },
            {
                "id": "time_vars",
                "alias": "Time vars",
                "triggers": [{"trigger": "time", "at": "16:02:00", "id": "afternoon"}],
                "actions": [
                    {"action": PHONE, "data": {
                        "platform": "{{ trigger.platform }}",
                        "id": "{{ trigger.id }}",
                        "now": "{{ trigger.now.strftime('%Y-%m-%d %H:%M:%S %Z') }}",
                        "tz": "{{ trigger.now.tzinfo }}",
                        "entity_id": "{{ trigger.entity_id }}",
                        "description": "{{ trigger.description }}",
                        "keys": KEYS,
                    }}
                ],
            },
        ],
        [temp("35"), wait(30), temp("25"), wait(150)],
    ),
    (
        "service_trigger_variables",
        "automation.trigger with `variables`: `trigger` is {platform: None} and "
        "the variables are there; `trigger.id` is undefined.",
        [
            {
                "id": "manual",
                "alias": "Manual",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge"}],
                "actions": [
                    {"action": PHONE, "data": {
                        "platform": "{{ trigger.platform }}",
                        "who": "{{ who }}",
                        "n": "{{ n + 1 }}",
                        "id": "{{ trigger.id }}",
                    }}
                ],
            }
        ],
        [wait(1), {"op": "service", "domain": "automation", "service": "trigger",
                   "data": {"entity_id": "automation.manual",
                            "variables": {"who": "me", "n": 41}}}, wait(1)],
    ),
    (
        "variables",
        "Top-level `variables` (static, templated, chained, reading `trigger` "
        "and `this`) and `trigger_variables` (rendered limited at attach, and "
        "available to the trigger's templates and the actions).",
        [
            {
                "id": "with_variables",
                "alias": "With variables",
                "trigger_variables": {"limit": 27, "label": "{{ 'Lounge' ~ ' room' }}",
                                      "sensor": "sensor.lounge_temperature"},
                "variables": {
                    "unit": "°C",
                    "value": "{{ trigger.to_state.state | float }}",
                    "over": "{{ value - limit }}",
                    "summary": "{{ label }}: {{ value }}{{ unit }} ({{ over }} over)",
                    "nested": {"a": "{{ 1 + 1 }}", "b": ["{{ 'x' }}", 3]},
                    "who": "{{ this.attributes.friendly_name }}",
                },
                "triggers": [
                    {"trigger": "template",
                     "value_template": "{{ states(sensor) | float(0) > limit }}"}
                ],
                "actions": [
                    {"action": PHONE, "data": {
                        "message": "{{ summary }}",
                        "over": "{{ over }}",
                        "nested": "{{ nested }}",
                        "nested_a": "{{ nested.a + 1 }}",
                        "who": "{{ who }}",
                        "limit": "{{ limit }}",
                    }}
                ],
            },
            {
                "id": "limited_trigger_variables",
                "alias": "Limited trigger variables",
                "trigger_variables": {"now_state": "{{ states('sensor.lounge_temperature') }}"},
                "triggers": [{"trigger": "state", "entity_id": "sensor.lounge_temperature"}],
                "actions": [{"action": PHONE, "data": {"message": "never"}}],
            },
            {
                "id": "bad_variables",
                "alias": "Bad variables",
                "variables": {"x": "{{ 1 / 0 }}"},
                "triggers": [{"trigger": "state", "entity_id": "light.hall", "to": "on"}],
                "actions": [{"action": PHONE, "data": {"message": "never"}}],
            },
        ],
        [temp("26"), wait(1), temp("28.5"), wait(1), set_("light.hall", "on"), wait(1)],
    ),
    (
        "native_types",
        "Service data rendered with `template_complex` and `_parse_result`: "
        "numbers, lists, dicts, booleans, None, strings that stay strings, "
        "templated keys and lists, and data that is one template.",
        [
            {
                "id": "types",
                "alias": "Types",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "actions": [
                    {"action": PHONE, "data": {
                        "int": "{{ 5 }}",
                        "neg": "{{ -3 }}",
                        "float": "{{ 1.50 }}",
                        "exp": "{{ '1e3' }}",
                        "leading_zero": "{{ '007' }}",
                        "padded": "{{ ' 42 ' }}",
                        "list": "{{ [1, 'two', 3.0] }}",
                        "tuple": "{{ (1, 2) }}",
                        "dict": "{{ {'a': 1, 'b': [True, None]} }}",
                        "true": "{{ true }}",
                        "on": "{{ 'on' }}",
                        "none": "{{ none }}",
                        "empty": "{{ '' }}",
                        "text": "{{ 'hello' }} world",
                        "static": "plain text",
                        "static_num": 12,
                        "static_list": [1, "{{ 1 + 1 }}", {"k": "{{ 'v' }}"}],
                        "{{ 'templated_key' }}": "{{ 'x' * 3 }}",
                        "brace_text": "a { b } c",
                        "big": "{{ 2 ** 70 }}",
                        "str_list": "{{ \"['a', 'b']\" }}",
                    }},
                    {"action": PHONE, "data": "{{ {'message': 'whole', 'n': 2} }}"},
                    {"action": PHONE,
                     "data": {"message": "{{ 'data_template' }}", "a": 1},
                     "data_template": {"a": "{{ 2 }}", "b": "{{ 3 }}"}},
                ],
            }
        ],
        [set_("light.lounge", "on"), wait(1)],
    ),
    (
        "service_and_target_templates",
        "A templated action name and target (a dict of templates, and one "
        "template), entity ids rendered as a string or a list.",
        [
            {
                "id": "targets",
                "alias": "Targets",
                "triggers": [{"trigger": "state", "entity_id": "binary_sensor.lounge_window",
                              "to": "on"}],
                "actions": [
                    {"action": "notify.{{ 'mobile_app_' ~ 'phone' }}",
                     "data": {"message": "templated action"}},
                    {"action": "light.turn_on",
                     "target": {"entity_id": "{{ 'light.lounge' }}"},
                     "data": {"brightness": "{{ 100 + 28 }}"}},
                    {"action": "light.turn_on",
                     "target": {"entity_id": "{{ ['light.lounge', 'LIGHT.HALL'] }}"}},
                    {"action": "light.turn_on",
                     "target": {"entity_id": "{{ 'light.lounge, light.hall' }}"}},
                    {"action": "light.turn_on",
                     "target": "{{ {'entity_id': 'light.hall'} }}"},
                    {"action": "light.turn_on",
                     "target": {"entity_id": ["light.lounge", "{{ 'light.hall' }}"]}},
                    {"action": "light.turn_off",
                     "target": {"entity_id": "light.lounge"},
                     "data": {"transition": "{{ 2 }}"}},
                ],
            }
        ],
        [set_("binary_sensor.lounge_window", "on"), wait(1)],
    ),
    (
        "registry_id_targets",
        "Rendered targets may name entities by their entity registry id "
        "(`comp_entity_ids_or_uuids`): resolved to the current entity id; an "
        "unknown one fails the call.",
        [
            {
                "id": "by_registry_id",
                "alias": "By registry id",
                "variables": {"selected": "<uuid:light.hall>"},
                "triggers": [{"trigger": "state", "entity_id": "binary_sensor.lounge_window",
                              "to": "on"}],
                "actions": [
                    {"action": "light.turn_on", "target": {"entity_id": "{{ selected }}"}},
                    {"action": "light.turn_on",
                     "target": {"entity_id": "{{ ['<uuid:light.lounge>', 'light.hall'] }}"}},
                    {"action": "light.turn_on",
                     "target": {"entity_id": "{{ '<uuid:light.lounge>, light.hall' }}"}},
                    {"action": "light.turn_off", "target": {"entity_id": "<uuid:light.lounge>"}},
                    {"action": "light.turn_off",
                     "target": {"entity_id": "{{ '0123456789abcdef0123456789abcdef' }}"}},
                ],
            },
            {
                "id": "unknown_id_and_bad_data",
                "alias": "Unknown id and bad data",
                "triggers": [{"trigger": "state", "entity_id": "binary_sensor.lounge_window",
                              "to": "on"}],
                "actions": [
                    {"action": "light.turn_on",
                     "target": {"entity_id": "{{ '0123456789abcdef0123456789abcdef' }}"},
                     "data": {"brightness": "{{ 1 / 0 }}"}},
                ],
            },
            {
                "id": "unknown_static_id_and_bad_data",
                "alias": "Unknown static id and bad data",
                "triggers": [{"trigger": "state", "entity_id": "binary_sensor.lounge_window",
                              "to": "on"}],
                "actions": [
                    {"action": "light.turn_on",
                     "target": {"entity_id": "0123456789abcdef0123456789abcdef"},
                     "data": {"brightness": "{{ 1 / 0 }}"}},
                ],
            },
        ],
        [set_("binary_sensor.lounge_window", "on"), wait(1)],
        [],
        ["light.lounge", "light.hall"],
    ),
    (
        "render_errors",
        "Templates that fail while running: in data (an error, an undefined "
        "value printed and used, data that isn't a dict), in the action name, "
        "in a target, and in a condition. Each stops that run; the next "
        "trigger runs again.",
        [
            {
                "id": "zero",
                "alias": "Zero",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "actions": [
                    {"action": PHONE, "data": {"message": "{{ missing }} printed"}},
                    {"action": PHONE, "data": {"message": "{{ 1 / 0 }}"}},
                    {"action": PHONE, "data": {"message": "never"}},
                ],
            },
            {
                "id": "undefined_use",
                "alias": "Undefined use",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "actions": [{"action": PHONE, "data": {"message": "{{ missing + 1 }}"}}],
            },
            {
                "id": "not_a_dict",
                "alias": "Not a dict",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "actions": [{"action": PHONE, "data": "{{ [1, 2] }}"}],
            },
            {
                "id": "bad_service",
                "alias": "Bad service",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "actions": [{"action": "{{ 'not a service' }}"}],
            },
            {
                "id": "bad_target",
                "alias": "Bad target",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "actions": [{"action": "light.turn_on",
                             "target": {"entity_id": "{{ 'not valid' }}"}}],
            },
            {
                "id": "bad_condition",
                "alias": "Bad condition",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "conditions": [{"condition": "template", "value_template": "{{ 1 / 0 }}"}],
                "actions": [{"action": PHONE, "data": {"message": "never"}}],
            },
            {
                "id": "undefined_condition",
                "alias": "Undefined condition",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "conditions": ["{{ nothing }}"],
                "actions": [{"action": PHONE, "data": {"message": "never"}}],
            },
            {
                "id": "numeric_template_error",
                "alias": "Numeric template error",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "conditions": [
                    {"condition": "numeric_state", "entity_id": ["light.lounge", "light.hall"],
                     "value_template": "{{ 1 / 0 }}", "below": 5},
                    {"condition": "template", "value_template": "{{ x.y }}"},
                ],
                "actions": [{"action": PHONE, "data": {"message": "never"}}],
            },
            {
                "id": "numeric_template_values",
                "alias": "Numeric template values",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "conditions": [
                    {"condition": "numeric_state", "entity_id": "light.lounge",
                     "value_template": "{{ [1] }}", "below": 5},
                ],
                "actions": [{"action": PHONE, "data": {"message": "never"}}],
            },
            {
                "id": "numeric_template_true",
                "alias": "Numeric template true",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "conditions": [
                    {"condition": "numeric_state", "entity_id": "light.lounge",
                     "value_template": "{{ true }}", "below": 5, "above": 0.5},
                    {"condition": "numeric_state", "entity_id": "light.lounge",
                     "value_template": "{{ 'unknown' }}", "below": 5},
                ],
                "actions": [{"action": PHONE, "data": {"message": "never"}}],
            },
        ],
        [set_("light.lounge", "on"), wait(1)],
    ),
    (
        "invalid_templates",
        "A template that doesn't compile makes its automation invalid "
        "(unavailable), wherever it is.",
        [
            {
                "id": "bad_syntax",
                "alias": "Bad syntax",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge"}],
                "actions": [{"action": PHONE, "data": {"message": "{{ 1 + }}"}}],
            },
            {
                "id": "static_condition",
                "alias": "Static condition",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge"}],
                "conditions": ["not a template"],
                "actions": [{"action": PHONE, "data": {"message": "x"}}],
            },
            {
                "id": "good",
                "alias": "Good",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge"}],
                "actions": [{"action": PHONE, "data": {"message": "{{ 'still runs' }}"}}],
            },
        ],
        [set_("light.lounge", "on"), wait(1)],
    ),
    (
        "template_trigger_entity",
        "A template trigger on a sensor: fires when the template becomes true, "
        "not while it stays true, and re-arms when it is false; with its "
        "`trigger` variables.",
        [
            {
                "id": "hot_template",
                "alias": "Hot template",
                "triggers": [{"trigger": "template", "id": "hot",
                              "value_template": "{{ states('sensor.lounge_temperature') | float(0) > 27 }}"}],
                "actions": [
                    {"action": PHONE, "data": {
                        "message": "{{ trigger.to_state.state }}",
                        "platform": "{{ trigger.platform }}",
                        "id": "{{ trigger.id }}",
                        "idx": "{{ trigger.idx }}",
                        "entity_id": "{{ trigger.entity_id }}",
                        "from": "{{ trigger.from_state.state }}",
                        "for": "{{ trigger.for }}",
                        "description": "{{ trigger.description }}",
                        "keys": KEYS,
                    }}
                ],
            }
        ],
        [temp("26"), wait(5), temp("28"), wait(5), temp("29"), wait(5),
         set_("sensor.lounge_humidity", "50", HUMID), wait(5), temp("20"), wait(5),
         temp("30"), wait(5)],
    ),
    (
        "template_trigger_for",
        "A template trigger with `for`: fires once the template has stayed "
        "true that long; going false first cancels it.",
        [
            {
                "id": "hot_for",
                "alias": "Hot for",
                "triggers": [{"trigger": "template", "for": {"seconds": 30},
                              "value_template": "{{ states('sensor.lounge_temperature') | float(0) > 27 }}"}],
                "actions": [{"action": PHONE, "data": {
                    "message": "{{ trigger.to_state.state }} for {{ trigger.for }}"}}],
            }
        ],
        [temp("28"), wait(20), temp("20"), wait(20), temp("29"), wait(10), temp("30"),
         wait(40)],
    ),
    (
        "template_trigger_time",
        "Template triggers that read the clock re-render every minute (at "
        "second 0); ones that don't, never do.",
        [
            {
                "id": "minute_three",
                "alias": "Minute three",
                "triggers": [{"trigger": "template",
                              "value_template": "{{ now().minute == 3 }}"}],
                "actions": [{"action": PHONE, "data": {
                    "message": "{{ trigger.description }}",
                    "entity_id": "{{ trigger.entity_id }}",
                    "to": "{{ trigger.to_state }}"}}],
            },
            {
                "id": "time_or_light",
                "alias": "Time or light",
                "triggers": [{"trigger": "template",
                              "value_template": "{{ is_state('light.hall', 'on') and now().minute >= 2 }}"}],
                "actions": [{"action": PHONE, "data": {"message": "{{ trigger.description }}"}}],
            },
        ],
        [wait(30), set_("light.hall", "on"), wait(300)],
    ),
    (
        "template_trigger_domain",
        "A template trigger that iterates a domain is rate limited to one "
        "render a second; a specific entity it reads is not.",
        [
            {
                "id": "two_lights",
                "alias": "Two lights",
                "triggers": [{"trigger": "template",
                              "value_template": "{{ states.light | selectattr('state', 'eq', 'on') | list | count >= 2 }}"}],
                "actions": [{"action": PHONE, "data": {"message": "{{ trigger.description }}"}}],
            }
        ],
        [set_("light.lounge", "on"), wait(5), set_("light.lounge", "off"), wait(0.2),
         set_("light.lounge", "on"), wait(0.2), set_("light.hall", "on"), wait(5),
         set_("light.hall", "off"), wait(5), set_("light.attic", "on"), wait(0.1),
         set_("light.hall", "on"), wait(5)],
    ),
    (
        "template_trigger_errors",
        "A template trigger whose template fails: a warning when it is set "
        "up, a warning the first time it fails after that, none while it keeps "
        "failing, and it fires once it renders true.",
        [
            {
                "id": "bad_float",
                "alias": "Bad float",
                "triggers": [{"trigger": "template",
                              "value_template": "{{ states('sensor.lounge_humidity') | float > 50 }}"}],
                "actions": [{"action": PHONE, "data": {"message": "humid"}}],
            }
        ],
        [wait(5), set_("sensor.lounge_humidity", "x", HUMID), wait(5),
         set_("sensor.lounge_humidity", "45", HUMID), wait(5),
         set_("sensor.lounge_humidity", "y", HUMID), wait(5),
         set_("sensor.lounge_humidity", "55", HUMID), wait(5)],
        # Failing from the start.
        [set_("sensor.lounge_humidity", "abc", HUMID)],
    ),
    (
        "numeric_value_template",
        "numeric_state with `value_template` (trigger and condition), reading "
        "the `state` variable.",
        [
            {
                "id": "thermostat",
                "alias": "Thermostat",
                "triggers": [{"trigger": "numeric_state", "entity_id": "climate.lounge",
                              "value_template": "{{ state.attributes.current_temperature }}",
                              "above": 21}],
                "conditions": [{"condition": "numeric_state", "entity_id": "climate.lounge",
                                "value_template": "{{ state.attributes.target_temperature - 1 }}",
                                "below": 25}],
                "actions": [{"action": PHONE, "data": {
                    "message": "{{ trigger.to_state.attributes.current_temperature }}"}}],
            },
            {
                "id": "thermostat_bad",
                "alias": "Thermostat bad",
                "triggers": [{"trigger": "numeric_state", "entity_id": "climate.lounge",
                              "value_template": "{{ state.attributes.mode }}",
                              "above": 21}],
                "actions": [{"action": PHONE, "data": {"message": "never"}}],
            },
        ],
        [set_("climate.lounge", "heat", {"current_temperature": 22, "target_temperature": 22,
                                         "mode": "x"}),
         wait(1),
         set_("climate.lounge", "heat", {"current_temperature": 20, "target_temperature": 22,
                                         "mode": "x"}),
         wait(1),
         set_("climate.lounge", "heat", {"current_temperature": 21.5, "target_temperature": 22,
                                         "mode": "x"}),
         wait(1),
         set_("climate.lounge", "heat", {"current_temperature": 20, "target_temperature": 27,
                                         "mode": "x"}),
         wait(1),
         set_("climate.lounge", "heat", {"current_temperature": 23, "target_temperature": 27,
                                         "mode": "x"}),
         wait(1)],
    ),
    (
        "choose_templates",
        "choose with template conditions (and the shorthand), picking a "
        "choice or the default by the trigger.",
        [
            {
                "id": "chooser",
                "alias": "Chooser",
                "triggers": [{"trigger": "state", "entity_id": "sensor.lounge_temperature"}],
                "actions": [
                    {"choose": [
                        {"conditions": [{"condition": "template",
                                         "value_template": "{{ trigger.to_state.state | int > 30 }}"}],
                         "sequence": [{"action": PHONE, "data": {"message": "very hot {{ trigger.to_state.state }}"}}]},
                        {"conditions": "{{ trigger.to_state.state | int > 25 }}",
                         "sequence": [{"action": PHONE, "data": {"message": "hot {{ trigger.to_state.state }}"}}]},
                    ],
                     "default": [{"action": PHONE, "data": {"message": "fine {{ trigger.to_state.state }}"}}]},
                ],
            },
            {
                "id": "chooser_errors",
                "alias": "Chooser errors",
                "triggers": [{"trigger": "state", "entity_id": "sensor.lounge_temperature"}],
                "actions": [
                    {"choose": [
                        {"conditions": ["{{ true }}", "{{ 1 / 0 }}", "{{ 1 / 0 }}"],
                         "sequence": [{"action": PHONE, "data": {"message": "never"}}]},
                        {"conditions": [{"condition": "numeric_state",
                                         "entity_id": "sensor.missing", "below": 5}],
                         "sequence": [{"action": PHONE, "data": {"message": "never"}}]},
                    ],
                     "default": [{"action": PHONE, "data": {"message": "default"}}]},
                ],
            },
        ],
        [temp("31"), wait(1), temp("27"), wait(1), temp("20"), wait(1)],
    ),
    (
        "wait_for_trigger_templates",
        "wait_for_trigger with a template trigger and a state trigger; the "
        "actions after it read `wait`.",
        [
            {
                "id": "waiter",
                "alias": "Waiter",
                "triggers": [{"trigger": "state", "entity_id": "light.lounge", "to": "on"}],
                "actions": [
                    {"wait_for_trigger": [
                        {"trigger": "template",
                         "value_template": "{{ states('sensor.lounge_temperature') | float(0) > 27 }}"},
                        {"trigger": "state", "entity_id": "binary_sensor.lounge_window",
                         "to": "on", "id": "window"},
                    ]},
                    {"action": PHONE, "data": {
                        "platform": "{{ wait.trigger.platform }}",
                        "id": "{{ wait.trigger.id }}",
                        "to": "{{ wait.trigger.to_state.state }}",
                        "completed": "{{ wait.completed }}",
                        "remaining": "{{ wait.remaining }}",
                        "first": "{{ trigger.to_state.state }}",
                    }},
                ],
            }
        ],
        [set_("light.lounge", "on"), wait(5), temp("28"), wait(5),
         set_("light.lounge", "off"), wait(1), set_("light.lounge", "on"), wait(5),
         set_("binary_sensor.lounge_window", "on"), wait(5)],
    ),
]


def check_versions():
    for pkg, want in PINNED.items():
        have = importlib.metadata.version(pkg)
        if have != want:
            sys.exit(f"{pkg} is {have}; these vectors are made with {want}")


class VirtualLoop(asyncio.SelectorEventLoop):
    """An event loop whose clock moves only when told to."""

    now = 0.0

    def time(self):
        return self.now


LOOP = VirtualLoop()


def wall():
    return START.timestamp() + LOOP.now


def iso(seconds):
    return dt.datetime.fromtimestamp(START.timestamp() + seconds, dt.UTC).isoformat()


async def settle():
    for _ in range(200):
        await asyncio.sleep(0)


async def advance(seconds):
    """Move the clock on, running every timer due on the way in order."""
    target = LOOP.now + seconds
    while True:
        await settle()
        due = [h.when() for h in LOOP._scheduled if not h.cancelled()]
        nxt = min(due, default=None)
        if nxt is None or nxt > target:
            break
        LOOP.now = max(LOOP.now, nxt)
    LOOP.now = target
    await settle()


class Logs(logging.Handler):
    def __init__(self):
        super().__init__(logging.DEBUG)
        self.records = []

    def emit(self, record):
        if record.name.startswith(LOGGERS) and record.levelno >= logging.WARNING:
            self.records.append((LOOP.now, record.levelname, record.name, record.getMessage()))


def patch_clock():
    """`time.time`, `dt_util.utcnow` and `dt_util.now` follow the loop, in
    every module that has imported them."""
    from homeassistant.util import dt as dt_util

    orig = {"utcnow": dt_util.utcnow, "now": dt_util.now}

    def utcnow():
        return dt.datetime.fromtimestamp(wall(), dt.UTC)

    def now(time_zone=None):
        return dt.datetime.fromtimestamp(wall(), time_zone or dt_util.get_default_time_zone())

    new = {id(orig["utcnow"]): utcnow, id(orig["now"]): now}
    time.time = wall

    def rebind():
        for module in list(sys.modules.values()):
            names = getattr(module, "__dict__", None)
            if not names or not module.__name__.startswith("homeassistant"):
                continue
            for key, value in list(names.items()):
                if id(value) in new and value in orig.values():
                    setattr(module, key, new[id(value)])

    return rebind


def plain(value):
    """Service data as JSON: tuples and sets as lists."""
    if isinstance(value, dict):
        return {str(k): plain(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [plain(v) for v in value]
    if isinstance(value, set):
        return sorted(plain(v) for v in value)
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    return f"<{type(value).__name__}> {value}"


async def run(automations, steps, extra_states, registered, registry):
    """Run one scenario; `registry` gets the entity registry entries made
    for `registered`, and `automations` their ids."""
    from homeassistant import bootstrap, core, loader
    from homeassistant.config_entries import ConfigEntries
    from homeassistant.helpers import entity_registry as er
    from homeassistant.setup import async_setup_component

    random.seed(1)
    LOOP.now = 0.0
    out = []
    logs = Logs()
    root = logging.getLogger()
    root.addHandler(logs)
    with tempfile.TemporaryDirectory() as config_dir:
        hass = core.HomeAssistant(config_dir)
        hass.config.skip_pip = True
        await hass.config.async_load()
        await hass.config.async_set_time_zone(TZ)
        loader.async_setup(hass)
        hass.config_entries = ConfigEntries(hass, {})
        await hass.config_entries.async_initialize()
        await bootstrap.async_load_base_functionality(hass)

        def record(kind, **data):
            out.append({"t": iso(LOOP.now), "kind": kind, **data})

        async def service(call):
            record("call", domain=call.domain, service=call.service, data=plain(dict(call.data)))

        for domain, name in [("notify", "mobile_app_phone"), ("light", "turn_on"),
                             ("light", "turn_off")]:
            hass.services.async_register(domain, name, service)

        def triggered(event):
            record("triggered", data=dict(event.data))

        hass.bus.async_listen("automation_triggered", triggered)
        # Registry entries first (their states come after), then their ids
        # where the automations say `<uuid:entity_id>`.
        entries = er.async_get(hass)
        for entity_id in registered:
            domain, object_id = entity_id.split(".")
            entry = entries.async_get_or_create(
                domain, "vectors", object_id, suggested_object_id=object_id
            )
            assert entry.entity_id == entity_id, entry.entity_id
            registry.append({"entity_id": entity_id, "id": entry.id})
        text = json.dumps(automations)
        for r in registry:
            text = text.replace(f"<uuid:{r['entity_id']}>", r["id"])
        automations[:] = json.loads(text)
        for s in BASE_STATES + extra_states:
            hass.states.async_set(s["entity_id"], s["state"], s["attributes"])
        assert await async_setup_component(hass, "homeassistant", {})
        logs.records.clear()
        await async_setup_component(hass, "automation", {"automation": automations})
        await settle()
        await hass.async_start()
        await settle()
        for a in automations:
            entity = f"automation.{a['id']}"
            state = hass.states.get(entity)
            record("automation", entity_id=entity, state=state.state if state else None)

        def flush_logs():
            for t, level, logger, msg in logs.records:
                out.append({"t": iso(t), "kind": "log", "level": level, "logger": logger,
                            "msg": msg})
            logs.records.clear()

        flush_logs()
        for step in steps:
            if step["op"] == "set":
                hass.states.async_set(step["entity_id"], step["state"], step["attributes"])
                await settle()
            elif step["op"] == "wait":
                await advance(step["s"])
            elif step["op"] == "service":
                hass.async_create_task(
                    hass.services.async_call(step["domain"], step["service"], step["data"],
                                             blocking=True)
                )
                await settle()
            flush_logs()
        await hass.async_stop(force=True)
    root.removeHandler(logs)
    # Automations triggered by one change run side by side, in an order that
    # depends on Python's string hashing: within one instant, observations
    # are grouped by kind and by automation (each one's own order is kept).
    def key(e):
        who = {
            "log": lambda: e["logger"],
            "automation": lambda: e["entity_id"],
            "triggered": lambda: e["data"]["entity_id"],
        }.get(e["kind"], lambda: "")()
        return (e["t"], e["kind"], who)

    return sorted(out, key=key)


async def scenarios(rebind):
    result = []
    for entry in SCENARIOS:
        name, doc, automations, steps = entry[:4]
        extra = entry[4] if len(entry) > 4 else []
        registered = entry[5] if len(entry) > 5 else []
        rebind()
        automations = json.loads(json.dumps(automations))
        registry = []
        expected = await run(automations, steps, extra, registered, registry)
        scenario = {
            "name": name,
            "doc": doc,
            "states": BASE_STATES + extra,
            "automations": automations,
            "steps": steps,
            "expected": expected,
        }
        if registry:
            scenario["registry"] = registry
        result.append(scenario)
    return result


def main():
    check_versions()
    os.environ["TZ"] = TZ
    time.tzset()
    logging.basicConfig(handlers=[logging.NullHandler()])
    logging.getLogger().setLevel(logging.DEBUG)
    rebind = patch_clock()
    asyncio.set_event_loop(LOOP)
    # Import what the scenarios use, so their clock functions are rebound.
    import homeassistant.components.automation  # noqa: F401
    import homeassistant.components.homeassistant.triggers.numeric_state  # noqa: F401
    import homeassistant.components.homeassistant.triggers.state  # noqa: F401
    import homeassistant.components.homeassistant.triggers.time  # noqa: F401
    import homeassistant.components.template.trigger  # noqa: F401

    rebind()
    data = {"start": START.isoformat(), "time_zone": TZ,
            "scenarios": LOOP.run_until_complete(scenarios(rebind))}
    (OUT / "templates.json").write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
