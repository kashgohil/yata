//! Layout goldens (PLAN.md M9): fixture HTML → box tree → `x/y/w/h` dump.
//!
//! Every fixture in `tests/fixtures/layout/` is laid out at the width its
//! first line asks for (`<!-- width: 40 -->`, default 80) and compared against
//! a `.boxes` golden. The dump is `inspector::box_lines` — the same text `F3`
//! and `--dump-boxes` print, so a golden always pins something a reviewer can
//! put on screen.
//!
//! **Two tiers, because a regenerable golden cannot pin a spec:**
//!
//! - `fixtures/layout/*.boxes` — *regression* goldens. Generated from the
//!   code with `UPDATE_SNAPSHOTS=1 cargo test --test layout`; worth exactly
//!   one thing, telling a reviewer which outputs moved.
//! - `fixtures/layout/spec/*.boxes` — *spec* goldens. Hand-written from the
//!   CSS before the code exists, with the arithmetic recorded in a `#` header
//!   inside the file. `UPDATE_SNAPSHOTS=1` **refuses** to write these: making
//!   one pass means changing the code, which is the whole point.

use std::fs;
use std::path::{Path, PathBuf};

use yata::headless;
use yata::html;

/// Column width when a fixture's first line does not ask for one.
const DEFAULT_WIDTH: u16 = 80;

const REGENERATE: &str = "UPDATE_SNAPSHOTS=1 cargo test --test layout";

/// Base URL for resolving fixture `src` attributes. Nothing is fetched.
const FIXTURE_BASE: &str = "https://fixture.test/page";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// Regenerable from the code.
    Regression,
    /// Hand-written; regeneration must refuse to touch it.
    Spec,
}

/// What a golden did on this run — the summary a PR pastes instead of prose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    Added,
    Changed,
    Unchanged,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/layout")
}

/// Every `*.html` in a directory, sorted, so the run order (and the summary)
/// is the same on every machine.
fn fixtures_in(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "html"))
        .collect();
    paths.sort();
    paths
}

/// `<!-- width: 40 -->` on line 1, else [`DEFAULT_WIDTH`]. The width lives in
/// the fixture rather than a table in this file so a reviewer reading the HTML
/// can do the arithmetic without leaving it.
fn fixture_width(html: &str) -> u16 {
    let first = html.lines().next().unwrap_or_default();
    let Some(rest) = first.trim().strip_prefix("<!--") else {
        return DEFAULT_WIDTH;
    };
    let Some(body) = rest.strip_suffix("-->") else {
        return DEFAULT_WIDTH;
    };
    let Some(value) = body.trim().strip_prefix("width:") else {
        return DEFAULT_WIDTH;
    };
    value.trim().parse().unwrap_or(DEFAULT_WIDTH)
}

fn fixture_viewport_height(html: &str) -> u16 {
    html.lines()
        .take(2)
        .find_map(|line| {
            line.trim()
                .strip_prefix("<!-- viewport-height:")?
                .strip_suffix("-->")
        })
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1)
}

#[test]
fn grid_text_is_an_anonymous_item_and_absolute_children_do_not_reserve_a_cell() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:1fr 1fr}.abs{position:absolute}</style><div class=g>text<span>next</span><i class=abs>floating</i></div>"#,
        20,
    );
    assert!(dump.contains("grid cols=[10, 10] rows=[1]"), "{dump}");
    assert!(dump.contains("#text \"text\"  x=0 y=0"), "{dump}");
    assert!(dump.contains("#text \"next\"  x=10 y=0"), "{dump}");
}

#[test]
fn inline_grid_keeps_its_own_formatting_root_when_it_is_a_grid_item() {
    let dump = box_dump(
        r#"<style>.outer{display:grid;grid-template-columns:1fr}.inner{display:inline-grid;grid-template-columns:1fr 1fr}</style><div class=outer><span class=inner><b>left</b><b>right</b></span></div>"#,
        20,
    );
    assert_eq!(dump.matches(" grid cols=").count(), 2, "{dump}");
    assert!(dump.contains("#text \"left\"  x=0 y=0"), "{dump}");
    assert!(dump.contains("#text \"right\"  x=10 y=0"), "{dump}");
}

#[test]
fn sticky_grid_item_keeps_its_static_row_space() {
    let dump = box_dump(
        "<!-- viewport-height: 4 -->\n<style>.g{display:grid}.sticky{position:sticky;top:0}p{margin:0}</style><div class=g><p class=sticky>head</p><p>after</p></div>",
        20,
    );
    assert!(dump.contains("#text \"head\"  x=0 y=0"), "{dump}");
    assert!(dump.contains("#text \"after\"  x=0 y=1"), "{dump}");
    assert!(dump.contains("sticky top 0"), "{dump}");
}

