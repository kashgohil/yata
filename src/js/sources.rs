//! Where a page's script comes from: `<script>` elements in document order
//! (M10.2), modelled on `style/sources.rs`.
//!
//! A pure DOM walk — it reads the tree and returns descriptions. It does not
//! execute anything, resolve a URL or fetch a byte; `js::run_pass` runs what
//! this returns and M10.10 fills in the external slots.
//!
//! One ordered list rather than two, for the same reason the stylesheet walk
//! keeps one: execution order *is* document order. A `<script src>` that this
//! task cannot run yet still occupies its slot, so the inline scripts around
//! it keep their positions — and their names — the day M10.10 starts filling
//! it in.

use crate::dom::{Dom, NodeData, NodeId};

/// One `<script>` the document asks for, in the order it asks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Script {
    /// An inline script's text, already in hand.
    Inline { name: String, source: String },
    /// A `<script src>`'s URL, exactly as the page wrote it. Recognised so its
    /// slot exists; M10.10 fetches and runs it.
    External { src: String },
    /// A `<script>` this engine does not run, and why. Reported rather than
    /// dropped silently: to a reader, "nothing happened" and "we ignored what
    /// the page asked for" look identical, and M10.7's console pane is where
    /// the difference belongs. It takes no execution slot, so the `inline#N`
    /// names either side of it are unchanged.
    Skipped { name: String, reason: String },
}

impl Script {
    /// What errors and `--dump-js` call this script. Inline scripts have no
    /// name of their own, so they take their document-order slot: `inline#2`
    /// is the second `<script>` in the document, counting external ones, which
    /// is what keeps the name stable when M10.10 starts running those.
    pub fn name(&self) -> &str {
        match self {
            Script::Inline { name, .. } => name,
            Script::External { src } => src,
            Script::Skipped { name, .. } => name,
        }
    }
}

/// The MIME types that mean "this is a classic script, run it". Anything else
/// — `module`, `application/json`, `text/template`, an importmap — is data or
/// a language we do not implement, and is skipped here rather than at
/// execution time, so that what is skipped is decided in exactly one place.
///
/// `module` is deliberately absent: ES modules defer, have their own scope and
/// need a loader, so running one as a classic script would be wrong rather
/// than merely incomplete.
const CLASSIC_TYPES: [&str; 6] = [
    "text/javascript",
    "application/javascript",
    "application/ecmascript",
    "text/ecmascript",
    "application/x-javascript",
    "text/jscript",
];

/// Containers the walk refuses to descend into. Not a matter of *type* — the
/// elements inside are real `<script>`s — so the walk has to refuse.
///
/// `<template>` holds a parse of something the page may clone later; a browser
/// keeps it in a separate fragment where nothing executes. `<noscript>`, with
/// scripting enabled, is not parsed as markup at all — it is raw text — so
/// there is no script element in there to find. Our tokenizer does parse it as
/// markup, so this is where the same result is reached. Getting this wrong
/// would make the engine both hide a page's no-JS fallback (M10.2's UA rule)
/// *and* run the script inside it: the worst of both.
///
/// One list, read by both the document walk and [`connected_script`], because
/// M11.5's dynamic path has to reach the same answer as the parsed one rather
/// than hold its own opinion about where a script does not run.
const INERT: [&str; 2] = ["template", "noscript"];

/// Every script the document asks for, in the order it asks, **with the
/// element that asked**.
///
/// The node travels with the description because "this element has already
/// been accounted for" is the DOM's own rule for never running a script twice
/// (M11.5), and this walk is the only place that knows which element a slot
/// came from. Without it a page that moved one of its own `<script>` elements
/// would re-run it.
pub fn sources(dom: &Dom) -> Vec<(NodeId, Script)> {
    let mut out = Vec::new();
    collect(dom, dom.root, &mut out);
    out
}

