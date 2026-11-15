#!/usr/bin/env python3
"""Compare what `hars` would have done (shadow mode) with what Home Assistant did.

    scripts/shadow-diff.py --ha-db home-assistant_v2.db --hars-log shadow.jsonl

HA side: the `call_service` and `automation_triggered` rows of its recorder
database (opened read-only), or a JSON Lines file in the same shape as the
shadow log (`--ha-jsonl`). hars side: the shadow log (`HARS_SHADOW=1`, see
docs/SHADOW.md).

Calls are matched in time order: the same service, the same targets and the
same data, within --tolerance seconds (the roadmap's bar is 1 s). A call with
the same service and targets but different data is a *mismatch*; anything left
is *missing* (HA did it, hars didn't) or *extra* (hars would have, HA didn't).
Automation triggers match on entity id and time.

By default only calls made on behalf of an automation, in the domains hars holds
back, are compared: a call whose context is the context of an
`automation_triggered` (or a child of it). --all-contexts includes calls from
the UI and the apps too.

Exit status: 0 when nothing is missing, extra or mismatched and hars's log has
no gaps; 1 otherwise; 2 on a usage or input error (an unreadable log, a
reversed window): a result is never given from input it could not read.
"""

import argparse
import bisect
import collections
import datetime
import json
import sqlite3
import sys

# `ha_core::shadow::RUNS`: the domains whose calls hars runs, so never logs.
RUNS = {
    "automation",
    "homeassistant",
    "system_log",
    "persistent_notification",
    "frontend",
    "weather",
    "labs",
}


class InputError(Exception):
    pass


_CROCKFORD = {c: i for i, c in enumerate("0123456789ABCDEFGHJKMNPQRSTVWXYZ")}


def ulid_bytes(text):
    """A context id's 16 bytes, as HA's recorder stores it (`context_id_bin`)."""
    if not text:
        return None
    try:
        n = 0
        for c in text.upper():
            n = n * 32 + _CROCKFORD[c]
        return n.to_bytes(16, "big")
    except (KeyError, OverflowError):
        return None


def parse_time(text):
    """Epoch seconds from a number or an ISO 8601 time (naive means UTC)."""
    try:
        return float(text)
    except ValueError:
        t = datetime.datetime.fromisoformat(text.replace("Z", "+00:00"))
        if t.tzinfo is None:
            t = t.replace(tzinfo=datetime.timezone.utc)
        return t.timestamp()


def norm(value):
    """JSON with numbers compared as numbers (40 == 40.0) and sorted keys."""
    if isinstance(value, bool) or value is None or isinstance(value, str):
        return value
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, list):
        return [norm(v) for v in value]
    if isinstance(value, dict):
        return {k: norm(v) for k, v in sorted(value.items())}
    return value


def entity_ids(data):
    """`entity_id` as a sorted tuple, as HA reads a string, a list or `all`."""
    ids = data.get("entity_id")
    if ids is None:
        return ()
    if isinstance(ids, str):
        ids = [i.strip() for i in ids.split(",")]
    return tuple(sorted(i.lower() for i in ids))


class Call:
    def __init__(self, t, domain, service, data, ctx=None):
        """`ctx`: {"id", "parent_id", "user_id"} with ids as bytes (or None)."""
        self.t = t
        self.domain = domain
        self.service = service
        self.targets = entity_ids(data)
        rest = {k: v for k, v in data.items() if k != "entity_id"}
        self.data = json.dumps(norm(rest), sort_keys=True)
        self.ctx = ctx

    def key(self):
        return (self.domain, self.service, self.targets)

    def show(self):
        stamp = datetime.datetime.fromtimestamp(self.t, datetime.timezone.utc)
        stamp = stamp.strftime("%Y-%m-%d %H:%M:%S.%f")[:-3]
        targets = ",".join(self.targets) or "-"
        return f"{stamp}Z {self.domain}.{self.service} [{targets}] {self.data}"


class Trigger:
    def __init__(self, t, entity_id, name, ctx_id=None):
        self.t = t
        self.entity_id = entity_id
        self.name = name
        self.ctx_id = ctx_id

    def show(self):
        stamp = datetime.datetime.fromtimestamp(self.t, datetime.timezone.utc)
        return f"{stamp.strftime('%Y-%m-%d %H:%M:%S.%f')[:-3]}Z {self.entity_id} ({self.name})"


