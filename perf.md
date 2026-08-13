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

---

## M5 — box model + real layout (2026-07-31)

Layout is now a box tree with margin/padding/border/width/max-width, inline
line boxes, and a display list. Machine A, criterion `--sample-size 20`.

| Measure | Result | Budget |
|---|---|---|
| `layout en.wikipedia.org 80-col` | **3.82 ms** | < 100 ms |
| full pipeline danluu.com (parse+style+layout+paint) | **0.55 ms** | < 50 ms |
| scroll step (existing bench, unchanged path) | still ≪ 5 ms | < 5 ms |

Both M5 fast gates land with headroom: Wikipedia layout is ~26× inside budget,
danluu's full pipeline ~90× inside. The box tree is denser than M3's line list
but the work is still dominated by the cascade on large pages, not geometry.

HN header spacing: `.hnname { margin-right: 5px }` resolves to 1 cell
(nonzero → ≥1). M5.2 initially only resolved the length on the cascade; the
review-fix pass applies inline horizontal margins in the IFC, so
`Hacker Newsnew` becomes `Hacker News new`.

---

## M6 — interaction (2026-07-31)

Mouse hit-testing, link hints, Tab focus, history with scroll restore,
`:visited`, and `:hover` restyle-without-relayout. Machine A.

Hover is one full `style_tree` + display-list rebuild over the existing box
tree — the same restyle cost as M4, not an incremental pass. PLAN.md §4 lists
incremental restyle under "later, only if measured"; today the M4 restyle
bench (~41 ms Wikipedia) is the ceiling for a hover on a large page. Geometry
does not re-run: the `layouts` counter stays flat across a hover transition
(unit test). Hint overlay construction walks the cached layout tree only —
no restyle, no relayout.

| Measure | Result | Budget |
|---|---|---|
| hover path | restyle + recolour + paint; **0** relayouts | keypress &lt; 10 ms target; full restyle is the cost |
| hint list | layout-tree walk of visible links only | &lt; 10 ms overlay after `f` (no cascade) |
| scroll step | unchanged path | ≪ 5 ms |

Demo path: open HN, `f` + label to follow a thread, `H` back with scroll
restored; move the pointer over a link and watch `a:hover` recolour without
a layout row blip on `F4`.

---

## M7 — scrolling & polish (2026-07-31)

Resize anchoring, synthetic error pages (DNS/TLS/HTTP/content-type), in-page
search (`/` `n`/`N`), and `?` help generated from the keybinding table.
Machine A, criterion `--sample-size 20–30`.

| Measure | Result | Budget |
|---|---|---|
| scroll step Wikipedia 120×40 | **47.4 µs** (45.2–49.5) | &lt; 5 ms |
| scroll step Wikipedia 200×50 | **67.7 µs** (63.1–71.9) | &lt; 5 ms |
| layout Wikipedia 80-col | **3.87 ms** | &lt; 100 ms |
| full pipeline danluu.com | **0.57 ms** | &lt; 50 ms |

All gates hold with large headroom (scroll ~100× inside budget). Absolute
scroll cost has drifted up from M3's ~28 µs as paint and App draw gained
overlays (focus, hints, search highlight check); the path still never
restyles or relayouts, and unit tests pin `layouts` flat across search/`n`.

Search highlight is paint-time reverse-video over match rectangles from a
layout-tree walk — no restyle. Resize records the top visible node before
relayout and restores its `y` afterward (UX §3.6). Error pages replace the
viewport body; `--dump` still prints any HTTP body (curl semantics).

Demo path: open a page, `?` to learn bindings, `/alpha` + Enter, `n`/`N` to
walk hits; kill the network and confirm `r` retries from the error page;
resize mid-article and stay on the same paragraph.

---

## M8 — images (2026-08-01)

Async `<img>` fetch/decode (`image` crate), replaced boxes, Unicode half-block
paint, optional Kitty graphics, and a memory-capped RGBA LRU. Machine A.

**Fast gate:** images must not enter the scroll path. Half-block rasters are
baked into the display list when an image lands (or as a placeholder
checkerboard); scrolling re-emits that list at a new offset with **0**
relayouts (unit test: `scroll_with_images_does_not_relayout`).

