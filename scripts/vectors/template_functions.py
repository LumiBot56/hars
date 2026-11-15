#!/usr/bin/env python3
"""Vectors for Home Assistant's template functions: states, type casts,
maths and time.

    .venv/bin/python -I scripts/vectors/template_functions.py

Runs HA 2026.6.1's `helpers/template` (Jinja2 3.1.6, ciso8601 2.3.3) on a
`HomeAssistant` holding the states in
crates/ha-core/src/template/vectors/home.json (an input, committed), in its
time zone, with the clock stopped at its `now`, and writes:

- crates/ha-core/src/template/vectors/functions.json: each template
  rendered with `Template.async_render_to_info` (what `render_template`
  uses), lenient and strict: the result as HA's WebSocket would send it,
  the error, what HA's `log_fn` was told, and the `listeners` that
  `render_template` would send with it (`RenderInfo` through
  `_render_infos_to_track_states`, with `has_time`).
- crates/ha-api/src/ws/vectors/render_template_states.json: whole
  `render_template` conversations against the same home.

Only the clock is faked: `homeassistant.util.dt.now` and `utcnow` return
the fixture's `now`. A naive datetime's `timestamp()` uses the process's
time zone (C `mktime`), so `TZ` is set to the home's, as an HA install
does (the Supervisor and HA's container docs set `TZ`).
"""

import asyncio
import datetime as dt
import importlib.metadata
import json
import logging
import os
import pathlib
import re
import sys
import tempfile
import time
import zoneinfo

PINNED = {"homeassistant": "2026.6.1", "jinja2": "3.1.6", "ciso8601": "2.3.3"}
ROOT = pathlib.Path(__file__).resolve().parents[2]
CORE = ROOT / "crates/ha-core/src/template/vectors"
API = ROOT / "crates/ha-api/src/ws/vectors"
HOME = json.loads((CORE / "home.json").read_text())

