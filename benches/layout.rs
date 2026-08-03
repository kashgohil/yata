//! M5 fast gate: layout alone on the Wikipedia fixture must stay under 100 ms.
//! Full pipeline on danluu.com is also timed (gate < 50 ms).
//!
//! M9 adds a third: a page that is mostly flex. The ladder pages use flex in
//! places (danluu's index, Wikipedia's image montages) but neither is dominated
//! by it, so neither would show the cost of the flex path moving. The deck
//! below is built in code rather than committed as a fixture so that the same
//! bench can be run against a pre-M9 checkout — where it measures the same DOM
//! laid out as plain blocks, which is exactly the before/after this milestone
//! owes.
//!
//! Run: `cargo bench --bench layout`

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use yata::html;
use yata::layout::{self, Hidden};
use yata::style;

fn wiki_dom_styles() -> (yata::dom::Dom, yata::style::Styles) {
    let html = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/en.wikipedia.org.html"
    ))
    .unwrap();
    let dom = html::parse(&html);
    let sheets = style::sources::inline_sheets(&dom);
    let refs: Vec<_> = sheets.iter().collect();
    let styles = style::style_tree(&dom, &refs);
    (dom, styles)
}

fn danluu_pipeline() {
    let html = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/danluu.com.html"
    ))
    .unwrap();
    let dom = html::parse(&html);
    let sheets = style::sources::inline_sheets(&dom);
    let refs: Vec<_> = sheets.iter().collect();
    let styles = style::style_tree(&dom, &refs);
    let tree = layout::layout_document(&dom, &styles, 80, Hidden::Respect);
    let _list = yata::paint::paint(&tree);
    let _lines = layout::lines_from_tree(&tree);
}

/// A card deck: a wrapping flex row of 300 flex columns, each with a
/// space-between header row inside it. Every part of M9's hot path is on it —
/// item generation, intrinsic measurement of each item, line collection with a
/// gap, grow and shrink, and a nested container per card.
fn flex_heavy_html() -> String {
    let mut s = String::from(
        "<!doctype html><html><head><style>\
         body { margin: 0 } div, p { margin: 0 }\
         .deck { display: flex; flex-wrap: wrap; gap: 8px }\
         .card { display: flex; flex-direction: column; flex: 1 1 160px }\
         .head { display: flex; justify-content: space-between }\
         .tag { flex: 0 0 48px }\
         </style></head><body><div class=\"deck\">",
    );
    for i in 0..300 {
        s.push_str(&format!(
            "<div class=\"card\"><div class=\"head\"><span class=\"tag\">t{i}</span>\
             <span>card title {i}</span></div>\
             <p>a line of body text long enough to need measuring and breaking</p></div>"
        ));
    }
    s.push_str("</div></body></html>");
    s
}

fn layout_benches(c: &mut Criterion) {
    let (dom, styles) = wiki_dom_styles();
    c.bench_function("layout en.wikipedia.org 80-col", |b| {
        b.iter(|| {
            black_box(layout::layout_document(
                black_box(&dom),
                black_box(&styles),
                80,
                Hidden::Respect,
            ))
        })
    });

    c.bench_function("full pipeline danluu.com (parse+style+layout+paint)", |b| {
        b.iter(|| {
            danluu_pipeline();
            black_box(())
        })
    });

    let flex_src = flex_heavy_html();
    let flex_dom = html::parse(&flex_src);
    let flex_sheets = style::sources::inline_sheets(&flex_dom);
    let flex_refs: Vec<_> = flex_sheets.iter().collect();
    let flex_styles = style::style_tree(&flex_dom, &flex_refs);
    c.bench_function("layout flex deck 80-col", |b| {
        b.iter(|| {
            black_box(layout::layout_document(
                black_box(&flex_dom),
                black_box(&flex_styles),
                80,
                Hidden::Respect,
            ))
        })
    });
}

criterion_group!(benches, layout_benches);
criterion_main!(benches);
