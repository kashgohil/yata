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

**We do:** dispatch reader-originated `click`, `focus`, `blur`, `input`,
`change` and `submit`, plus lifecycle and inserted-script events, but never
`keydown`, `keyup` or `keypress`. A page cannot see — or capture — `j`, `f`,
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

### Inserted scripts use a bounded, later-turn queue

**We do:** run a connected `<script>` inserted directly with `appendChild` or
`insertBefore`, or activated by assigning `src`, in a later event-loop turn.
Inserted scripts run when ready rather than waiting behind document slots, and
at most 32 are adopted per page. A script inside an inserted subtree, one
created through `innerHTML`, or a clone taken from `<template>` remains inert;
an inserted external script does not delay the document's `load` event.
**A browser does:** discover executable scripts throughout inserted subtrees,
applies the HTML scripting flags to clones and fragment parsing, and lets a
fetched classic script delay `load`; ordering depends on parser-insertion and
`async` state.
**Why:** direct insertions cross a bounded host-to-loop queue without making
every subtree insertion an unbounded walk or re-entering JavaScript from the
tick that performed the mutation.
**Trigger:** a page whose application bootstrap is nested in an inserted
wrapper/template clone, or whose initialization depends on an inserted classic
script completing before `load`.

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

**We do:** return plain arrays from `children`, `querySelectorAll`,
`getElementsByTagName`, `getElementsByClassName` and `selectedOptions`. They
never update and expose no `namedItem`; `for (const c of el.children)
c.remove()` visits every element exactly once.
**A browser does:** return live `HTMLCollection`/`NodeList` objects that change
underneath an iteration — which is why that same loop skips every other
element in a browser.
**Why:** a snapshot is what a page usually means, and the live version's most
famous property is being a bug generator.
**Trigger:** a page that depends on a collection updating — holding one across
a mutation and expecting the new nodes.

### A script's reflected `src` is not resolved

**We do:** return the `<script>` element's attribute text from `el.src`, while
the fetch path resolves that text against the document URL.
**A browser does:** expose an absolute resolved URL from the `src` property.
**Why:** all subresource resolution remains in the browser loop; duplicating it
inside the JavaScript binding could make the reflected and fetched URLs
disagree.
**Trigger:** a page that compares or rewrites a relative script URL through
the reflected property.

### Fragment side effects settle after the assigning tick

**We do:** apply `location.hash` after the script turn, so the assigning script
reads the old URL until the next tick. A target is resolved once; a target with
no box falls back to its nearest laid-out ancestor, and no `:target` state is
cascaded.
**A browser does:** expose the new hash synchronously, scroll to the target
under its rendered-fragment rules, and match `:target`; later insertion does
not generally repeat an already-completed fragment navigation.
**Why:** navigation is a host effect applied after JavaScript returns, and the
layout tree cannot distinguish merged visible inline content from every
unboxed target. Retrying would move a reader after they had begun reading.
**Trigger:** a page reads back a just-assigned hash, styles essential content
with `:target`, or needs an unboxed target to land somewhere other than its
laid-out ancestor.

### Scoped restyle assumes selectors depend only on a node and its ancestors

**We do:** after bounded attribute-only mutations, recascade only the changed
subtrees and their descendants.
**A browser does:** preserve results for
the full selector language, including selectors whose result depends on a
sibling or descendant elsewhere.
**Why:** yata currently has descendant/child combinators and ancestor-derived
state only, so the narrowed walk is equivalent and keeps a leaf mutation under
the interaction budget.
**Trigger:** adding sibling combinators, `:has()`, `:target`, or any selector
whose answer depends outside the changed subtree requires redesigning the
invalidation boundary first.

### There is no `Node` interface, no CSSOM, and no `getComputedStyle`

**We do:** hand out one kind of handle. `document.createTextNode` returns one
for a text node — so a page can create, append and read text nodes — but
nothing else ever yields one: no query returns a text node, `children` is
elements only, and comments are never exposed at all. There is no `Node`
interface behind the handle, so `childNodes`, `nodeType` and `firstChild` do
not exist (`parentNode` does), and a text-node handle answers `tagName` with
`null`. `el.style.color` does not exist either (though `setAttribute('style',
…)` works and reaches computed values), and neither does `getComputedStyle`.
**A browser does:** all of it.
**Why:** each is a surface with its own object model, and M10's binding list is
what the ladder needs rather than what the DOM has.
**Trigger:** a page whose layout depends on reading back a computed value —
measuring an element and positioning against it.

### Attribute-selector values have no case-sensitivity flag

