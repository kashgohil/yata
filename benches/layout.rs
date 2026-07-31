//! M5 fast gate: layout alone on the Wikipedia fixture must stay under 100 ms.
//! Full pipeline on danluu.com is also timed (gate < 50 ms).
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
}

criterion_group!(benches, layout_benches);
criterion_main!(benches);