fn collect(dom: &Dom, node: NodeId, out: &mut Vec<(NodeId, Script)>) {
    if let NodeData::Element { tag, .. } = &dom.node(node).data {
        if INERT.contains(&tag.as_str()) {
            return;
        }
        if tag == "script" {
            // Skipped scripts hold no slot, so this counts the ones that can
            // actually run.
            let position = out
                .iter()
                .filter(|(_, s)| !matches!(s, Script::Skipped { .. }))
                .count()
                + 1;
            out.push((node, describe(dom, node, &format!("inline#{position}"))));
            // A script's children are its text, never more elements.
            return;
        }
    }
    for child in dom.children(node) {
        collect(dom, child, out);
    }
}

/// What one `<script>` element asks for. The single place that decision is
/// made: the document walk above and the dynamic path below must not be able
/// to disagree about whether an element is a classic script, or about whether
/// its `src` beats its text.
///
/// `name` is what an *inline* script will be called. It is the caller's
/// business because the two callers number them differently — the document
/// walk by document position, the queue by insertion order — and neither
/// numbering is knowable from one element.
fn describe(dom: &Dom, node: NodeId, name: &str) -> Script {
    match script_type(dom, node) {
        Some(unrun) => Script::Skipped {
            name: format!("<script type={unrun}>"),
            reason: format!("not run: `{unrun}` is not a classic script"),
        },
        None => match dom.attr(node, "src").filter(|src| !src.trim().is_empty()) {
            // `src` wins: a script element with one ignores its own inline
            // text, exactly as the HTML spec says.
            Some(src) => Script::External {
                src: src.to_string(),
            },
            None => Script::Inline {
                name: name.to_string(),
                source: text_of(dom, node),
            },
        },
    }
}

/// What a `<script>` element asks for **now that it is part of the document**
/// (M11.5) — the dynamic counterpart of [`sources`], for one node the engine
/// has been told about rather than a tree it walks.
///
/// `None` means there is nothing here to run, for one of three reasons that
/// are deliberately answered in the same place: the node is not a `<script>`
/// element at all, it is not connected to the document (a page can build a
/// whole subtree before inserting it, and until then a browser runs nothing
/// in it), or it sits inside one of [`INERT`]'s containers. That last one is
/// the reason this function exists rather than a tag check at the call site:
/// `<template>` and `<noscript>` are decided *here*, once, for both paths.
///
/// A `Some(Script::Skipped)` is a `<script>` we will not run and can say why —
/// the same answer the document walk gives, reported by the same code in the
/// queue.
pub fn connected_script(dom: &Dom, node: NodeId, name: &str) -> Option<Script> {
    let NodeData::Element { tag, .. } = &dom.node(node).data else {
        return None;
    };
    if tag != "script" {
        return None;
    }
    // One walk answers both questions, because they are the same walk: is the
    // document up there, and is anything inert in between. Bounded by
    // `dom::MAX_DEPTH`, so this cannot become a long climb.
    let mut walk = dom.node(node).parent;
    while let Some(up) = walk {
        if up == dom.root {
            return Some(describe(dom, node, name));
        }
        if let NodeData::Element { tag, .. } = &dom.node(up).data
            && INERT.contains(&tag.as_str())
        {
            return None;
        }
        walk = dom.node(up).parent;
    }
    None
}

/// `<script>` content is raw text to the tokenizer, so this is one text child
/// in practice; concatenating is what makes it not depend on that.
fn text_of(dom: &Dom, node: NodeId) -> String {
    let mut source = String::new();
    for child in dom.children(node) {
        if let NodeData::Text(text) = &dom.node(child).data {
            source.push_str(text);
        }
    }
    source
}

