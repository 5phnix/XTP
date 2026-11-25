"""The pass list of an `xtp-pix walk` table, and where light goes.

    python -I tools/pix/passes.py <walk.tsv> [--hdr-only] [--drop 0.2] [--names]

Prints one line per measured output of each pass (pass, events, the first
programs, kind, resource, size, format, mean and max luminance, the share
of texels that are zero, NaN or infinite), then, for every float target
(the formats light is kept in), its luminance from one writer to the next,
marking the passes after which its mean falls below --drop times what it
was (the light disappears there) and the first writes that are already
black. Reads only the table xtp-pix wrote; no capture, no replay.
"""

import csv

csv.field_size_limit(1 << 30)
import sys
from collections import defaultdict

FORMATS = {
    2: "RGBA32F", 10: "RGBA16F", 11: "RGBA16", 16: "RG32F", 20: "D32S8", 24: "RGB10A2", 26: "R11G11B10F",
    28: "RGBA8", 29: "RGBA8_SRGB", 34: "RG16F", 35: "RG16", 40: "D32F", 41: "R32F", 42: "R32U", 45: "D24S8",
    49: "RG8", 54: "R16F", 56: "R16", 57: "R16U", 61: "R8", 62: "R8U", 67: "RGB9E5", 87: "BGRA8", 91: "BGRA8_SRGB",
}
# Where a renderer keeps light: float colour formats.
HDR = {2, 10, 26, 67}


def fmt_name(f):
    try:
        return FORMATS.get(int(f), f"f{f}")
    except ValueError:
        return f or "?"


def num(x, default=0.0):
    try:
        return float(x)
    except (TypeError, ValueError):
        return default


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    path = argv[1]
    hdr_only = "--hdr-only" in argv
    names = "--names" in argv
    drop = 0.2
    if "--drop" in argv:
        drop = float(argv[argv.index("--drop") + 1])
    rows = list(csv.DictReader(open(path, encoding="utf-8", errors="replace"), delimiter="\t"))
    history = defaultdict(list)
    for r in rows:
        if r["when"] != "after":
            continue
        f = num(r["format"], -1)
        texels = num(r["texels"])
        zero = num(r["zero"]) / texels if texels else 0
        bad = (num(r["nan"]) + num(r["inf"])) / texels if texels else 0
        span = r["call"].split("; last:")[0]
        if not hdr_only or int(f) in HDR:
            res = r["resource"] if names else r["id"]
            print(
                f"q{r['queue']} P{r['pass']:>4} {span[:44]:44} {r['programs'][:40]:40} {r['kind']} {res[:28]:28} "
                f"{r['size']:>13} {fmt_name(r['format']):>10} lum {num(r['mean_lum']):.4g}/{num(r['max_lum']):.4g} "
                f"zero {zero:.0%} nan/inf {bad:.0%} {r['note']}"
            )
        if int(f) in HDR and texels:
            history[(r["queue"], r["id"], r["size"])].append(r)
    print()
    print("Float targets, writer by writer (mean luminance; ! = falls below --drop of the previous, 0 = first write black):")
    for (queue, rid, size), rs in history.items():
        line = []
        prev = None
        for r in rs:
            m = num(r["mean_lum"])
            mark = ""
            if prev is None and m < 1e-6:
                mark = "0"
            elif prev is not None and prev > 1e-6 and m < drop * prev:
                mark = "!"
            line.append(f"P{r['pass']}:{m:.3g}{mark}")
            prev = m
        name = rs[0]["resource"] if names else ""
        print(f"q{queue} id {rid} {size} {fmt_name(rs[0]['format'])} {name}: " + " ".join(line))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
