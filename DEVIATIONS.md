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

### The tree may not nest deeper than 128

**We do:** refuse to nest past `dom::MAX_DEPTH` — a script's `appendChild`
throws, and the parser attaches the node to the deepest permitted ancestor
instead, flattening rather than discarding the content.
**A browser does:** nest as deep as a page likes; the limits are in the
thousands and are rarely reached.
**Why:** style and layout both recurse over the tree, so a deep enough subtree
overflows the native stack — a process abort, not an error, and nothing a page
or a `catch` can recover from. Measured, layout dies between 200 and 300 levels
on a 2 MB thread. The cap lives in the arena because that is the one place
every node enters the tree, from a hostile script and a hostile *server* alike.
For scale, the ladder's deepest page is Wikipedia at 62.
**Trigger:** a real page whose content is wrong because of the cap — nesting
past 128 that a reader would notice. The fix then is not a bigger number: it is
making the style and layout walks iterative, which removes the constraint
instead of moving it.

### The execution model

**We do:** finish parsing a page, style it, lay it out and **paint it**, and
only then run its scripts — as a separate turn of the event loop. A script
never blocks the parser, and can never delay first paint.
**A browser does:** stop its parser at every classic `<script>`, run it against
a half-built document, and let it `document.write` into the token stream.
**Why:** our parse finishes on a worker and arrives whole (M2), so there is no
open token stream to stop or write into — and UX §3.2 says the page appears as
soon as it is parsed, which a blocking script would break. `document.write` and
`writeln` exist only to say this in the console.
**Trigger:** a page whose first paint is visibly *wrong* rather than merely
early — a spinner or placeholder that a reader sees and reacts to before the
script replaces it.

### `defer` and `async` both mean "document order"

**We do:** run every script in document order, ignoring both attributes.
**A browser does:** treat `defer` as "after parsing, in order" and `async` as
"whenever it arrives, out of order".
**Why:** nothing blocks the parser here, so `defer` is already what everything
does. `async` is the only one that would change anything, and what it would
change is *execution order* — the one property M10.10's queue exists to
guarantee, for a benefit (parallel execution) a single-threaded engine cannot
take.
**Trigger:** a page that depends on an `async` script running before an earlier
one, which is a race a browser also loses.

### A script inserted by a script never runs

**We do:** build the execution queue once, from the parsed document.
`document.body.appendChild(scriptElement)` adds an element and nothing else —
no fetch, no execution.
**A browser does:** fetch and run it.
**Why:** the queue is what makes execution order independent of arrival order
(M10.10), and a script that can extend the queue while it drains reopens
exactly the ordering question the queue answers.
**Trigger:** a ladder page whose content arrives through a script it injects —
which is how a great deal of third-party JavaScript bootstraps itself.

### Scripts are not blocked on pending stylesheets

**We do:** run a script as soon as its slot is ready, whether or not the page's
`<link>` stylesheets have arrived.
**A browser does:** block a script that follows a stylesheet until that sheet
has loaded, because the script may read computed styles.
**Why:** nothing can observe the difference — `getComputedStyle` does not
exist here.
**Trigger:** implementing `getComputedStyle`, which would make the ordering
observable the day it lands.

### `type=module` is not run at all

**We do:** skip a module script and say so in the console.
**A browser does:** fetch it, resolve its imports, and run it deferred with its
own scope.
**Why:** modules need a loader and a resolver, which is a milestone rather than
a feature.
**Trigger:** a ladder page whose only script is a module.

## The DOM, as JavaScript sees it

### The arena never shrinks, and a removed node's handle stays valid forever

**We do:** keep every node for the life of the page. `remove()` detaches
without freeing, ids are never reused, and a handle a script kept on a removed
node still reads correctly. Measured: 100,000 append/remove pairs leave 100,002
nodes and ~11.1 MB; a listener-bearing node costs ~940 bytes.
**A browser does:** collect a detached node once the page drops its last
reference to it.
**Why:** stable ids are what make a handle mean the same node forever, which is
what stops a stale reference from silently reading a *different* element.
Freeing would need the indirection that guarantee exists to avoid.
**Trigger:** a page at steady state — not growing its own DOM — whose memory
climbs past the PLAN.md §4 budget of 100 MB. The 100-navigation curve in
`perf.md` shows it is all reclaimed on navigation, so this is a within-page
concern only.