def context_of(row):
    ctx = row.get("context") or {}
    return {
        "id": ulid_bytes(ctx.get("id")),
        "parent_id": ulid_bytes(ctx.get("parent_id")),
        "user_id": ctx.get("user_id"),
    }


def read_jsonl(path):
    """(calls, triggers, starts, gaps) from a shadow-format JSON Lines file.

    A line that is not JSON is an error, except one cut off by a crash: the
    last line, or one the next start of hars marked off with `shadow_started`."""
    calls, triggers, starts, gaps = [], [], [], []
    bad = None
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
                kind = row.get("type")
            except (json.JSONDecodeError, AttributeError):
                if bad is not None:
                    raise InputError(f"{path}:{bad}: not JSON")
                bad = n
                continue
            if bad is not None and kind != "shadow_started":
                raise InputError(f"{path}:{bad}: not JSON")
            if bad is not None:
                print(f"{path}:{bad}: skipping a line cut off by a crash", file=sys.stderr)
                bad = None
            try:
                if kind == "call_service":
                    calls.append(
                        Call(row["t"], row["domain"], row["service"],
                             row.get("service_data") or {}, context_of(row))
                    )
                elif kind == "automation_triggered":
                    d = row.get("data") or {}
                    triggers.append(
                        Trigger(row["t"], d.get("entity_id"), d.get("name"), context_of(row)["id"])
                    )
                elif kind == "shadow_started":
                    starts.append(row["t"])
                elif kind == "shadow_gap":
                    gaps.append((row["t"], row["until"], row.get("dropped", 0)))
            except (KeyError, TypeError) as e:
                raise InputError(f"{path}:{n}: a {kind} line without {e}")
    if bad is not None:
        print(f"{path}:{bad}: skipping a last line cut off by a crash", file=sys.stderr)
    return calls, triggers, starts, gaps


def read_ha_db(path, since, until):
    """(calls, triggers) from HA's recorder database, read-only."""
    uri = "file:" + path.replace("?", "%3f").replace("#", "%23") + "?mode=ro"
    db = sqlite3.connect(uri, uri=True)
    cols = {r[1] for r in db.execute("PRAGMA table_info(events)")}
    for need in ("time_fired_ts", "event_type_id", "data_id"):
        if need not in cols:
            raise SystemExit(f"{path}: not a Home Assistant recorder database (events.{need} missing)")
    has_ctx = {"context_id_bin", "context_parent_id_bin", "context_user_id_bin"} <= cols
    select = "e.time_fired_ts, d.shared_data, t.event_type"
    if has_ctx:
        select += ", e.context_id_bin, e.context_parent_id_bin, e.context_user_id_bin"
    rows = db.execute(
        f"SELECT {select} FROM events e"
        " JOIN event_types t ON t.event_type_id = e.event_type_id"
        " JOIN event_data d ON d.data_id = e.data_id"
        " WHERE t.event_type IN ('call_service', 'automation_triggered')"
        " AND e.time_fired_ts >= ? AND e.time_fired_ts < ?"
        " ORDER BY e.time_fired_ts",
        (since, until),
    )
    calls, triggers = [], []
    for row in rows:
        t, shared, kind = row[:3]
        ctx = None
        if has_ctx:
            ctx = {"id": bytes(row[3]) if row[3] else None,
                   "parent_id": bytes(row[4]) if row[4] else None,
                   "user_id": row[5]}
        try:
            data = json.loads(shared)
        except (TypeError, json.JSONDecodeError):
            continue
        if kind == "call_service":
            calls.append(Call(t, data["domain"], data["service"], data.get("service_data") or {}, ctx))
        else:
            triggers.append(Trigger(t, data.get("entity_id"), data.get("name"), ctx and ctx["id"]))
    return calls, triggers, has_ctx


def automation_made(call, contexts):
    """A call an automation made: its context is the one of an
    `automation_triggered`, or a child of it."""
    ctx = call.ctx or {}
    return ctx.get("id") in contexts or ctx.get("parent_id") in contexts


def window(items, since, until):
    return [i for i in items if since <= i.t < until]


