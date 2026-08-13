# DEVIATIONS.md — where this engine knowingly differs from a browser

Every entry is a **choice**, not a bug: something the engine does differently
from a browser on purpose, usually because a terminal is not a canvas. Each one
says what we do, what a browser does, why, and **the trigger** — the observable
condition that would make us change it. "When we get around to it" is not a
trigger; a page rendering wrong is.

Not in this file: things nobody has implemented *yet* in the ordinary sense of
a roadmap (PLAN.md owns those), and things a browser also does. Both were
proposed for it during M9 and both were rejected — see the end.

---

## Units

### 8px ≈ 1 cell, 16px ≈ 1 line

**We do:** resolve every CSS length to whole terminal cells — `px / 8` across,
`px / 16` down, `1em` = 2 cells × 1 line, flat. A nonzero length never rounds
away to nothing: it floors at one cell, so a 1px border and a 5px spacer are
both one cell.
**A browser does:** sub-pixel geometry at the page's real scale, with `em`
following `font-size`.
**Why:** a cell is the smallest thing a terminal can address. There is no half
cell to round into, and a spacer that rounds to zero deletes the separation the
page asked for — Hacker News's 30 `height: 5px` rows between stories are the
case that decided the floor.
**Trigger:** a ladder page whose layout is wrong *because* of the quantum
rather than merely coarse — a design whose columns only add up at sub-cell
precision. The fix then is fractional accumulation with rounding at paint, not
a different constant.

### `font-size` does not exist

**We do:** ignore `font-size` entirely. `em` is a constant.
**A browser does:** resolve `em`, `rem` and `%` font sizes against an inherited
size, which changes the meaning of every `em` length on the page.
**Why:** one cell is one character; there is no smaller or larger glyph to
render, so a font scale has nothing to act on except the lengths written in
`em`.
**Trigger:** a page that sets `font-size` on a container and sizes its boxes in
`em`, where the two disagree enough to overlap or clip. The fix is to carry a
scale factor through the cascade and apply it in `Length::Em`, not to render
different type.

---

## Block layout and sizing (M9.2)

### No parent–child margin collapsing

**We do:** collapse margins between *adjacent siblings* only.
**A browser does:** also collapse a parent's top margin with its first child's,
and its bottom with its last child's, when no border, padding or line separates
them.
**Why:** the sibling case is the one that keeps prose readable, and it was the
one M5 needed. The parent–child case never blocked a ladder page.
**Trigger:** a page with visibly doubled gaps at the top or bottom of a
container — the signature of an uncollapsed parent margin.

### `height` is a used height, not a minimum

**We do:** take `height: <length>` as *the* height. The flow advances by it
whatever the content did, so content taller than the box paints over what
follows (the initial `overflow: visible` does not clip).
**A browser does:** the same for a block box — but treats `height` as a
*minimum* on table rows and cells, which grow to fit.
**Why:** CSS 2.1 §10.5 for the block case, which is what this is. The table
case is a divergence we inherit from blockifying tables (below): a page's
`display: table-row` becomes a block and its `height` stops being a floor.
**Trigger:** Wikipedia's portal box is the live instance — `.portalbox-entry`
sets `height: 1.9em` on a `display: table-row`, and its content overlaps the
row below. Any ladder page where a table-shaped layout overlaps itself is this
entry, and the fix is to make `height` a minimum for blockified table parts.

### Percentage heights against an unbounded column resolve to `auto`

**We do:** resolve a percentage height to `auto` when the containing block has
no definite height — which the page column always is, being as tall as its
content. The same rule applies to a percentage main size inside an auto-height
flex column.
**A browser does:** the same for an indefinite containing block. The difference
is how often it happens here: a browser's viewport gives `html`/`body` a
definite height and ours does not, so `height: 100%` chains that work in a
browser resolve to `auto` here.
**Why:** the page scrolls; there is no viewport height for a percentage to be a
percentage of.
**Trigger:** a page whose full-height layout (`height: 100%` from `html` down)
collapses to content height and loses a background or a sticky footer.

---

## Overflow (M9.3)