# Each is rendered lenient and strict, with no variables.
TEMPLATES = [
    # --- states ---------------------------------------------------------
    "{{ states('light.kitchen') }}",
    "{{ states('LIGHT.Kitchen') }}",
    "{{ states('light.missing') }}",
    "{{ states('Light.Bad') }}",
    "{{ states(1) }}",
    "{{ states(none) }}",
    "{{ states(x) }}",
    "{{ states('sensor.temperature', with_unit=True) }}",
    "{{ states('sensor.temperature', rounded=True) }}",
    "{{ states('sensor.power', rounded=True) }}",
    "{{ states('sensor.power', with_unit=True) }}",
    "{{ states('sensor.power', rounded=False, with_unit=True) }}",
    "{{ states('light.kitchen', with_unit=True) }}",
    "{{ 'light.kitchen' | states }}",
    "{{ states.light.kitchen }}",
    "{{ states.binary_sensor.door }}",
    "{{ states.sensor.nested }}",
    "{{ states.light.kitchen.state }}",
    "{{ states.light.kitchen.attributes }}",
    "{{ states.light.kitchen.attributes.brightness + 1 }}",
    "{{ states.light.kitchen.attributes['rgb_color'][0] }}",
    "{{ states.light.kitchen.attributes.missing }}",
    "{{ states.light.kitchen.attributes.effect }}",
    "{{ states.light.kitchen.last_changed }}",
    "{{ states.light.kitchen.last_updated }}",
    "{{ states.light.kitchen.last_reported }}",
    "{{ states.light.kitchen.name }}",
    "{{ states.light.porch.name }} {{ states.sensor.power.name }}",
    "{{ states.light.kitchen.domain }} {{ states.light.kitchen.object_id }} {{ states.light.kitchen.entity_id }}",
    "{{ states.light.kitchen.entity_id }}",
    "{{ states.sensor.temperature.state_with_unit }}",
    "{{ states.sensor.power.state_with_unit }}",
    "{{ states.light.kitchen.state_with_unit }}",
    "{{ states.light.kitchen.last_changed_timestamp }}",
    "{{ states.light.kitchen.last_updated_timestamp }}",
    "{{ states.light.kitchen.foo }}",
    "{{ states.light.kitchen['state'] }}",
    "{{ states.light.kitchen['foo'] }}",
    "{{ states.sensor.nested.attributes.data.a[2] }}",
    "{{ states.sensor.nested.attributes.data }}",
    "{{ states.light.missing }}",
    "{{ states.light.missing.state }}",
    "{{ states.light.missing is none }}",
    "{{ states.Light }}",
    "{{ states.Light.kitchen }}",
    "{{ states.light.Bad }}",
    "{{ states['light'] }}",
    "{{ states['light.kitchen'].state }}",
    "{{ states['light.Bad'] }}",
    "{{ states['light.missing'] }}",
    "{{ states.light['kitchen'].state }}",
    "{{ states }}",
    "{{ states.light }}",
    "{{ states | count }}",
    "{{ states | list | count }}",
    "{{ states.light | count }}",
    "{{ states.light | list | count }}",
    "{{ states.sensor | map(attribute='entity_id') | list }}",
    "{{ states | map(attribute='entity_id') | list }}",
    "{{ states.light | selectattr('state', 'eq', 'on') | map(attribute='name') | list }}",
    "{% for s in states.sensor %}{{ s.entity_id }}={{ s.state }};{% endfor %}",
    "{% for s in states %}{{ s.domain }} {% endfor %}",
    "{{ states.light | list }}",
    "{{ states.sensor.temperature.state | float + 1 }}",
    "{{ states.light.kitchen.state == 'on' }}",
    "{{ states.light.kitchen is defined }}",
    "{{ states.nodomain | list }}",
    # is_state, state_attr, is_state_attr, has_value
    "{{ is_state('light.kitchen', 'on') }}",
    "{{ is_state('light.kitchen', 'off') }}",
    "{{ is_state('light.kitchen', ['off', 'on']) }}",
    "{{ is_state('light.missing', 'unknown') }}",
    "{{ is_state('LIGHT.KITCHEN', 'on') }}",
    "{{ is_state('light.kitchen', 1) }}",
    "{{ 'light.kitchen' is is_state('on') }}",
    "{{ is_state(x, 'on') }}",
    "{{ state_attr('light.kitchen', 'brightness') }}",
    "{{ state_attr('light.kitchen', 'missing') }}",
    "{{ state_attr('light.missing', 'brightness') }}",
    "{{ state_attr('light.kitchen', 'rgb_color') }}",
    "{{ state_attr('sensor.nested', 'data') }}",
    "{{ state_attr('sun.sun', 'next_rising') }}",
    "{{ 'light.kitchen' | state_attr('brightness') }}",
    "{{ state_attr(x, 'brightness') }}",
    "{{ is_state_attr('light.kitchen', 'brightness', 255) }}",
    "{{ is_state_attr('light.kitchen', 'brightness', 255.0) }}",
    "{{ is_state_attr('light.kitchen', 'brightness', '255') }}",
    "{{ is_state_attr('light.kitchen', 'effect', none) }}",
    "{{ is_state_attr('light.kitchen', 'missing', none) }}",
    "{{ is_state_attr('light.kitchen', 'rgb_color', [255, 0, 0]) }}",
    "{{ is_state_attr('light.missing', 'brightness', 255) }}",
    "{{ 'light.kitchen' is is_state_attr('brightness', 255) }}",
    "{{ has_value('light.kitchen') }}",
    "{{ has_value('switch.unavailable_one') }}",
    "{{ has_value('sensor.unknown_one') }}",
    "{{ has_value('light.missing') }}",
    "{{ 'light.kitchen' | has_value }}",
    "{{ 'light.kitchen' is has_value }}",
    "{{ ['light.kitchen', 'switch.unavailable_one', 'sensor.unknown_one'] | select('has_value') | list }}",
    "{{ has_value(x) }}",
    # --- type casts -----------------------------------------------------
    "{{ float('1.5') }}",
    "{{ ' 2 ' | float }}",
    "{{ '1_000.5' | float }}",
    "{{ '-Infinity' | float }}",
    "{{ 'nan' | float }}",
    "{{ '1e3' | float }}",
    "{{ '.5' | float }} {{ '5.' | float }} {{ '+5' | float }}",
    "{{ '1__0' | float(0) }} {{ '_1' | float(0) }} {{ '1_' | float(0) }}",
    "{{ true | float }}",
    "{{ 7 | float }}",
    "{{ 'abc' | float }}",
    "{{ 'abc' | float(0) }}",
    "{{ float('abc', 0) }}",
    "{{ float('abc', default=none) }}",
    "{{ none | float }}",
    "{{ [] | float }}",
    "{{ '' | float(1) }}",
    "{{ x | float }}",
    "{{ x | float(0) }}",
    "{{ '١٢.٥' | float }}",
    "{{ '42' | int }}",
    "{{ '42.7' | int }}",
    "{{ 4.9 | int }} {{ -4.9 | int }}",
    "{{ ' 12 ' | int }}",
    "{{ '1_000' | int }}",
    "{{ '1e3' | int }}",
    "{{ 'abc' | int }}",
    "{{ 'abc' | int(0) }}",
    "{{ int('abc', default=-1) }}",
    "{{ '0x1F' | int(base=16) }}",
    "{{ '0x1F' | int(0, 16) }}",
    "{{ '1F' | int(base=16) }}",
    "{{ '0b11' | int(base=0) }}",
    "{{ '010' | int(base=0) }}",
    "{{ 'z' | int(base=36) }}",
    "{{ '12' | int(base=1) }}",
    "{{ 'inf' | int }}",
    "{{ 'nan' | int }}",
    "{{ 'nan' | int(0) }}",
    "{{ true | int }}",
    "{{ none | int(1) }}",
    "{{ none | int }}",
    "{{ 99999999999999999999 | int }}",
    "{{ '١٢' | int }}",
    "{{ x | int }}",
    "{{ int(1.5e300) }}",
    "{{ 'on' | bool }} {{ 'Yes' | bool }} {{ ' TRUE ' | bool }} {{ 'enable' | bool }} {{ '1' | bool }}",
    "{{ 'off' | bool }} {{ 'disable' | bool }} {{ 'No' | bool }} {{ '0' | bool }}",
    "{{ 1 | bool }} {{ 0 | bool }} {{ 2.5 | bool }} {{ 0.0 | bool }}",
    "{{ none | bool }}",
    "{{ 'maybe' | bool }}",
    "{{ 'maybe' | bool(false) }}",
    "{{ bool('maybe', default=none) }}",
    "{{ [] | bool }}",
    "{{ x | bool }}",
    "{{ bool('on') }}",
    "{{ is_number('1.5') }} {{ is_number('abc') }} {{ is_number('inf') }} {{ is_number('nan') }}",
    "{{ is_number(none) }} {{ is_number(1) }} {{ is_number(true) }} {{ is_number('1e5') }}",
    "{{ '2' | is_number }} {{ '2' is is_number }} {{ 'x' is is_number }}",
    "{{ is_number(x) }}",
    "{{ 'a' is string_like }} {{ 1 is string_like }} {{ none is string_like }}",
    # --- maths ----------------------------------------------------------
    "{{ e }} {{ pi }} {{ tau }}",
    "{{ sin(0.5) }} {{ cos(pi) }} {{ tan(1) }}",
    "{{ asin(0.5) }} {{ acos(0.5) }} {{ atan(1) }}",
    "{{ acos(2) }}",
    "{{ acos(2, 0) }}",
    "{{ sin('x', default=1) }}",
    "{{ 'inf' | sin }}",
    "{{ 0.5 | sin }}",
    "{{ atan2(1, 2) }}",
    "{{ atan2([1, 2]) }}",
    "{{ atan2([1, 'a'], 0) }}",
    "{{ atan2(1, 'a') }}",
    "{{ (1, 2) | atan2 }}",
    "{{ 1 | atan2(2) }}",
    "{{ log(100, 10) }} {{ log(e) }} {{ log(8, 2) }}",
    "{{ log(0) }}",
    "{{ log(-1, default=0) }}",
    "{{ log(10, 1) }}",
    "{{ log('a', 10, 1) }}",
    "{{ log(1, 'x') }}",
    "{{ 100 | log(10) }}",
    "{{ sqrt(16) }} {{ '9' | sqrt }}",
    "{{ sqrt(-1) }}",
    "{{ sqrt('a', 0) }}",
    "{{ sin(x) }}",
    "{{ average([1, 2, 3]) }} {{ average(1, 2, 3) }} {{ [1.5, 2] | average }}",
    "{{ average([]) }}",
    "{{ average([], 0) }}",
    "{{ average([1, 'a']) }}",
    "{{ average([1, 'a'], default=0) }}",
    "{{ average(5) }}",
    "{{ average() }}",
    "{{ average([1e308, 1e308]) }}",
    "{{ average([0.1] * 10) }}",
    "{{ average('12') }}",
    "{{ median([3, 1, 2]) }} {{ median([1, 2, 3, 4]) }} {{ median([1, 3]) }}",
    "{{ median(['b', 'a', 'c']) }}",
    "{{ median(['b', 'a']) }}",
    "{{ median([]) }}",
    "{{ median([], 0) }}",
    "{{ median(1, 2, 3) }}",
    "{{ median(['a', 1]) }}",
    "{{ median([true, false]) }}",
    "{{ statistical_mode([1, 2, 2, 3]) }} {{ statistical_mode([1, 1, 2, 2]) }} {{ statistical_mode(['a', 'b', 'b']) }}",
    "{{ statistical_mode([]) }}",
    "{{ statistical_mode([], 0) }}",
    "{{ statistical_mode(1, 2, 2) }}",
    "{{ statistical_mode([[1]]) }}",
    "{{ statistical_mode([1, 1.0, true]) }}",
    "{{ statistical_mode('abb') }}",
    "{{ min(1, 2, 3) }} {{ max(1, 2, 3) }} {{ min([3, 1]) }} {{ max('abc') }}",
    "{{ min() }}",
    "{{ min(5) }}",
    "{{ min([1, 'a']) }}",
    "{{ max([{'a': 1}, {'a': 2}], attribute='a') }}",
    "{{ min(['B', 'a']) }} {{ max(['B', 'a'], case_sensitive=true) }}",
    "{{ max([]) }}",
    "{{ bitwise_and(12, 10) }} {{ bitwise_or(12, 10) }} {{ bitwise_xor(12, 10) }}",
    "{{ 12 | bitwise_and(10) }}",
    "{{ bitwise_and(1.5, 1) }}",
    "{{ bitwise_and(true, true) }} {{ bitwise_xor(true, 1) }}",
    "{{ '1.5' | add(2) }} {{ 1 | add(2.5) }}",
    "{{ 'a' | add(1) }}",
    "{{ 'a' | add(1, 0) }}",
    "{{ 1 | add('x') }}",
    "{{ '2' | multiply(3) }} {{ '2' | multiply(1.5) }}",
    "{{ 'x' | multiply(2, 0) }}",
    "{{ 2 | multiply('x') }}",
    "{{ 2.5 | round }} {{ 3.5 | round }} {{ -2.5 | round }} {{ 2.675 | round(2) }} {{ 0.125 | round(2) }}",
    "{{ 1.23456 | round(3) }} {{ '1.5' | round }} {{ 7 | round(1) }}",
    "{{ 'abc' | round }}",
    "{{ 'abc' | round(default=0) }}",
    "{{ 1.234 | round(1, 'ceil') }} {{ 1.234 | round(1, 'floor') }} {{ -1.234 | round(1, 'floor') }}",
    "{{ 1.3 | round(0, 'half') }} {{ 1.2 | round(2, 'half') }} {{ 1.25 | round(0, 'half') }}",
    "{{ 15 | round(-1) }} {{ 25 | round(-1) }} {{ 1234.5 | round(-2) }}",
    "{{ 'inf' | round }}",
    "{{ 'nan' | round }}",
    "{{ 'nan' | round(1) }}",
    "{{ 1e20 | round }}",
    "{{ 2.5 | round(0, 'common') }} {{ 2.55 | round(precision=1) }}",
    "{{ 2.5 | round('x') }}",
    "{{ 2.5 | round(1.5) }}",
    "{{ 0.5 | round(0, 'ceil') }} {{ -0.5 | round(0, 'ceil') }}",
    "{{ x | round }}",
    "{{ clamp(5, 0, 10) }} {{ clamp(-1, 0, 10) }} {{ clamp(15, 0, 10) }} {{ 5 | clamp(0, 3) }} {{ clamp('5', '0', '3') }}",
    "{{ clamp('a', 0, 1) }}",
    "{{ wrap(370, 0, 360) }} {{ wrap(-10, 0, 360) }} {{ wrap(5, 3, 3) }} {{ 370 | wrap(0, 360) }}",
    "{{ wrap('a', 0, 1) }}",
    "{{ remap(5, 0, 10, 0, 100) }} {{ remap(15, 0, 10, 0, 100) }} {{ remap(5, 0, 10, 0, 10) }}",
    "{{ remap(15, 0, 10, 0, 100, edges='clamp') }} {{ remap(15, 0, 10, 0, 100, edges='wrap') }}",
    "{{ remap(15, 0, 10, 0, 100, edges='mirror') }} {{ remap(-5, 0, 10, 0, 100, edges='mirror') }} {{ remap(25, 0, 10, 0, 100, edges='mirror') }}",
    "{{ remap(3, 0, 10, 0, 100, steps=4) }} {{ 3 | remap(0, 10, 0, 100) }}",
    "{{ remap(5, 0, 0, 0, 1) }}",
    "{{ remap(5, 1, 1, 0, 10, edges='wrap') }}",
    "{{ remap('a', 0, 1, 0, 1) }}",
    # --- time -----------------------------------------------------------
    "{{ now() }}",
    "{{ utcnow() }}",
    "{{ now().year }} {{ now().month }} {{ now().day }} {{ now().hour }} {{ now().minute }} {{ now().second }} {{ now().microsecond }}",
    "{{ now().tzinfo }} {{ utcnow().tzinfo }}",
    "{{ now().isoformat() }} {{ utcnow().isoformat() }}",
    "{{ now().strftime('%Y-%m-%d %H:%M:%S %Z %z %a %A %b %B %j %p %I %y %f %w %%') }}",
    "{{ utcnow().strftime('%Z %z %H') }}",
    "{{ now().strftime('%U %W %u %G %V %e %D %T %F %R %h %n %t %C %g %k %l %M') }}",
    "{{ now().strftime('%c|%x|%X') }}",
    "{{ now().timestamp() }}",
    "{{ now().date() }} {{ now().time() }} {{ now().weekday() }} {{ now().isoweekday() }}",
    "{{ now().replace(hour=0, minute=0) }}",
    "{{ now().replace(month=10, day=25, hour=1) }}",
    "{{ now().replace(day=31, month=4) }}",
    "{{ now() - timedelta(hours=1) }}",
    "{{ now() + timedelta(days=1) }}",
    "{{ timedelta(hours=1) + now() }}",
    "{{ now() > today_at('00:00') }} {{ now() < today_at('00:00') }}",
    "{{ now() - today_at() }}",
    "{{ (now() - today_at()).total_seconds() }}",
    "{{ utcnow() - now() }} {{ utcnow() == now() }}",
    "{{ now() - states.light.kitchen.last_changed }}",
    "{{ (now() - states.light.kitchen.last_changed).seconds }}",
    "{{ [now()] }}",
    "{{ [utcnow()] }}",
    "{{ [now().date(), now().time()] }}",
    "{{ now().date() - as_datetime('2026-03-01').date() }}",
    "{{ now() is datetime }} {{ 'x' is datetime }} {{ now().date() is datetime }}",
    "{{ now() < now() + timedelta(seconds=1) }}",
    "{{ as_datetime('2026-03-29 12:00') < now() }}",
    "{{ as_datetime('2026-03-29 12:00') == now() }}",
    "{{ as_datetime('2026-03-29 12:00') - now() }}",
    "{{ now() == 1 }}",
    "{{ now() < 1 }}",
    "{{ now() + 1 }}",
    "{{ now() - 'x' }}",
    "{{ now() | string }}",
    "{{ now() ~ '' }}",
    "{{ today_at() }}",
    "{{ today_at('10:15') }}",
    "{{ today_at('01:30') }}",
    "{{ today_at('10:15:30') }}",
    "{{ today_at('10:15:30:40') }}",
    "{{ today_at('25:00') }}",
    "{{ today_at('10') }}",
    "{{ today_at('x') }}",
    "{{ '10:15' | today_at }}",
    "{{ today_at(none) }}",
    "{{ as_timestamp('2026-03-29T01:00:00+00:00') }}",
    "{{ as_timestamp('2026-03-29 12:00') }}",
    "{{ as_timestamp('2026-03-29T12:00:00.5+01:00') }}",
    "{{ as_timestamp('bad') }}",
    "{{ as_timestamp('bad', 0) }}",
    "{{ as_timestamp(states.light.kitchen.last_changed) }}",
    "{{ as_timestamp(1234) }}",
    "{{ as_timestamp(none, 0) }}",
    "{{ now() | as_timestamp }}",
    "{{ as_timestamp(x) }}",
    "{{ as_datetime('2026-03-29T01:02:03Z') }}",
    "{{ as_datetime('2026-03-29') }}",
    "{{ as_datetime('2026-03') }}",
    "{{ as_datetime('20260329') }}",
    "{{ as_datetime('2026-03-29T01') }}",
    "{{ as_datetime('2026-03-29T24:00') }}",
    "{{ as_datetime('2026-03-29 1:2:3') }}",
    "{{ as_datetime('2026-3-29 01:02:03.5+0130') }}",
    "{{ as_datetime('2026-03-29T01:02:03,5-05') }}",
    "{{ as_datetime('2026-03-29T01:02:03.1234567') }}",
    "{{ as_datetime('2026-03-29T01:02:03+01:00') }}",
    "{{ as_datetime('2026-03-29T01:02:03+00:00').tzinfo }} {{ as_datetime('2026-03-29T01:02:03+01:00').tzinfo }}",
    "{{ [as_datetime('2026-03-29T01:02:03+01:00')] }}",
    "{{ as_datetime('2026-02-30') }}",
    "{{ as_datetime(1700000000) }}",
    "{{ as_datetime('1700000000.5') }}",
    "{{ as_datetime(true) }}",
    "{{ as_datetime('bad') }}",
    "{{ as_datetime(none) }}",
    "{{ as_datetime(none, 0) }}",
    "{{ as_datetime([]) }}",
    "{{ as_datetime(now()) }}",
    "{{ as_datetime(now().date()) }}",
    "{{ '2026-03-29T01:02:03+01:00' | as_datetime }}",
    "{{ as_datetime(x) }}",
    "{{ as_local(utcnow()) }}",
    "{{ as_local(as_datetime('2026-10-25T00:30:00Z')) }}",
    "{{ as_local(as_datetime('2026-10-25T01:30:00Z')) }}",
    "{{ as_local(as_datetime('2026-03-29 12:00')) }}",
    "{{ utcnow() | as_local }}",
    "{{ as_local(now()) }}",
    "{{ as_local('x') }}",
    "{{ [as_local(as_datetime('2026-10-25T01:30:00Z'))] }}",
    "{{ as_local(as_datetime('2026-10-25T01:30:00Z')) - as_local(as_datetime('2026-10-25T00:30:00Z')) }}",
    "{{ as_local(as_datetime('2026-10-25T01:30:00Z')).timestamp() }}",
    "{{ 1774747800 | timestamp_custom }}",
    "{{ 1774747800 | timestamp_custom('%H:%M') }} {{ 1774747800 | timestamp_custom('%H:%M', false) }}",
    "{{ 1774747800.75 | timestamp_custom('%H:%M:%S.%f %Z') }}",
    "{{ 'x' | timestamp_custom }}",
    "{{ 'x' | timestamp_custom('%H', true, 0) }}",
    "{{ 1e20 | timestamp_custom }}",
    "{{ 1774747800 | timestamp_local }} {{ 1792891800 | timestamp_local }} {{ 1792895400 | timestamp_local }}",
    "{{ 'x' | timestamp_local(0) }}",
    "{{ '1774747800' | timestamp_local }}",
    "{{ 1774747800.5 | timestamp_utc }} {{ true | timestamp_utc }} {{ -1 | timestamp_utc }}",
    "{{ none | timestamp_utc }}",
    "{{ strptime('2026-03-29 10:15', '%Y-%m-%d %H:%M') }}",
    "{{ strptime('29/03/26', '%d/%m/%y') }}",
    "{{ strptime('2026-03-29T10:15:00+0100', '%Y-%m-%dT%H:%M:%S%z') }}",
    "{{ strptime('2026-03-29T10:15:00Z', '%Y-%m-%dT%H:%M:%S%z') }}",
    "{{ strptime('Mar 29 2026', '%b %d %Y') }}",
    "{{ strptime('sunday march 29 2026', '%A %B %d %Y') }}",
    "{{ strptime('10:15 PM', '%I:%M %p') }}",
    "{{ strptime('2026-3-9 1:2:3.5', '%Y-%m-%d %H:%M:%S.%f') }}",
    "{{ strptime('2026  088', '%Y %j') }}",
    "{{ strptime('x', '%Y') }}",
    "{{ strptime('x', '%Y', 0) }}",
    "{{ strptime('2026-03-29 extra', '%Y-%m-%d') }}",
    "{{ strptime('2026-02-30', '%Y-%m-%d') }}",
    "{{ strptime(1, '%Y') }}",
    "{{ timedelta(hours=1) }} {{ timedelta(1) }} {{ timedelta(days=1, hours=2, minutes=3, seconds=4.5) }}",
    "{{ timedelta(seconds=-1) }} {{ timedelta(hours=1.5) }} {{ timedelta(milliseconds=1.5) }} {{ timedelta(microseconds=0.5) }} {{ timedelta(microseconds=1.5) }}",
    "{{ timedelta(weeks=1) + timedelta(days=1) }} {{ timedelta(days=-1.5) }} {{ timedelta(0.1) }}",
    "{{ timedelta(minutes=90).total_seconds() }} {{ timedelta(hours=1).seconds }} {{ timedelta(days=-1).days }} {{ timedelta(seconds=1.5).microseconds }}",
    "{{ timedelta(days=1000000000) }}",
    "{{ timedelta(hours='x') }}",
    "{{ timedelta(hours=1) * 2 }} {{ 2.5 * timedelta(hours=1) }} {{ timedelta(hours=1) / 2 }} {{ timedelta(hours=1) / 7 }}",
    "{{ timedelta(hours=3) / timedelta(hours=1) }} {{ timedelta(hours=1) // timedelta(minutes=7) }} {{ timedelta(hours=1) // 7 }}",
    "{{ -timedelta(hours=1) }} {{ timedelta(hours=1) - timedelta(hours=2) }}",
    "{{ timedelta(hours=1) / 0 }}",
    "{{ timedelta(hours=1) == timedelta(minutes=60) }} {{ timedelta(hours=1) < timedelta(hours=2) }} {{ timedelta(0) == 0 }}",
    "{{ [timedelta(hours=1), timedelta(days=-1, microseconds=5)] }}",
    "{{ 'y' if timedelta(0) else 'n' }} {{ 'y' if timedelta(1) else 'n' }}",
    "{{ timedelta(hours=1) * 'x' }}",
    "{{ as_timedelta('PT10M') }} {{ as_timedelta('1 day, 2:03:04') }} {{ as_timedelta('10:00') }} {{ as_timedelta('-1:00:00') }}",
    "{{ as_timedelta('P1DT2H') }} {{ as_timedelta('-P1DT2H') }} {{ as_timedelta('P0.5D') }} {{ as_timedelta('PT1,5S') }}",
    "{{ as_timedelta('1 2:03:04.5') }} {{ as_timedelta('3 days 04:05:06') }} {{ as_timedelta('15') }}",
    "{{ as_timedelta('bad') }}",
    "{{ as_timedelta(5) }}",
    "{{ relative_time(states.light.kitchen.last_changed) }}",
    "{{ relative_time(now()) }}",
    "{{ relative_time(now() + timedelta(hours=1)) }}",
    "{{ relative_time('x') }}",
    "{{ relative_time(as_datetime('2026-03-28 10:00')) }}",
    "{{ states.light.porch.last_changed | relative_time }}",
    "{{ time_since(states.sensor.temperature.last_changed) }}",
    "{{ time_since(states.sensor.temperature.last_changed, 2) }}",
    "{{ time_since(now() - timedelta(days=400, hours=5), 3) }}",
    "{{ time_since(now() - timedelta(seconds=89), 2) }}",
    "{{ time_since(now() - timedelta(days=45)) }}",
    "{{ time_until(now() + timedelta(hours=26, minutes=30), 2) }}",
    "{{ time_until(now() + timedelta(seconds=30.6)) }}",
    "{{ time_until(now() - timedelta(hours=1)) }}",
    "{{ time_until('x') }}",
    "{{ time_until(now() + timedelta(days=800), 0) }}",
    "{{ states.sun.sun.attributes.next_rising | as_datetime | time_until }}",
    # Datetime methods, comparisons and sorting.
    "{{ now().tzname() }} {{ now().utcoffset() }} {{ utcnow().utcoffset() }} {{ as_datetime('2026-03-29').tzname() }}",
    "{{ now().replace(tzinfo=none) }} {{ now().replace(tzinfo=utcnow().tzinfo) }}",
    "{{ now().date().isoformat() }} {{ now().time().isoformat() }} {{ now().time().strftime('%H:%M') }}",
    "{{ now().date() + timedelta(days=3, hours=30) }} {{ now().date() - timedelta(hours=1) }}",
    "{{ [timedelta(1), timedelta(0), timedelta(hours=1)] | sort }}",
    "{{ [today_at('10:00'), today_at('09:00')] | max }} {{ [today_at('10:00'), today_at('09:00')] | min }}",
    "{{ now() in [utcnow()] }} {{ utcnow() is eq(now()) }}",
    "{{ as_datetime('2026-10-25 01:30') | as_local }} {{ (as_datetime('2026-10-25 01:30') | as_local).timestamp() }}",
    "{{ as_timestamp('2026-10-25 01:30') }} {{ as_timestamp('2026-03-29 01:30') }}",
    # glibc's `%P` and `-` flag aren't macOS's: not in the vectors.
    "{{ now().strftime('%G %V %g %u %C %y %e %k %l %r %h') }}",
    "{{ as_datetime('2026-01-01T00:00:00').strftime('%U %W %V %G %j %a') }}",
    "{{ as_datetime('2027-01-01').strftime('%V %G %U %W') }} {{ as_datetime('2024-12-30').strftime('%V %G') }}",
    "{{ timedelta(hours=-1).total_seconds() }} {{ timedelta(days=1) // 2 }} {{ timedelta(days=1) % timedelta(hours=5) }}",
    "{{ 2 * timedelta(hours=1) }} {{ timedelta(hours=1) * true }} {{ timedelta(hours=1) / 2.5 }}",
    "{{ as_datetime('2026-03-29T01:02:03+00:00') == as_datetime('2026-03-29T02:02:03+01:00') }}",
    "{{ as_datetime('2026-03-29') == as_datetime('2026-03-29').date() }}",
    "{{ now().date() < now() }}",
    "{{ now() < now().date() }}",
    "{{ strptime('29 March 2026 10:15:30.25 pm', '%d %B %Y %I:%M:%S.%f %p') }}",
    "{{ strptime('2026-03-29T10:15:00-05:30', '%Y-%m-%dT%H:%M:%S%z') }}",
    "{{ [strptime('2026-03-29T10:15:00+0100', '%Y-%m-%dT%H:%M:%S%z')] }} {{ strptime('2026-03-29T10:15:00+0100', '%Y-%m-%dT%H:%M:%S%z').tzinfo }}",
    "{{ strptime('12/31 23:59', '%m/%d %H:%M') }} {{ strptime('2026', '%Y') }}",
    "{{ states.sensor | selectattr('attributes.unit_of_measurement', 'defined') | map(attribute='entity_id') | list }}",
    "{{ states | selectattr('state', 'in', ['on', 'off']) | map(attribute='entity_id') | list }}",
    "{{ states.light.kitchen.attributes | length }} {{ states.light.kitchen.attributes.supported_color_modes | first }}",
    "{{ states.light | map(attribute='last_changed') | max }}",
    "{{ states.sensor.temperature.state | float | round(1) }} {{ states('sensor.power') | int(0) }}",
    # PEP 495: between zones, a time whose offset depends on `fold` equals
    # nothing; ordering is unaffected.
    "{{ as_local(as_datetime('2026-10-25T01:30:00+00:00')) == as_datetime('2026-10-25T01:30:00+00:00') }}",
    "{{ as_local(as_datetime('2026-10-25T01:30:00+00:00')) != as_datetime('2026-10-25T01:30:00+00:00') }}",
    "{{ as_datetime('2026-10-25T01:30:00+00:00') == as_local(as_datetime('2026-10-25T01:30:00+00:00')) }}",
    "{{ as_local(as_datetime('2026-10-25T00:30:00+00:00')) == as_datetime('2026-10-25T00:30:00+00:00') }}",
    "{{ as_local(as_datetime('2026-10-25T00:30:00+00:00')) != as_datetime('2026-10-25T00:30:00+00:00') }}",
    "{{ as_local(as_datetime('2026-10-25T01:30:00Z')) <= as_datetime('2026-10-25T01:30:00Z') }} {{ as_local(as_datetime('2026-10-25T01:30:00Z')) < as_datetime('2026-10-25T01:30:01Z') }}",
    "{{ as_local(as_datetime('2026-10-25T02:30:00Z')) == as_datetime('2026-10-25T02:30:00Z') }}",
    "{{ today_at('01:30') == as_datetime('2026-03-29T01:30:00Z') }} {{ today_at('01:30') != as_datetime('2026-03-29T01:30:00Z') }}",
    # A state compared: `TemplateStateBase.__eq__` collects it.
    "{{ states.light.kitchen == none }}",
    "{{ states.light.kitchen != none }}",
    "{{ none == states.light.kitchen }}",
    "{{ states.light.kitchen == states.light.kitchen }}",
    "{{ states.light.kitchen == states.light.porch }}",
    "{{ states.light.kitchen == (states.light | first) }}",
    "{{ states.light.kitchen in [states.light.kitchen] }}",
    # `%j` past the end of the year.
    "{{ strptime('2025 366', '%Y %j').isoformat() }} {{ strptime('2024 366', '%Y %j') }}",
    # Multipliers too small to matter, and too large.
    "{{ timedelta(seconds=1) * 1e-100 }} {{ timedelta(seconds=-1) * 1e-100 }} {{ timedelta(seconds=1) * -1e-100 }} {{ timedelta(seconds=1) * 5e-324 }}",
    "{{ timedelta(seconds=1) / 1e300 }} {{ timedelta(seconds=-1) / 1e300 }} {{ timedelta(seconds=1) / -1e300 }}",
    "{{ timedelta(microseconds=3) * 0.5 }} {{ timedelta(microseconds=1) * 0.5 }} {{ timedelta(days=1) * 1e-11 }} {{ timedelta(seconds=1) * 1e9 }}",
    "{{ timedelta(seconds=1) * 1e300 }}",
    "{{ timedelta(seconds=1) / 1e-300 }}",
    "{{ timedelta(seconds=1) * 1e15 }}",
    "{{ timedelta(days=999999999) * 2.0 }}",
    # `==` inside lists and dicts, and `in`: each item's own `__eq__`.
    "{{ [as_datetime('2026-01-01T00:00:00')] == [as_datetime('2026-01-01T00:00:00Z')] }}",
    "{% set d = as_local(as_datetime('2026-10-25T01:30:00Z')) %}{{ d == as_datetime('2026-10-25T01:30:00Z') }} {{ [d] == [as_datetime('2026-10-25T01:30:00Z')] }} {{ [d] == [d] }}",
    "{% set d = as_local(as_datetime('2026-10-25T01:30:00Z')) %}{{ {'a': {'b': d}} == {'a': {'b': as_datetime('2026-10-25T01:30:00Z')}} }} {{ {'a': [now()]} == {'a': [utcnow()]} }}",
    "{% set d = as_local(as_datetime('2026-10-25T01:30:00Z')) %}{{ as_datetime('2026-10-25T01:30:00Z') in [d] }} {{ d in [d] }} {{ utcnow() in [now()] }} {{ as_datetime('2026-01-01T00:00:00') in [as_datetime('2026-01-01T00:00:00Z')] }}",
    "{{ [timedelta(hours=1)] == [timedelta(minutes=60)] }} {{ timedelta(0) in [0, timedelta(0)] }} {{ [now().date()] == [now()] }}",
    "{{ [states.light.kitchen] == [states.light.kitchen] }}",
    "{{ [states.light.kitchen] == [states.light.porch] }}",
    # `statistics.median`: `(a + b) / 2` with the values' own operators.
    "{{ median([timedelta(seconds=1), timedelta(seconds=3)], default='bad') }} {{ median([timedelta(seconds=3), timedelta(seconds=1), timedelta(seconds=2)]) }}",
    "{{ median([today_at('10:00'), today_at('09:00')], default='bad') }} {{ median([today_at('10:00'), today_at('09:00'), today_at('11:00')]) }}",
    "{{ median([today_at('10:00'), today_at('09:00')]) }}",
    "{{ median([timedelta(days=999999999), timedelta(days=999999999)]) }}",
    # `unique`: a Python `set` (hash, then `==`).
    "{{ [as_datetime('2024-01-01T00:00:00'), as_datetime('2024-01-01T00:00:00+00:00')] | unique | list | count }}",
    "{{ [timedelta(hours=1), timedelta(minutes=60), now(), utcnow(), now().date()] | unique | list | count }}",
    "{{ [1, 1.0, true, 'A', 'a', none, none, 0, false] | unique | list }} {{ ['A', 'a'] | unique(case_sensitive=true) | list }}",
    "{{ [(1, 2), (1, 2), (2, 1)] | unique | list }}",
    "{{ [(1, 2), (1, [2])] | unique | list }}",
    "{{ [[1], [1]] | unique | list }}",
    "{{ [{'a': 1}] | unique | list }}",
    "{{ ([states.light.kitchen, states.light.porch] * 3) | unique | list | count }}",
    "{{ [states.light.kitchen.attributes] | unique | list }}",
    "{{ states.light | unique(attribute='state') | map(attribute='entity_id') | list }}",
    "{{ states.light.kitchen.attributes | unique | list }}",
    "{{ states | unique(attribute='domain') | map(attribute='entity_id') | list }}",
]

