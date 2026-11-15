#!/usr/bin/env python3
"""Vectors for the companion app's push notifications (`mobile_app`'s notify).

    python3.14 -m venv .venv
    .venv/bin/pip install homeassistant==2026.6.1 PyNaCl==1.6.2 numpy==2.3.2 av==16.0.1 \\
        PyTurboJPEG==1.8.3 Pillow==12.2.0 hass-nabucasa==2.2.0 hassil==3.5.0 \\
        home-assistant-intents==2026.6.1 ha-ffmpeg==3.2.2 mutagen==1.47.0 \\
        pymicro-vad==1.0.1 pyspeex-noise==1.0.2 aiohasupervisor==0.4.3 \\
        securetar==2026.4.1 cronsim==2.7
    .venv/bin/python -I scripts/vectors/mobile_app_notify.py
    cargo test -p ha-api ws::mobile_app

(Importing `mobile_app` imports `cloud`, and through it `camera` and
`stream`: their requirements are installed for the imports only.)
Checked: HA 2026.6.1, voluptuous 0.15.2, aiohttp 3.13.5.

Boots Home Assistant 2026.6.1's core in a temporary config directory with
`http`, `websocket_api` and `mobile_app` (and what it depends on), adds the
fake registrations below as config entries, and runs each scenario: service
calls (`notify.mobile_app_<device>`, `notify.notify`, `notify.send_message`),
the app's WebSocket commands (`mobile_app/push_notification_channel`,
`_confirm`, `unsubscribe_events`, a closed connection) and the passing of
time. It writes crates/ha-api/src/mobile_app/vectors/notify.json: for each
step, what HA sent, in order (WebSocket messages and requests to the push
relay, with their raw bodies), the call's result or error, errors raised by
the confirm timer, the notify entities' states and the integration's log; and HA's rate-limit log line (`log_rate_limits`)
for a range of relay answers.

What is faked, as only things outside HA may be:

- the push relay (mobile-apps.home-assistant.io) is a local aiohttp server
  answering with each scenario's canned replies; its address is written as
  `<relay>`, and a closed port as `<closed>`;
- the clock: `async_call_later` in `push_notification.py` is recorded (its
  delay is checked to be 10 s) and fired when a step advances time; the
  10 s `asyncio.timeout` in `notify.py` is checked and shortened so a relay
  that never answers times out at once; `utcnow` in `notify.py` is fixed;
- randomness: confirm ids are `000…1`, `000…2`, … in order;
- the WebSocket transport: messages go through `ActiveConnection.async_handle`
  and come back through its `send_message`, as `render_template.py` does.

Times in states are written as `<time>`. Run it twice: the output must not
change.
"""

import asyncio
import contextlib
import importlib.metadata
import json
import logging
import pathlib
import re
import socket
import sys
import tempfile
import types
from datetime import UTC, datetime
from unittest.mock import patch

PINNED = {"homeassistant": "2026.6.1", "voluptuous": "0.15.2", "aiohttp": "3.13.5"}
OUT = pathlib.Path(__file__).resolve().parents[2] / "crates/ha-api/src/mobile_app/vectors"
NOW = datetime(2026, 6, 1, 12, 0, 0, tzinfo=UTC)
RATE_LIMITS = {
    "successful": 3,
    "errors": 1,
    "maximum": 150,
    "resetsAt": "2026-06-02T00:00:00.000Z",
}

PHONE_HOOK = "0f" * 32
TABLET_HOOK = "1e" * 32
PLAIN_HOOK = "2d" * 32
OLD_HOOK = "3c" * 32
OTHER_HOOK = "4b" * 32


