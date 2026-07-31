//! The M4 fast gate (PLAN.md §6): a full restyle of the Wikipedia fixture must
//! come in under 100 ms, and the rule index must be shown to beat the naive
//! matcher rather than asserted to. Run: `cargo bench --bench style`.
//!
//! Everything the stage does not own — reading the fixture, parsing the HTML,
//! parsing the CSS — happens outside the measured loop. What is left is exactly
//! what `App::restyle` runs when a stylesheet lands.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;

use yata::css::Stylesheet;
use yata::dom::{Dom, NodeData, NodeId};
use yata::style::matching::RuleIndex;
use yata::style::{sources, style_tree, ua_stylesheet};

fn page() -> (Dom, Vec<Stylesheet>) {
    let html = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/en.wikipedia.org.html"
    ))
    .expect("committed fixture must exist");
    let dom = yata::html::parse(&html);
    let sheets = sources::inline_sheets(&dom);
    (dom, sheets)
}

fn elements(dom: &Dom) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![dom.root];
    while let Some(id) = stack.pop() {
        if matches!(dom.node(id).data, NodeData::Element { .. }) {
            out.push(id);
        }
        stack.extend(dom.children(id));
    }
    out
}

/// The gate: DOM + stylesheets → computed values for every node.
fn restyle(c: &mut Criterion) {
    let (dom, sheets) = page();
    let refs: Vec<&Stylesheet> = sheets.iter().collect();
    c.bench_function("restyle en.wikipedia.org", |b| {
        b.iter(|| style_tree(black_box(&dom), black_box(&refs)))
    });
}

/// The index against the matcher it replaces, over the same elements and the
/// same rules, so the payoff is a ratio rather than a claim (PLAN.md §4: "feel
/// the difference"). The naive side gets a small sample count because it is
/// slow by construction — that is the point of the comparison, not a reason to
/// spend minutes measuring it.
fn matching(c: &mut Criterion) {
    let (dom, sheets) = page();
    let mut refs: Vec<&Stylesheet> = vec![ua_stylesheet()];
    refs.extend(sheets.iter());
    let index = RuleIndex::build(&refs);
    let elements = elements(&dom);
    println!(
        "matching {} elements against {} selectors",
        elements.len(),
        index.candidate_count()
    );

    c.bench_function("match all elements: rule index", |b| {
        b.iter(|| {
            let mut hits = 0;
            for &node in &elements {
                hits += index
                    .matches(&dom, node, &yata::style::StyleContext::default())
                    .len();
            }
            black_box(hits)
        })
    });

    let mut slow = c.benchmark_group("naive");
    slow.sample_size(10)
        .measurement_time(Duration::from_secs(10));
    slow.bench_function("match all elements: every rule", |b| {
        b.iter(|| {
            let mut hits = 0;
            for &node in &elements {
                hits += index
                    .matches_naive(&dom, node, &yata::style::StyleContext::default())
                    .len();
            }
            black_box(hits)
        })
    });
    slow.finish();
}

/// The same comparison against a stylesheet the size real sites ship.
///
/// Wikipedia's committed fixture carries only its inline blocks — 291
/// selectors — while the sheets it links from `load.php` run to thousands, and
/// nearly all of those rules are for pages, skins and widgets this article
/// never uses. That is the shape being modelled here: 2 000 rules whose
/// classes appear nowhere in the document, on top of the page's real sheets.
///
/// (Duplicating the real sheets instead would be the wrong experiment — it
/// multiplies the rules that *do* match, so both curves grow together and the
/// index looks no better. Measured: at 10x duplication, index 391 ms and naive
/// 601 ms, both ~10x their baseline.)
fn matching_at_scale(c: &mut Criterion) {
    let (dom, sheets) = page();
    let filler: String = (0..2000)
        .map(|i| format!(".yata-absent-{i} {{ color: red }}\n"))
        .collect();
    let filler = yata::css::parse(&filler);
    let mut refs: Vec<&Stylesheet> = vec![ua_stylesheet(), &filler];
    refs.extend(sheets.iter());
    let index = RuleIndex::build(&refs);
    let elements = elements(&dom);
    println!(
        "at scale: {} elements against {} selectors",
        elements.len(),
        index.candidate_count()
    );

    let mut group = c.benchmark_group("2000 unmatched rules");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(15));
    group.bench_function("rule index", |b| {
        b.iter(|| {
            let mut hits = 0;
            for &node in &elements {
                hits += index
                    .matches(&dom, node, &yata::style::StyleContext::default())
                    .len();
            }
            black_box(hits)
        })
    });
    group.bench_function("every rule", |b| {
        b.iter(|| {
            let mut hits = 0;
            for &node in &elements {
                hits += index
                    .matches_naive(&dom, node, &yata::style::StyleContext::default())
                    .len();
            }
            black_box(hits)
        })
    });
    group.finish();
}

criterion_group!(benches, restyle, matching, matching_at_scale);
criterion_main!(benches);
