"""Port function names and translation units from doldecomp/mkdd (CC0) into sheets/functions.csv.

The community project targets the MarioClub_us debug build, whose addresses differ from
GM4E01 retail. The link order is the same, though, so we align the two builds' function
lists by size sequence and accept matches only inside runs of identical sizes. Each
accepted match gets the community name, and its unit is set to the source file the
community splits.txt assigns it to.

Inputs : ../MKDD/community/MarioClub_us/{symbols.txt,splits.txt}
Output : sheets/functions.csv (name/unit/status/notes updated in place), sheets/symbol_matches.csv
"""
import csv
import re
from difflib import SequenceMatcher
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
COMMUNITY = ROOT.parent / "MKDD" / "community" / "MarioClub_us"
SHEETS = ROOT / "sheets"
MAX_GAP = 24  # largest gap paired positionally in pass 2
MIN_RUN = 3  # matching blocks shorter than this are ignored unless sizes are distinctive
SYM_RE = re.compile(r"^(\S+) = (\.\w+):0x([0-9A-Fa-f]+); // type:function size:0x([0-9A-Fa-f]+)")


def load_symbols():
    out = []
    for line in open(COMMUNITY / "symbols.txt", encoding="utf-8"):
        m = SYM_RE.match(line)
        if m and m.group(2) in (".init", ".text"):
            out.append((int(m.group(3), 16), int(m.group(4), 16), m.group(1)))
    out.sort()
    return out


def load_splits():
    """Return sorted [(start, end, source_file)] for .init/.text ranges."""
    ranges, cur = [], None
    for line in open(COMMUNITY / "splits.txt", encoding="utf-8"):
        if line and not line[0].isspace() and line.rstrip().endswith(":") and line.strip() != "Sections:":
            cur = line.strip()[:-1]
            continue
        m = re.match(r"\s+(\.init|\.text)\s+start:0x([0-9A-Fa-f]+) end:0x([0-9A-Fa-f]+)", line)
        if m and cur:
            ranges.append((int(m.group(2), 16), int(m.group(3), 16), cur))
    ranges.sort()
    return ranges


def unit_name(path):
    stem = re.sub(r"\.(cpp|cxx|cc|c|s)$", "", path)
    return re.sub(r"[^A-Za-z0-9_]", "_", stem)


def main():
    syms = load_symbols()
    splits = load_splits()
    with open(SHEETS / "functions.csv", newline="", encoding="utf-8") as fh:
        reader = csv.DictReader(fh)
        cols = reader.fieldnames
        rows = list(reader)
    rows.sort(key=lambda r: int(r["addr"], 16))

    a = [int(r["size"]) for r in rows]
    b = [s[1] for s in syms]
    sm = SequenceMatcher(None, a, b, autojunk=False)
    matches = []
    for blk in sm.get_matching_blocks():
        if blk.size == 0:
            continue
        if blk.size < MIN_RUN and max(a[blk.a:blk.a + blk.size]) < 0x100:
            continue  # short run of common small sizes: too ambiguous
        for k in range(blk.size):
            matches.append((blk.a + k, blk.b + k))

    # Pass 2: a gap bounded by matches on both sides that holds the same number of functions in
    # both builds is paired positionally (debug-only asserts change sizes, not order).
    positional = set()
    anchors = sorted(matches)
    for (a0, b0), (a1, b1) in zip(anchors, anchors[1:]):
        n = a1 - a0 - 1
        if 0 < n <= MAX_GAP and n == b1 - b0 - 1:
            for k in range(1, n + 1):
                positional.add((a0 + k, b0 + k))
    matches = sorted(set(matches) | positional)

    def split_for(addr):
        lo, hi = 0, len(splits)
        while lo < hi:
            mid = (lo + hi) // 2
            if splits[mid][0] <= addr:
                lo = mid + 1
            else:
                hi = mid
        if lo and splits[lo - 1][0] <= addr < splits[lo - 1][1]:
            return splits[lo - 1][2]
        return None

    report = []
    for ia, ib in matches:
        r = rows[ia]
        daddr, size, name = syms[ib]
        src = split_for(daddr)
        report.append({"addr": r["addr"], "old_name": r["name"], "name": name,
                       "debug_addr": f"0x{daddr:08X}", "size": size, "source_file": src or ""})
        if r["name"].startswith("FUN_") or r["name"] == name:
            r["name"] = name
        if src:
            r["unit"] = unit_name(src)
        if r["status"] == "raw":
            r["status"] = "named"
        how = "positional" if (ia, ib) in positional else "size-run"
        report[-1]["match"] = how
        note = f"name from doldecomp/mkdd MarioClub_us {daddr:#010x} ({how})"
        if note not in r["notes"]:
            r["notes"] = (r["notes"] + "; " if r["notes"] else "") + note

    with open(SHEETS / "functions.csv", "w", newline="", encoding="utf-8") as fh:
        w = csv.DictWriter(fh, cols)
        w.writeheader()
        w.writerows(rows)
    with open(SHEETS / "symbol_matches.csv", "w", newline="", encoding="utf-8") as fh:
        w = csv.DictWriter(fh, ["addr", "old_name", "name", "debug_addr", "size", "source_file", "match"])
        w.writeheader()
        w.writerows(report)
    print(f"{len(syms)} community functions, {len(rows)} retail functions, "
          f"{len(matches)} matched ({100 * len(matches) / len(rows):.1f}%), "
          f"{len(positional)} positional, {sum(1 for x in report if x['source_file'])} with source file")


if __name__ == "__main__":
    main()