### `auto` and `scroll` clip; there are no inner scrollers

**We do:** clip for every `overflow` value except `visible`.
**A browser does:** give the box its own scrollbar and let the reader reach the
rest.
**Why:** a terminal has one scroll position, and it belongs to the page. Inner
scrollers are PLAN.md M11+.
**Trigger:** a ladder page whose main content sits inside an `overflow: auto`
box, where clipping loses the article rather than a decoration.

### A one-axis clip does not promote the other axis

**We do:** clip exactly the axis that asked for it. `overflow-x: hidden` with
`overflow-y: visible` leaves the vertical axis genuinely unclipped.
**A browser does:** compute the `visible` axis to `auto` and give it a
scrollbar, so both axes end up clipped-with-scroll.
**Why:** the promotion exists to make room for a scrollbar. We have no
scrollbars, and promoting would clip content for a reason that does not apply.
**Trigger:** a page that relies on the promotion to hide vertical overflow
after setting `overflow-x: hidden` — content spilling where a browser cut it.

### A cut border closes along the clip edge

**We do:** emit a border cut by an ancestor's clip as the intersected
rectangle, so it draws a closed box along the clip edge.
**A browser does:** paint the border where it is and let the clip remove the
part outside, leaving the box visibly open on the cut side.
**Why:** the display list carries rectangles, and box-drawing characters are
chosen per rectangle; an open-sided box needs the renderer to know which sides
were cut.
**Trigger:** a page where the closed edge reads as a real border — a card that
looks complete when it is actually cut off, hiding that there is more.

---

## The viewport (M9.12)

### A flex line wider than the terminal is culled at the column edge

**We do:** lay the line out at its real width — the boxes past the edge exist,
with their real `x` — and cull at paint. There is no horizontal scroll and no
sideways scrollbar; the reader reaches the content by widening the terminal,
which relayouts.
**A browser does:** scroll horizontally.
**Why:** nothing is dropped in *layout*, which would make the geometry a lie
and break hit-testing and search for boxes that exist. It is dropped at the
frame, where the terminal really does end. Shrinking items past their
`flex-shrink: 0` to make the line fit was the alternative and is worse: a page
that says "this column is exactly 320px" would render at some other width and
look correct.
**Trigger:** a ladder page whose primary content lands past the column edge at
80 cells. Horizontal scrolling is the fix, and it is a viewport feature, not a
layout one.

---

## Intrinsic sizing (M9.4)

### A flex container's intrinsic width does not scale item contributions

**We do:** build a flex container's min/max-content width out of its items'
*own* intrinsic sizes — a nowrap row sums them and adds its gaps, a wrapping
row's min-content is its largest single item, a column takes the largest item
on both sizes.
**A browser does:** css-flexbox-1 §9.9 sizes a container from its items'
*contributions*, which scale each item by its flex fraction. That can report a
smaller max-content width for a container whose items can grow.
**Why:** the unscaled version is exact at the width that matters — at a
container width equal to either of these numbers, §9.7 has no free space to
distribute and hands every item exactly the size that went into the answer.
The scaling only changes the answer in between.
**Trigger:** a flex container nested inside something that sizes to content
(another flex item, a shrink-to-fit inline-block) coming out visibly too wide.

---

## Flex (M9.6–M9.10)

### Overflow alignment is *safe* on both axes

**We do:** pack an overflowing line at main-start, and an item taller than its
line at cross-start.
**A browser does:** `justify-content: center`/`space-around`/`space-evenly`
fall back to *unsafe* `center` (css-align-3 §9.3), so an overflowing row hangs
off both ends.
**Why:** a browser's start-edge overflow is recoverable — the reader scrolls
left to it. A terminal has no negative column: the first item would not be
clipped, it would be gone.
**Trigger:** horizontal scrolling arriving (see the viewport entry). It makes
the unsafe fallback recoverable, at which point it is just correct.

### A replaced box does not stretch