def check_versions():
    for pkg, want in PINNED.items():
        have = importlib.metadata.version(pkg)
        if have != want:
            sys.exit(f"{pkg} is {have}; these vectors are made with {want}")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def registration(name, hook, device, app_data, os_version="27.0", user="<user>"):
    """A config entry's `data`, as the app's registration leaves it. (HA's
    `async_setup_entry` needs `os_version`, so every registration has one.)"""
    data = {
        "app_data": app_data,
        "app_id": "io.robbie.HomeAssistant",
        "app_name": "Home Assistant",
        "app_version": "2026.9.0 (2026.2874)",
        "device_id": device,
        "device_name": name,
        "manufacturer": "Apple",
        "model": "iPhone17,1",
        "os_name": "iOS",
        "os_version": os_version,
        "supports_encryption": False,
        "user_id": user,
        "webhook_id": hook,
    }
    return data


CLOUD = {"push_token": "fake-push-token", "push_url": "<relay>/api/sendPushNotification"}
PHONE = registration("Phone", PHONE_HOOK, "11111111-2222-4333-8444-555555555555", CLOUD)
TABLET = registration(
    "Tablet", TABLET_HOOK, "11111111-2222-4333-8444-666666666666", {"push_websocket_channel": True}
)
PLAIN = registration("Plain", PLAIN_HOOK, "11111111-2222-4333-8444-777777777777", {})
OLD = registration(
    "Old Phone",
    OLD_HOOK,
    "11111111-2222-4333-8444-888888888888",
    {"push_token": "old-push-token", "push_url": "<relay>/api/sendPushNotification"},
    os_version="17.0",
)
REFUSED = registration(
    "Phone",
    PHONE_HOOK,
    "11111111-2222-4333-8444-555555555555",
    {"push_token": "fake-push-token", "push_url": "<closed>/api/sendPushNotification"},
)
OTHER = registration(
    "Other", OTHER_HOOK, "11111111-2222-4333-8444-999999999999", CLOUD, user="<other>"
)


def reply(status, body, content_type="application/json"):
    """A canned relay reply: `body` is JSON-encoded unless it is a str."""
    text = body if isinstance(body, str) else json.dumps(body)
    return {"status": status, "content_type": content_type, "body": text}


HANG = {"hang": True}


def call(service, data, domain="notify"):
    return {"call": [domain, service, data]}


def ws(conn, msg):
    return {"ws": [conn, msg]}


def channel(conn, id_, hook, confirm=None):
    msg = {"id": id_, "type": "mobile_app/push_notification_channel", "webhook_id": hook}
    if confirm is not None:
        msg["support_confirm"] = confirm
    return ws(conn, msg)


def confirm(conn, id_, hook, confirm_id):
    return ws(
        conn,
        {
            "id": id_,
            "type": "mobile_app/push_notification_confirm",
            "webhook_id": hook,
            "confirm_id": confirm_id,
        },
    )


def cid(n):
    return f"{n:032x}"


ADVANCE = {"advance": 10}
SERVICES = {"services": True}

# Every relay error the service turns into a `HomeAssistantError` (or not).
ERROR_REPLIES = [
    reply(400, {"message": "Invalid push token"}),
    reply(403, {"message": "Forbidden."}),
    reply(429, {"message": "Rate limited", "rateLimits": RATE_LIMITS}),
    reply(500, {"errorMessage": "Lambda failed"}),
    reply(503, {}),
    reply(404, {"message": "Not found!", "errorMessage": "ignored"}),
    reply(502, "<html>Bad gateway</html>", "text/html"),
    reply(200, "ok", "text/plain"),
    reply(200, "nope"),
    reply(200, ""),
    reply(200, "  "),
    reply(200, {"rateLimits": {"successful": 1}}),
    reply(200, {"rateLimits": {**RATE_LIMITS, "resetsAt": "soon"}}),
    reply(200, []),
    reply(200, "null"),
    reply(200, "7"),
    reply(400, []),
    reply(400, {"message": ""}),
    reply(200, {"message": "ignored on success"}, "application/json; charset=utf-8"),
    reply(201, {}, "application/problem+json"),
]