# Conversations against the home: messages sent on one connection, in order.
def rt(n, template, **kw):
    return {"id": n, "type": "render_template", "template": template, **kw}


CONVERSATIONS = [
    ("one entity", [rt(1, "{{ states('light.kitchen') }}")]),
    ("a domain", [rt(1, "{{ states.light | count }}")]),
    ("all states", [rt(1, "{{ states | count }}")]),
    ("the time", [rt(1, "{{ now().hour }}")]),
    ("several", [rt(1, "{{ is_state('light.kitchen', 'on') and has_value('light.missing') }} {{ states.sensor | list | count }} {{ now() > today_at('00:00') }}")]),
    ("error", [rt(1, "{{ states.Light }}", report_errors=True)]),
]


def check_versions():
    for pkg, want in PINNED.items():
        have = importlib.metadata.version(pkg)
        if have != want:
            sys.exit(f"{pkg} is {have}; these vectors are made with {want}")


def write(folder, name, data):
    folder.mkdir(parents=True, exist_ok=True)
    (folder / name).write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")


def as_sent(value):
    """`value` as HA's WebSocket would send it (see render_template.py)."""
    from homeassistant.helpers.json import json_bytes

    try:
        sent = json.loads(json_bytes(value))
    except (TypeError, ValueError):
        return {"unserializable": True}
    if isinstance(sent, str) and re.search(r"<[^<>]* at 0x[0-9a-f]+>", sent):
        return {"repr_with_address": True}
    return {"json": sent}


