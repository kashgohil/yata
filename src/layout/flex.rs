//! Flexbox's core arithmetic: css-flexbox-1 §9.3, *line collection* and §9.6
//! step 15, *`align-content`* (M9.10), §9.7, *resolve the flexible lengths*
//! (M9.6), §9.5, *main-axis alignment* (M9.7), and §9.4/§9.6, *cross sizing and
//! cross-axis alignment* (M9.8), in whole terminal cells (PLAN.md M9).
//!
//! Kept out of `engine` deliberately. This is the part of flex layout that is
//! pure arithmetic over numbers — no DOM, no styles, no boxes — so it can be
//! read against the spec's pseudocode line by line and tested the same way.
//! `engine` decides what the items *are* (§4) and where their resolved sizes
//! go; everything between those two ends is here.
//!
//! **Main-axis coordinates.** [`place`] talks in distances from *main-start*,
//! never in `x`. That is what makes a reversed direction a mapping in the
//! engine — main-start is the far edge, so the same offsets are subtracted
//! instead of added — rather than a second copy of the placement rules with the
//! signs changed. [`cross_place`] and [`align_lines`] are written the same way,
//! in distances from *cross-start*, so `wrap-reverse` is the same mapping on
//! the other axis. Between them they serve all four directions and both wrap
//! orders: nothing in this file knows which physical axis is which, or which
//! way either of them points.
//!
//! **One axis has to be measured from built boxes, and which one depends on the
//! direction.** A size this file can be handed as a number is one the engine
//! resolved without laying anything out, and in this engine that means a
//! *width*: `intrinsic` measures widths, and nothing measures heights. So for a
//! row the main axis is the free one and the cross axis is the measured one —
//! which is why the cross-axis functions here take an item's outer size as an
//! input rather than producing it. For a column (M9.9) it is the other way
//! round: the engine builds each item to learn its main size and hands the
//! numbers in, and the cross axis is the one it knows in advance. The
//! asymmetry is real; it just is not fixed to an axis.
//!
//! **Whole cells.** The spec distributes fractions of a pixel and lets the
//! rasteriser sort it out; a terminal has no such luxury, so the fractions are
//! carried through the algorithm in `f64` and only quantized at the end, where
//! the rule is: floor every size, then hand the leftover cells to the earliest
//! items in main-axis order. That keeps two invariants a reader can see —
//! items that grow to fill a line leave no hole at its end, and no item is
//! ever a fraction of a cell narrower than its neighbour for no reason.

use std::ops::Range;

use crate::style::values::{AlignContent, AlignItems, JustifyContent};

/// One flex item's inputs to §9.7, all in cells and all on the main axis.
#[derive(Clone, Copy, Debug)]
pub(super) struct Item {
    /// §9.2 step 3: the size the item starts from, content-box.
    pub(super) base: i32,
    /// The base size clamped by this item's own min/max — §9.2 step 4's
    /// hypothetical main size.
    pub(super) hypothetical: i32,
    /// Used minimum main size, content-box. This is §4.5's *automatic minimum
    /// size* when `min-width` is `auto`, which is what stops a flex row from
    /// shredding a word one cell at a time.
    pub(super) min: i32,
    pub(super) max: Option<i32>,
    pub(super) grow: f32,
    pub(super) shrink: f32,
    /// Margin + border + padding on the main axis. An item's *outer* size is
    /// its main size plus this, and free space is measured against outer
    /// sizes — an item's own edges are not free space.
    pub(super) outer_edges: i32,
}

impl Item {
    fn clamp(&self, size: f64) -> f64 {
        let mut size = size;
        if let Some(max) = self.max {
            size = size.min(max as f64);
        }
        size.max(self.min as f64).max(0.0)
    }
}

/// §9.3 step 5: cut the items into flex lines, as ranges over the order the
/// caller gave them.
///
/// Walk the items in order, adding each to the current line while it still
/// fits the container's inner main size — the item's own outer size plus the
/// gap that would precede it — and starting a new line the first time one does
/// not. A `nowrap` container skips all of that: one line, whatever it costs,
/// which is what every flex container in this engine was until M9.10.
///
/// **The sizes read here are the *hypothetical* ones**, from before §9.7 flexed
/// anything, and that is the spec's rule rather than an approximation. Wrapping
/// decides which items share a line; §9.7 then decides how each line's items
/// divide it. Collect on post-grow sizes instead and a row of `flex: 1` items
/// wraps by sizes it only has *because* of where it wrapped, which is circular:
/// three items with a 20-cell basis in 80 cells belong on one line, and the
/// fact that they then grow to 26 cells each cannot be allowed to break them
/// into two.
///
/// **A line always holds at least one item**, even one wider than the whole
/// container — there is nowhere else to put it, and an item dropped or given a
/// line of its own *and* a second empty line would both be worse than the
/// overflow.
pub(super) fn collect_lines(
    items: &[Item],
    inner_main: i32,
    gap: i32,
    wrap: bool,
) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    if items.is_empty() {
        return lines;
    }
    if !wrap {
        lines.push(0..items.len());
        return lines;
    }
    let mut start = 0;
    let mut used = 0;
    for (idx, item) in items.iter().enumerate() {
        // Saturating: these are stylesheet numbers, and `flex-basis: 99999em`
        // is a legal thing to write. Such an item overflows its own line, which
        // is what it asks for; an overflowing add would be a panic a page could
        // trigger (PLAN.md §1.5).
        let outer = item.hypothetical.saturating_add(item.outer_edges);
        if idx == start {
            // The first item on a line is never asked whether it fits.
            used = outer;
            continue;
        }
        let extended = used.saturating_add(gap).saturating_add(outer);
        if extended > inner_main {
            lines.push(start..idx);
            start = idx;
            used = outer;
        } else {
            used = extended;
        }
    }
    lines.push(start..items.len());
    lines
}