**We do:** leave an `<img>` item at its own cross size under
`align-items: stretch`.
**A browser does:** stretch the box; the image inside keeps its aspect ratio.
**Why:** the box *is* the picture here — growing it would rescale the raster to
fill the row.
**Trigger:** a replaced item that needs a background or border filling the
line's cross size, which needs the box and the raster to stop being the same
thing.

### A stretched nested flex container does not re-lay-out

**We do:** grow a nested flex container's box when its parent stretches it, and
leave its own items at the heights they were built with.
**A browser does:** the new definite cross size is the child container's main
or cross size, and its items fill it.
**Why:** it needs a second layout pass over the subtree, which M9.8's task
explicitly forbade and M9.9's definite-size plumbing did not extend to.
**Trigger:** nested flex on a ladder page where the inner row's items are
visibly short of the outer row's height.

### `align-items: baseline` in a column degrades to `flex-start`

**We do:** treat `baseline` as `flex-start` when the container is a column.
**A browser does:** align the items' first baselines — which in a column means
along the horizontal cross axis.
**Why:** a baseline is a *row* in a cell grid. A column's cross axis is the
horizontal one, so there is no shared row to stitch items to.
**Trigger:** none that a cell grid admits. This entry exists so a reader who
finds the value doing nothing finds the decision instead of a bug.

---

## Inline layout (M8.2 / M9.11)

### Images flush the line; `inline-block` flows inside it

**We do:** give an inline `<img>` rows of its own, breaking the line around it,
while an `inline-block` sits inside the line beside its text.
**A browser does:** flow both inside the line as atomic inlines.
**Why:** M8.2 predates the atomic-inline path M9.11 built, and moving images
onto it needs a `Piece` kind carrying the image box, an `emit_image` that
places rather than flushes, and a decision about a replaced box's baseline
(§10.8.1 gives it the bottom margin edge, which `atomic_rows` already
synthesises).
**Trigger:** a page whose small inline images — icons, badges, flags — each
take a row of their own and shred the paragraph. The goldens that would move
are `img-box` and `flex-replaced`.

### `vertical-align` is unimplemented

**We do:** align every atomic inline on the baseline.
**A browser does:** honour `top`, `middle`, `bottom`, `text-top`, lengths, …
**Why:** the property was never parsed; the baseline case is the one every
ladder page needs.
**Trigger:** a page whose icons or badges sit visibly wrong against their text.

### Shrink-to-fit measures the rest of the line, not the containing block

**We do:** size an `inline-block` against what is left of the line it lands on,
so it wraps inside itself rather than moving down whole.
**A browser does:** size it against the containing block, the same width
wherever it lands.
**Why:** on an 80-cell terminal the alternative wastes the tail of every line
that a badge does not fit into.
**Trigger:** the same box rendering at two different widths at two column
widths, where a page depends on the width being stable. The paragraph that
would move is named in `spec/inline-block.boxes`.

### An atomic inline's baseline ignores its `overflow`

**We do:** use an atomic inline's last line box as its baseline whatever its
`overflow` is.
**A browser does:** CSS 2.1 gives an atomic inline with non-`visible` overflow
its bottom margin edge instead.
**Why:** the last-line-box rule is the one that makes a badge sit on the
sentence's rule; the overflow exception did not come up.
**Trigger:** a clipped inline-block sitting visibly too high against its text.

### An inline-level `<hr>` inside an inline element loses its rule

**We do:** build an empty atomic box — the 1em UA margins a browser would give
it, and no rule.
**A browser does:** draw the rule.
**Why:** `<hr>` is the one element whose real layout lives in a function the
atomic path does not call.
**Trigger:** any page that does this on purpose. It is pinned by a test because
"no rule" and "no surrounding text" are one typo apart.

---

## JavaScript (M10)

### yata's keybindings always win: key events never reach the page