**We do:** support presence, exact, word, hyphen and substring attribute
selectors in CSS and DOM queries, but always compare values case-sensitively;
the trailing `i` flag is rejected.
**A browser does:** accept `i` (and `s`) flags that explicitly select ASCII
case-insensitive or case-sensitive value matching.
**Why:** the selector parser has one bounded value-comparison path and no flag
state; silently accepting `i` would produce the wrong match set.
**Trigger:** a ladder page whose styling or DOM query depends on a selector
such as `[href="X" i]`.

### Named element globals are absent

**We do:** not expose `<div id=box>` as `window.box`.
**A browser does:** expose it, for historical reasons everyone regrets.
**Why:** it puts page-controlled names into the global scope.
**Trigger:** a page that relies on it, which some old ones do.

## Events

### Events exist only at native surfaces yata owns

**We do:** dispatch `click`, `focus`, `blur`, `input`, `change`, `submit`,
`DOMContentLoaded`, document `load`, and inserted-script `load`/`error`.
There are no mouse-move/hover, unload-family, or synthetic events; no
`dispatchEvent`, `el.click()`, `form.submit()` or `requestSubmit()`, and no
inline/`on*` handler properties except inserted scripts' `onload`/`onerror`.
Cross-document teardown silently drops the old host.
**A browser does:** expose the general event and programmatic-activation
models, handler attributes/properties, and teardown events.
**Why:** native actions are bounded messages owned by the browser loop; a
general page-created event/action queue needs its own re-entrancy and
navigation policy.
**Trigger:** a ladder page whose only activation is programmatic, whose form
logic uses handler properties, or whose correctness depends on teardown or
hover events.

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

### `fetch` is same-origin only and has a small credentials model

**We do:** reject a cross-origin `fetch` with a console line the reader can
see. Same-origin and `include` send matching cookies; `omit` does not, so
`include` has no behavior beyond `same-origin`.
**A browser does:** send cross-origin requests and let CORS decide who may
*read* the response; `include` may send credentials cross-origin.
**Why:** we have no CORS implementation, and `fetch` reads bodies — so allowing
arbitrary cross-origin reads would let any page pull whatever the reader's
network position can reach (an intranet, a localhost service) and post it back
out.
**Trigger:** a ladder page whose content comes from an API on another host. The
fix then is real CORS — preflights, `Access-Control-*`, opaque responses — not
a flag, because the restriction exists precisely because we cannot tell an
allowed read from a forbidden one.

### No `history.pushState` or `XMLHttpRequest`

**We do:** leave both undefined rather than stubbed.
**A browser does:** implement them.
**Why:** a page can feature-detect an absence; it cannot detect a `pushState`
that silently does nothing. `pushState` needs URL rewriting without a fetch,
which is a bigger idea than a binding.
**Trigger:** for `XMLHttpRequest`, a page that uses it instead of `fetch` —
still common in older code.

## Cookies, requests, forms, and controls (M11.6–M11.13)

### Cookies are bounded process state

**We do:** keep at most 50 cookies per host (4 KiB each) in process memory;
`Domain` is parsed but every cookie remains host-only. Document responses and
redirect hops may set them. Subresource responses cannot, and an HTTP page's
HTTPS subresource is treated as cross-origin for credentials even when the
host matches.
**A browser does:** persist eligible cookies, implement domain/public-suffix
rules, accepts subresource response cookies, and scopes cookies by domain/path
with `Secure` rather than suppressing all mixed-scheme origin credentials.
**Why:** persistence and safe superdomain cookies require a profile policy and
public-suffix data; subresource messages deliberately carry no session-mutating
headers, and the same-origin fetch boundary is scheme + host + port.
**Trigger:** a site requires yesterday's login, shares login between sibling
hosts, sets its session through a subresource, or mixes HTTP and HTTPS for an
authenticated same-host resource.

### Redirect requests are fresh loop work

**We do:** report each document hop to the event loop, apply its cookies, and
spawn a fresh request/client. Subresources still follow redirects inside one
worker, so hop cookies are lost and path-specific cookies are not recomputed.
**A browser does:** reuse connection pools and apply cookie policy at every
document and subresource hop.
**Why:** a document hop changes reader-visible navigation/session state and
must be loop-owned; subresources retain their bounded one-worker path. A fresh
client is the current worker ownership boundary.
**Trigger:** a redirected stylesheet/script/image/fetch depends on a hop cookie
or changed path scope, or connection setup dominates a real navigation budget.

### Form submission is URL-encoded and never replayed

