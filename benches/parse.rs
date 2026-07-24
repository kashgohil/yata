//! The M2 fast gate (PLAN.md §6): tokenizer + tree builder over the committed
//! Wikipedia fixture must come in under 50 ms. Run: `cargo bench --bench parse`.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn parse_wikipedia(c: &mut Criterion) {
    let html = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/en.wikipedia.org.html"
    ))
    .expect("committed fixture must exist");
    c.bench_function("parse en.wikipedia.org fixture", |b| {
        b.iter(|| yata::html::parse(black_box(&html)))
    });
}

criterion_group!(benches, parse_wikipedia);
criterion_main!(benches);
