//! Flexbox's core arithmetic: css-flexbox-1 §9.7, *resolve the flexible
//! lengths*, in whole terminal cells (PLAN.md M9, task M9.6).
//!
//! Kept out of `engine` deliberately. This is the part of flex layout that is
//! pure arithmetic over numbers — no DOM, no styles, no boxes — so it can be
//! read against the spec's pseudocode line by line and tested the same way.
//! `engine` decides what the items *are* (§4) and where their resolved sizes
//! go; everything between those two ends is here.
//!
//! **Whole cells.** The spec distributes fractions of a pixel and lets the
//! rasteriser sort it out; a terminal has no such luxury, so the fractions are
//! carried through the algorithm in `f64` and only quantized at the end, where
//! the rule is: floor every size, then hand the leftover cells to the earliest
//! items in main-axis order. That keeps two invariants a reader can see —
//! items that grow to fill a line leave no hole at its end, and no item is
//! ever a fraction of a cell narrower than its neighbour for no reason.

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
}