### `innerHTML` parses without a context element

**We do:** parse the fragment as a document and adopt `<body>`'s children, so
`div.innerHTML = '<td>cell</td>'` keeps the `<td>`.
**A browser does:** parse with the target element as the insertion context,
which drops the cell and keeps `cell`.
**Why:** the context-sensitive algorithm is a second parser mode, and the
difference only shows for table parts written outside a table.
**Trigger:** a page that builds table rows through `innerHTML` on a non-table
element and renders them wrong.

### Collections are snapshots, not live

**We do:** return a plain array from `children` and `querySelectorAll`. It
never updates, and `for (const c of el.children) c.remove()` visits every
element exactly once.
**A browser does:** return live `HTMLCollection`/`NodeList` objects that change
underneath an iteration — which is why that same loop skips every other
element in a browser.
**Why:** a snapshot is what a page usually means, and the live version's most
famous property is being a bug generator.
**Trigger:** a page that depends on a collection updating — holding one across
a mutation and expecting the new nodes.

### There is no `Node` interface, no CSSOM, and no `getComputedStyle`

**We do:** hand JavaScript element handles only. Text and comment nodes are
never exposed, so `childNodes`, `nodeType`, `firstChild` and `parentNode` do
not exist; `el.style.color` does not exist (though `setAttribute('style', …)`
works and reaches computed values); `getComputedStyle` does not exist.
**A browser does:** all of it.
**Why:** each is a surface with its own object model, and M10's binding list is
what the ladder needs rather than what the DOM has.
**Trigger:** a page whose layout depends on reading back a computed value —
measuring an element and positioning against it.

### Attribute selectors do not parse, in CSS or in `querySelector`

**We do:** reject `[attr]` — the CSS parser has never supported it (M4), and
`querySelector` reuses that parser, so `querySelector('script[data-x]')` throws
`SyntaxError`.
**A browser does:** support the whole selector grammar.
**Why:** one selector syntax and one matcher is the rule; the gap is M4's, and
M10 inherited it rather than growing a second parser.
**Trigger:** already met — danluu.com's analytics script calls
`querySelector('script[data-cf-beacon]')` and throws. The fix belongs in the
CSS parser, where it fixes the cascade at the same time.

### `getElementsByTagName` and `getElementsByClassName` are absent

**We do:** not implement them. M10.4 left them out on the evidence available
then — no ladder fixture appeared to need them.
**A browser does:** implement both, returning live collections.
**Why:** they were judged unnecessary, and that judgement is now known to be
wrong: motherfuckingwebsite.com's Google Analytics snippet calls
`getElementsByTagName` and throws at its fourth line.
**Trigger:** already met. This is the clearest single fix M11 could make to how
much of the real web runs.

### Named element globals are absent

**We do:** not expose `<div id=box>` as `window.box`.
**A browser does:** expose it, for historical reasons everyone regrets.
**Why:** it puts page-controlled names into the global scope.
**Trigger:** a page that relies on it, which some old ones do.

## Events

### Key events never reach the page — yata's bindings always win

*(See the entry above under JavaScript (M10.8), which states this in full.)*

### Only `click`, `DOMContentLoaded` and `load` exist

**We do:** dispatch those three. No `mousemove`/`mouseover` (so `:hover` stays
a style-only concept a page cannot observe), no `submit`/`change` (forms are
M11), no `focus`/`blur`, and no synthetic dispatch — there is no `el.click()`
or `dispatchEvent`, which is what keeps the dispatcher private to the engine.
**A browser does:** dispatch the whole event set, and lets a page raise its
own.
**Why:** these three are what the ladder's interaction needs; every other
event is surface without a caller.
**Trigger:** a page whose only interaction is a form or a hover menu.

## Timers

### Every timer is clamped to 4 ms, not just nested ones

