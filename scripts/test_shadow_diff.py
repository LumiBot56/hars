"""Tests for shadow-diff.py: python3 -m unittest discover -s scripts"""

import contextlib
import importlib.util
import io
import json
import os
import sqlite3
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("shadow_diff", os.path.join(HERE, "shadow-diff.py"))
sd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sd)

T0 = 1_768_464_000.0  # 2026-01-15T08:00:00Z
_B32 = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"


def ulid(n):
    """A context id: the number as a 26-character ULID."""
    out = ""
    for _ in range(26):
        out = _B32[n % 32] + out
        n //= 32
    return out


# An automation's run has one context, shared by its trigger and its calls
# (a time trigger has no parent). A person's call has its own, with a user.
AUTO = {"id": ulid(1), "parent_id": None, "user_id": None}
USER = {"id": ulid(3), "parent_id": None, "user_id": "abc"}


def call(t, domain, service, data, ctx=AUTO):
    return {"t": t, "type": "call_service", "domain": domain, "service": service,
            "service_data": data, "context": ctx}


def trig(t, entity, ctx=AUTO):
    return {"t": t, "type": "automation_triggered",
            "data": {"name": entity, "entity_id": entity, "source": "x"}, "context": ctx}


class Run:
    """Write the two sides, run the tool, keep its output and status."""

    def __init__(self, test, ha, hars, *extra, starts=(T0 - 10,), db=False, auto=True):
        if auto:
            # Both sides' automation started the run whose context AUTO is.
            ha, hars = [trig(T0 - 5, "automation.run")] + ha, [trig(T0 - 5, "automation.run")] + hars
        self.dir = tempfile.TemporaryDirectory()
        test.addCleanup(self.dir.cleanup)
        d = self.dir.name
        log = os.path.join(d, "hars.jsonl")
        with open(log, "w") as f:
            for s in starts:
                f.write(json.dumps({"t": s, "type": "shadow_started"}) + "\n")
            for row in hars:
                f.write(json.dumps(row) + "\n")
        args = ["--hars-log", log, "--until", str(T0 + 3600), *extra]
        if db:
            path = os.path.join(d, "ha.db")
            make_db(path, ha)
            args += ["--ha-db", path]
        else:
            path = os.path.join(d, "ha.jsonl")
            with open(path, "w") as f:
                for row in ha:
                    f.write(json.dumps(row) + "\n")
            args += ["--ha-jsonl", path]
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.status = sd.main(args)
        self.out = out.getvalue()


def make_db(path, rows):
    """HA's recorder schema, as much as the tool reads."""
    db = sqlite3.connect(path)
    db.executescript("""
        CREATE TABLE event_types (event_type_id INTEGER PRIMARY KEY, event_type TEXT);
        CREATE TABLE event_data (data_id INTEGER PRIMARY KEY, shared_data TEXT);
        CREATE TABLE events (event_id INTEGER PRIMARY KEY, event_type_id INTEGER, data_id INTEGER,
            time_fired_ts REAL, context_id_bin BLOB, context_parent_id_bin BLOB,
            context_user_id_bin BLOB);
        INSERT INTO event_types VALUES (1, 'call_service'), (2, 'automation_triggered');
    """)
    for i, row in enumerate(rows, 1):
        if row["type"] == "call_service":
            data = {"domain": row["domain"], "service": row["service"],
                    "service_data": row["service_data"]}
            type_id = 1
        else:
            data, type_id = row["data"], 2
        ctx = row["context"]
        db.execute("INSERT INTO event_data VALUES (?, ?)", (i, json.dumps(data)))
        db.execute(
            "INSERT INTO events (event_type_id, data_id, time_fired_ts, context_id_bin,"
            " context_parent_id_bin, context_user_id_bin) VALUES (?, ?, ?, ?, ?, ?)",
            (type_id, i, row["t"], sd.ulid_bytes(ctx["id"]), sd.ulid_bytes(ctx["parent_id"]),
             ctx["user_id"] and b"\x02"),
        )
    db.commit()
    db.close()


LIGHT = {"entity_id": ["light.a"], "brightness_pct": 40}