/// The `type` this engine will not run, or `None` when the element is a
/// classic script. An absent or empty `type` means JavaScript; otherwise the
/// value must be a JavaScript MIME type, compared without its parameters
/// (`text/javascript; charset=utf-8`) and case-insensitively.
fn script_type(dom: &Dom, node: NodeId) -> Option<String> {
    let ty = dom.attr(node, "type")?;
    let essence = ty.split(';').next().unwrap_or("").trim();
    let classic = essence.is_empty()
        || CLASSIC_TYPES
            .iter()
            .any(|known| essence.eq_ignore_ascii_case(known));
    (!classic).then(|| essence.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    /// The descriptions alone. The elements they came from are the dynamic
    /// path's business (M11.5); every test below is about what the document
    /// asks for and in what order.
    fn of(src: &str) -> Vec<Script> {
        sources(&html::parse(src))
            .into_iter()
            .map(|(_, script)| script)
            .collect()
    }

    fn inline(name: &str, source: &str) -> Script {
        Script::Inline {
            name: name.to_string(),
            source: source.to_string(),
        }
    }

    fn external(src: &str) -> Script {
        Script::External {
            src: src.to_string(),
        }
    }

    fn skipped(ty: &str) -> Script {
        Script::Skipped {
            name: format!("<script type={ty}>"),
            reason: format!("not run: `{ty}` is not a classic script"),
        }
    }

    #[test]
    fn scripts_come_out_in_document_order() {
        assert_eq!(
            of("<script>a()</script><p>x</p><script>b()</script>"),
            vec![inline("inline#1", "a()"), inline("inline#2", "b()")]
        );
    }

    #[test]
    fn an_external_script_holds_its_slot_without_disturbing_the_order() {
        // M10.10 fills the middle slot. Until it does, the inline scripts on
        // either side must keep both their order and their names — a name that
        // shifted when external scripts started running would silently
        // invalidate every error message and dump that mentions one.
        assert_eq!(
            of("<script>a()</script><script src=lib.js></script><script>c()</script>"),
            vec![
                inline("inline#1", "a()"),
                external("lib.js"),
                inline("inline#3", "c()")
            ]
        );
    }

    #[test]
    fn a_script_with_src_ignores_its_inline_text() {
        assert_eq!(
            of("<script src=lib.js>never_runs()</script>"),
            vec![external("lib.js")]
        );
        // An empty `src` is not a URL; the element falls back to its text.
        assert_eq!(
            of("<script src=''>a()</script>"),
            vec![inline("inline#1", "a()")]
        );
    }

    #[test]
    fn only_classic_script_types_run() {
        assert_eq!(of("<script>a()</script>"), vec![inline("inline#1", "a()")]);
        assert_eq!(
            of("<script type=''>a()</script>"),
            vec![inline("inline#1", "a()")]
        );
        assert_eq!(
            of("<script type='text/javascript'>a()</script>"),
            vec![inline("inline#1", "a()")]
        );
        assert_eq!(
            of("<script type='TEXT/JavaScript; charset=utf-8'>a()</script>"),
            vec![inline("inline#1", "a()")]
        );
        assert_eq!(
            of("<script type='application/javascript'>a()</script>"),
            vec![inline("inline#1", "a()")]
        );

        // Not classic scripts: data, templates, and modules. They are
        // *reported* rather than dropped — M10.7 shows them in the console, so
        // a page whose script never ran can say why.
        assert_eq!(
            of("<script type=module>a()</script>"),
            vec![skipped("module")]
        );
        assert_eq!(
            of("<script type='application/json'>{}</script>"),
            vec![skipped("application/json")]
        );
        assert_eq!(
            of("<script type='text/template'><p>x</p></script>"),
            vec![skipped("text/template")]
        );
        assert_eq!(
            of("<script type='importmap'>{}</script>"),
            vec![skipped("importmap")]
        );
    }

    #[test]
    fn a_skipped_type_still_shifts_the_names_of_what_follows() {
        // The slot numbering counts `<script>` elements we run, not elements
        // in the document: a JSON blob is not a script, so it takes no slot.
        assert_eq!(
            of("<script type='application/json'>{}</script><script>a()</script>"),
            vec![skipped("application/json"), inline("inline#1", "a()")]
        );
    }

    #[test]
    fn an_empty_script_is_still_a_script() {
        // It does nothing, but dropping it would renumber everything after it.
        assert_eq!(
            of("<script></script><script>a()</script>"),
            vec![inline("inline#1", ""), inline("inline#2", "a()")]
        );
    }

    #[test]
    fn script_inside_an_inert_container_never_runs() {
        // `<template>` contents are a parse the page may clone later, not part
        // of the document; a browser never executes them.
        assert_eq!(of("<template><script>a()</script></template>"), vec![]);
        // `<noscript>` is the page's fallback *for a client that does not run
        // scripts*. This one does — M10.2 hides the element for exactly that
        // reason — so running the script inside it would be incoherent: the
        // reader would lose the fallback and get its side effects anyway.
        assert_eq!(of("<noscript><script>a()</script></noscript>"), vec![]);

        // And an inert container takes no slot, so it does not renumber the
        // scripts that really do run.
        assert_eq!(
            of("<script>a()</script><noscript><script>b()</script></noscript><script>c()</script>"),
            vec![inline("inline#1", "a()"), inline("inline#2", "c()")]
        );
    }

    #[test]
    fn scripts_are_found_wherever_they_sit() {
        assert_eq!(
            of("<head><script>h()</script></head><body><div><script>d()</script></div></body>"),
            vec![inline("inline#1", "h()"), inline("inline#2", "d()")]
        );
    }

    // ---- the dynamic path (M11.5) ----

    /// `connected_script` for the first element with `id`, or for the node id
    /// given directly when the fixture has no id to name it by.
    fn connected(html: &str, id: &str) -> Option<Script> {
        let dom = html::parse(html);
        let node = (0..dom.node_count() as u32)
            .map(NodeId)
            .find(|&n| dom.attr(n, "id") == Some(id))
            .expect("the fixture has no such element");
        connected_script(&dom, node, "inserted#1")
    }

    #[test]
    fn a_connected_script_element_asks_for_the_same_thing_the_walk_says_it_does() {
        assert_eq!(
            connected("<script id=s>a()</script>", "s"),
            Some(inline("inserted#1", "a()"))
        );
        assert_eq!(
            connected("<script id=s src=lib.js></script>", "s"),
            Some(external("lib.js"))
        );
        // The same `type` rule, from the same code: a module is reported, not
        // silently run as a classic script.
        assert_eq!(
            connected("<script id=s type=module>a()</script>", "s"),
            Some(skipped("module"))
        );
        // Not a script element at all.
        assert_eq!(connected("<p id=s>x</p>", "s"), None);
    }

    #[test]
    fn a_script_inside_an_inert_container_is_not_a_connected_script() {
        // The must-not the parsed walk already answers: the dynamic path has
        // to reach the same answer rather than having its own opinion, so it
        // reads the same `INERT` list through the same climb.
        assert_eq!(
            connected("<template><script id=s>a()</script></template>", "s"),
            None
        );
        assert_eq!(
            connected("<noscript><script id=s>a()</script></noscript>", "s"),
            None
        );
        // Nested deeper than one level: the climb, not a parent check.
        assert_eq!(
            connected(
                "<template><div><script id=s>a()</script></div></template>",
                "s"
            ),
            None
        );
    }

    #[test]
    fn a_script_that_is_not_in_the_document_is_not_a_connected_script() {
        let mut dom = html::parse("<p>page</p>");
        let orphan = dom.create_element("script", vec![]);
        assert_eq!(connected_script(&dom, orphan, "inserted#1"), None);

        // Appended into a *detached* subtree: still nothing to run, because
        // the climb never reaches the document.
        let holder = dom.create_element("div", vec![]);
        dom.append(holder, orphan).unwrap();
        assert_eq!(connected_script(&dom, orphan, "inserted#1"), None);

        // And the moment the holder joins the document, it is.
        dom.append(dom.root, holder).unwrap();
        assert_eq!(
            connected_script(&dom, orphan, "inserted#1"),
            Some(inline("inserted#1", ""))
        );
    }

    #[test]
    fn wikipedia_asks_for_a_pile_of_inline_scripts() {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/en.wikipedia.org.html"
        ));
        let found = sources(&html::parse(fixture));
        let inlines = found
            .iter()
            .filter(|(_, s)| matches!(s, Script::Inline { .. }))
            .count();
        assert!(inlines > 0, "the fixture carries inline script");
        // Whatever the mix, names are unique — they identify a slot.
        let mut names: Vec<&str> = found.iter().map(|(_, s)| s.name()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "two scripts share a name");
    }
}