def stop_the_clock(now):
    from homeassistant.util import dt as dt_util

    dt_util.utcnow = lambda: now
    dt_util.now = lambda time_zone=None: now.astimezone(time_zone or dt_util.get_default_time_zone())


async def home():
    """A `HomeAssistant` holding the fixture's states, in its time zone."""
    from homeassistant.core import HomeAssistant
    from homeassistant.helpers import device_registry as dr, entity_registry as er
    from homeassistant.util import dt as dt_util

    os.environ["TZ"] = HOME["time_zone"]
    time.tzset()
    hass = HomeAssistant(tempfile.mkdtemp())
    await hass.config.async_set_time_zone(HOME["time_zone"])
    dr.async_setup(hass)
    await dr.async_load(hass)
    await er.async_load(hass)
    registry = er.async_get(hass)
    for entity_id, precision in HOME["display_precision"].items():
        domain, object_id = entity_id.split(".")
        entry = registry.async_get_or_create(domain, "vectors", object_id, suggested_object_id=object_id)
        assert entry.entity_id == entity_id
        registry.async_update_entity_options(entity_id, domain, {"display_precision": precision})
    for s in HOME["states"]:
        changed = dt_util.parse_datetime(s["last_changed"]).timestamp()
        hass.states.async_set(s["entity_id"], s["state"], s["attributes"], timestamp=changed)
        if "last_updated" in s:
            # An attribute-only change: `last_changed` stays.
            updated = dt_util.parse_datetime(s["last_updated"]).timestamp()
            attributes = {**s["attributes"], "_": 1}
            hass.states.async_set(s["entity_id"], s["state"], attributes, timestamp=updated)
            hass.states.async_set(s["entity_id"], s["state"], s["attributes"], timestamp=updated)
    stop_the_clock(dt_util.parse_datetime(HOME["now"]))
    return hass


