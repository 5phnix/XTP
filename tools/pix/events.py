"""A summary of an `xtp-pix events` table: what one captured frame did.

    python -I tools/pix/events.py <events.tsv> [--program HASH]... [--calls NAME]...

Prints, per queue, how many calls of each kind the frame made; the draws
and dispatches with XTP's marker before them (`XTP_D3D12_DRED=1`) counted
by program hash; and, with --program, every draw or dispatch after a
marker naming that hash, with its arguments (--calls NAME lists every call
of that name instead). Reads only the table; no capture, no replay.
"""

import csv
import re
import sys
from collections import Counter, defaultdict

WORK = ("Draw", "Dispatch", "ExecuteIndirect")
MARKER = re.compile(r"^(draw|indexed|dispatch|mesh|indirect) ")


def leaves(xml):
    """The leaf elements of PIX's XML call data, as (tag, text)."""
    return re.findall(r"<([A-Za-z0-9_]+)>([^<]*)</\1>", xml or "")


def compact(name, xml):
    args = [f"{t}={v}" for t, v in leaves(xml) if t not in ("this", "returnValue")]
    return f"{name}({', '.join(args)})"


def marker_text(row):
    if MARKER.match(row["name"]):
        return row["name"]
    if "Marker" in row["name"]:
        for _, text in leaves(row["call"]):
            text = text.replace("&#60;", "<").replace("&#62;", ">").replace("&amp;", "&")
            if MARKER.match(text):
                return text
    return None


def programs(marker):
    return "+".join(re.findall(r"\| ([0-9a-f]{16}):", marker or ""))


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    only = [argv[i + 1] for i, a in enumerate(argv) if a == "--program" and i + 1 < len(argv)]
    calls = [argv[i + 1] for i, a in enumerate(argv) if a == "--calls" and i + 1 < len(argv)]
    rows = list(csv.DictReader(open(argv[1], encoding="utf-8", errors="replace"), delimiter="\t"))
    kinds = defaultdict(Counter)
    by_program = defaultdict(Counter)
    marker = None
    listed = []
    for r in rows:
        q = r["queue"]
        kinds[q][r["name"]] += 1
        m = marker_text(r)
        if m:
            marker = m
            continue
        if r["name"].startswith(WORK):
            p = programs(marker) or "(no marker)"
            by_program[q][p] += 1
            if any(h in p for h in only):
                listed.append((q, r["index"], p, compact(r["name"], r["call"]), (marker or "")[:300]))
            marker = None
        if r["name"] in calls:
            listed.append((q, r["index"], "", compact(r["name"], r["call"]), ""))
    for q in sorted(kinds):
        work = sum(n for k, n in kinds[q].items() if k.startswith(WORK))
        print(f"queue {q}: {sum(kinds[q].values())} events, {work} draws and dispatches")
        for k, n in kinds[q].most_common(40):
            print(f"  {n:7d} {k}")
        if by_program[q]:
            print(f"  by XTP program ({len(by_program[q])} programs):")
            for p, n in by_program[q].most_common(25):
                print(f"  {n:7d} {p}")
    for q, i, p, c, m in listed:
        print(f"q{q} #{i} {p} {c}" + (f"  {{{m}}}" if m else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