def pair(ours, theirs, tolerance, same):
    """Match `ours` to `theirs` (both sorted by time) one to one: each of ours,
    in time order, takes the *earliest* unmatched one of theirs within
    `tolerance` for which `same(a, b)` holds. With one tolerance for every
    item that finds a complete matching whenever there is one (taking the
    nearest instead can strand a later item). Returns (pairs, ours left,
    theirs left)."""
    times = [b.t for b in theirs]
    used = [False] * len(theirs)
    pairs, left = [], []
    for a in ours:
        lo = bisect.bisect_left(times, a.t - tolerance)
        hi = bisect.bisect_right(times, a.t + tolerance)
        best = next((i for i in range(lo, hi) if not used[i] and same(a, theirs[i])), None)
        if best is None:
            left.append(a)
        else:
            used[best] = True
            pairs.append((a, theirs[best]))
    return pairs, left, [b for b, u in zip(theirs, used) if not u]


def compare_calls(ha, hars, tolerance):
    exact, extra, missing = pair(
        hars, ha, tolerance, lambda a, b: a.key() == b.key() and a.data == b.data
    )
    # What is left may be the same call with other data.
    mismatched, extra, missing = pair(
        extra, missing, tolerance, lambda a, b: a.key() == b.key()
    )
    return exact, mismatched, extra, missing


def compare_triggers(ha, hars, tolerance):
    return pair(hars, ha, tolerance, lambda a, b: a.entity_id == b.entity_id)


def percentile(values, p):
    if not values:
        return 0.0
    values = sorted(values)
    return values[min(len(values) - 1, int(p * len(values)))]