/// Resolve every item's used main size (content-box cells), in the order given
/// — which the caller has already put in order-modified document order.
///
/// `inner_main` is the container's inner main size and `total_gap` the space
/// the gaps between items take out of it.
pub(super) fn resolve(items: &[Item], inner_main: i32, total_gap: i32) -> Vec<i32> {
    if items.is_empty() {
        return Vec::new();
    }
    // Gaps are not the items' to take: they come off the top, and everything
    // below divides what is left.
    let space = (inner_main - total_gap).max(0) as f64;
    let edges_total: f64 = items.iter().map(|i| i.outer_edges as f64).sum();

    // §9.7 step 1: sum the hypothetical outer main sizes. Under the container's
    // inner main size means there is room to grow; at or over it means the
    // items must shrink. One comparison decides which factor is in play for
    // the whole line — items never grow and shrink in the same pass.
    let hypothetical_total: f64 = items
        .iter()
        .map(|i| (i.hypothetical + i.outer_edges) as f64)
        .sum();
    let growing = hypothetical_total < space;

    // §9.7 step 2: size inflexible items and freeze them. An item is
    // inflexible when its factor is zero, or when flexing would move it *away*
    // from the size its own min/max already forced it to.
    let mut target: Vec<f64> = Vec::with_capacity(items.len());
    let mut frozen: Vec<bool> = Vec::with_capacity(items.len());
    for item in items {
        let factor = if growing { item.grow } else { item.shrink };
        let inflexible = factor == 0.0
            || (growing && item.base > item.hypothetical)
            || (!growing && item.base < item.hypothetical);
        frozen.push(inflexible);
        target.push(if inflexible {
            item.hypothetical as f64
        } else {
            item.base as f64
        });
    }

    // §9.7 step 3: the initial free space, measured with the *base* sizes of
    // everything still unfrozen. Only the "flex factors add up to less than
    // one" rule below reads it, and that rule needs the number from before the
    // loop started moving things.
    let initial_free = free_space(items, &target, &frozen, space, edges_total);

    // §9.7 step 4: the loop. Each pass either freezes an item that violated a
    // min/max, or freezes every remaining item and ends — so it runs at most
    // once per item.
    while frozen.iter().any(|&f| !f) {
        // (a) Remaining free space: frozen items at the size they froze at,
        // unfrozen ones back at their base size, because this pass is about to
        // redistribute what they hold.
        let mut free = free_space(items, &target, &frozen, space, edges_total);
        let factor_sum: f64 = items
            .iter()
            .zip(&frozen)
            .filter(|(_, f)| !**f)
            .map(|(i, _)| if growing { i.grow } else { i.shrink } as f64)
            .sum();
        // (b) The fractional-factor rule: `flex-grow: 0.5` means "take half of
        // what you could have", not "take everything because you are the only
        // item asking". Only bites when the sum is under one, and never makes
        // the distribution bigger than the space really available.
        if factor_sum < 1.0 {
            let scaled = initial_free * factor_sum;
            if scaled.abs() < free.abs() {
                free = scaled;
            }
        }

        if free != 0.0 && factor_sum > 0.0 {
            if growing {
                let sum: f64 = unfrozen(items, &frozen).map(|i| i.grow as f64).sum();
                for (idx, item) in items.iter().enumerate() {
                    if !frozen[idx] && sum > 0.0 {
                        target[idx] = item.base as f64 + free * (item.grow as f64 / sum);
                    }
                }
            } else {
                // Shrinking is weighted by the *scaled* factor — shrink factor
                // times the item's own base size. Two items at `flex-shrink: 1`
                // do not give up the same number of cells; the wider one gives
                // up more, in proportion to what it has. Without the scaling a
                // narrow item can be shrunk past nothing while a wide one
                // barely moves.
                let sum: f64 = unfrozen(items, &frozen)
                    .map(|i| i.shrink as f64 * i.base as f64)
                    .sum();
                for (idx, item) in items.iter().enumerate() {
                    if !frozen[idx] && sum > 0.0 {
                        let scaled = item.shrink as f64 * item.base as f64;
                        target[idx] = item.base as f64 - free.abs() * (scaled / sum);
                    }
                }
            }
        }

        // (c) Fix min/max violations, and (d) freeze by the sign of the total:
        // all-or-nothing, so an item that wanted to be smaller and one that
        // wanted to be bigger cannot cancel each other out and strand the loop.
        let mut total_violation = 0.0;
        let mut violation = vec![0.0; items.len()];
        for (idx, item) in items.iter().enumerate() {
            if frozen[idx] {
                continue;
            }
            let clamped = item.clamp(target[idx]);
            violation[idx] = clamped - target[idx];
            total_violation += violation[idx];
            target[idx] = clamped;
        }
        if total_violation == 0.0 {
            break;
        }
        for idx in 0..items.len() {
            if !frozen[idx]
                && ((total_violation > 0.0 && violation[idx] > 0.0)
                    || (total_violation < 0.0 && violation[idx] < 0.0))
            {
                frozen[idx] = true;
            }
        }
    }

    quantize(items, &target, space)
}

fn unfrozen<'a>(items: &'a [Item], frozen: &'a [bool]) -> impl Iterator<Item = &'a Item> {
    items
        .iter()
        .zip(frozen)
        .filter(|(_, f)| !**f)
        .map(|(i, _)| i)
}

/// Space left over once every item's edges, every frozen item's target size
/// and every unfrozen item's *base* size are accounted for (§9.7 step 4a).
fn free_space(items: &[Item], target: &[f64], frozen: &[bool], space: f64, edges: f64) -> f64 {
    let used: f64 = items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            if frozen[idx] {
                target[idx]
            } else {
                item.base as f64
            }
        })
        .sum();
    space - edges - used
}

/// Fractions of a cell cannot be drawn, so every size floors and the cells
/// that rounding dropped go to the **earliest items in main-axis order**.
///
/// That rule is the one to remember about this function: with three items
/// splitting 80 cells, the sizes are 27 / 27 / 26 and never 26 / 27 / 27. The
/// leftover is only ever handed to items that actually lost a fraction here,
/// which is what keeps it from quietly eating the free space that
/// `justify-content` (M9.7) is supposed to place — an item frozen at its
/// hypothetical size has an integral target and is skipped.
fn quantize(items: &[Item], target: &[f64], space: f64) -> Vec<i32> {
    let mut sizes: Vec<i32> = target.iter().map(|t| t.floor().max(0.0) as i32).collect();
    let used: i32 = sizes
        .iter()
        .zip(items)
        .map(|(s, i)| s + i.outer_edges)
        .sum();
    let mut leftover = space as i32 - used;
    if leftover <= 0 {
        return sizes;
    }
    for (idx, item) in items.iter().enumerate() {
        if leftover == 0 {
            break;
        }
        let lost_a_fraction = target[idx] > target[idx].floor();
        let room = item.max.is_none_or(|max| sizes[idx] < max);
        if lost_a_fraction && room {
            sizes[idx] += 1;
            leftover -= 1;
        }
    }
    sizes
}

/// One item as §9.5 sees it: how much of the line it takes, and whether either
/// of its main-axis margins is `auto` and so entitled to a share of what is
/// left. Nothing else about an item matters to alignment.
#[derive(Clone, Copy, Debug)]
pub(super) struct Slot {
    /// Outer main size: the size §9.7 resolved plus this item's own margin,
    /// border and padding, with any `auto` margin counted as zero.
    pub(super) outer: i32,
    pub(super) auto_start: bool,
    pub(super) auto_end: bool,
}

/// Where §9.5 put one item, in main-axis coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Placed {
    /// Cells from the container's main-start content edge to this item's
    /// main-start *margin* edge.
    pub(super) main_start: i32,
    /// Cells this item's main-start `auto` margin absorbed, and its main-end
    /// one. Zero unless the item asked for an auto margin on that side and
    /// there was free space to give it.
    pub(super) auto_start: i32,
    pub(super) auto_end: i32,
}