SCENARIOS = [
    (
        "cloud",
        "Cloud push through the relay: bodies, titles, the default title, coercion.",
        [PHONE, TABLET, PLAIN, OLD],
        [
            reply(200, {"rateLimits": RATE_LIMITS}),
            reply(201, {}),
            reply(202, {"rateLimits": RATE_LIMITS}),
            reply(200, {}),
            reply(200, {}),
            reply(200, {"rateLimits": RATE_LIMITS}),
        ],
        [
            SERVICES,
            call(
                "mobile_app_phone",
                {
                    "message": "Hello ✓",
                    "title": "Hi",
                    # Keys in sorted order: hars's JSON objects are sorted,
                    # HA's keep their order, and that difference is
                    # documented, not tested.
                    "data": {"push": {"badge": 1, "sound": "default"}, "url": "/lovelace/0"},
                },
            ),
            call("mobile_app_old_phone", {"message": 42, "title": "Home Assistant"}),
            call("mobile_app_phone", {"message": "x", "title": "", "target": ["ignored"]}),
            call("send_message", {"entity_id": "notify.phone", "message": "Entity", "title": "Home Assistant"}),
            call("send_message", {"entity_id": "notify.phone", "message": "Empty title", "title": ""}),
            call("send_message", {"entity_id": ["notify.phone", "notify.missing"], "message": 1.5}),
            call("send_message", {"entity_id": "notify.missing", "message": "Nobody"}),
        ],
    ),
    (
        "cloud_errors_legacy",
        "Each relay answer through `notify.mobile_app_phone`: errors are logged, not raised.",
        [PHONE],
        ERROR_REPLIES,
        [call("mobile_app_phone", {"message": f"m{i}"}) for i in range(len(ERROR_REPLIES))],
    ),
    (
        "cloud_errors_entity",
        "Each relay answer through `notify.send_message`: errors are raised.",
        [PHONE],
        ERROR_REPLIES,
        [
            call("send_message", {"entity_id": "notify.phone", "message": f"m{i}"})
            for i in range(len(ERROR_REPLIES))
        ],
    ),
    (
        "cloud_timeout",
        "A relay that never answers: HA's 10 s timeout.",
        [PHONE],
        [HANG, HANG],
        [
            call("mobile_app_phone", {"message": "late"}),
            call("send_message", {"entity_id": "notify.phone", "message": "late"}),
        ],
    ),
    (
        "cloud_refused",
        "A relay that refuses the connection.",
        [REFUSED],
        [],
        [
            call("mobile_app_phone", {"message": "x"}),
            call("send_message", {"entity_id": "notify.phone", "message": "x"}),
        ],
    ),
    (
        "notify_service",
        "`notify.notify`: targets, all registrations, local-only and push-less ones, schemas.",
        [PHONE, TABLET, PLAIN, OLD],
        [reply(200, {})] * 8,
        [
            call("notify", {"message": "All"}),
            call("notify", {"message": "One", "target": PHONE_HOOK, "data": {"push": {"badge": 1}}}),
            call("notify", {"message": "Plain", "target": [PLAIN_HOOK]}),
            call("notify", {"message": "x", "target": ["nope"]}),
            call("notify", {"message": "Empty", "target": []}),
            call("notify", {"message": "Two", "target": [OLD_HOOK, PHONE_HOOK], "title": "T"}),
            call("mobile_app_tablet", {"message": "x"}),
            call("send_message", {"entity_id": "notify.tablet", "message": "x"}),
            call("mobile_app_phone", {}),
            call("mobile_app_phone", {"message": None}),
            call("mobile_app_phone", {"message": ["a"]}),
            call("mobile_app_phone", {"message": "x", "data": [1]}),
            call("mobile_app_phone", {"message": "x", "data": None}),
            call("mobile_app_phone", {"message": "x", "title": None}),
            call("mobile_app_phone", {"message": "x", "extra": 1}),
            call("mobile_app_phone", {"message": "x", "target": 5}),
            call("send_message", {"entity_id": "notify.phone"}),
            call("send_message", {"entity_id": "notify.phone", "message": "x", "data": {}}),
            call("send_message", {"message": "x"}),
        ],
    ),
    (
        "local",
        "The local push channel without confirmation.",
        [PHONE, TABLET],
        [reply(200, {})],
        [
            channel(0, 1, TABLET_HOOK),
            call("mobile_app_tablet", {"message": "Local", "title": "T", "data": {"tag": "x"}}),
            call("send_message", {"entity_id": "notify.tablet", "message": "Entity", "title": "E"}),
            call("notify", {"message": "Both"}),
            confirm(0, 2, TABLET_HOOK, "abc"),
            ws(0, {"id": 3, "type": "unsubscribe_events", "subscription": 1}),
            call("mobile_app_tablet", {"message": "Gone"}),
            call("send_message", {"entity_id": "notify.tablet", "message": "Gone"}),
        ],
    ),
    (
        "confirm",
        "The local push channel with confirmation: confirmed in time.",
        [PHONE],
        [],
        [
            channel(0, 1, PHONE_HOOK, True),
            call("mobile_app_phone", {"message": "A"}),
            confirm(0, 2, PHONE_HOOK, cid(1)),
            confirm(0, 3, PHONE_HOOK, cid(1)),
            ADVANCE,
            call("send_message", {"entity_id": "notify.phone", "message": "B"}),
            confirm(0, 4, PHONE_HOOK, cid(2)),
            ADVANCE,
        ],
    ),
    (
        "confirm_timeout",
        "Unconfirmed for 10 s: the channel is dropped and everything pending goes by cloud.",
        [PHONE],
        [reply(200, {})] * 3,
        [
            channel(0, 1, PHONE_HOOK, True),
            call("mobile_app_phone", {"message": "A"}),
            call("send_message", {"entity_id": "notify.phone", "message": "B"}),
            ADVANCE,
            call("mobile_app_phone", {"message": "C"}),
            confirm(0, 2, PHONE_HOOK, cid(1)),
            ws(0, {"id": 3, "type": "unsubscribe_events", "subscription": 1}),
        ],
    ),
    (
        "confirm_timeout_local_only",
        "Unconfirmed on a registration with no cloud push to fall back to.",
        [TABLET],
        [],
        [
            channel(0, 1, TABLET_HOOK, True),
            call("mobile_app_tablet", {"message": "A"}),
            ADVANCE,
            call("mobile_app_tablet", {"message": "B"}),
        ],
    ),
    (
        "replaced",
        "A second channel for the same app replaces the first; what was pending goes by cloud.",
        [PHONE],
        [reply(200, {})],
        [
            channel(0, 1, PHONE_HOOK, True),
            call("mobile_app_phone", {"message": "A"}),
            channel(1, 1, PHONE_HOOK),
            call("mobile_app_phone", {"message": "B"}),
            ADVANCE,
            confirm(0, 2, PHONE_HOOK, cid(1)),
        ],
    ),
    (
        "replaced_error",
        "Replacing a channel whose pending `send_message` fails by cloud fails the new channel.",
        [PHONE],
        [reply(500, {}), reply(200, {})],
        [
            channel(0, 1, PHONE_HOOK, True),
            call("send_message", {"entity_id": "notify.phone", "message": "A"}),
            channel(1, 1, PHONE_HOOK),
            call("mobile_app_phone", {"message": "B"}),
        ],
    ),
    (
        "closed",
        "The app's connection closes: what was pending goes by cloud.",
        [PHONE],
        [reply(200, {})] * 2,
        [
            channel(0, 1, PHONE_HOOK, True),
            call("send_message", {"entity_id": "notify.phone", "message": "A"}),
            {"close": 0},
            call("mobile_app_phone", {"message": "B"}),
            ADVANCE,
        ],
    ),
    (
        "shared_data",
        "`notify.notify` shares one message between its targets: a confirm id rides along.",
        [PHONE, TABLET],
        [reply(200, {})],
        [
            channel(0, 1, TABLET_HOOK, True),
            call("notify", {"message": "Both", "target": [TABLET_HOOK, PHONE_HOOK]}),
            ADVANCE,
        ],
    ),
    (
        "ws_errors",
        "The commands' checks: unknown and foreign webhooks, no channel, schemas.",
        [PHONE, OTHER],
        [],
        [
            channel(0, 1, "nope"),
            confirm(0, 2, "nope", "x"),
            channel(0, 3, OTHER_HOOK),
            confirm(0, 4, OTHER_HOOK, "x"),
            confirm(0, 5, PHONE_HOOK, "x"),
            ws(0, {"id": 6, "type": "mobile_app/push_notification_channel"}),
            ws(0, {"id": 7, "type": "mobile_app/push_notification_confirm", "webhook_id": PHONE_HOOK}),
            channel(0, 8, PHONE_HOOK, "yes"),
            channel(0, 9, PHONE_HOOK, False),
            confirm(0, 10, PHONE_HOOK, "x"),
        ],
    ),
]


