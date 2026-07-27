# perf.md — the numbers log

PLAN.md §4 owns the budgets; this file is the evidence that they hold. One
section per milestone, appended to, never rewritten: a regression is only
visible if the old number is still here.

**Rules.** Release build (`--release` / `cargo bench`). Fixtures, not the live
web, so a number means the engine and not somebody's CDN. Every row says which
machine produced it — these are wall-clock times, not portable constants.

**Machine A** — Apple M4 Pro, macOS 15 (Darwin 25.5), rustc stable, 2026-07.

---

## M3 — text layout (2026-07-25)

### Fast gate: scroll step < 5 ms

`cargo bench --bench scroll`, Wikipedia fixture (1.5 MB, ~3.6k laid-out lines).
One step is the whole keypress→screen path: resolve `j`, move the offset, paint
the visible slice, diff against the previous frame, serialize the changed cells.
Parse and layout run once outside the loop — the scroll path is forbidden to
touch them, and the bench would be measuring the wrong thing if it did.

| Frame | Scroll step | Budget | Headroom |
|---|---|---|---|
| 120×40 | **28.5 µs** (27.9–29.2) | < 5 ms | 175× |
| 200×50 | **44.4 µs** (43.0–45.8) | < 5 ms | 113× |

Run-to-run spread on this machine is about ±5%; treat the third digit as noise.
At either end of the page the walk turns around rather than jumping to the top,
so every measured iteration is one line step and no whole-page jump is averaged
in with them.

The write goes to `io::sink()`: `Renderer::present` diffs and writes in one
call, so what is measured is draw + diff + serialize, everything short of
handing the bytes to the terminal. Each step really does emit ~1.1 KB of escape
bytes at 120×40 — the diff is doing work, not skipping an unchanged screen.

Two sizes because the column caps at 90 cells: a wider terminal buys no extra
text, only more blank gutter for the diff to walk. The cost tracks cell count,
as it should.

### Pipeline: parse + layout per page

`yata --timing <url>` against the committed fixtures served from
`127.0.0.1` (`python3 -m http.server`), so `fetch` is loopback overhead and the
engine stages stand on their own. Median run; `--timing` prints to one decimal,
so treat ±0.1 ms as the floor.

| Page | fetch | parse | layout | parse + layout | Budget |
|---|---|---|---|---|---|
| example.com | 2.5 ms | 0.0 ms | 0.0 ms | ~0.0 ms | — |
| motherfuckingwebsite.com | 2.1 ms | 0.1 ms | 0.1 ms | 0.2 ms | — |
| **danluu.com** | 2.6 ms | 0.4 ms | 0.2 ms | **0.6 ms** | < 50 ms full pipeline |
| news.ycombinator.com | 1.8 ms | 0.4 ms | 0.1 ms | 0.5 ms | — |
| **en.wikipedia.org** | 2.2 ms | 12.7 ms | 1.9 ms | **14.6 ms** | < 250 ms full pipeline |

Both gated pages sit two orders of magnitude inside their budget — danluu at 1%
of 50 ms, Wikipedia at 6% of 250 ms. Layout is the cheaper of the two stages
everywhere; parse dominates, which is why parse is the one that runs on the
worker thread.

Layout also runs on every resize (M3.2), so Wikipedia's 1.9 ms is the cost of a
terminal drag frame. That is inside the 10 ms keypress budget with room to
spare, and no async layout is needed.

Wikipedia layout was 2.4 ms when M3.3 first landed. The M3.3 review pointed out
that the new run buffer copied every word into a fresh `String`; the buffer now
borrows `&str` straight from the arena's text nodes, which took it to 1.9 ms
(7 runs, median) — about 20% of the stage, for one lifetime parameter.

### M2 fast gate, re-measured on the same machine

`cargo bench --bench parse` — **13.0 ms** (13.0–13.1) on the Wikipedia fixture,
gate < 50 ms. Criterion reports no change against the M2.3 baseline (p = 0.78),
as expected: nothing in this milestone touches the parser.

---

## M4 — CSS: parser, cascade, inheritance (2026-07-27)

Machine A. All numbers from `cargo bench` unless marked otherwise. The
Wikipedia fixture carries its 21 inline `<style>` blocks — 291 selectors once
the user-agent sheet is added — and 13 399 elements.

### Fast gate: full restyle < 100 ms

`cargo bench --bench style`, `restyle en.wikipedia.org`: DOM + stylesheets →
one `ComputedStyle` per node, everything parsed outside the loop. This is
exactly what `App::restyle` runs when a sheet lands.

| Stage | Wikipedia fixture | Budget |
|---|---|---|
| **Full restyle** | **41.4 ms** (41.4–41.9) | < 100 ms |
| Source walk + parse of all 21 inline sheets | 0.5 ms (`--timing`-style, release) | — |
| Layout after the restyle | 1.9 ms (M3 number, unchanged) | — |

Inside the gate at 41% of it. The honest caveat: those 291 selectors are the
page's *inline* blocks only. Its `load.php` sheets are not committed, and they
are far larger — see the scaling numbers below for what that does (nothing, to
the indexed path).

### Rule index vs the naive matcher

Every element matched both ways, same rules, same page (PLAN.md §4 asks to
"feel the difference"). The naive side runs 10 samples rather than 100 because
it is slow by construction.