**We do:** support GET and `application/x-www-form-urlencoded` POST. Reload and
forward after POST issue GET and never retain/replay the body; multipart and
text/plain are refused. Submitter `formaction`, `formmethod` and `formenctype`
are ignored, and an invalid method is refused instead of defaulting to GET.
**A browser does:** can prompt before resubmission, supports the other
encodings and submitter overrides, and uses GET as the invalid-value default.
**Why:** history stores only URL and scroll, so credentials never enter a
checkpoint; unsupported encodings/methods fail visibly instead of silently
changing the request.
**Trigger:** a ladder flow needs upload/plain encoding, a submitter override,
or legitimate POST replay.

### Terminal implicit submission and control interaction are deliberate

**We do:** Enter submits any ancestral form, including a multi-field
button-less form, and omits a default submit button's name/value unless that
button was activated. Enter in a textarea submits rather than inserting a new
line. Selects use an in-page keyboard mode; reset, label activation,
`form=""`, disabled-fieldset propagation, file/specialized controls and
clipboard/selection editing are absent.
**A browser does:** apply the one-field/default-submitter algorithm, edit
textarea newlines, presents a platform select UI, and implements those form and
editing surfaces.
**Why:** a keyboard-only reader needs one discoverable way to send a form and
one explicit mode-exit rule; controls without a safe bounded terminal action
are not guessed.
**Trigger:** a site cannot be used because it relies on a default submitter,
textarea line entry, labels/external ownership/reset, or an unsupported
specialized control.

### Live form properties expose a bounded Web IDL subset

**We do:** expose live value/checked/selected state and the native events above;
`selectedOptions` is a snapshot, `selectedIndex` uses finite truncation rather
than full Web IDL `long` coercion, and submit events have no `submitter`.
Programmatic activation/handler properties and stateful control pseudo-classes
are absent.
**A browser does:** provide live collections, Web IDL conversions, submitter,
activation APIs/handlers, and selectors such as `:checked` and `:focus`.
**Why:** the shipped bindings cover bounded reader-originated native actions
without introducing a general re-entrant action queue or mutating markup to
mirror UI state.
**Trigger:** page logic observes choices through a held collection, depends on
full coercion/`submitter`, activates a control in script, or styles essential
state through a control pseudo-class.

## Tables and positioned/grid layout (M11.14–M11.19)

### Tables are DOM-derived terminal grids

**We do:** derive table/row/cell roles only from HTML elements, honor bounded
HTML `colspan`/`rowspan`, choose shared auto-sized terminal columns, and paint
one width-based shared edge. There are no anonymous table objects, CSS
`display: table-*`, captions/columns, fixed layout, spacing/default cell
padding, HTML width, parser repair, border style/color conflict resolution, or
accessibility table APIs.
**A browser does:** construct the CSS table formatting model, repair malformed
table markup, runs standard auto/fixed algorithms and collapsed/separate border
models, and exposes semantic accessibility structure.
**Why:** final cell rectangles and edges remain bounded layout output in cells;
inventing anonymous structure or semantics from a repaired tree would exceed
the DOM-derived model shipped in M11.
**Trigger:** a page loses or overlaps data because its table depends on CSS
roles, captions/columns/fixed sizing/spacing, styled border conflicts, parser
repair, or accessibility navigation.

### Positioned boxes use physical terminal-cell equations

**We do:** support static/relative/absolute with physical insets. Absolute
containing blocks are the nearest non-static ancestor's padding box, otherwise
the synthetic document root's content box; flex/grid/table/overflow do not
implicitly establish one. Start insets win, auto size between opposing insets
is bounded to at least one cell, and DOM order is paint order.
**A browser does:** implement the full over-constraint/shrink-to-fit equations,
logical insets, additional containing-block creators, stacking contexts,
`z-index`, transforms and anchors.
**Why:** the single terminal layout pass must produce positive bounded final
rectangles without a compositor or second sizing pass.
**Trigger:** essential content is sized/placed differently because it relies on
a non-position containing block, logical/transform geometry, full auto-margin
equations, or stacking order.

### Fixed and sticky use one page viewport

**We do:** fix boxes to the terminal page viewport. Sticky adjustment happens
from cached display-list geometry for document scrolling and physical
top/left start edges only; nested scrollers and end/dual-edge constraints are
unsupported.
**A browser does:** distinguish layout/visual viewports and support sticky
constraints inside scroll containers on both edges, with stacking contexts and
transform effects.
**Why:** yata has one viewport and one document scroll offset; scroll must not
restyle or relayout.
**Trigger:** mobile viewport distinctions, a nested scroller, bottom/right
sticky, or stacking/transform behavior makes primary chrome unreachable.

### Grid is an explicit row-major terminal subset