def report(args, since, until, ha_calls, hars_calls, ha_trig, hars_trig, starts):
    exact, mismatched, extra, missing = compare_calls(ha_calls, hars_calls, args.tolerance)
    t_pairs, t_extra, t_missing = compare_triggers(ha_trig, hars_trig, args.tolerance)
    skews = [abs(a.t - b.t) for a, b in exact]
    out = {
        "window": [since, until],
        "starts": starts,
        "calls": {
            "ha": len(ha_calls),
            "hars": len(hars_calls),
            "matched": len(exact),
            "mismatched": len(mismatched),
            "missing": len(missing),
            "extra": len(extra),
            "skew_p50": percentile(skews, 0.5),
            "skew_p95": percentile(skews, 0.95),
            "skew_max": max(skews, default=0.0),
        },
        "triggers": {
            "ha": len(ha_trig),
            "hars": len(hars_trig),
            "matched": len(t_pairs),
            "missing": len(t_missing),
            "extra": len(t_extra),
        },
    }
    if args.json:
        out["mismatched"] = [[b.show(), a.show()] for a, b in mismatched]
        out["missing"] = [c.show() for c in missing]
        out["extra"] = [c.show() for c in extra]
        out["triggers_missing"] = [t.show() for t in t_missing]
        out["triggers_extra"] = [t.show() for t in t_extra]
        print(json.dumps(out, indent=2))
        return out

    def iso(t):
        return datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")

    c, t = out["calls"], out["triggers"]
    print(f"window {iso(since)} .. {iso(until)}, tolerance {args.tolerance:g} s")
    if len(starts) > 1:
        print(f"hars (re)started {len(starts)} times: {', '.join(iso(s) for s in starts)}")
        print("  a restart is a gap in hars's side; use --exclude START,END to leave one out")
    print(
        f"calls:    HA {c['ha']}, hars {c['hars']}: matched {c['matched']}, "
        f"mismatched {c['mismatched']}, missing {c['missing']}, extra {c['extra']}"
    )
    print(
        f"          timing skew of matches: p50 {c['skew_p50']:.3f} s, "
        f"p95 {c['skew_p95']:.3f} s, max {c['skew_max']:.3f} s"
    )
    print(
        f"triggers: HA {t['ha']}, hars {t['hars']}: matched {t['matched']}, "
        f"missing {t['missing']}, extra {t['extra']}"
    )
    per = collections.defaultdict(lambda: [0, 0])
    for x in ha_trig:
        per[x.entity_id][0] += 1
    for x in hars_trig:
        per[x.entity_id][1] += 1
    if per:
        print("per automation (HA / hars):")
        for k, (a, b) in sorted(per.items(), key=lambda kv: str(kv[0])):
            print(f"  {'ok ' if a == b else 'DIFF'} {k}: {a} / {b}")

    def lines(title, items, fmt):
        if not items:
            return
        print(f"\n{title} ({len(items)}):")
        for item in items[: args.limit]:
            print("  " + fmt(item))
        if len(items) > args.limit:
            print(f"  … {len(items) - args.limit} more (--limit)")

    lines("MISMATCHED (HA's call, then hars's)", mismatched, lambda p: f"HA:   {p[1].show()}\n  hars: {p[0].show()}")
    lines("MISSING: HA did it, hars would not have", missing, lambda x: x.show())
    lines("EXTRA: hars would have, HA did not", extra, lambda x: x.show())
    lines("TRIGGERS MISSING: HA's automation fired, hars's did not", t_missing, lambda x: x.show())
    lines("TRIGGERS EXTRA: hars's automation fired, HA's did not", t_extra, lambda x: x.show())
    return out


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--ha-db", help="Home Assistant's recorder database (SQLite)")
    p.add_argument("--ha-jsonl", help="HA's calls in the shadow log's format")
    p.add_argument("--hars-log", required=True, help="hars's shadow log (JSON Lines)")
    p.add_argument("--since", help="start of the window (epoch seconds or ISO 8601); default: hars's first start")
    p.add_argument("--until", help="end of the window; default: now")
    p.add_argument("--exclude", action="append", default=[], metavar="START,END",
                   help="leave a span out of both sides (hars was down); repeatable")
    p.add_argument("--tolerance", type=float, default=1.0, help="seconds (default 1)")
    p.add_argument("--all-contexts", action="store_true",
                   help="compare calls from the UI and apps too, not only automations'")
    p.add_argument("--limit", type=int, default=20, help="rows to list per section")
    p.add_argument("--json", action="store_true", help="print the result as JSON")
    args = p.parse_args(argv)
    if bool(args.ha_db) == bool(args.ha_jsonl):
        p.error("give exactly one of --ha-db and --ha-jsonl")

    try:
        hars_calls, hars_trig, starts, gaps = read_jsonl(args.hars_log)
        since = parse_time(args.since) if args.since else (min(starts) if starts else None)
        until = parse_time(args.until) if args.until else datetime.datetime.now(datetime.timezone.utc).timestamp()
        excludes = []
        for span in args.exclude:
            a, _, b = span.partition(",")
            excludes.append((parse_time(a), parse_time(b)))
        if since is None:
            raise InputError("the log has no shadow_started line: give --since")
        if since >= until:
            raise InputError("the window is empty or reversed: --since must be before --until")
        if args.ha_db:
            ha_calls, ha_trig, has_ctx = read_ha_db(args.ha_db, since, until)
        else:
            ha_calls, ha_trig, _, _ = read_jsonl(args.ha_jsonl)
            has_ctx = True
    except (InputError, ValueError, OSError, sqlite3.Error) as e:
        print(f"shadow-diff: {e}", file=sys.stderr)
        return 2
    if not has_ctx and not args.all_contexts:
        print("note: this database has no context columns, so calls are not told apart by who made them",
              file=sys.stderr)

    # A gap in hars's log (a write that failed) is time it can't vouch for:
    # leave it out of both sides, and the result is not clean.
    excludes += [(a, b) for a, b, _ in gaps]

    def keeper(triggers):
        contexts = {t.ctx_id for t in triggers if t.ctx_id}

        def keep(c):
            if c.domain in RUNS:
                return False
            return args.all_contexts or not has_ctx or automation_made(c, contexts)

        return keep

    # Each side's automation calls are those in a context one of its own
    # `automation_triggered` events started.
    ha_calls = list(filter(keeper(ha_trig), ha_calls))
    hars_calls = list(filter(keeper(hars_trig), hars_calls))

    def inside(items):
        items = window(items, since, until)
        return [i for i in items if not any(a <= i.t < b for a, b in excludes)]

    ha_calls, hars_calls = inside(ha_calls), inside(hars_calls)
    ha_trig, hars_trig = inside(ha_trig), inside(hars_trig)
    for xs in (ha_calls, hars_calls, ha_trig, hars_trig):
        xs.sort(key=lambda x: x.t)
    out = report(args, since, until, ha_calls, hars_calls, ha_trig, hars_trig, starts)
    clean = all(out["calls"][k] == 0 for k in ("mismatched", "missing", "extra")) and all(
        out["triggers"][k] == 0 for k in ("missing", "extra")
    )
    if gaps:
        clean = False
        if not args.json:
            dropped = sum(n for _, _, n in gaps)
            print(f"\nhars's log has {len(gaps)} gap(s), {dropped} decision(s) lost: not clean")
    if not args.json:
        print("\nCLEAN" if clean else "\nDIFFERENCES FOUND")
    return 0 if clean else 1


if __name__ == "__main__":
    sys.exit(main())