#[test]
fn fixed_grid_child_does_not_reserve_a_cell() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:1fr 1fr}.fixed{position:fixed;top:0}p{margin:0}</style><div class=g><p class=fixed>fixed</p><p>flow</p></div>"#,
        20,
    );
    assert!(dump.contains("grid cols=[10, 10] rows=[1]"), "{dump}");
    assert!(dump.contains("#text \"flow\"  x=0 y=0"), "{dump}");
    assert!(dump.contains("fixed viewport"), "{dump}");
}

#[test]
fn one_cell_grid_tracks_handle_cjk_and_long_words_without_bad_geometry() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:1px}p{margin:0}</style><div class=g><p>界界</p><p>unbroken</p></div>"#,
        20,
    );
    // The first auto row contains two width-two CJK glyphs; the second wraps
    // an eight-character word. Both boxes remain finite one-cell tracks.
    assert!(dump.contains("grid cols=[1] rows=[2, 8]"), "{dump}");
    assert!(dump.contains("#text \"界\"  x=0 y=0 w=2 h=1"), "{dump}");
    assert!(dump.contains("#text \"u\"  x=0 y=2 w=1 h=1"), "{dump}");
}

#[test]
fn hostile_grid_sums_stay_bounded_and_never_make_negative_rectangles() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:repeat(256,99999999999em);column-gap:99999999999em}p{margin:0}</style><div class=g><p>a</p><p>b</p></div>"#,
        1,
    );
    let root = dump
        .lines()
        .find(|line| line.contains(" grid cols="))
        .unwrap();
    assert_eq!(
        root.matches(", ").count(),
        255,
        "track list was not bounded: {root}"
    );
    assert!(!dump.contains(" w=-"), "{dump}");
    assert!(!dump.contains(" h=-"), "{dump}");
    assert!(!dump.contains(" x=-"), "{dump}");
    assert!(!dump.contains(" y=-"), "{dump}");
}

#[test]
fn percentage_and_repeat_fr_tracks_resolve_against_the_grid_width() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:25% repeat(2, 1fr)}p{margin:0}</style><div class=g><p>a</p><p>b</p><p>c</p></div>"#,
        20,
    );
    // 25% of 20 is five. The remaining fifteen cells divide deterministically
    // between the two `fr` tracks, with the rounding cell going first.
    assert!(dump.contains("grid cols=[5, 8, 7] rows=[1]"), "{dump}");
    assert!(dump.contains("#text \"b\"  x=5 y=0"), "{dump}");
    assert!(dump.contains("#text \"c\"  x=13 y=0"), "{dump}");
}

#[test]
fn minmax_caps_an_auto_track_without_second_pass_layout() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:minmax(auto, 2em)}p{margin:0}</style><div class=g><p>longword</p></div>"#,
        20,
    );
    // The item's min-content contribution is eight, but `2em` is four cells.
    // It wraps in that final width rather than triggering a track-sizing retry.
    assert!(dump.contains("grid cols=[4] rows=[2]"), "{dump}");
    assert!(dump.contains("#text \"long\"  x=0 y=0 w=4 h=1"), "{dump}");
}

#[test]
fn auto_placement_skips_cells_reserved_by_a_span() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:repeat(3, 1fr)}.span{grid-column:1 / span 2}p{margin:0}</style><div class=g><p class=span>wide</p><p>auto</p></div>"#,
        20,
    );
    // Twenty cells divide 7/7/6. The explicit item reserves columns one and
    // two, so the auto item starts at the third track's x=14.
    assert!(dump.contains("grid cols=[7, 7, 6] rows=[1]"), "{dump}");
    assert!(dump.contains("#text \"auto\"  x=14 y=0"), "{dump}");
}

#[test]
fn row_spans_reserve_every_covered_cell_for_auto_placement() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:1fr 1fr}.tall{grid-row:1 / span 2}p{margin:0}</style><div class=g><p class=tall>tall</p><p>top</p><p>bottom</p></div>"#,
        20,
    );
    assert!(dump.contains("grid cols=[10, 10] rows=[1, 1]"), "{dump}");
    assert!(dump.contains("#text \"top\"  x=10 y=0"), "{dump}");
    assert!(dump.contains("#text \"bottom\"  x=10 y=1"), "{dump}");
}