class ShadowDiff(unittest.TestCase):
    def test_a_matching_week_is_clean(self):
        ha = [trig(T0, "automation.hall"), call(T0 + 0.1, "light", "turn_on", LIGHT)]
        hars = [trig(T0 + 0.3, "automation.hall"),
                call(T0 + 0.4, "light", "turn_on", {"entity_id": "light.a", "brightness_pct": 40.0})]
        for db in (False, True):
            r = Run(self, ha, hars, db=db)
            self.assertEqual(r.status, 0, r.out)
            self.assertIn("matched 1", r.out)
            self.assertIn("CLEAN", r.out)

    def test_each_kind_of_difference(self):
        ha = [
            call(T0 + 10, "light", "turn_on", LIGHT),                       # matched
            call(T0 + 20, "light", "turn_on", {"entity_id": "light.a", "brightness_pct": 80}),
            call(T0 + 30, "light", "turn_off", {"entity_id": "light.b"}),   # missing
            trig(T0 + 40, "automation.a"),                                   # trigger missing
        ]
        hars = [
            call(T0 + 10.2, "light", "turn_on", LIGHT),
            call(T0 + 20.3, "light", "turn_on", {"entity_id": "light.a", "brightness_pct": 70}),  # mismatch
            call(T0 + 50, "light", "turn_on", {"entity_id": "light.c"}),    # extra
            trig(T0 + 60, "automation.b"),                                   # trigger extra
        ]
        r = Run(self, ha, hars, "--json")
        self.assertEqual(r.status, 1)
        out = json.loads(r.out)
        self.assertEqual(
            {k: out["calls"][k] for k in ("matched", "mismatched", "missing", "extra")},
            {"matched": 1, "mismatched": 1, "missing": 1, "extra": 1},
        )
        self.assertEqual((out["triggers"]["missing"], out["triggers"]["extra"]), (1, 1))
        self.assertIn("brightness_pct\": 80", out["mismatched"][0][0])
        self.assertIn("brightness_pct\": 70", out["mismatched"][0][1])

    def test_the_tolerance_is_inclusive_of_one_second_and_no_more(self):
        ha = [call(T0, "light", "turn_on", LIGHT)]
        inside = Run(self, ha, [call(T0 + 0.99, "light", "turn_on", LIGHT)])
        self.assertEqual(inside.status, 0, inside.out)
        outside = Run(self, ha, [call(T0 + 1.5, "light", "turn_on", LIGHT)])
        self.assertEqual(outside.status, 1)
        wide = Run(self, ha, [call(T0 + 1.5, "light", "turn_on", LIGHT)], "--tolerance", "2")
        self.assertEqual(wide.status, 0)

    def test_one_call_matches_one_call(self):
        # Two calls of HA's cannot both be explained by one of hars's.
        ha = [call(T0, "light", "turn_on", LIGHT), call(T0 + 0.5, "light", "turn_on", LIGHT)]
        r = Run(self, ha, [call(T0 + 0.2, "light", "turn_on", LIGHT)], "--json")
        out = json.loads(r.out)
        self.assertEqual((out["calls"]["matched"], out["calls"]["missing"]), (1, 1))

    def test_who_made_the_call_and_the_domain(self):
        ha = [
            call(T0, "light", "turn_on", LIGHT, USER),                  # a person's: ignored
            call(T0 + 1, "automation", "trigger", {}, AUTO),            # hars runs these: ignored
            call(T0 + 2, "light", "turn_on", LIGHT, AUTO),
        ]
        hars = [call(T0 + 2, "light", "turn_on", LIGHT, AUTO)]
        for db in (False, True):
            r = Run(self, ha, hars, db=db)
            self.assertEqual(r.status, 0, r.out)
        r = Run(self, ha, hars, "--all-contexts")
        self.assertEqual(r.status, 1)  # the person's call is now HA's alone

    def test_window_and_excluded_spans(self):
        ha = [call(T0 - 100, "light", "turn_on", LIGHT),                # before hars started
              call(T0 + 500, "light", "turn_on", LIGHT)]                # while hars was down
        r = Run(self, ha, [], "--exclude", f"{T0 + 400},{T0 + 600}")
        self.assertEqual(r.status, 0, r.out)
        r = Run(self, ha, [])
        self.assertEqual(r.status, 1)
        r = Run(self, ha, [], starts=(T0 - 10, T0 + 700))
        self.assertIn("started 2 times", r.out)

    def test_entity_lists_compare_as_sets_and_numbers_as_numbers(self):
        ha = [call(T0, "light", "turn_on", {"entity_id": ["light.b", "light.a"], "transition": 2})]
        hars = [call(T0, "light", "turn_on", {"entity_id": "light.a,light.b", "transition": 2.0})]
        self.assertEqual(Run(self, ha, hars).status, 0)

    def test_a_time_triggered_automation_is_compared_too(self):
        # A time trigger has no parent context; its calls still belong to it.
        ha = [call(T0, "light", "turn_on", LIGHT)]
        hars = [call(T0, "light", "turn_off", LIGHT)]
        r = Run(self, ha, hars, "--json")
        self.assertEqual(r.status, 1)
        out = json.loads(r.out)
        self.assertEqual((out["calls"]["missing"], out["calls"]["extra"]), (1, 1))

    def test_a_complete_pairing_is_found_when_there_is_one(self):
        # HA at 0 and 1 s, hars at 0.6 and 1.6 s, tolerance 1 s: pairing each
        # with the nearer one strands the second.
        ha = [call(T0, "light", "turn_on", LIGHT), call(T0 + 1, "light", "turn_on", LIGHT)]
        hars = [call(T0 + 0.6, "light", "turn_on", LIGHT), call(T0 + 1.6, "light", "turn_on", LIGHT)]
        r = Run(self, ha, hars)
        self.assertEqual(r.status, 0, r.out)

    def test_a_cut_off_last_line_is_skipped_and_a_bad_one_inside_is_not(self):
        r = Run(self, [], [])
        path = os.path.join(r.dir.name, "hars.jsonl")
        with open(path, "a") as f:
            f.write('{"t": 1768464000.5, "type": "call_ser')
        calls, trig_, starts, gaps = sd.read_jsonl(path)
        self.assertEqual((len(calls), len(trig_), len(starts)), (0, 1, 1))
        # A restart after the crash marks the torn line off: still fine.
        with open(path, "a") as f:
            f.write("\n" + json.dumps({"t": T0, "type": "shadow_started"}) + "\n")
        self.assertEqual(len(sd.read_jsonl(path)[2]), 2)
        # A bad line with something real after it is corruption.
        with open(path, "a") as f:
            f.write("not json\n" + json.dumps(call(T0, "light", "turn_on", LIGHT)) + "\n")
        with self.assertRaises(sd.InputError):
            sd.read_jsonl(path)
        r = Run(self, [], [])
        with open(os.path.join(r.dir.name, "ha.jsonl"), "a") as f:
            f.write("garbage\n" + json.dumps(trig(T0, "automation.a")) + "\n")
        out = io.StringIO()
        with contextlib.redirect_stderr(out), contextlib.redirect_stdout(io.StringIO()):
            status = sd.main(["--hars-log", os.path.join(r.dir.name, "hars.jsonl"),
                              "--ha-jsonl", os.path.join(r.dir.name, "ha.jsonl")])
        self.assertEqual(status, 2)
        self.assertIn("not JSON", out.getvalue())

    def test_a_reversed_window_is_an_error_not_a_pass(self):
        out = io.StringIO()
        with contextlib.redirect_stderr(out):
            r = Run(self, [], [], "--since", str(T0 + 100), "--until", str(T0))
        self.assertEqual(r.status, 2)

    def test_a_gap_in_hars_log_makes_the_result_not_clean(self):
        gap = {"t": T0 + 50, "until": T0 + 80, "type": "shadow_gap", "dropped": 3}
        ha = [call(T0 + 60, "light", "turn_on", LIGHT)]  # in the gap: left out, not "missing"
        r = Run(self, ha, [gap])
        self.assertEqual(r.status, 1)
        self.assertIn("1 gap", r.out)
        self.assertIn("missing 0", r.out)


if __name__ == "__main__":
    unittest.main()
