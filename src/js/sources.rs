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

/// Every script the document asks for, in the order it asks.
pub fn sources(dom: &Dom) -> Vec<Script> {
    let mut out = Vec::new();
    collect(dom, dom.root, &mut out);
    out
}

fn collect(dom: &Dom, node: NodeId, out: &mut Vec<Script>) {
    if let NodeData::Element { tag, .. } = &dom.node(node).data
        && tag == "script"
    {
        if runs_as_classic_script(dom, node) {
            match dom.attr(node, "src").filter(|src| !src.trim().is_empty()) {
                // `src` wins: a script element with one ignores its own inline
                // text, exactly as the HTML spec says.
                Some(src) => out.push(Script::External {
                    src: src.to_string(),
                }),
                None => out.push(Script::Inline {
                    name: format!("inline#{}", out.len() + 1),
                    source: text_of(dom, node),
                }),
            }
        }
        // A script's children are its text, never more elements.
        return;
    }
    for child in dom.children(node) {
        collect(dom, child, out);
    }
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

/// An absent or empty `type` means JavaScript; otherwise the value must be a
/// JavaScript MIME type, compared without its parameters
/// (`text/javascript; charset=utf-8`) and case-insensitively.
fn runs_as_classic_script(dom: &Dom, node: NodeId) -> bool {
    let Some(ty) = dom.attr(node, "type") else {
        return true;
    };
    let essence = ty.split(';').next().unwrap_or("").trim();
    essence.is_empty()
        || CLASSIC_TYPES
            .iter()
            .any(|known| essence.eq_ignore_ascii_case(known))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn of(src: &str) -> Vec<Script> {
        sources(&html::parse(src))
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

        // Not classic scripts: data, templates, and modules.
        assert_eq!(of("<script type=module>a()</script>"), vec![]);
        assert_eq!(of("<script type='application/json'>{}</script>"), vec![]);
        assert_eq!(of("<script type='text/template'><p>x</p></script>"), vec![]);
        assert_eq!(of("<script type='importmap'>{}</script>"), vec![]);
    }

    #[test]
    fn a_skipped_type_still_shifts_the_names_of_what_follows() {
        // The slot numbering counts `<script>` elements we run, not elements
        // in the document: a JSON blob is not a script, so it takes no slot.
        assert_eq!(
            of("<script type='application/json'>{}</script><script>a()</script>"),
            vec![inline("inline#1", "a()")]
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
    fn scripts_are_found_wherever_they_sit() {
        assert_eq!(
            of("<head><script>h()</script></head><body><div><script>d()</script></div></body>"),
            vec![inline("inline#1", "h()"), inline("inline#2", "d()")]
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
            .filter(|s| matches!(s, Script::Inline { .. }))
            .count();
        assert!(inlines > 0, "the fixture carries inline script");
        // Whatever the mix, names are unique — they identify a slot.
        let mut names: Vec<&str> = found.iter().map(Script::name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "two scripts share a name");
    }
}