#[test]
fn row_tracks_use_vertical_units_and_definite_height_for_percent_and_fr() {
    let dump = box_dump(
        r#"<style>.g{display:grid;height:8em;grid-template-columns:1fr;grid-template-rows:32px 25% 1fr}p{margin:0}</style><div class=g><p>a</p><p>b</p><p>c</p></div>"#,
        20,
    );
    // 32px is two terminal lines, 25% of the definite eight-line content
    // height is two, and the fractional row receives the remaining four.
    assert!(dump.contains("grid cols=[20] rows=[2, 2, 4]"), "{dump}");
    assert!(dump.contains("#text \"b\"  x=0 y=2"), "{dump}");
    assert!(dump.contains("#text \"c\"  x=0 y=4"), "{dump}");
}

#[test]
fn indefinite_fractional_and_percentage_rows_keep_content_visible() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-rows:1fr 50%}p{margin:0}</style><div class=g><p>first</p><p>second</p></div>"#,
        20,
    );
    assert!(dump.contains("grid cols=[6] rows=[1, 1]"), "{dump}");
    assert!(dump.contains("#text \"second\"  x=0 y=1"), "{dump}");
}

#[test]
fn end_lines_and_end_anchored_spans_reserve_the_expected_columns() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:repeat(3,1fr)}p{margin:0}.end{grid-column-end:4}.span{grid-column:span 2 / 4}</style><div class=g><p class=end>end</p><p class=span>span</p><p>auto</p></div>"#,
        12,
    );
    assert!(dump.contains("grid cols=[4, 4, 4] rows=[1, 1]"), "{dump}");
    assert!(dump.contains("#text \"end\"  x=8 y=0"), "{dump}");
    assert!(dump.contains("#text \"span\"  x=4 y=1"), "{dump}");
    assert!(dump.contains("#text \"auto\"  x=0 y=0"), "{dump}");
}

#[test]
fn inline_grid_is_atomic_inside_an_inline_formatting_context() {
    let dump = box_dump(
        r#"<style>p{margin:0}.i{display:inline-grid;grid-template-columns:1fr 1fr}</style><p>before <span class=i><b>a</b><b>b</b></span> after</p>"#,
        20,
    );
    assert!(dump.contains("<span.i> grid cols=[1, 1]"), "{dump}");
    // The inline grid is an ordinary child of the line, not a block that
    // forces `after` onto a new row.
    assert!(dump.contains("#text \"after\"  x=9 y=0"), "{dump}");
}

#[test]
fn inline_grid_shrink_to_fit_sums_its_explicit_column_contributions() {
    let dump = box_dump(
        r#"<style>p{margin:0}.i{display:inline-grid;grid-template-columns:auto auto}.item{display:block;margin:0}</style><p>x<span class=i><span class=item>left</span><span class=item>right</span></span>y</p>"#,
        20,
    );
    assert!(dump.contains("<span.i> grid cols=[4, 5]"), "{dump}");
    assert!(dump.contains("#text \"right\"  x=5 y=0"), "{dump}");
    assert!(dump.contains("#text \"y\"  x=10 y=0"), "{dump}");
}

#[test]
fn inline_flex_keeps_its_flex_root_when_it_is_a_grid_item() {
    let dump = box_dump(
        r#"<style>.outer{display:grid;grid-template-columns:1fr}.inner{display:inline-flex}.inner b{flex:1}</style><div class=outer><span class=inner><b>left</b><b>right</b></span></div>"#,
        20,
    );
    assert!(dump.contains("<span.inner> flex row"), "{dump}");
    assert!(dump.contains("#text \"left\"  x=0 y=0"), "{dump}");
    assert!(dump.contains("#text \"right\"  x=10 y=0"), "{dump}");
}

#[test]
fn table_grid_item_uses_the_grid_resolved_cell_width() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:1fr}table{margin:0}</style><div class=g><table><tr><td>left</td><td>right</td></tr></table></div>"#,
        20,
    );
    assert!(dump.contains("<div.g> grid cols=[20]"), "{dump}");
    assert!(dump.contains("table <table>  x=0 y=0 w=20"), "{dump}");
    assert!(dump.contains("#text \"left\"  x=0 y=0"), "{dump}");
}

#[test]
fn form_controls_and_replaced_images_are_ordinary_grid_items() {
    let dump = box_dump(
        r#"<style>.g{display:grid;grid-template-columns:1fr 1fr}</style><div class=g><input value=field><img src=pic.png width=16 height=32 alt=pic></div>"#,
        20,
    );
    assert!(dump.contains("grid cols=[10, 10] rows=[2]"), "{dump}");
    assert!(
        dump.contains("<input …> field value \"field\"  x=1 y=0 w=8 h=1"),
        "{dump}"
    );
    assert!(
        dump.contains("img \"pic\" https://fixture.test/pic.png  x=10 y=0 w=2 h=2"),
        "{dump}"
    );
}