/// §9.5 *main-axis alignment*: hand out the space §9.7 left over and say where
/// each item's margin edge starts.
///
/// `items` is the line in main-axis order — each item's outer main size (the
/// size §9.7 resolved plus its own margin, border and padding) and which of
/// its two main-axis margins are `auto`. A reversed direction passes its items
/// in the same order and flips the offsets afterwards.
///
/// The order of business is the spec's and matters: gaps come out first (they
/// are not free space, they are structure), then **auto margins take
/// everything that is left**, and only if none claimed it does
/// `justify-content` get to distribute. `margin-left: auto` on the last nav
/// item is how the web pushes it to the right, and a `justify-content` that
/// ran anyway would fight it.
pub(super) fn place(
    items: &[Slot],
    gap: i32,
    inner_main: i32,
    justify: JustifyContent,
) -> Vec<Placed> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    // Saturating throughout: these numbers come from a stylesheet, and a page
    // is free to write `gap: 99999em`. The line ends up overflowing, which is
    // what such a gap asks for — a panic would not be a page (PLAN.md §1.5).
    let total_gap = gap.saturating_mul(n as i32 - 1);
    let used = items
        .iter()
        .fold(total_gap, |acc, item| acc.saturating_add(item.outer));
    // Negative free space is *overflow*, and there is nothing to distribute:
    // auto margins are treated as zero (§9.5 step 1 only fires "if the
    // remaining free space is positive") and every `justify-content` value
    // packs at main-start.
    //
    // **A deliberate departure, stated rather than hidden.** css-align-3 §9.3
    // only falls back this way for `space-between`; `space-around` and
    // `space-evenly` fall back to `center`, and `center` is *unsafe* by
    // default, so a browser lets an overflowing row hang off both ends. That
    // is the behaviour of `safe center` instead, chosen here because the
    // overflow a browser hides at the start edge is recoverable — the reader
    // scrolls left — and in a terminal it is not: there is no negative column,
    // and the first item would simply be gone.
    let free = inner_main.saturating_sub(used).max(0);

    let mut placed = vec![Placed::default(); n];

    // Nothing to hand out, or nowhere to hand it but the end of the line: the
    // items pack at main-start and no slot arithmetic is needed. This is the
    // overflow case, and it is also what most rows on a real page are —
    // `flex-start` is the initial value — so it is worth not allocating for.
    if free == 0 || (justify == JustifyContent::FlexStart && !any_auto(items)) {
        return offsets(placed, items, gap, 0, &[]);
    }

    // §9.5 step 1: auto margins absorb *all* the free space, split equally
    // between every auto margin on the line — not per item, so one item with
    // `margin: 0 auto` and one with a single `margin-left: auto` do not get
    // the same share.
    if any_auto(items) {
        let auto_slots: Vec<i32> = items
            .iter()
            .flat_map(|item| [i32::from(item.auto_start), i32::from(item.auto_end)])
            .collect();
        let shares = split(free, &auto_slots);
        for (idx, item) in placed.iter_mut().enumerate() {
            item.auto_start = shares[idx * 2];
            item.auto_end = shares[idx * 2 + 1];
        }
        return offsets(placed, items, gap, 0, &[]);
    }

    // §9.5 step 6: whatever is left, distributed by `justify-content`. Every
    // value is the same shape — some space before the first item, some between
    // each adjacent pair, some after the last — so the six of them are six
    // weightings of those slots rather than six placement loops. `space-around`
    // is why the weights are integers: its end spaces are half its inner ones,
    // which in halves is 1 and 2.
    let (lead_w, between_w, end_w) = match justify {
        JustifyContent::FlexStart => (0, 0, 1),
        JustifyContent::FlexEnd => (1, 0, 0),
        JustifyContent::Center => (1, 0, 1),
        // A single item has no "between" to put the space in, so it stays at
        // main-start and the space goes after it.
        JustifyContent::SpaceBetween if n > 1 => (0, 1, 0),
        JustifyContent::SpaceBetween => (0, 0, 1),
        JustifyContent::SpaceAround => (1, 2, 1),
        JustifyContent::SpaceEvenly => (1, 1, 1),
    };
    let mut weights = Vec::with_capacity(n + 1);
    weights.push(lead_w);
    weights.extend(std::iter::repeat_n(between_w, n - 1));
    weights.push(end_w);
    let spacing = split(free, &weights);
    offsets(placed, items, gap, spacing[0], &spacing[1..n])
}

fn any_auto(items: &[Slot]) -> bool {
    items.iter().any(|i| i.auto_start || i.auto_end)
}

/// Walk the line once, main-start to main-end, adding up what each item and
/// the space beside it costs.
///
/// This is the only place an item's main-axis position is decided, for every
/// alignment and both row directions — which is what keeps `Σ(outer sizes) +
/// gaps + spacing == inner main size` a property of the code rather than a
/// coincidence that has to be re-checked per value.
fn offsets(
    mut placed: Vec<Placed>,
    items: &[Slot],
    gap: i32,
    lead: i32,
    between: &[i32],
) -> Vec<Placed> {
    let mut cursor = lead;
    for (idx, item) in placed.iter_mut().enumerate() {
        item.main_start = cursor;
        cursor = cursor
            .saturating_add(items[idx].outer)
            .saturating_add(item.auto_start)
            .saturating_add(item.auto_end);
        if idx + 1 < items.len() {
            // An empty `between` is the packed case: gaps still separate the
            // items, there is simply nothing extra to put beside them.
            let extra = between.get(idx).copied().unwrap_or(0);
            cursor = cursor.saturating_add(gap).saturating_add(extra);
        }
    }
    placed
}

/// Split `total` cells between weighted slots, whole cells only.
///
/// Every slot gets the floor of its share and the cells rounding dropped go to
/// the **earliest** slots that asked for any — the same rule [`quantize`] uses
/// on item sizes, for the same reason: one rule for leftover cells is one rule
/// to remember, and it makes `Σ(slots) == total` exact. The visible
/// consequence is that an odd number of cells to centre leaves the extra one
/// before the item rather than after it.
fn split(total: i32, weights: &[i32]) -> Vec<i32> {
    let sum: i64 = weights.iter().map(|&w| w as i64).sum();
    if total <= 0 || sum <= 0 {
        return vec![0; weights.len()];
    }
    let mut out: Vec<i32> = weights
        .iter()
        .map(|&w| (total as i64 * w as i64 / sum) as i32)
        .collect();
    // Each slot lost less than a whole cell to the floor, so the leftover is
    // smaller than the number of slots that wanted anything and this loop
    // always empties it.
    let mut leftover = total - out.iter().sum::<i32>();
    for (slot, &weight) in out.iter_mut().zip(weights) {
        if leftover == 0 {
            break;
        }
        if weight > 0 {
            *slot += 1;
            leftover -= 1;
        }
    }
    out
}

/// One item as §9.4 (*cross sizing*) and §9.6 (*cross-axis alignment*) see it,
/// all in cells and all on the cross axis.
///
/// `outer` is a size the engine already knows, not one this file computes. For
/// a **row** that means the item has been laid out by the time this struct is
/// built — its cross size is its content's height, which nothing knows until
/// the content exists — so a row's cross alignment necessarily runs last. For a
/// **column** the cross size is a width, definite from the container's content
/// box, so the same struct is built *before* the item is (M9.9). Which of the
/// two it is, is the caller's business; nothing here can tell.
#[derive(Clone, Copy, Debug)]
pub(super) struct CrossItem {
    /// Outer cross size: the item's margin box on the cross axis.
    pub(super) outer: i32,
    /// Distance from the item's cross-start margin edge to its baseline — the
    /// row its first line box sits on, or, for an item with no line box at
    /// all, its cross-end border edge (§8.3's synthesis rule). Only read when
    /// `align` is `Baseline`.
    pub(super) baseline: i32,
    /// `align-self` already resolved against the container's `align-items`.
    pub(super) align: AlignItems,
    /// Whether the cross-start / cross-end margin is `auto`, which on this
    /// axis too means "give me the free space" — and overrides `align`.
    pub(super) auto_start: bool,
    pub(super) auto_end: bool,
}

impl CrossItem {
    /// Whether this item is one of the ones stitched to the line's shared
    /// baseline row.
    ///
    /// Asking for `baseline` is not enough: §9.4 step 8 collects the items
    /// "whose `align-self` is `baseline` **and whose cross-axis margins are
    /// both non-auto**", because an auto margin claims the free space before
    /// alignment is ever consulted (§9.6 step 1) and such an item is therefore
    /// never placed at the shared row. One predicate, read by both the sizing
    /// and the placing, is what keeps the line from being sized for a
    /// placement that will not happen.
    fn baseline_aligned(&self) -> bool {
        self.align == AlignItems::Baseline && !self.auto_start && !self.auto_end
    }
}

/// Where §9.6 put one item on the cross axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CrossPlaced {
    /// Cells from the line's cross-start edge to this item's cross-start
    /// *margin* edge.
    pub(super) cross_start: i32,
    /// Cells this item's `auto` cross margins absorbed. Zero unless the item
    /// asked for one and the line had room to give it.
    pub(super) auto_start: i32,
    pub(super) auto_end: i32,
}

/// §9.4 step 8: the line's cross size, from the items on it.
///
/// Two groups, and the larger wins. Items not stitched to the shared baseline
/// row need room for themselves, so they ask for their own outer cross size.
/// The baseline group is aligned at one row, so what it needs is the deepest
/// anything in it reaches *above* that row plus the deepest anything reaches
/// below it — which can be more than any single item's height, and is the
/// reason a label next to a bordered heading makes its row taller than either
/// of them alone.
pub(super) fn cross_size(items: &[CrossItem]) -> i32 {
    let (above, below) = baseline_extents(items);
    let tallest = items
        .iter()
        .filter(|i| !i.baseline_aligned())
        .map(|i| i.outer)
        .max()
        .unwrap_or(0);
    tallest.max(above.saturating_add(below))
}