class Relay:
    """The local stand-in for the push relay."""

    def __init__(self, out):
        self.out = out
        self.replies = []
        self.runner = None
        self.url = None

    async def start(self):
        from aiohttp import web

        async def handle(request):
            body = await request.text()
            self.out.append(
                {
                    "relay": {
                        "method": request.method,
                        "path": request.path,
                        "content_type": request.headers.get("Content-Type"),
                        "body": body,
                    }
                }
            )
            canned = self.replies.pop(0)
            if canned.get("hang"):
                await asyncio.sleep(3600)
            return web.Response(
                status=canned["status"],
                body=canned["body"].encode(),
                headers={"Content-Type": canned["content_type"]},
            )

        app = web.Application()
        app.router.add_post("/api/sendPushNotification", handle)
        self.runner = web.AppRunner(app)
        await self.runner.setup()
        port = free_port()
        await web.TCPSite(self.runner, "127.0.0.1", port).start()
        self.url = f"http://127.0.0.1:{port}"

    async def stop(self):
        await self.runner.cleanup()


class Logs(logging.Handler):
    def __init__(self):
        super().__init__(logging.DEBUG)
        self.records = []

    def emit(self, record):
        if record.exc_info:
            record.exc_info = None
        self.records.append([record.levelname, record.getMessage()])