/// The stage under test — the same function `--dump-boxes` prints through, so
/// a golden always pins boxes a reviewer can put on screen with the flag or
/// `F3`. The base URL is a fixed fake: fixtures are offline, and image `src`
/// only has to resolve, not fetch.
fn box_dump(html: &str, width: u16) -> String {
    headless::box_dump_with_viewport(
        &mut html::parse(html),
        Some(FIXTURE_BASE),
        width,
        fixture_viewport_height(html),
    )
}

/// Strip a spec golden's `# ` header: the derivation is for the reader, the
/// box lines are the assertion. A header line is `#` followed by a space or
/// the end of the line, and only counts at the top of the file — so a dumped
/// `#text "…"` box is never mistaken for prose.
fn strip_header(golden: &str) -> &str {
    let mut rest = golden;
    while rest.starts_with("# ") || rest.starts_with("#\n") {
        let end = rest.find('\n').map_or(rest.len(), |i| i + 1);
        rest = &rest[end..];
    }
    rest
}

/// The first line that differs, with both sides and the fixture path — not a
/// 200-line dump a reviewer has to diff by eye.
fn describe_mismatch(golden_path: &Path, tier: Tier, expected: &str, got: &str) -> String {
    let mut lines = expected.lines().zip(got.lines()).enumerate();
    let (n, want, have) = match lines.find(|(_, (a, b))| a != b) {
        Some((i, (a, b))) => (i + 1, a.to_string(), b.to_string()),
        // Same prefix, different length: the first missing/extra line is the
        // difference, and "<missing>" is more honest than an empty string.
        None => {
            let n = expected.lines().count().min(got.lines().count());
            let missing = "<missing>".to_string();
            (
                n + 1,
                expected.lines().nth(n).map_or(missing.clone(), Into::into),
                got.lines().nth(n).map_or(missing, Into::into),
            )
        }
    };
    let advice = match tier {
        Tier::Regression => format!("if this move is intended, regenerate: {REGENERATE}"),
        Tier::Spec => "spec goldens are hand-written: fix the layout, not the golden".into(),
    };
    format!(
        "{}\n  line {n}\n  expected: {want}\n  got:      {have}\n  {advice}",
        golden_path.display()
    )
}

/// Compare one dump against one golden. `update` is the caller's decision, not
/// this function's, so the refusal test can exercise "update requested on a
/// spec golden" without touching the process environment.
fn check_golden(
    golden_path: &Path,
    tier: Tier,
    got: &str,
    update: bool,
) -> Result<Outcome, String> {
    let existing = fs::read_to_string(golden_path).ok();
    let expected = existing.as_deref().map(strip_header);
    let outcome = match expected {
        None => Outcome::Added,
        Some(e) if e == got => Outcome::Unchanged,
        Some(_) => Outcome::Changed,
    };
    if update && tier == Tier::Regression {
        if outcome != Outcome::Unchanged {
            fs::write(golden_path, got)
                .map_err(|e| format!("cannot write {}: {e}", golden_path.display()))?;
        }
        return Ok(outcome);
    }
    match expected {
        // A spec golden is written by hand, so pointing at the regeneration
        // command would send the reader to a command that refuses to write it.
        None => Err(match tier {
            Tier::Regression => format!(
                "missing golden {}\n  create it with: {REGENERATE}",
                golden_path.display()
            ),
            // Deliberately not printing today's output next to this message:
            // a spec golden pasted from the dump is the one thing this tier
            // exists to prevent.
            Tier::Spec => format!(
                "missing spec golden {}\n  write it by hand from the CSS — \
                 regeneration will not create it",
                golden_path.display()
            ),
        }),
        Some(e) if e == got => Ok(Outcome::Unchanged),
        Some(e) => Err(describe_mismatch(golden_path, tier, e, got)),
    }
}

fn run_tier(dir: &Path, tier: Tier, update: bool, report: &mut Vec<String>) -> Vec<String> {
    let mut failures = Vec::new();
    for html_path in fixtures_in(dir) {
        let name = html_path.file_stem().expect("fixture name").to_owned();
        let html = fs::read_to_string(&html_path).expect("fixture must be readable");
        let got = box_dump(&html, fixture_width(&html));
        let golden = html_path.with_extension("boxes");
        match check_golden(&golden, tier, &got, update) {
            // The tier is part of the name in the summary: `spec/text-wrap`
            // and `text-wrap` are different fixtures with the same stem.
            Ok(outcome) => report.push(format!(
                "  {:<9} {}{}",
                format!("{outcome:?}").to_lowercase(),
                if tier == Tier::Spec { "spec/" } else { "" },
                name.to_string_lossy()
            )),
            Err(msg) => failures.push(msg),
        }
    }
    failures
}