**We do:** support bounded fixed/percentage/auto/fr tracks, `minmax`, numeric
`repeat`, gaps, positive lines/spans, and sparse DOM-order row auto-placement
with implicit auto rows. There are no intrinsic min/max-content algorithms,
named lines/areas, negative lines, dense/column flow, implicit columns,
auto-fill/fit, ordering, subgrid, masonry, grid stacking, or scroll-container
semantics.
**A browser does:** implement the complete multi-phase track sizing and
placement model plus those features.
**Why:** explicit cell geometry can be resolved once into the ordinary box
tree and reused consistently by paint, hit testing, inspectors and scroll.
**Trigger:** a ladder page's main content is wrong because it depends on a
missing intrinsic, named/dense/implicit/subgrid, stacking, or scrolling rule.

## Cache, tabs, bookmarks, reader mode, and restore (M11.20–M11.24)

### The HTTP cache stores document representations, not pages

**We do:** keep a bounded private in-memory document-only cache. Freshness is
`max-age`/`no-cache`/`no-store` plus `Age`; validation is ETag/If-None-Match;
unsupported `Vary` disables storage. Redirects, POSTs, subresources and script
fetches are excluded, and stale entries are never served on error.
**A browser does:** use persistent general caches with Date/Expires/heuristics,
Last-Modified, broader Vary handling, stale policies, and separately may keep
live page state in a back/forward cache.
**Why:** raw bytes can safely rebuild a fresh runtime through the normal
pipeline without persisting credentials or live state.
**Trigger:** offline/restart use, Last-Modified/Expires-only content, a safe
unsupported Vary case, or a required stale-on-error response exposes the gap.

### Tabs are bounded live contexts in one process

**We do:** keep at most 16 ordered live tabs on one UI thread/channel/renderer.
Cookies, document/decoded-image caches and localStorage are shared; page state
and sessionStorage are tab-local. Background work continues, closing does not
cancel network work, and stable tab identity drops late results. Operations are
keyboard-only; there is no suspension/isolation, miss coalescing, reorder,
duplicate/reopen/pin, mouse tabs, `window.open`, or `_blank` support.
**A browser does:** offer richer tab APIs/UI and commonly isolate, suspend,
cancels or discards background contexts.
**Why:** page-addressed generations make one bounded event loop deterministic
without letting equal tab-local generations cross contexts.
**Trigger:** a background tab harms responsiveness/resources, a late worker is
not inert, or a site/workflow requires browser tab APIs or richer operations.

### Bookmarks are a bounded private local list

**We do:** store at most 1,024 newest-first URL/title snapshots in one atomic
versioned file loaded once. There is no live watching, locking/merge, standard
interchange, sync, folders/tags/edit/search/sort/favicons/bookmarklets, or title
refresh; concurrent processes are last-successful-writer-wins.
**A browser does:** generally offers richer organization/interchange/sync and
coordinates its profile database.
**Why:** a small immutable snapshot lets a dedicated worker persist without
blocking the UI or exposing a shared profile format.
**Trigger:** concurrent processes lose changes or readers need interchange,
sync, organization, editing/search, or current titles.

### Reader mode is a live-DOM prose projection

**We do:** deterministically select and prune a bounded subtree, apply UA-only
reader styling, and keep node identity/live mutations without rewriting the
DOM. It is not general article extraction, sanitization, script isolation, or
saved/offline reading.
**A browser does:** reader products may use broader extraction/sanitization and
can provide an isolated or saved reading document.
**Why:** projecting the live arena preserves focus/search/fragments/events and
tab ownership while leaving the normal page intact.
**Trigger:** a plausible article selects the wrong prose/chrome, hostile page
script makes the projection unsafe, or offline/saved reading becomes a product
claim.

### Session restore is a fresh-navigation recipe

**We do:** persist up to 16 ordered current URLs, active ordinal and bounded
normal-page scroll in one private atomic file. Restore performs ordinary fresh
navigations and omits history, titles, runtime/DOM, cookies/storage/caches,
forms/focus/search/inspectors and reader mode. A CLI URL or early session
mutation wins over a late load; saves coalesce for 250 ms and abrupt
termination may lose the newest snapshot.
**A browser does:** can restore richer live/history state, arbitrate profiles
and crashes, and coordinate multiple processes.
**Why:** shallow checkpoints are cheap to submit to a dedicated worker and
never serialize credentials or live engine internals.
**Trigger:** restart must preserve history/live state/offline bytes, users need
restore arbitration/profile UI, or abrupt/multi-process use loses unacceptable
state.

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
| floats (`float`, `clear`) | the box is a plain block | a page whose sidebar or pull-quote stacks instead of sitting beside the text |
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