/// How far the baseline-aligned items reach above and below their shared row.
fn baseline_extents(items: &[CrossItem]) -> (i32, i32) {
    items
        .iter()
        .filter(|i| i.baseline_aligned())
        .fold((0, 0), |(above, below), i| {
            (
                above.max(i.baseline.max(0)),
                below.max((i.outer - i.baseline).max(0)),
            )
        })
}

/// Where §9.6 step 15 put one flex line on the cross axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct FlexLine {
    /// The line's used cross size — the one `cross_size` computed, plus
    /// whatever share of the container's leftover cross space `stretch` gave
    /// it.
    pub(super) cross: i32,
    /// Cells from the container's cross-start content edge to this line's
    /// cross-start edge.
    pub(super) cross_start: i32,
}

/// §9.6 step 15, *`align-content`*: stack the lines and hand out whatever cross
/// space the container has left over.
///
/// **This is [`place`] again, one axis over.** css-align gives
/// `justify-content` and `align-content` the same value list because they are
/// the same operation — pack some boxes into a larger box and divide what is
/// left — and here they are literally the same code: the lines are the slots,
/// the cross gap is the gap, and only `stretch` needs a rule of its own. Which
/// is worth more than the twenty lines it saves: `align-content: space-between`
/// rounds its leftover cells exactly the way `justify-content: space-between`
/// does, without either having to be re-checked against the other.
///
/// `stretch` — the initial value — grows every line by an equal share of the
/// leftover instead of moving it, cells that rounding dropped going to the
/// earliest lines, and then has nothing left to distribute. A container whose
/// cross size came from its own contents has no leftover in the first place, so
/// every value of `align-content` does nothing there, which is what a browser
/// shows and the reason the property looks broken on single-line containers.
pub(super) fn align_lines(
    lines: &[i32],
    gap: i32,
    inner_cross: i32,
    align: AlignContent,
) -> Vec<FlexLine> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut cross = lines.to_vec();
    if align == AlignContent::Stretch {
        let free = inner_cross.saturating_sub(used_cross(&cross, gap)).max(0);
        let weights = vec![1; cross.len()];
        for (line, share) in cross.iter_mut().zip(split(free, &weights)) {
            *line += share;
        }
    }
    let slots: Vec<Slot> = cross
        .iter()
        .map(|&outer| Slot {
            outer,
            auto_start: false,
            auto_end: false,
        })
        .collect();
    // A stretched line took the free space already, so what is left to pack is
    // nothing and `flex-start` is the cheapest way to say "in order, from
    // cross-start". Auto margins have no counterpart here — a *line* has no
    // margins — which is why `place`'s step-1 branch never fires for lines.
    let justify = match align {
        AlignContent::FlexStart | AlignContent::Stretch => JustifyContent::FlexStart,
        AlignContent::FlexEnd => JustifyContent::FlexEnd,
        AlignContent::Center => JustifyContent::Center,
        AlignContent::SpaceBetween => JustifyContent::SpaceBetween,
        AlignContent::SpaceAround => JustifyContent::SpaceAround,
    };
    place(&slots, gap, inner_cross, justify)
        .iter()
        .zip(cross)
        .map(|(p, cross)| FlexLine {
            cross,
            cross_start: p.main_start,
        })
        .collect()
}

/// What a stack of lines costs the cross axis: their own sizes plus the gap
/// between each adjacent pair.
pub(super) fn used_cross(lines: &[i32], gap: i32) -> i32 {
    let gaps = gap.saturating_mul(lines.len() as i32 - 1).max(0);
    lines.iter().fold(gaps, |acc, &c| acc.saturating_add(c))
}