| Measure | Result | Budget |
|---|---|---|
| scroll with image boxes present | same path as text; layouts counter flat | ≪ 5 ms scroll step |
| firm `width`/`height` + late decode | repaint only (0 relayout) | — |
| soft size + late decode | one relayout when pixels arrive | — |
| LRU default cap | 32 MiB RGBA; back/forward hits cache | — |

Kitty is a post-`present` side channel (delete-all + place visible), same
discipline as OSC 52 yank. Half-blocks always paint into the cell buffer so
non-Kitty terminals stay correct.

Demo path: open a page with images (or inject via local fixture server);
placeholders appear immediately; pixels pop in without freezing scroll;
`j`/`k` stay instant; on Kitty, true pixels overlay the same rects.


---

## M9 — flexbox (2026-08-04)

Block sizing (`height`, min/max clamps, `box-sizing`), `overflow` clipping,
intrinsic sizing, the whole of flexbox, and atomic inlines. Machine A.

**Method.** Every row is a **before/after pair measured on the same day, on the
same machine, alternating** — `AFTER, BEFORE, AFTER, BEFORE` — because this
machine drifts 5–10% between runs and a single before-then-after comparison
would report the drift as the change. "Before" is the M9 parent commit
(`6dd6b73`, M8 tip) built from a worktree, not the numbers in the M8 section
above: those were taken on another day and are not comparable at this
resolution. Criterion `--sample-size 20 --measurement-time 5`; two rounds each,
both reported.

### The gates

| Measure | Before (M8 tip) | After (M9) | Budget |
|---|---|---|---|
| full pipeline danluu.com (`--timing` total, median of 7) | 2.9 ms | **3.2 ms** | < 50 ms |
| full pipeline en.wikipedia.org (`--timing` total, median of 7) | 64.6 ms | **66.0 ms** | < 250 ms |
| `bench layout` en.wikipedia.org 80-col | 4.25 / 4.33 ms | **5.29 / 5.58 ms** | < 100 ms |
| `bench layout` full pipeline danluu.com | 583 / 584 µs | **772 / 783 µs** | < 50 ms |
| `bench layout` flex deck 80-col (new) | 526 / 524 µs | **1.208 / 1.219 ms** | — |
| `bench scroll` 120×40 | 53.9 / 54.1 µs | **52.3 / 52.3 µs** | < 5 ms |
| `bench scroll` 200×50 | 76.0 / 75.7 µs | **74.0 / 74.1 µs** | < 5 ms |
| Peak RSS, Wikipedia (`--dump-boxes`, full layout) | 42.5 MB | **45.3 MB** | < 100 MB |

Both gated pages stay two orders of magnitude inside budget: danluu at 6% of
50 ms, Wikipedia at 26% of 250 ms — and Wikipedia's total is still 67% *style*,
a stage M9 never touched.

### Where the time went

Layout got slower everywhere, by three different amounts, and each has a
different cause.

**Wikipedia layout, +24% (4.3 → 5.4 ms).** This page is not a flex page — it
has two flex containers in the taxobox and a portal box. The cost is the work
M9.2 and M9.3 added to *every* box, flex or not: resolving `height`, the four
clamps and `box-sizing` on each block, and carrying a clip rectangle down the
walk. It is a per-box tax, and the page has 13 399 elements.