def as_error(err):
    out = {"type": type(err).__name__, "message": str(err)}
    for key in ("translation_domain", "translation_key", "translation_placeholders"):
        if getattr(err, key, None) is not None:
            out[key] = getattr(err, key)
    return out


TIME = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?\+00:00$")


def scrub_state(state):
    if state is None:
        return None
    return "<time>" if TIME.match(state.state) else state.state


def resolve(value, relay, closed):
    """The fake registration with the relay's real address in it."""
    if isinstance(value, dict):
        return {k: resolve(v, relay, closed) for k, v in value.items()}
    if isinstance(value, str):
        return value.replace("<relay>", relay).replace("<closed>", closed)
    return value


def unresolve(value, relay, closed, users):
    if isinstance(value, dict):
        return {k: unresolve(v, relay, closed, users) for k, v in value.items()}
    if isinstance(value, list):
        return [unresolve(v, relay, closed, users) for v in value]
    if isinstance(value, str):
        value = value.replace(relay, "<relay>").replace(closed, "<closed>")
        for name, uid in users.items():
            value = value.replace(uid, name)
    return value


async def run(name, regs, replies, steps):
    from homeassistant import bootstrap, core, loader
    from homeassistant.auth import auth_manager_from_config
    from homeassistant.components.mobile_app import notify as ma_notify
    from homeassistant.components.mobile_app import push_notification
    from homeassistant.components.websocket_api import connection
    from homeassistant.components.websocket_api.messages import message_to_json_bytes
    from homeassistant.config_entries import ConfigEntries, ConfigEntry
    from homeassistant.setup import async_setup_component
    from homeassistant.util import dt as dt_util

    out = []
    relay = Relay(out)
    await relay.start()
    relay.replies = list(replies)
    closed = f"http://127.0.0.1:{free_port()}"

    timers = []
    ids = iter(range(1, 1000))

    def call_later(hass, delay, action):
        assert delay == 10, delay
        timer = {"action": action, "cancelled": False}
        timers.append(timer)

        def cancel():
            timer["cancelled"] = True

        return cancel

    real_timeout = asyncio.timeout

    def short_timeout(delay):
        assert delay == 10, delay
        return real_timeout(0.2)

    fake_asyncio = types.SimpleNamespace(timeout=short_timeout)
    fake_dt = types.SimpleNamespace(utcnow=lambda: NOW, parse_datetime=dt_util.parse_datetime)

    logs = Logs()
    logger = logging.getLogger("homeassistant.components.mobile_app")
    logger.setLevel(logging.DEBUG)
    logger.propagate = False
    logger.addHandler(logs)

    with tempfile.TemporaryDirectory() as config_dir, contextlib.ExitStack() as stack:
        stack.enter_context(patch.object(push_notification, "async_call_later", call_later))
        stack.enter_context(
            patch.object(push_notification, "random_uuid_hex", lambda: cid(next(ids)))
        )
        stack.enter_context(patch.object(ma_notify, "asyncio", fake_asyncio))
        stack.enter_context(patch.object(ma_notify, "dt_util", fake_dt))

        hass = core.HomeAssistant(config_dir)
        hass.config.skip_pip = True
        await hass.config.async_load()
        loader.async_setup(hass)
        hass.config_entries = ConfigEntries(hass, {})
        await hass.config_entries.async_initialize()
        await bootstrap.async_load_base_functionality(hass)
        hass.auth = await auth_manager_from_config(hass, [{"type": "homeassistant"}], [])
        user = await hass.auth.async_create_user("Ann")
        other = await hass.auth.async_create_user("Bob")
        users = {"<user>": user.id, "<other>": other.id}
        http = {"server_port": free_port(), "server_host": "127.0.0.1"}
        assert await async_setup_component(hass, "http", {"http": http})
        assert await async_setup_component(hass, "websocket_api", {})
        assert await async_setup_component(hass, "mobile_app", {})
        for i, reg in enumerate(regs):
            data = resolve(reg, relay.url, closed)
            data["user_id"] = users[data["user_id"]]
            entry = ConfigEntry(
                data=data,
                discovery_keys={},
                domain="mobile_app",
                entry_id=f"entry{i}",
                minor_version=1,
                options={},
                source="registration",
                subentries_data=None,
                title=data["device_name"],
                unique_id=f"{data['app_id']}-{data['device_id']}",
                version=1,
            )
            await hass.config_entries.async_add(entry)
        await hass.async_start()
        await hass.async_block_till_done()
        logs.records.clear()

        conns = {}

        def conn(n):
            if n not in conns:

                def send(message, n=n):
                    if isinstance(message, dict):
                        message = message_to_json_bytes(message)
                    out.append({"ws": [n, json.loads(message)]})

                conns[n] = connection.ActiveConnection(
                    logging.getLogger("vectors"), hass, send, user, None, None
                )
            return conns[n]

        entities = sorted(
            {f"notify.{slug}" for slug in ("phone", "tablet", "plain", "old_phone", "other")}
        )
        recorded = []
        for step in steps:
            out.clear()
            logs.records.clear()
            result = {}
            if "call" in step:
                domain, service, data = step["call"]
                try:
                    await hass.services.async_call(domain, service, data, blocking=True)
                    result["result"] = "ok"
                except Exception as err:  # noqa: BLE001
                    result["result"] = {"error": as_error(err)}
            elif "ws" in step:
                n, msg = step["ws"]
                conn(n).async_handle(msg)
            elif "close" in step:
                conn(step["close"]).async_handle_close()
            elif "advance" in step:
                errors = []
                for timer in list(timers):
                    timers.remove(timer)
                    if timer["cancelled"]:
                        continue
                    try:
                        await timer["action"](dt_util.utcnow())
                    except Exception as err:  # noqa: BLE001
                        errors.append(as_error(err))
                result["timer_errors"] = errors
            elif "services" in step:
                result["services"] = sorted(hass.services.async_services_for_domain("notify"))
            await hass.async_block_till_done(wait_background_tasks=True)
            for _ in range(5):
                await asyncio.sleep(0)
            await hass.async_block_till_done(wait_background_tasks=True)
            states = {e: scrub_state(hass.states.get(e)) for e in entities}
            recorded.append(
                unresolve(
                    {
                        "step": step,
                        **result,
                        "sent": list(out),
                        "states": {k: v for k, v in states.items() if v is not None},
                        "log": list(logs.records),
                    },
                    relay.url,
                    closed,
                    users,
                )
            )
        for c in conns.values():
            c.async_handle_close()
        await hass.async_block_till_done(wait_background_tasks=True)
        await hass.async_stop(force=True)
    logger.removeHandler(logs)
    await relay.stop()
    return recorded