#[test]
fn layout_goldens() {
    let update = std::env::var("UPDATE_SNAPSHOTS").is_ok();
    let dir = fixtures_dir();
    let mut report = Vec::new();

    let mut failures = run_tier(&dir, Tier::Regression, update, &mut report);
    // Spec goldens get `update = false` even under UPDATE_SNAPSHOTS=1: they
    // are the tier that bites when behaviour is wrong rather than merely
    // different, and a golden a script can rewrite pins nothing.
    failures.extend(run_tier(&dir.join("spec"), Tier::Spec, false, &mut report));

    if update {
        println!("layout goldens ({REGENERATE}):");
        for line in &report {
            println!("{line}");
        }
        println!("  spec/ left untouched by design");
    }
    assert!(
        failures.is_empty(),
        "{} layout golden(s) failed:\n\n{}\n",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn update_refuses_to_rewrite_a_spec_golden() {
    // A spec golden with a deliberately wrong number: even with the update
    // flag on, the harness must fail and leave the file exactly as it was.
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("wrong-spec.boxes");
    let wrong = "# derivation: deliberately wrong\n<p>  x=0 y=0 w=99 h=1\n";
    fs::write(&path, wrong).unwrap();

    let err = check_golden(&path, Tier::Spec, "<p>  x=0 y=0 w=40 h=1\n", true)
        .expect_err("a wrong spec golden must fail");
    assert!(err.contains("w=99"), "{err}");
    assert!(err.contains("hand-written"), "{err}");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        wrong,
        "the spec golden was rewritten"
    );
}

#[test]
fn a_missing_line_fails_with_the_line_number_and_both_sides() {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("truncated.boxes");
    // The golden is the dump with its second line deleted — the exact damage
    // the PR evidence describes.
    fs::write(&path, "<body>  x=0 y=0 w=40 h=2\n  <p>  x=0 y=1 w=40 h=1\n").unwrap();
    let got = "<body>  x=0 y=0 w=40 h=2\n  <h1>  x=0 y=0 w=40 h=1\n  <p>  x=0 y=1 w=40 h=1\n";

    let err = check_golden(&path, Tier::Regression, got, false).expect_err("must fail");
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("expected: "), "{err}");
    assert!(err.contains("<h1>"), "no `got` side in: {err}");
    assert!(err.contains(REGENERATE), "no regeneration hint in: {err}");
    assert!(
        err.lines().count() <= 5,
        "failure output must stay readable:\n{err}"
    );
}

#[test]
fn regeneration_writes_a_regression_golden_and_reports_it() {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("regen.boxes");
    let _ = fs::remove_file(&path);
    let dump = "<p>  x=0 y=0 w=40 h=1\n";

    assert_eq!(
        check_golden(&path, Tier::Regression, dump, true).unwrap(),
        Outcome::Added
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), dump);
    // A clean re-run is `unchanged` — the property the PR evidence checks with
    // an empty `git diff`.
    assert_eq!(
        check_golden(&path, Tier::Regression, dump, true).unwrap(),
        Outcome::Unchanged
    );
    assert_eq!(
        check_golden(&path, Tier::Regression, "<p>  x=0 y=0 w=41 h=1\n", true).unwrap(),
        Outcome::Changed
    );
}

#[test]
fn width_comes_from_the_first_line_comment() {
    assert_eq!(fixture_width("<!-- width: 24 -->\n<p>x</p>"), 24);
    assert_eq!(fixture_width("<!--width:100-->\n<p>x</p>"), 100);
    assert_eq!(fixture_width("<p>no comment</p>"), DEFAULT_WIDTH);
    assert_eq!(fixture_width("<!-- a note -->\n<p>x</p>"), DEFAULT_WIDTH);
    // A width comment on any line but the first is prose, not configuration.
    assert_eq!(fixture_width("<p>x</p>\n<!-- width: 24 -->"), DEFAULT_WIDTH);
}

#[test]
fn spec_headers_are_stripped_but_text_boxes_survive() {
    assert_eq!(strip_header("# why\n# more\n<p>  x=0\n"), "<p>  x=0\n");
    // Hash-space is the marker: a top-level `#text` box is content, not header.
    let text_box = "#text \"hi\"  x=0 y=0 w=2 h=1\n";
    assert_eq!(strip_header(text_box), text_box);
    assert_eq!(
        strip_header("<p>  x=0\n# not a header\n"),
        "<p>  x=0\n# not a header\n"
    );
}