**We do:** dispatch `click` to the page, and nothing else. `keydown`, `keyup`
and `keypress` are not bound, so a page cannot see — or capture — `j`, `f`,
`/`, `q` or any other key. Every keystroke belongs to the browser.
**A browser does:** deliver every key to the focused element first, and let a
page call `preventDefault()` on it. Gmail's `j`/`k` and every "press `/` to
search" box work that way.
**Why:** UX §3.3 makes this keyboard-first, and CLAUDE.md gives keybindings
exactly one source of truth — the table `keys::BINDINGS` holds and the `?`
overlay is generated from. A page that could take `q` could trap the reader in
it. Between page fidelity and a reader who can always leave, the reader wins,
and this is the one place in M10 where we do not even try to match a browser.
**Trigger:** a ladder page whose *only* interaction is a keyboard handler —
where a reader cannot do the thing the page exists for without keys reaching
it. The fix then is not "dispatch everything": it is a narrow, explicit
allowance (keys yata has no binding for, or a mode the reader opts into),
because the invariant being protected is that the reader can always get out.

### `load` does not wait for images

**We do:** fire `DOMContentLoaded` and then `load` at the end of the
document-order script pass, before any image has been fetched or decoded.
**A browser does:** fire `load` only once every subresource — images included
— has finished, which is why pages measure images inside it.
**Why:** images arrive on worker threads as their own messages (M8), long
after the pass, and holding `load` for them would mean holding it for a fetch
that may never complete. A page's `load` handler running early is a smaller
lie than one that never runs.
**Trigger:** a ladder page whose `load` handler measures an image — reads
`naturalWidth`, or sizes something against a photo — and lays itself out wrong
because the bytes were not there yet.

### A listener keeps its node alive, and its node is never freed

**We do:** keep every registration for the life of the page. `remove()`
detaches a node without freeing it (ids are never reused, M10.3), and the
listener registry holds the callback and the target, so a page that creates
and drops listener-bearing nodes grows until it navigates. Measured: **~940 bytes
per listener-bearing node** (500 of them add 0.47 MB of JS heap). The
execution budget caps one tick at roughly 3,400 such nodes, so a single script
cannot reach 10⁵ — but M10.9's timers will let a page get there across many.
**A browser does:** collect a detached node and its listeners once the page
drops its last reference.
**Why:** the arena's ids are stable by design, which is what makes a handle a
page holds mean the same node forever; freeing nodes would require the
indirection that guarantee exists to avoid.
**Trigger:** a page at steady state — not growing its own DOM — whose memory
climbs past the PLAN.md §4 budget of 100 MB. At ~1 KB each that is about 10⁵
retained nodes, which needs roughly 30 ticks of a script running flat out.

---

## Not implemented at all

These are absences rather than choices, listed because "what hole is going to
bite us later" is the question this file answers.

| Missing | What happens instead | Trigger |
|---|---|---|
| `position: absolute` / `fixed` / `sticky` | the box stays in flow | a ladder page whose nav or dialog lands in the middle of the article |
| floats (`float`, `clear`) | the box is a plain block | a page whose sidebar or pull-quote stacks instead of sitting beside the text |
| `grid` | blockifies; `inline-grid` folds into `inline-block` | a page whose main layout is grid and stacks into one column |
| tables | blockify — no column widths, no row/cell sizing | a data table whose columns do not line up |
| `white-space` | ignored; only the `<pre>` **tag** preserves whitespace | a page using `white-space: pre` or `nowrap` on anything else |
| `line-height` | one line box is one row | a page that reserves vertical space with `line-height` |
| writing modes, `direction` | LTR only | any RTL page |

---

## Considered for this file and rejected

Each was deposited as a deviation by an M9 commit message. None is one — a
register that claims a browser does something it does not is worse than a
register with a gap in it.

- **An `<li>` that is a flex container loses its marker.** A browser does the
  same: `display: flex` replaces `display: list-item`. This is why danluu.com's
  index lost 196 bullets in M9 and why that is correct.
- **Whitespace between flex items disappears.** A browser does the same:
  anonymous flex items containing only whitespace are not generated (§4). It is
  worth knowing, because markup written with newlines between items renders
  with no space between them unless a `gap` says so — but it is not a
  divergence.
- **An auto-height flex column does not wrap, whatever `flex-wrap` says.** A
  browser does the same: wrapping needs an edge to wrap at, and a column as
  tall as its own items has no main size to overflow. `height` or `max-height`
  puts the edge there, and then it wraps.