# `resetsAt` (and the counts) for `log_rate_limits`, at `NOW`.
RATE_LIMIT_LINES = [
    RATE_LIMITS,
    {**RATE_LIMITS, "resetsAt": "2026-06-01T12:00:05.5Z"},
    {**RATE_LIMITS, "resetsAt": "2026-06-03T13:01:02Z"},
    {**RATE_LIMITS, "resetsAt": "2026-06-02T12:00:00Z"},
    {**RATE_LIMITS, "resetsAt": "2026-06-01T11:59:50Z"},
    {**RATE_LIMITS, "resetsAt": "2026-05-30T12:00:00Z"},
    {**RATE_LIMITS, "resetsAt": "2026-06-02T12:00:00+01:00"},
    {**RATE_LIMITS, "resetsAt": "2026-06-01 12:30:00+00:00"},
    {**RATE_LIMITS, "resetsAt": "2026-06-01T12:30:00"},
    {**RATE_LIMITS, "resetsAt": "soon"},
    {**RATE_LIMITS, "resetsAt": 5},
    {"successful": 1, "maximum": 2, "errors": 0},
    {"resetsAt": "2026-06-02T00:00:00Z", "maximum": 2, "errors": 0},
    {"resetsAt": "2026-06-02T00:00:00Z", "successful": 1, "errors": 0},
    {"resetsAt": "2026-06-02T00:00:00Z", "successful": 1, "maximum": 2},
    {**RATE_LIMITS, "successful": 2.7, "maximum": 150.0},
    [],
    "rateLimits",
    7,
    None,
]