**We do:** floor every delay at `timers::MIN_DELAY` = 4 ms.
**A browser does:** clamp only *nested* timers to 4 ms; a first-level
`setTimeout(f, 1)` really is 1 ms.
**Why:** the floor is what stops `setTimeout(f, 0)` re-arming itself into a
spin, and 4 ms is under half the 10 ms keypress→screen budget, so a timer
firing at the floor still leaves the loop most of a budget for a keystroke.
**Trigger:** a page whose animation is visibly wrong at 250 ticks a second.

### No `requestAnimationFrame`

**We do:** not implement it. A page animating through `rAF` gets nothing.
**A browser does:** call back before each repaint, at the display's rate.
**Why:** a terminal has no frame clock to synchronise to — no vsync, no
compositor — and the renderer draws when something changed. The honest callback
rate would be "whenever we felt like it", which `setTimeout` already offers
without pretending.
**Trigger:** a ladder page whose content only appears from inside a `rAF`
callback. The fix then is to alias it to a 16 ms timer and say so, not to
invent a frame clock.

## Web APIs

### `localStorage` is per-session and never touches disk

**We do:** keep storage in memory, scoped per origin, with a 1 MB quota per
origin per area. It dies with the process.
**A browser does:** persist it across restarts, with roughly 5 MB.
**Why:** the UI thread does no disk I/O (CLAUDE.md) and `setItem` is
synchronous inside a tick, so durability means either blocking the loop or a
persistence worker with its own consistency story. And persistent per-origin
storage is a tracking surface a browser for *reading* does not need.
**Trigger:** a page whose usefulness depends on remembering across runs — a
reader-mode preference, a login.

### `fetch` is same-origin only, and never sends credentials

**We do:** reject a cross-origin `fetch` with a console line the reader can
see, and send no credentials with any request.
**A browser does:** send the request and let CORS decide who may *read* the
response; it sends cookies when the page asks.
**Why:** we have no CORS implementation, and `fetch` reads bodies — so allowing
arbitrary cross-origin reads would let any page pull whatever the reader's
network position can reach (an intranet, a localhost service) and post it back
out. No credentials because there are none: cookies are M11.
**Trigger:** a ladder page whose content comes from an API on another host. The
fix then is real CORS — preflights, `Access-Control-*`, opaque responses — not
a flag, because the restriction exists precisely because we cannot tell an
allowed read from a forbidden one.

### No cookies, no `history.pushState`, no `XMLHttpRequest`

**We do:** leave all three undefined rather than stubbed.
**A browser does:** implement them.
**Why:** a page can feature-detect an absence; it cannot detect a `pushState`
that silently does nothing. Cookies are M11's; `pushState` needs URL rewriting
without a fetch, which is a bigger idea than a binding.
**Trigger:** for `XMLHttpRequest`, a page that uses it instead of `fetch` —
still common in older code.

## Budgets

### A script gets 100 ms, and then it is stopped

**We do:** interrupt any script, listener, timer callback or promise
continuation that runs longer than `js::SCRIPT_BUDGET` = 100 ms. The overrun is
uncatchable, becomes an error in the console, and the host stays usable.
**A browser does:** let it run, and eventually offer the reader a dialog.
**Why:** JS runs on the UI thread, so a runaway script is a frozen browser.
Measured worst case for every runaway shape M10.13 tried: **one budget, ~102
ms** — a script, a listener and a timer callback are all interrupted on the
same terms, and nothing chains inside a tick. `q` still quits within one
budget, which is the rule PLAN.md §1.5 asked for.
**Trigger:** an honest page whose script legitimately needs more than 100 ms of
straight-line CPU and is killed for it.

### A page may hold a core indefinitely through legal ticks

**We do:** accept it. A 4 ms interval whose callback burns its whole budget
keeps one core busy forever, and no per-page duty-cycle budget exists.
**A browser does:** the same, mostly — with more cores to spare.
**Why:** every individual tick is bounded and the loop serves input *between*
ticks, so the reader can still scroll, still quit and still read. A duty-cycle
budget would be the fix; nothing on the ladder needs it.
**Trigger:** a real page that makes the terminal unpleasant to use while it is
open.

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
