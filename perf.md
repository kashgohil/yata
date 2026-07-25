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