| Selectors in play | Rule index | Every rule | Ratio |
|---|---|---|---|
| 291 (the page's own) | **39.9 ms** | 63.5 ms | 1.6× |
| 2 291 (+2 000 rules that match nothing here) | **40.0 ms** | 375.0 ms | **9.4×** |

The first row is the surprise, and it is worth understanding before trusting
the second. The index proposes **6.1 candidates per element** out of 291 — a
48× cut in candidates — yet only buys 1.6× in time. The reason: the candidates
it discards are exactly the ones the naive matcher also discards cheaply, on a
one-comparison tag or class test. What survives in both paths is the expensive
part — walking ancestors for a descendant combinator, over an average element
depth of 20.6.

The second row is the one that matters for real pages. Rules that cannot match
this document cost the index *nothing* (they sit in buckets nobody looks up)
and cost the naive matcher a comparison per element: 2 000 extra rules leave
the index at 40.0 ms and take the naive matcher from 63.5 ms to 375 ms. Real
stylesheets are mostly rules that do not apply to the page in front of you,
which is the case the index is for.

(Duplicating the page's own sheets 10× is the wrong way to test this and was
tried first: it multiplies the rules that *do* match, so both curves grow
together — index 391 ms, naive 601 ms, both ~10× their baseline. The bench
comment records this so nobody repeats it.)

### An optimization that did not pay

`cascade` allocated a `Vec` per element for candidate slots and another for
matched candidates — the same allocation-per-item shape that cost 20% of M3's
layout (M3.3's run buffer). Replacing both with buffers reused across the walk
measured **+0.9% (p = 0.15)**: nothing, inside noise. Reverted rather than
kept, because the profile above says why it could not help — the time is in the
ancestor walk, not in the allocator.

What would actually move this number is an ancestor filter (bloom filter over
each element's ancestor tags/classes/ids, rejecting descendant selectors
without walking). PLAN.md §4 files that under "later, only if measured".
It is now measured; it is still not needed, because 41.4 ms is inside the gate
and no user-facing path runs a full restyle today. It becomes urgent when M6
puts `:hover` on the restyle path against a 10 ms keypress budget — at which
point the right fix may be invalidation (restyle the hovered subtree) rather
than a faster full restyle.

### Scroll gate, re-run after the cascade landed

`cargo bench --bench scroll`, after M4.4 moved layout onto computed values:
**29.2 µs** at 120×40 and **45.9 µs** at 200×50, against M3's 28.5 µs and
44.4 µs. Criterion reports no change (p = 0.46, p = 0.28). Scrolling still
touches neither style nor layout — it repaints cached lines, and the cascade
landing did not sneak into that path.

---

## M5.0 — whitespace text nodes kept in the DOM (2026-07-27)

Machine A. The tree builder no longer drops whitespace-only text between block
tags (browsers keep it; layout collapses it), so every tree grows. Everything
this could plausibly cost, measured.

| Measure | Before (M4 tip) | After | Budget |
|---|---|---|---|
| Wikipedia arena | 24 484 nodes | **25 596** (+4.5%) | — |
| `bench parse` | 13.0 ms | **12.8 ms** (−2.0%) | < 50 ms |
| `bench style` restyle | 41.4 ms | **41.1 ms** (−1.4%) | < 100 ms |
| `bench scroll` 120×40 | 29.2 µs | **31.0 µs** (+5.7%) | < 5 ms |
| `bench scroll` 200×50 | 45.9 µs | **48.2 µs** (+4.4%) | < 5 ms |
| Peak RSS, Wikipedia render | — | **35.7 MB** | < 100 MB |

Parse and restyle did not get slower despite 1 112 more nodes, which says the
per-node cost of a text node is nearly nothing: the parser stopped doing a
`trim()`-and-lookup per whitespace run, and a text node's cascade is a copy of
its parent's inherited values.

The scroll step really did get ~5% slower, and the cause is not the node count.
Same page, same 3 073 laid-out lines, but **9 381 → 10 199 spans** (+8.7%): a
space between two inline elements is now its own span, so the painter makes
more `put_str` calls and the renderer's diff walks more style runs. Correct
output, slightly more of it. At 31 µs against a 5 ms gate this is 160× inside
budget and not worth chasing — but it is worth knowing that layout does not
merge adjacent spans that share a style, and M5 rewrites that code anyway.

Ladder `--dump-text` diff: example.com, motherfuckingwebsite.com, danluu.com
and news.ycombinator.com are byte-identical. Wikipedia goes 3 271 → 3 267 lines
and its navbox hlists become readable — `Anatomy Genetics Dwarf cat Kitten` in
place of `AnatomyGeneticsDwarf catKitten`, which is the whole reason for the
task.

Hacker News's header is still glued (`Hacker Newsnew | past`) and this was not
the cause: its source has no whitespace between those elements at all. The
separation comes from `.hnname { margin-right: 5px }` in news.css, and margins
arrive with M5's box model.

### M4 review addendum: the pipeline with `style` in it (2026-07-27)

The `F4`/`--timing` table was still M3's — fetch, parse, layout, frame — so the
most expensive UI-thread stage on a large page was invisible in the product's
own instrument while being the one thing M4 added. With the `style` row in:

`yata --timing en.wikipedia.org` (fixture over loopback):

| fetch | parse | **style** | layout | frame | total |
|---|---|---|---|---|---|
| 11.8 ms | 23.9 ms | **42.9 ms** | 1.8 ms | 0.0 ms | **80.4 ms** |

Against PLAN.md §4's < 250 ms full-pipeline budget for a large Wikipedia
article, with style now counted: 32% of it. Style is the largest engine stage
on this page — larger than parse — which is the number that would have gone
unnoticed for another milestone without the row.