/// §9.6 *cross-axis alignment*: where each item's cross-start margin edge sits
/// inside a line of `line_cross` cells.
///
/// The order of business mirrors §9.5's on the main axis, for the same reason:
/// **auto margins take the free space first**, and only an item that claimed
/// none of it is aligned by `align-self`. An item with `margin: auto 0` is
/// centred whatever its `align-self` says, which is the rule that lets one item
/// in a row centre itself without the container agreeing to centre all of them.
///
/// `stretch` places at cross-start here and the box gets its stretched cross
/// size elsewhere — grown into it on a row, built at it on a column. An item
/// that cannot stretch (a specified cross size, an auto margin) is left exactly
/// where `flex-start` would have put it, which is what the spec says it falls
/// back to.
pub(super) fn cross_place(items: &[CrossItem], line_cross: i32) -> Vec<CrossPlaced> {
    let (max_above, _) = baseline_extents(items);
    items
        .iter()
        .map(|item| {
            // Negative free space is overflow, and there is nothing to hand
            // out. The same deliberate *safe* fallback as §9.5's: an item
            // centred out of an overflowing line would hang off the top of the
            // page, and unlike a browser a terminal has no rows above row 0 to
            // scroll back to.
            let free = line_cross.saturating_sub(item.outer).max(0);
            if free > 0 && (item.auto_start || item.auto_end) {
                let shares = split(
                    free,
                    &[i32::from(item.auto_start), i32::from(item.auto_end)],
                );
                return CrossPlaced {
                    cross_start: 0,
                    auto_start: shares[0],
                    auto_end: shares[1],
                };
            }
            let cross_start = match item.align {
                AlignItems::FlexStart | AlignItems::Stretch => 0,
                AlignItems::FlexEnd => free,
                // The odd cell goes above the item, the same "earliest slot
                // first" rule `split` uses everywhere else in this file.
                AlignItems::Center => split(free, &[1, 1])[0],
                // Every baseline-aligned item drops by the difference between
                // the line's deepest baseline and its own, so the offset is
                // never negative: an item that would have sat above the line's
                // cross-start edge is what *pushed the edge down* in
                // `cross_size`, rather than escaping the line.
                //
                // The `min` never binds on a line sized by `cross_size` — the
                // deepest baseline is exactly what that sizing left room for.
                // It binds when the container stated a cross size too small for
                // its own contents, and it is the same safe rule the other four
                // values follow there: no item starts past the line's end.
                AlignItems::Baseline => (max_above - item.baseline).max(0).min(free),
            };
            CrossPlaced {
                cross_start,
                ..CrossPlaced::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An item with no edges, no clamps and the initial factors (`0 1 auto`
    /// as the shorthand spells them), sized from `base`.
    fn item(base: i32) -> Item {
        Item {
            base,
            hypothetical: base,
            min: 0,
            max: None,
            grow: 0.0,
            shrink: 1.0,
            outer_edges: 0,
        }
    }

    fn grow(base: i32, grow: f32) -> Item {
        Item { grow, ..item(base) }
    }

    #[test]
    fn items_that_fit_keep_their_hypothetical_sizes() {
        // `flex-grow: 0` is the initial value: room left over is not an
        // invitation to take it. Where that room *goes* is M9.7's question.
        let items = [item(10), item(10), item(10)];
        assert_eq!(resolve(&items, 80, 0), [10, 10, 10]);
    }

    #[test]
    fn grow_splits_free_space_and_rounding_goes_to_the_earliest() {
        // Three `flex: 1` items: base 0, so all 80 cells are free space and
        // each wants 26.666…. Floors to 26 · 3 = 78, and the two cells that
        // rounding dropped go to the first two items.
        let items = [grow(0, 1.0), grow(0, 1.0), grow(0, 1.0)];
        let sizes = resolve(&items, 80, 0);
        assert_eq!(sizes, [27, 27, 26]);
        assert_eq!(sizes.iter().sum::<i32>(), 80, "a grown line leaves no hole");
    }

    #[test]
    fn grow_is_proportional_and_a_zero_factor_never_moves() {
        // 30-cell bases, 90 used of 120, so 30 cells are free and split 1:2.
        let items = [grow(30, 1.0), grow(30, 2.0), grow(30, 0.0)];
        assert_eq!(resolve(&items, 120, 0), [40, 50, 30]);
    }

    #[test]
    fn shrink_is_weighted_by_the_scaled_factor() {
        // Same shrink factor, different bases: the wider item gives up more,
        // in proportion to what it has. 60 + 30 = 90 in 60 cells, so 30 must
        // go: scaled factors 60 and 30 sum to 90, so the wide item loses
        // 30 × 60/90 = 20 and the narrow one 30 × 30/90 = 10.
        let items = [item(60), item(30)];
        assert_eq!(resolve(&items, 60, 0), [40, 20]);
    }

    #[test]
    fn an_item_that_hits_its_minimum_freezes_and_the_others_absorb_it() {
        // The case a single pass gets wrong. Bases 40/40/40 in 80 cells; the
        // first pass gives every item 26.666…, which puts the third under its
        // 30-cell minimum. It freezes at 30 and the remaining 30 cells of
        // overflow are re-divided between the other two: 40 - 15 = 25 each.
        let items = [
            item(40),
            item(40),
            Item {
                min: 30,
                ..item(40)
            },
        ];
        let sizes = resolve(&items, 80, 0);
        assert_eq!(sizes, [25, 25, 30]);
        assert_eq!(sizes.iter().sum::<i32>(), 80);
        // A single pass would have stopped at 26 + 26 + 30 = 82 and overflowed.
    }

    #[test]
    fn a_maximum_freezes_a_growing_item_and_the_rest_take_the_remainder() {
        // 0-base items with equal grow factors want 30 each of 90 cells; the
        // first is capped at 10, so the other two divide 80.
        let items = [
            Item {
                max: Some(10),
                ..grow(0, 1.0)
            },
            grow(0, 1.0),
            grow(0, 1.0),
        ];
        let sizes = resolve(&items, 90, 0);
        assert_eq!(sizes, [10, 40, 40]);
        assert_eq!(sizes.iter().sum::<i32>(), 90);
    }

    #[test]
    fn flex_factors_summing_under_one_take_only_their_share() {
        // `flex-grow: 0.5` on the only item: half the free space, not all of
        // it. 100 cells free, so the item ends at 50.
        let items = [grow(0, 0.5)];
        assert_eq!(resolve(&items, 100, 0), [50]);
        // Two of them take a quarter each, and 50 cells stay unclaimed.
        let items = [grow(0, 0.25), grow(0, 0.25)];
        assert_eq!(resolve(&items, 100, 0), [25, 25]);
    }

    #[test]
    fn edges_and_gaps_come_off_before_anything_is_distributed() {
        // Two items with 4 cells of edges each and a 2-cell gap: 80 - 2 - 8
        // leaves 70 to divide, so 35 of *content* each.
        let items = [
            Item {
                outer_edges: 4,
                ..grow(0, 1.0)
            },
            Item {
                outer_edges: 4,
                ..grow(0, 1.0)
            },
        ];
        let sizes = resolve(&items, 80, 2);
        assert_eq!(sizes, [35, 35]);
        let outer: i32 = sizes.iter().map(|s| s + 4).sum();
        assert_eq!(outer + 2, 80, "outer sizes plus the gap fill the line");
    }

    #[test]
    fn no_item_is_ever_negative_however_little_room_there_is() {
        // A container narrower than the items' own edges. Nothing can fit;
        // nothing may go negative either — a negative width would panic the
        // rasteriser or, worse, quietly paint backwards.
        let items = [
            Item {
                outer_edges: 10,
                ..item(20)
            },
            Item {
                outer_edges: 10,
                ..item(20)
            },
        ];
        for width in 0..12 {
            let sizes = resolve(&items, width, 0);
            assert!(sizes.iter().all(|&s| s >= 0), "width {width}: {sizes:?}");
        }
    }

    #[test]
    fn the_line_is_filled_exactly_at_every_width() {
        // The integer-cell invariant, swept: whenever the items can fill the
        // line they fill it exactly, and when they cannot they never overflow
        // it by a rounding error.
        let items = [grow(0, 1.0), grow(0, 2.0), grow(5, 1.0)];
        for width in 20..=120 {
            let sizes = resolve(&items, width, 2);
            let used: i32 = sizes.iter().sum::<i32>() + 2;
            assert_eq!(used, width, "width {width}: {sizes:?}");
            assert_eq!(sizes.len(), 3, "no item was lost");
        }
    }

    // §9.3 step 5, line collection (M9.10).

    /// How many items `collect_lines` put on each line. The ranges themselves
    /// are an indexing detail; the cut is the behaviour.
    fn lines(items: &[Item], inner_main: i32, gap: i32) -> Vec<usize> {
        collect_lines(items, inner_main, gap, true)
            .iter()
            .map(|line| line.len())
            .collect()
    }

    #[test]
    fn nowrap_keeps_every_item_on_one_line_however_little_room_there_is() {
        // The initial value, and every flex container in this engine before
        // M9.10: three items that need 150 cells stay on one 80-cell line and
        // overflow it.
        let items = [item(50), item(50), item(50)];
        let single = collect_lines(&items, 80, 0, false);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0], 0..3);
        assert!(collect_lines(&[], 80, 0, true).is_empty());
    }

    #[test]
    fn a_line_takes_items_until_the_next_one_does_not_fit() {
        // Six 20-cell items in 80 cells: the fourth fits exactly, the fifth
        // would make 100.
        let items = [item(20); 6];
        assert_eq!(lines(&items, 80, 0), [4, 2]);
        // One cell narrower and the fourth is the one that no longer fits.
        assert_eq!(lines(&items, 79, 0), [3, 3]);
    }

    #[test]
    fn the_gap_before_an_item_is_part_of_whether_it_fits() {
        // Four 20-cell items fill 80 cells exactly; add a 4-cell gap between
        // each pair and the fourth needs 92, so three share the line.
        let items = [item(20); 4];
        assert_eq!(lines(&items, 80, 0), [4]);
        assert_eq!(lines(&items, 80, 4), [3, 1]);
    }

    #[test]
    fn an_items_own_edges_count_towards_the_line_it_asks_for() {
        // Free space is measured against *outer* sizes here as everywhere
        // else: three items of 20 cells with 10 cells of edges each need 90.
        let padded = Item {
            outer_edges: 10,
            ..item(20)
        };
        assert_eq!(lines(&[padded; 3], 90, 0), [3]);
        assert_eq!(lines(&[padded; 3], 89, 0), [2, 1]);
    }

    #[test]
    fn the_size_that_decides_a_line_is_the_hypothetical_one() {
        // §9.2 step 4 has already run, so an item with a 40-cell basis and a
        // 20-cell maximum takes 20 cells of the line rather than 40 — and four
        // of them share a line that two of their base sizes would have filled.
        let clamped = Item {
            base: 40,
            hypothetical: 20,
            max: Some(20),
            ..item(40)
        };
        assert_eq!(lines(&[clamped; 4], 80, 0), [4]);
    }

    #[test]
    fn a_line_always_holds_at_least_one_item() {
        // An item wider than the whole container gets a line to overflow on
        // its own. Dropping it, or leaving an empty line in front of it, would
        // both be worse than the overflow.
        assert_eq!(lines(&[item(200); 2], 80, 0), [1, 1]);
        // ...and a gap wider than the container never empties a line either.
        assert_eq!(lines(&[item(1); 2], 10, 400), [1, 1]);
    }

    #[test]
    fn every_item_lands_on_exactly_one_line_at_every_width() {
        // The structural invariant, swept: whatever the container's width, the
        // lines partition the items in order — none lost, none duplicated, and
        // no line empty. A `0` width is included because a terminal can be
        // dragged to nothing and a flex container must still produce boxes.
        let items = [item(7), item(13), item(5), item(21), item(1)];
        let all: Vec<usize> = (0..items.len()).collect();
        for width in 0..=120 {
            let lines = collect_lines(&items, width, 2, true);
            assert!(
                lines.iter().all(|line| !line.is_empty()),
                "width {width}: an empty line {lines:?}"
            );
            let covered: Vec<usize> = lines.iter().flat_map(|line| line.clone()).collect();
            assert_eq!(covered, all, "width {width}: {lines:?}");
        }
    }

    // §9.6 step 15, `align-content` (M9.10). Cross-axis coordinates again:
    // `cross_start` counts from cross-start, whichever edge that is.

    const ALL_CONTENT: [AlignContent; 6] = [
        AlignContent::FlexStart,
        AlignContent::FlexEnd,
        AlignContent::Center,
        AlignContent::SpaceBetween,
        AlignContent::SpaceAround,
        AlignContent::Stretch,
    ];

    fn line_starts(lines: &[i32], gap: i32, inner: i32, align: AlignContent) -> Vec<i32> {
        align_lines(lines, gap, inner, align)
            .iter()
            .map(|line| line.cross_start)
            .collect()
    }

    fn line_sizes(lines: &[i32], gap: i32, inner: i32, align: AlignContent) -> Vec<i32> {
        align_lines(lines, gap, inner, align)
            .iter()
            .map(|line| line.cross)
            .collect()
    }

    #[test]
    fn align_content_distributes_the_leftover_cross_space_six_ways() {
        // Two 1-row lines in a 6-row container: 4 rows to place, and the same
        // six answers `justify-content` gives on the main axis.
        let lines = [1, 1];
        assert_eq!(line_starts(&lines, 0, 6, AlignContent::FlexStart), [0, 1]);
        assert_eq!(line_starts(&lines, 0, 6, AlignContent::FlexEnd), [4, 5]);
        assert_eq!(line_starts(&lines, 0, 6, AlignContent::Center), [2, 3]);
        assert_eq!(
            line_starts(&lines, 0, 6, AlignContent::SpaceBetween),
            [0, 5]
        );
        // Half a share at each end and a whole one between: 1 : 2 : 1 over 4
        // rows divides exactly.
        assert_eq!(line_starts(&lines, 0, 6, AlignContent::SpaceAround), [1, 4]);
        // `stretch` is the odd one out: it grows the lines instead of moving
        // them, and then has nothing left to hand out.
        assert_eq!(line_starts(&lines, 0, 6, AlignContent::Stretch), [0, 3]);
        assert_eq!(line_sizes(&lines, 0, 6, AlignContent::Stretch), [3, 3]);
    }

    #[test]
    fn stretch_hands_the_odd_row_to_the_earliest_line() {
        // 5 rows over 2 lines is 2.5 each: the odd row goes to the first line,
        // which is `split`'s rule everywhere in this file.
        assert_eq!(line_sizes(&[1, 1], 0, 7, AlignContent::Stretch), [4, 3]);
        // The gap comes off first, exactly as it does on the main axis: 7 rows
        // less a 1-row gap leaves 6 to divide.
        assert_eq!(line_sizes(&[1, 1], 1, 7, AlignContent::Stretch), [3, 3]);
    }

    #[test]
    fn a_content_sized_cross_axis_leaves_align_content_nothing_to_do() {
        // A container as big as its own lines has no leftover space, so every
        // value — `stretch` included — stacks them from cross-start. This is
        // why `align-content` looks broken on most pages that reach for it.
        for align in ALL_CONTENT {
            assert_eq!(line_starts(&[2, 3], 1, 6, align), [0, 3], "{align:?}");
            assert_eq!(line_sizes(&[2, 3], 1, 6, align), [2, 3], "{align:?}");
        }
    }

    #[test]
    fn lines_neither_overlap_nor_overflow_at_any_cross_size() {
        // The integer-cell invariant on the cross axis, swept the way the main
        // axis's is: lines stay in order, keep their gap, and the values that
        // claim both edges land exactly on the far one.
        let lines = [2, 3, 1];
        let gap = 1;
        let content = used_cross(&lines, gap);
        for inner in 0..=40 {
            for align in ALL_CONTENT {
                let placed = align_lines(&lines, gap, inner, align);
                let label = format!("inner {inner}, {align:?}: {placed:?}");
                assert!(placed[0].cross_start >= 0, "{label}");
                for pair in placed.windows(2) {
                    assert!(
                        pair[1].cross_start - (pair[0].cross_start + pair[0].cross) >= gap,
                        "{label}: lines overlap or ate the gap"
                    );
                }
                let end = placed[2].cross_start + placed[2].cross;
                if inner < content {
                    // Overflow: nothing to hand out, and nothing handed out.
                    assert_eq!(placed[0].cross_start, 0, "{label}");
                    assert_eq!(end, content, "{label}");
                    continue;
                }
                assert!(end <= inner, "{label}: overflowed by rounding");
                // `stretch` grows the lines into every spare cell and
                // `space-between` pushes the last one onto the far edge, so
                // both have to end exactly there.
                if matches!(align, AlignContent::Stretch | AlignContent::SpaceBetween) {
                    assert_eq!(end, inner, "{label}: left a hole");
                }
            }
        }
    }

    // §9.5, main-axis alignment (M9.7). Everything below is in main-axis
    // coordinates: `main_start` counts from main-start, whichever edge that is.

    const ALL: [JustifyContent; 6] = [
        JustifyContent::FlexStart,
        JustifyContent::FlexEnd,
        JustifyContent::Center,
        JustifyContent::SpaceBetween,
        JustifyContent::SpaceAround,
        JustifyContent::SpaceEvenly,
    ];

    /// A line of items with no auto margins, from their outer main sizes.
    fn line(outer: &[i32]) -> Vec<Slot> {
        outer
            .iter()
            .map(|&outer| slot(outer, false, false))
            .collect()
    }

    fn slot(outer: i32, auto_start: bool, auto_end: bool) -> Slot {
        Slot {
            outer,
            auto_start,
            auto_end,
        }
    }

    /// Where each item's margin edge lands, for a line with no auto margins.
    fn starts(outer: &[i32], gap: i32, inner: i32, justify: JustifyContent) -> Vec<i32> {
        place(&line(outer), gap, inner, justify)
            .iter()
            .map(|p| p.main_start)
            .collect()
    }

    #[test]
    fn justify_content_distributes_the_free_space_six_ways() {
        // Three 20-cell items in 80 cells: 20 cells to place.
        let outer = [20, 20, 20];
        assert_eq!(
            starts(&outer, 0, 80, JustifyContent::FlexStart),
            [0, 20, 40]
        );
        assert_eq!(starts(&outer, 0, 80, JustifyContent::FlexEnd), [20, 40, 60]);
        assert_eq!(starts(&outer, 0, 80, JustifyContent::Center), [10, 30, 50]);
        assert_eq!(
            starts(&outer, 0, 80, JustifyContent::SpaceBetween),
            [0, 30, 60],
            "first at main-start, last at main-end, 10 between each pair"
        );
        // 20 cells over 6 half-slots is 3.33 each: floors to 3 / 6 / 6 / 3 and
        // the 2 cells rounding dropped go to the earliest slots, so the lead
        // becomes 4 and the first inner space 7.
        assert_eq!(
            starts(&outer, 0, 80, JustifyContent::SpaceAround),
            [4, 31, 57]
        );
        assert_eq!(
            starts(&outer, 0, 80, JustifyContent::SpaceEvenly),
            [5, 30, 55],
            "four equal spaces of 5, ends included"
        );
    }

    #[test]
    fn gaps_are_reserved_before_justify_content_divides_anything() {
        // Three 20-cell items and two 8-cell gaps in 80: 16 cells are
        // structure, so only 4 are free. `space-between` puts 2 in each inner
        // slot on top of the gap already there, and the row ends flush.
        let outer = [20, 20, 20];
        assert_eq!(
            starts(&outer, 8, 80, JustifyContent::SpaceBetween),
            [0, 30, 60]
        );
        assert_eq!(starts(&outer, 8, 80, JustifyContent::Center), [2, 30, 58]);
        // A gap never appears before the first item or after the last: with
        // `flex-start` the row starts at 0 and the 4 spare cells stay at the
        // end.
        assert_eq!(
            starts(&outer, 8, 80, JustifyContent::FlexStart),
            [0, 28, 56]
        );
    }

    #[test]
    fn space_between_needs_two_items_to_have_a_between() {
        // One item has no inner slot, so `space-between` leaves it at
        // main-start rather than centring it or pushing it to main-end.
        assert_eq!(starts(&[20], 0, 80, JustifyContent::SpaceBetween), [0]);
        // Two items go to the two edges.
        assert_eq!(
            starts(&[20, 20], 0, 80, JustifyContent::SpaceBetween),
            [0, 60]
        );
        // `space-around` and `space-evenly` do centre a single item — both
        // reduce to one space at each end.
        assert_eq!(starts(&[20], 0, 80, JustifyContent::SpaceAround), [30]);
        assert_eq!(starts(&[20], 0, 80, JustifyContent::SpaceEvenly), [30]);
    }

    #[test]
    fn an_odd_cell_lands_before_the_item_rather_than_after_it() {
        // 5 cells to centre 5 cells of item. A terminal has no half cells, and
        // the rule for the one that is left over is M9.6's: earliest slot
        // first, which here is the lead.
        assert_eq!(starts(&[5], 0, 10, JustifyContent::Center), [3]);
    }

    #[test]
    fn auto_margins_take_the_free_space_before_justify_content_sees_it() {
        // §9.5 step 1 runs first and leaves step 6 nothing. `margin-left: auto`
        // on the last of three items is the nav-bar idiom: everything packs at
        // main-start and the last item is pushed to main-end — even though the
        // container asked for `center`, which would have started the row at 10.
        let placed = place(
            &[
                slot(20, false, false),
                slot(20, false, false),
                slot(20, true, false),
            ],
            0,
            80,
            JustifyContent::Center,
        );
        assert_eq!(
            placed.iter().map(|p| p.main_start).collect::<Vec<_>>(),
            [0, 20, 40]
        );
        assert_eq!(placed[2].auto_start, 20, "the auto margin took all 20");
        assert_eq!(placed[2].auto_end, 0);
    }

    #[test]
    fn free_space_is_split_between_auto_margins_not_between_items() {
        // `margin: 0 auto` on the only item: two auto margins, 30 cells each,
        // which is what centres it.
        let placed = place(&[slot(20, true, true)], 0, 80, JustifyContent::FlexStart);
        assert_eq!(placed[0].main_start, 0);
        assert_eq!((placed[0].auto_start, placed[0].auto_end), (30, 30));

        // One auto margin on each of two items: 60 free cells, two margins, 30
        // each. The split counts *margins*, so an item with two of them would
        // have taken twice what these get.
        let placed = place(
            &[slot(10, true, false), slot(10, true, false)],
            0,
            80,
            JustifyContent::FlexStart,
        );
        assert_eq!(
            placed.iter().map(|p| p.main_start).collect::<Vec<_>>(),
            [0, 40],
            "the first item's margin box is 30 + 10 cells wide"
        );
        assert_eq!(placed[0].auto_start, 30);
        assert_eq!(placed[1].auto_start, 30);
    }

    #[test]
    fn overflow_packs_from_main_start_whatever_the_alignment_asked_for() {
        // Negative free space is not distributed: auto margins are zero (§9.5
        // step 1 only fires when free space is positive) and every packing
        // value falls back to main-start. Centring here would push the first
        // item 20 cells off the main-start edge, where nothing can scroll it
        // back into view.
        for justify in ALL {
            assert_eq!(
                starts(&[40, 40, 40], 0, 80, justify),
                [0, 40, 80],
                "{justify:?}"
            );
        }
        let placed = place(
            &[slot(50, true, true), slot(50, false, false)],
            0,
            80,
            JustifyContent::Center,
        );
        assert_eq!((placed[0].auto_start, placed[0].auto_end), (0, 0));
        assert_eq!(placed[1].main_start, 50);
    }

    #[test]
    fn every_cell_of_the_line_is_accounted_for_at_every_width() {
        // The integer-cell invariant for placement, swept the way
        // `the_line_is_filled_exactly_at_every_width` sweeps sizing: items
        // never overlap, never move backwards, always leave at least the gap
        // between them, and the space handed out is exactly the space there
        // was — no cell invented, none lost to a floor.
        let outer = [7, 13, 5];
        let gap = 2;
        for inner in 20..=120 {
            for justify in ALL {
                let placed = place(&line(&outer), gap, inner, justify);
                let free = inner - outer.iter().sum::<i32>() - gap * 2;
                let lead = placed[0].main_start;
                let between: Vec<i32> = (0..2)
                    .map(|i| placed[i + 1].main_start - placed[i].main_start - outer[i])
                    .collect();
                let end = placed[2].main_start + outer[2];
                let label = format!("inner {inner}, {justify:?}: {placed:?}");
                assert!(
                    between.iter().all(|&b| b >= gap),
                    "{label}: items overlap or ate a gap"
                );
                if free < 0 {
                    // Overflow: nothing to hand out, and nothing handed out.
                    assert_eq!(lead, 0, "{label}");
                    assert_eq!(between, [gap, gap], "{label}");
                    continue;
                }
                assert!(lead >= 0, "{label}");
                // No cell invented: a row with room to spare never reaches past
                // its own edge, however the free space rounded.
                assert!(end <= inner, "{label}: the row overflowed by rounding");
                // No cell lost: `space-between` claims both edges, so it is the
                // value that has to land exactly on the far one.
                if justify == JustifyContent::SpaceBetween {
                    assert_eq!(end, inner, "{label}: space-between left a hole");
                }
                // `space-evenly` claims every space is the same size, which in
                // whole cells means within one of every other.
                if justify == JustifyContent::SpaceEvenly {
                    let spaces = [lead, between[0] - gap, between[1] - gap, inner - end];
                    let lo = spaces.iter().min().copied().unwrap();
                    let hi = spaces.iter().max().copied().unwrap();
                    assert!(hi - lo <= 1, "{label}: uneven spaces {spaces:?}");
                }
            }
        }
    }

    // §9.4 and §9.6, the cross axis (M9.8). `cross_start` counts down from the
    // line's cross-start edge for a row, which is its top.

    /// An item of `outer` cells with no auto margins and no baseline of its
    /// own — the shape almost every item on a real page has.
    fn cross(outer: i32, align: AlignItems) -> CrossItem {
        CrossItem {
            outer,
            baseline: 0,
            align,
            auto_start: false,
            auto_end: false,
        }
    }

    /// Where each item's cross-start margin edge lands in a line of
    /// `line_cross` cells.
    fn cross_starts(items: &[CrossItem], line_cross: i32) -> Vec<i32> {
        cross_place(items, line_cross)
            .iter()
            .map(|p| p.cross_start)
            .collect()
    }

    #[test]
    fn the_line_is_as_tall_as_its_tallest_item() {
        let items = [
            cross(1, AlignItems::Stretch),
            cross(5, AlignItems::FlexStart),
            cross(3, AlignItems::Center),
        ];
        assert_eq!(cross_size(&items), 5);
        assert_eq!(cross_size(&[]), 0, "an empty line has no cross size");
    }

    #[test]
    fn align_items_places_an_item_inside_the_line_four_ways() {
        // One 1-cell item in a 5-cell line: 4 cells to place.
        for (align, start) in [
            (AlignItems::FlexStart, 0),
            (AlignItems::Center, 2),
            (AlignItems::FlexEnd, 4),
            // `stretch` sits at cross-start; the growing is the caller's job,
            // because an item that *cannot* stretch must still land here.
            (AlignItems::Stretch, 0),
        ] {
            assert_eq!(cross_starts(&[cross(1, align)], 5), [start], "{align:?}");
        }
    }

    #[test]
    fn an_odd_cell_lands_above_the_item_rather_than_below_it() {
        // 3 cells to centre 2 cells of item. The extra one goes to the
        // earliest slot — above — which is `split`'s rule everywhere else too.
        assert_eq!(cross_starts(&[cross(2, AlignItems::Center)], 5), [2]);
    }

    #[test]
    fn baseline_alignment_shares_a_row_and_grows_the_line() {
        // A three-row label whose text starts at its own top row, and a box
        // with a cell of border+padding above its single row of text. Their
        // baselines are 0 and 1, so the label drops a row to meet the box.
        let label = CrossItem {
            outer: 3,
            baseline: 0,
            ..cross(3, AlignItems::Baseline)
        };
        let boxed = CrossItem {
            outer: 2,
            baseline: 1,
            ..cross(2, AlignItems::Baseline)
        };
        let line = cross_size(&[label, boxed]);
        // Deepest above the shared row (1, the box's padding) plus deepest
        // below it (3, the label's rows) — one cell more than the tallest item
        // on the line, because the label now hangs a row lower than it did.
        assert_eq!(line, 4);
        assert_eq!(cross_starts(&[label, boxed], line), [1, 0]);
    }

    #[test]
    fn a_baseline_item_pushes_the_line_down_instead_of_escaping_it() {
        // The deepest baseline decides the shared row, so no offset is ever
        // negative — the item with the shallow baseline is the one that moves.
        let deep = CrossItem {
            outer: 4,
            baseline: 3,
            ..cross(4, AlignItems::Baseline)
        };
        let shallow = CrossItem {
            outer: 1,
            baseline: 0,
            ..cross(1, AlignItems::Baseline)
        };
        let line = cross_size(&[shallow, deep]);
        let starts = cross_starts(&[shallow, deep], line);
        assert_eq!(starts, [3, 0]);
        assert!(starts.iter().all(|&s| s >= 0));
        // Both baselines really do land on the same row.
        assert_eq!(starts[0] + shallow.baseline, starts[1] + deep.baseline);
    }

    #[test]
    fn a_baseline_item_shares_the_line_with_items_aligned_other_ways() {
        // The line has to be tall enough for both groups: a 6-cell
        // `flex-start` item beats the baseline group's 1 + 2.
        let plain = cross(6, AlignItems::FlexStart);
        let text = CrossItem {
            outer: 2,
            baseline: 1,
            ..cross(2, AlignItems::Baseline)
        };
        assert_eq!(cross_size(&[plain, text]), 6);
        // …and the baseline item still aligns to the baseline group's row, not
        // to the tall item's top or bottom.
        assert_eq!(cross_starts(&[plain, text], 6), [0, 0]);
    }

    #[test]
    fn an_auto_cross_margin_takes_an_item_out_of_the_baseline_group() {
        // M9.8 review. §9.4 step 8 collects the items whose `align-self` is
        // `baseline` *and whose cross-axis margins are both non-auto*, and the
        // "and" is load-bearing: an auto margin claims the free space before
        // alignment is consulted, so such an item is never placed at the shared
        // row and the line must not be sized as though it were.
        //
        // A padded item whose text starts on row 3, next to a 3-row item with
        // `margin-top: auto`. Counting the second in the group asks for
        // 3 above + 3 below = 6 rows; it belongs in the other group, where it
        // asks for its own 3 — so the line is the first item's 3 + 1 = 4.
        let padded = CrossItem {
            outer: 4,
            baseline: 3,
            ..cross(4, AlignItems::Baseline)
        };
        let pushed = CrossItem {
            outer: 3,
            baseline: 0,
            auto_start: true,
            ..cross(3, AlignItems::Baseline)
        };
        assert_eq!(cross_size(&[padded, pushed]), 4);
        // ...and it really is placed by its margin, not by its baseline: the
        // one free row goes above it.
        let placed = cross_place(&[padded, pushed], 4);
        assert_eq!(placed[0].cross_start, 0);
        assert_eq!(
            (placed[1].cross_start, placed[1].auto_start),
            (0, 1),
            "the auto margin takes the row, and the margin box fills the line"
        );
    }

    #[test]
    fn a_line_too_short_for_its_baselines_still_starts_every_item_inside_it() {
        // M9.8 review. A container that states a cross size smaller than its
        // contents need is the one case where a baseline offset could push an
        // item's *start* edge past the line's end. Every other alignment value
        // packs at cross-start there; baseline does the same, so "no item
        // begins outside the line" holds for all five.
        let deep = CrossItem {
            outer: 6,
            baseline: 5,
            ..cross(6, AlignItems::Baseline)
        };
        let shallow = CrossItem {
            outer: 2,
            baseline: 0,
            ..cross(2, AlignItems::Baseline)
        };
        // With the line the items asked for, the shallow one drops the full 5.
        assert_eq!(cross_size(&[deep, shallow]), 7);
        assert_eq!(cross_starts(&[deep, shallow], 7), [0, 5]);
        // Squeezed into 3 rows, it starts inside the line rather than below it.
        assert_eq!(cross_starts(&[deep, shallow], 3), [0, 1]);
        assert_eq!(cross_starts(&[deep, shallow], 1), [0, 0]);
    }

    #[test]
    fn auto_cross_margins_take_the_free_space_and_override_align_self() {
        // `margin: auto 0` centres the item even though it asked for
        // `flex-start`: §9.6 step 1 runs before the alignment property is read.
        let item = CrossItem {
            auto_start: true,
            auto_end: true,
            ..cross(2, AlignItems::FlexStart)
        };
        let placed = cross_place(&[item], 8);
        assert_eq!(placed[0].cross_start, 0, "the margin box fills the line");
        assert_eq!((placed[0].auto_start, placed[0].auto_end), (3, 3));

        // One auto margin takes all of it: `margin-top: auto` is how an item
        // pins itself to the bottom of a row.
        let item = CrossItem {
            auto_start: true,
            ..cross(2, AlignItems::FlexStart)
        };
        let placed = cross_place(&[item], 8);
        assert_eq!((placed[0].auto_start, placed[0].auto_end), (6, 0));
    }

    #[test]
    fn an_overflowing_item_packs_at_cross_start_however_it_asked_to_align() {
        // Negative free space is not distributed — the same safe fallback the
        // main axis makes, and for a sharper reason: there is no row above the
        // top of the page to scroll back to.
        for align in [
            AlignItems::FlexStart,
            AlignItems::FlexEnd,
            AlignItems::Center,
            AlignItems::Stretch,
            AlignItems::Baseline,
        ] {
            assert_eq!(cross_starts(&[cross(9, align)], 4), [0], "{align:?}");
        }
        let item = CrossItem {
            auto_start: true,
            auto_end: true,
            ..cross(9, AlignItems::FlexStart)
        };
        let placed = cross_place(&[item], 4);
        assert_eq!((placed[0].auto_start, placed[0].auto_end), (0, 0));
    }

    #[test]
    fn no_item_ever_lands_outside_the_line_at_any_size() {
        // The cross-axis counterpart of the main axis's exactness sweep: every
        // item sits inside the line whenever it fits, and never above its
        // cross-start edge even when it does not.
        let items = [
            cross(1, AlignItems::FlexStart),
            cross(3, AlignItems::Center),
            cross(2, AlignItems::FlexEnd),
            CrossItem {
                outer: 4,
                baseline: 2,
                ..cross(4, AlignItems::Baseline)
            },
            CrossItem {
                auto_start: true,
                ..cross(2, AlignItems::Center)
            },
        ];
        for line in 0..=20 {
            for (item, placed) in items.iter().zip(cross_place(&items, line)) {
                let label = format!("line {line}, {item:?} -> {placed:?}");
                assert!(placed.cross_start >= 0, "{label}");
                let outer = item.outer + placed.auto_start + placed.auto_end;
                if outer <= line {
                    assert!(placed.cross_start + outer <= line, "{label}");
                }
            }
        }
    }
}
