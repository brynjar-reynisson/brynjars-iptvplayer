#!/usr/bin/env python3
"""Measure how far a new .srt is out of sync with an .srt that is already in
the movie (same film, usually another language), using shared proper nouns.

    python measure_offset.py <reference.srt> <new.srt>

Prints the shift to apply to <new.srt> in seconds (use it as ffmpeg
`-itsoffset <shift>` before the new .srt input; negative = show earlier),
how many anchors agree, and whether the offset is constant over the film.
Exit code 0 = confident, 2 = not confident (read the output, do not guess).
"""
import re
import statistics
import sys
from collections import Counter, defaultdict

BIN = 0.5          # histogram bin, seconds
AGREE = 1.5        # anchors within this of the mode count as agreeing (not "share": repeated names pair wrongly by nature)
MAX_FREQ = 8       # a word is an anchor only if rare in both files


def cues(path):
    text = open(path, encoding="utf-8-sig", errors="replace").read().replace("\r", "")
    out = []
    for block in text.split("\n\n"):
        m = re.search(r"(\d+):(\d+):(\d+)[,.](\d+) --> ", block)
        if m:
            h, mi, s, ms = (int(x) for x in m.groups())
            body = " ".join(block.split("\n")[2:])
            out.append((h * 3600 + mi * 60 + s + ms / 1000.0, body))
    return out


def anchors(body):
    # Capitalised words that are not the first word of the line: names, places.
    words = re.findall(r"[^\W\d_]+", body)
    return {w.lower() for w in words[1:] if len(w) >= 4 and w[0].isupper()}


def index(cs):
    idx = defaultdict(list)
    for t, body in cs:
        for w in anchors(body):
            idx[w].append(t)
    return idx


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 1
    ref, new = index(cues(sys.argv[1])), index(cues(sys.argv[2]))
    pairs = []  # (new time, ref time - new time)
    for w, nt in new.items():
        rt = ref.get(w)
        if not rt or len(nt) > MAX_FREQ or len(rt) > MAX_FREQ:
            continue
        for a in nt:
            for b in rt:
                pairs.append((a, b - a))
    if len(pairs) < 20:
        print("Too few shared names to measure (%d). Compare one line by hand." % len(pairs))
        return 2
    bins = Counter(round(d / BIN) for _, d in pairs)
    mode = bins.most_common(1)[0][0] * BIN
    near = [(t, d) for t, d in pairs if abs(d - mode) <= AGREE]
    shift = statistics.median(d for _, d in near)
    share = len(near) / len(pairs)
    mid = statistics.median(t for t, _ in near)
    first = [d for t, d in near if t < mid]
    second = [d for t, d in near if t >= mid]
    drift = abs(statistics.median(first) - statistics.median(second)) if first and second else 0.0
    print("shift to apply to the new .srt: %.1f s  (ffmpeg -itsoffset %.1f)" % (shift, shift))
    print("anchors agreeing: %d of %d (%.0f %%)" % (len(near), len(pairs), share * 100))
    print("first half vs second half differ by %.1f s" % drift)
    # The peak must clearly beat any rival offset (wrong pairings are spread out).
    rival = max([c for b, c in bins.items() if abs(b * BIN - mode) > AGREE] or [0])
    peak = bins[round(mode / BIN)]
    print("best offset has %d anchors, the best rival %d" % (peak, rival))
    ok = len(near) >= 15 and peak >= 3 * max(rival, 1) and drift <= 1.5
    if not ok:
        print("NOT CONFIDENT: the offset is not constant or the files differ too much "
              "(different cut/frame rate?). Do not apply it blindly.")
        return 2
    print("Constant offset: safe to apply.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