def listeners(info):
    from homeassistant.helpers.event import _render_infos_to_track_states

    track = _render_infos_to_track_states([info])
    return {
        "all": track.all_states,
        "entities": sorted(track.entities),
        "domains": sorted(track.domains),
        "time": info.has_time,
    }


async def function_vectors():
    from homeassistant.exceptions import TemplateError
    from homeassistant.helpers import template

    out = []
    for source in TEMPLATES:
        for strict in (False, True):
            # A fresh home each time: HA caches template states (and their
            # cached properties) and folded constants, so a second render
            # of the same thing reads less and collects less than the first.
            hass = await home()
            log = []
            tpl = template.Template(source, hass)
            info = tpl.async_render_to_info(
                None, strict=strict, log_fn=lambda level, msg: log.append([logging.getLevelName(level), msg])
            )
            case = {"template": source, "strict": strict}
            try:
                case["result"] = as_sent(info.result())
            except TemplateError as err:
                case["error"] = str(err)
            case["log"] = log
            case["listeners"] = listeners(info)
            out.append(case)
            await hass.async_stop(force=True)
    return out


async def conversation(msgs):
    from homeassistant.auth.models import User
    from homeassistant.components.websocket_api import async_register_command
    from homeassistant.components.websocket_api import commands, connection
    from homeassistant.components.websocket_api import const as ws_const
    from homeassistant.components.websocket_api.messages import message_to_json_bytes

    hass = await home()
    hass.data[ws_const.DOMAIN] = {}
    async_register_command(hass, commands.handle_render_template)
    sent = []

    def send(message):
        if isinstance(message, dict):
            message = message_to_json_bytes(message)
        message = json.loads(message)
        # Sets serialise in hash order: sorted, as hars sends them.
        listeners = message.get("event", {}).get("listeners")
        if listeners:
            listeners["entities"] = sorted(listeners["entities"])
            listeners["domains"] = sorted(listeners["domains"])
        sent[-1].append(message)

    user = User(name="Ann", is_owner=False, is_active=True, system_generated=False, groups=[], perm_lookup=None)
    conn = connection.ActiveConnection(logging.getLogger("vectors"), hass, send, user, None, None)
    for msg in msgs:
        sent.append([])
        conn.async_handle(msg)
        await hass.async_block_till_done(wait_background_tasks=True)
        for _ in range(5):
            await asyncio.sleep(0)
        await hass.async_block_till_done(wait_background_tasks=True)
    conn.async_handle_close()
    await hass.async_stop(force=True)
    return [{"send": m, "replies": r} for m, r in zip(msgs, sent)]


async def conversations():
    return [{"name": name, "steps": await conversation(msgs)} for name, msgs in CONVERSATIONS]


def main():
    check_versions()
    logging.disable(logging.CRITICAL)
    write(CORE, "functions.json", asyncio.run(function_vectors()))
    write(API, "render_template_states.json", asyncio.run(conversations()))


if __name__ == "__main__":
    main()