def rate_limit_lines():
    """HA's `log_rate_limits` on each value, with `utcnow` at `NOW`."""
    from homeassistant.components.mobile_app import notify as ma_notify
    from homeassistant.util import dt as dt_util

    fake_dt = types.SimpleNamespace(utcnow=lambda: NOW, parse_datetime=dt_util.parse_datetime)
    logs = Logs()
    logger = logging.getLogger("homeassistant.components.mobile_app.notify")
    logger.setLevel(logging.DEBUG)
    logger.propagate = False
    logger.addHandler(logs)
    out = []
    with patch.object(ma_notify, "dt_util", fake_dt):
        for value in RATE_LIMIT_LINES:
            logs.records.clear()
            case = {"rateLimits": value}
            try:
                ma_notify.log_rate_limits("Phone", {"rateLimits": value}, logging.DEBUG)
                case["line"] = logs.records[0][1]
            except Exception as err:  # noqa: BLE001
                case["error"] = as_error(err)
            out.append(case)
    logger.removeHandler(logs)
    logger.propagate = True
    return {"now": NOW.isoformat(), "cases": out}


async def scenarios():
    out = []
    for name, note, regs, replies, steps in SCENARIOS:
        out.append(
            {
                "name": name,
                "note": note,
                "registrations": regs,
                "relay": replies,
                "steps": await run(name, regs, replies, steps),
            }
        )
    return out


def main():
    check_versions()
    logging.basicConfig(level=logging.CRITICAL)
    data = {"rate_limits": rate_limit_lines(), "scenarios": asyncio.run(scenarios())}
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "notify.json").write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
