#!/usr/bin/env python3
"""Independent re-implementation of the M0 cleaning rules, as a differential oracle.

Reads a raw directory (Socrata pages) and a snapshot built from it, applies
docs/snapshot/cleaning-rules.md from scratch in plain Python, and checks that
the snapshot has exactly the same rows, order, values, cleaning log and rejects.
Shares no code with the Rust implementation.

    python3 tools/oracle/snapshot_oracle.py <raw_dir> <snapshot_dir>

Requires pyarrow.
"""
import datetime as dt
import json
import math
import re
import struct
import sys
from collections import Counter, defaultdict
from pathlib import Path

import pyarrow.ipc as ipc

TEXT = ["agency", "complaint_type", "descriptor", "location_type", "incident_zip",
        "borough", "community_board", "status", "open_data_channel_type"]
FIELDS = ["unique_key", "created_date", "closed_date", *TEXT, "latitude", "longitude"]
COLUMNS = ["unique_key", "created_date", "closed_date", "agency", "complaint_type", "descriptor",
           "location_type", "incident_zip", "borough", "community_board", "status", "channel", "location"]
TS = re.compile(r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,6}))?$")
NUM = re.compile(r"^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$")
EPOCH = dt.datetime(1970, 1, 1)
WS = " \t\n\x0b\x0c\r"


def text(v):
    if v is None:
        return None
    if isinstance(v, bool) or isinstance(v, (dict, list)):
        raise SystemExit(f"non-scalar value {v!r}")
    return v if isinstance(v, str) else json.dumps(v)


def ts(s):
    m = TS.match(s) if s is not None else None
    if not m:
        return None
    y, mo, d, h, mi, se, frac = m.groups()
    try:
        t = dt.datetime(int(y), int(mo), int(d), int(h), int(mi), int(se))
    except ValueError:
        return None
    return (t - EPOCH) // dt.timedelta(microseconds=1) + int((frac or "").ljust(6, "0") or 0)


def key(s):
    if s is None or not re.fullmatch(r"[1-9][0-9]*", s) or int(s) > 2**63 - 1:
        return None
    return int(s)


def coord(s):
    if s is None or not NUM.match(s):
        return None
    v = float(s)
    return v if math.isfinite(v) else None


def f32(v):
    return struct.unpack("<f", struct.pack("<f", v))[0]


def main(raw_dir, snap_dir):
    raw_dir, snap_dir = Path(raw_dir), Path(snap_dir)
    fetch = json.loads((raw_dir / "fetch.json").read_text())
    lo, hi = ts(fetch["scope"]["gte"]), ts(fetch["scope"]["lt"])
    records = [r for p in fetch["pages"] for r in json.loads((raw_dir / p["file"]).read_text())]

    rejects = []  # (raw_key, rule)
    staged = []
    for r in records:
        k_raw = text(r.get("unique_key"))
        k = key(k_raw)
        if k is None:
            rejects.append((k_raw, "CR-02"))
            continue
        staged.append((k, r))

    groups = defaultdict(list)
    for k, r in staged:
        groups[k].append(r)
    admitted = []
    for k, rs in groups.items():
        if len(rs) > 1:
            values = {tuple(json.dumps(r.get(f), sort_keys=True) for f in FIELDS) for r in rs}
            if len(values) == 1:
                rejects += [(str(k), "CR-03")] * (len(rs) - 1)
                rs = rs[:1]
            else:
                rejects += [(str(k), "CR-04")] * len(rs)
                continue
        r = rs[0]
        c = ts(text(r.get("created_date")))
        if c is None:
            rejects.append((str(k), "CR-05"))
        elif not lo <= c < hi:
            rejects.append((str(k), "CR-12"))
        else:
            admitted.append((c, k, r))
    admitted.sort(key=lambda t: (t[0], t[1]))

    rows, log = [], []
    for i, (c, k, r) in enumerate(admitted):
        row = [k, c]
        closed_raw = text(r.get("closed_date"))
        closed = ts(closed_raw)
        if closed_raw is not None and closed is None:
            log.append((i, 2, "CR-06", closed_raw))
        row.append(closed)
        for ci, f in enumerate(TEXT, start=3):
            v = text(r.get(f))
            if v is None:
                row.append(None)
                continue
            t = v.strip(WS)
            if not t:
                log.append((i, ci, "CR-08", v))
                row.append(None)
            else:
                if t != v:
                    log.append((i, ci, "CR-07", v))
                row.append(t)
        la, lo_ = text(r.get("latitude")), text(r.get("longitude"))
        pa, po = coord(la), coord(lo_)
        if pa is not None and po is not None:
            row.append((f32(pa), f32(po)))
        else:
            row.append(None)
            if la is not None or lo_ is not None:
                log.append((i, 12, "CR-09", None))
        rows.append(row)

    data = ipc.open_file(snap_dir / "data.arrow").read_all()
    assert data.column_names == COLUMNS, data.column_names
    got_rows = []
    cols = [data.column(n).to_pylist() for n in COLUMNS]
    for i in range(data.num_rows):
        row = []
        for n, col in zip(COLUMNS, cols):
            v = col[i]
            if isinstance(v, dt.datetime):
                v = (v - EPOCH) // dt.timedelta(microseconds=1)
            elif isinstance(v, dict):
                v = (v["lat"], v["lon"])
            row.append(v)
        got_rows.append(row)

    problems = []
    if len(got_rows) != len(rows):
        problems.append(f"row count: snapshot {len(got_rows)}, oracle {len(rows)}")
    for i, (a, b) in enumerate(zip(got_rows, rows)):
        if a != b:
            problems.append(f"row {i}: snapshot {a} != oracle {b}")
            if len(problems) > 10:
                break

    got_log = ipc.open_file(snap_dir / "cleaning_log.arrow").read_all().to_pylist()
    got_log = [(e["row_index"], e["column"], e["rule_id"], e["raw_value"] if e["rule_id"] != "CR-09" else None)
               for e in got_log]
    if sorted(got_log, key=str) != sorted(log, key=str):
        problems.append(f"cleaning log differs: snapshot {len(got_log)} entries, oracle {len(log)}")

    got_rej = ipc.open_file(snap_dir / "rejects.arrow").read_all().to_pylist()
    if Counter((r["raw_unique_key"], r["rule_id"]) for r in got_rej) != Counter(rejects):
        problems.append("rejects differ")

    if problems:
        print("MISMATCH\n  " + "\n  ".join(problems))
        return 1
    print(f"OK: {len(rows)} rows, {len(log)} cleaning-log entries, {len(rejects)} rejects match the oracle")
    return 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