**danluu pipeline, +33% (583 → 776 µs).** All of it is `li { display: flex }`
on 196 list items: each one now generates flex items and measures its subtree's
intrinsic widths before laying it out. M9.6 already logged this arriving
(620 → 726 µs on that day's machine) and it has not grown since; the rest of M9
added nothing measurable to this page.

**Flex deck, +130% (525 µs → 1.21 ms).** This is the new bench, and the honest
caveat is that the two sides are not doing the same work: before M9, the same
DOM laid out as plain blocks, ignoring every `display: flex` on it. So the
number is not a regression — it is the price of the feature, on a page (300
flex columns inside a wrapping flex row, a nested header row each) built to be
nothing but flex. Roughly 2.3× block layout for the same tree is what
generating items, measuring each one's min/max-content width, collecting lines
and distributing free space costs.

Items are laid out **once**: measurement allocates no boxes, so there is no
measure-then-relayout pass. The one place a box is moved after being built is
M9.8's cross-axis shift, which moves a finished subtree rather than rebuilding
it.

### Scrolling still never relayouts

The scroll step came out *faster* on both frame sizes (−3%, inside this
machine's drift — read it as unchanged). It could hardly be otherwise: the
scroll path is cached display list → repaint at a new offset, and M9 changed
what goes into the list, not what happens to it afterwards. Pinned by tests
rather than by the bench alone — the `layouts` counter stays flat across
scroll, search, `n`/`N`, and hover, including hover on a link inside a flex
item (`flex_interaction::hover_inside_a_flex_item_restyles_without_relayout`).

Keypress→screen is the same story: the only keypress that relayouts is a
resize, whose cost is the layout row above (5.4 ms on Wikipedia, inside the
10 ms budget), and `flex_interaction::resize_keeps_a_flex_page_anchored`
pins that a flex page still relayouts exactly once for it.

### Memory

Peak RSS on the Wikipedia fixture went 42.5 → 45.3 MB, 45% of the 100 MB
budget.

The 2.8 MB is **not attributed** — it was measured, not explained, and the two
candidates were not separated: a denser layout tree (flex containers carry line
structure, M9.11's line boxes carry a row count) and `IntrinsicSizer`'s memo,
a `HashMap<NodeId, Sizes>` that can hold an entry per measured node and is live
for the whole of `layout_document`. Both are transient — the memo is dropped
with the `Engine` and the tree is replaced on the next layout — so this is peak
*during* a layout, not a leak, and M9 adds nothing that outlives a navigation.
Worth separating if the number ever approaches the budget; at 45% it is not.

## M10.6 — invalidation: what a script's tick costs (2026-08-13)

Release build, `en.wikipedia.org` (25,599 nodes) and `danluu.com` (1,014), mean
of 5 **interleaved** rounds — this machine drifts several percent between runs
of the same thing, so each round measures every case rather than measuring one
case five times and then the next.

    cargo test --release --lib measure_the_invalidation -- --ignored --nocapture --test-threads=1

The two measurements are `#[ignore]`d: they print numbers and assert nothing,
and running them in the debug default loop made `cargo test` ten times slower.

### One turn of the JS path, end to end

A turn is everything a click will cost once M10.8 dispatches one: the tick, the
invalidation it triggers, the draw, and the renderer's diff + present into a
sink. Dispatch itself is the one piece missing, and it is M10.8's to add.

| turn                              | danluu   | Wikipedia |
| --------------------------------- | -------- | --------- |
| tick that changes nothing         | 0.51 ms  |  9.8 ms   |
| attribute write, paint only       | 0.62 ms  | 45.9 ms   |
| attribute write, relayouting      | 0.62 ms  | 52.4 ms   |

### The stages behind those numbers (Wikipedia)

| stage                  | cost     |
| ---------------------- | -------- |
| restyle                | 43.4 ms  |
| one layout             |  5.2 ms  |
| `Styles::layout_eq`    |  0.48 ms |

### What the narrowing buys, and what it cannot

An attribute write that only changes paint skips layout: **52.4 → 45.9 ms**, a
6.5 ms saving that matches the 5.2 ms layout plus the paint it would have
dragged behind it. The comparison that decides costs 0.48 ms against the 5.2 ms
it avoids — an **11× crossover**, so it pays for itself whenever it succeeds and
costs 9% of a layout when it does not. That is the whole case for keeping it.

**The keypress→screen budget (PLAN.md §4: 10 ms) is met on an ordinary page and
missed badly on Wikipedia.** danluu's worst turn is 0.62 ms, 6% of the budget.
Wikipedia's is 52 ms, 5× over — and the narrowing cannot fix that, because
**43 of those 52 ms are restyle**, which every attribute write pays before
anything can decide whether layout is needed. Layout is 5 ms; the thing that
misses the budget is the cascade running over 25,599 nodes to answer a question
about one element.

The fix is not in this task and is deliberately not attempted here: it is
scoped restyle — recomputing only the subtree an attribute write can affect,
which needs the per-selector dependency tracking M10.6 explicitly rules out as
the wrong size for this milestone. Recorded here so the number is on the table
when M11 picks it up.

### Counters, not appearances

The four invariants are pinned by the `styles_run` / `layouts` / `paints`
counters rather than by what is on screen, because a stage that ran when it
should not have is invisible on screen and ruinous in a profile: a tick that
mutates nothing runs no stage at all; scrolling a script-built page runs none;
hover over script-built content restyles and repaints but never relayouts; a
resize relayouts exactly once and runs no script pass.

## M10.13 — hostile pages (2026-08-13)

Every case ran to completion with no panic, no abort, and a page that could
still be styled, laid out and scrolled afterwards. Times are the whole case
including the page load, `dev` build (the numbers a `cargo test` run prints).

| What was tried | What happened | Time |
| --- | --- | --- |
| `while (true) {}` in a script | interrupted, error in console | 102 ms |
| the same in a `click` listener | interrupted, page intact | 102 ms |
| the same in a timer callback | interrupted, page intact | 102 ms |
| unbounded recursion | `RangeError`, host reusable | 1.9 ms |
| allocation bomb | out-of-memory error, host reusable | 29 ms |
| a timer scheduling two more, 8 ticks | each tick its own budget | 2.1 ms |
| a promise that re-queues itself | stopped at the 10,000-job pump bound | 25 ms |
| 100,000 `appendChild` | interrupted mid-loop, tree consistent | 113 ms |
| 1 MB `innerHTML` | parsed and adopted | 175 ms |
| 10,000-deep nesting (`appendChild`) | refused past depth 128, script caught it | 2.6 ms |
| 10,000-deep nesting (`innerHTML`) | flattened at depth 128 | 32 ms |
| `document.body.remove()` | page lays out empty | 1.9 ms |
| a listener removing its own node | snapshot semantics hold | 2.0 ms |
| `location.href` ×10,000 | one navigation, last wins | 11 ms |
| `location.href` in dispatch / timer | one navigation each | 2.6 / 1.8 ms |
| `fetch()` ×10,000 | 32 requests, 9,968 refusals | — |
| `console.log(window)`, circular JSON | cycle guard, no hang | 2.3 ms |
| `document.write` | ignored with a console line | — |
| 100,000 `console.log` | ring buffer held at 500 | — |
| 100,000 `localStorage.setItem` | `QuotaExceededError` | — |

### The one that aborted the process

**Deep nesting.** Style and layout both recurse over the tree, so a deep
enough subtree overflows the native stack — a process abort, not an error, and
nothing a `catch` can reach. Measured on a 2 MB test thread: style survives
past 2,000 levels; **layout dies between 200 and 300.** Layout is the binding
constraint by an order of magnitude.

The fix is a rule in the arena rather than a guard at the script boundary,
because a hostile *server* can do the same thing with markup: `Dom::MAX_DEPTH`
is 128, refused with an exception for a script and flattened onto the deepest
permitted ancestor for the parser. Flattening rather than dropping, because
the alternative discards a page's text along with its structure.

Ladder depths for scale: example.com 7, motherfuckingwebsite 6, danluu 7,
Hacker News 15, **Wikipedia 62**. A page must be twice as deeply nested as the
deepest page in the suite before the cap is reachable.

### `q` still quits: the M10.1 rule holds

Every runaway shape costs **one budget**, ~102 ms, and every one of them is a
separate tick: a listener, a timer callback and a script are interrupted on
the same terms because they share one `Host::under_budget`. Nothing chains
inside a tick — a timer that schedules two more does not run them, it queues
two messages, and the loop serves input between messages. So worst-case
keypress→screen under adversarial JS is one budget plus the frame, and quit
latency is the same. M10.1's rule stands unchanged and needs no escape hatch.

**A page can still hold ~100% of one core forever** by scheduling a 4 ms
interval whose callback burns its whole budget. That is accepted, deliberately:
the reader can still scroll, still quit, and still see the page, because the
loop interleaves input between ticks. A per-page duty-cycle budget would be
the fix if a real page ever makes this unbearable; nothing on the ladder does.

### Memory over 100 navigations

Alternating a script-heavy page (300 nodes, 300 listeners, 50 console lines, a
pending timer, a storage write) with a light one, release build:

    after   0 navigations   9.9 MB
    after  20              10.6 MB
    after  40              10.7 MB
    after  60              10.8 MB
    after  80              10.8 MB
    after 100              10.8 MB

**Flat.** It plateaus after roughly 40 and does not climb again, so nothing
per-page is leaking: the arena, the host, its listeners, its timers and the
console all go with the page. What remains is the allocator's high-water mark
plus per-origin storage, which is quota-bounded.

## M10 — JavaScript (2026-08-13)

Release build, fixtures served from `127.0.0.1`, measurements **interleaved** —
this machine drifts 5–10% between runs, so each round measures every page
rather than one page repeatedly.

### The full pipeline, now with the script pass in it

Mean of 3 interleaved rounds, ignoring the first `fetch` (cold connection):

| stage | danluu.com | en.wikipedia.org |
| --- | --- | --- |
| fetch | 2.0 ms | 2.6 ms |
| parse | 0.3 ms | 13.0 ms |
| style | 0.4 ms | 42.9 ms |
| layout | 0.6 ms | 6.1 ms |
| **script** | **0.0 ms** | **1.1 ms** |
| frame | 0.0 ms | 0.0 ms |
| **total** | **3.3 ms** | **65.7 ms** |

**Both budgets hold** (PLAN.md §4: danluu < 50 ms, a Wikipedia article
< 250 ms) with room to spare, and the stage M10 added is the *cheapest* one on
both pages. Wikipedia's script row is 1.1 ms for three inline scripts, two of
which run and one of which throws on `document.cookie`; the page's cost is
still style, at 65% of the total.

### `benches/js.rs` — what M10 added, isolated

The pipeline benches gained the script pass inside their totals; this one
measures the four costs that did not exist before (criterion, release):

| what | time |
| --- | --- |
| engine startup | **794 µs** |
| a document-order pass building 200 nodes | 1.25 ms |
| `querySelectorAll('a')` on Wikipedia (incl. parse) | 21.4 ms |
| click → mutate → restyle → relayout | **99 µs** |

Two of these are worth stating plainly. **Starting an engine costs 0.8 ms**,
which is why "a page with no script never starts one" is a rule rather than an
optimisation — it is most of what a script-free page would otherwise pay for
JavaScript existing. And a **whole click round trip on a small page is 99 µs**,
1% of the keypress budget: interaction over script-built content is cheap, and
what makes Wikipedia expensive is the restyle, not the JavaScript.

### Keypress→screen, and what M10 did to it

From the M10.6 and M10.9 sections above, unchanged by later tasks:

- an ordinary page's worst turn is **0.62 ms**, 6% of the 10 ms budget;
- a keystroke arriving *between* ticks is answered in **~20 µs**, because
  scrolling touches no pipeline stage;
- a keystroke arriving *during* a tick waits for it — one budget at worst,
  ~102 ms, which M10.13 confirmed for every runaway shape;
- Wikipedia's own worst turn is 52 ms and **misses the budget**, of which
  43 ms is restyle. M10 did not cause that and cannot fix it; scoped restyle
  (M11) can.

### Scrolling still never restyles or relayouts

Unchanged, and now pinned on script-built content as well as parsed content:
`scrolling_a_script_built_page_runs_no_stage` scrolls 50 steps through a
200-element list built entirely by JavaScript and asserts all three stage
counters are flat.

### Memory

Wikipedia's peak RSS is unchanged by M10 at **45 MB**, 45% of the 100 MB
budget — the engine is not started at all for a page whose scripts do nothing,
and the article's three scripts allocate almost nothing.

Over a session, alternating a script-heavy page with a light one (M10.13's
measurement): 9.9 → 10.8 MB across 100 navigations, **flat after about 40**.
Nothing per-page leaks.

### Idle CPU: not confirmed

The gate asks for 0% with a script-heavy page loaded, and again with a
ten-second timer pending. **Neither has been measured**, and the number is not
claimed. Running the TUI in this environment is not possible — under
`script(1)` the binary writes 59 bytes of terminal setup and exits, because
stdin is not a terminal and the input thread's death quits the app.

What *is* established, in-process: the timer thread parks on a condvar and
produces no message while a ten-second deadline is outstanding
(`a_pending_timer_leaves_the_thread_parked`), `n` scheduled timers produce
exactly `n` messages, and the event loop's only wait remains a blocking
`recv`. The measurement someone with a terminal should make is two readings of
`top` — one with a script-heavy page idle, one with a long timer pending.
