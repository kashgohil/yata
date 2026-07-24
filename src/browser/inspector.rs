//! The `F1` DOM inspector's tree-to-lines transform: a parsed `Dom` becomes
//! one compact, indented line per node. Pure text — scrolling and drawing are
//! the `App`'s job, through the same `Viewport` machinery the page uses.
//!
//! This is deliberately terser than `html::debug_tree` (which `--dump-dom`
//! prints): the inspector is read on a live terminal, so ids/classes are
//! summarized CSS-style and long text is truncated. Snippet caps are measured
//! in cells with `unicode-width` (CLAUDE.md), never chars or bytes.

use unicode_width::UnicodeWidthChar;

use crate::dom::{Dom, NodeData, NodeId};

/// Cell caps for the variable-length parts of a line. Text gets the most room
/// (it is the content); URLs and comments are context, not content.
const TEXT_CAP: usize = 60;
const ATTR_CAP: usize = 40;

/// Render the whole tree, one node per line, two spaces of indent per depth
/// (the same shape as `debug_tree`, so the two views read alike).
pub fn tree_lines(dom: &Dom) -> Vec<String> {
    let mut out = Vec::new();
    push_node(dom, dom.root, 0, &mut out);
    out
}

fn push_node(dom: &Dom, id: NodeId, depth: usize, out: &mut Vec<String>) {
    let mut line = "  ".repeat(depth);
    match &dom.node(id).data {
        NodeData::Document => line.push_str("#document"),
        NodeData::Doctype(s) => {
            line.push_str("<!doctype ");
            line.push_str(&clip(s, ATTR_CAP));
            line.push('>');
        }
        NodeData::Comment(s) => {
            line.push_str("<!-- ");
            line.push_str(&clip(s, ATTR_CAP));
            line.push_str(" -->");
        }
        NodeData::Text(s) => {
            line.push_str("#text \"");
            line.push_str(&clip(s, TEXT_CAP));
            line.push('"');
        }
        NodeData::Element { tag, attrs } => line.push_str(&element_summary(tag, attrs)),
    }
    out.push(line);
    for child in dom.children(id) {
        push_node(dom, child, depth + 1, out);
    }
}

/// CSS-flavored element summary: `<a#nav.cls href="…">`. `id` and `class`
/// fold into the selector-like head; `href`/`src` (the attributes worth
/// reading in a tree) show truncated values; anything further is elided to a
/// single `…` so a soup of data-attributes can't drown the structure.
fn element_summary(tag: &str, attrs: &[(String, String)]) -> String {
    let mut s = String::from("<");
    s.push_str(tag);
    let mut elided = false;
    for (k, v) in attrs {
        match k.as_str() {
            "id" => {
                s.push('#');
                s.push_str(&clip(v, ATTR_CAP));
            }
            "class" => {
                for class in v.split_whitespace() {
                    s.push('.');
                    s.push_str(&clip(class, ATTR_CAP));
                }
            }
            "href" | "src" => {
                s.push(' ');
                s.push_str(k);
                s.push_str("=\"");
                s.push_str(&clip(v, ATTR_CAP));
                s.push('"');
            }
            _ => elided = true,
        }
    }
    if elided {
        s.push_str(" …");
    }
    s.push('>');
    s
}

/// Trim, collapse whitespace runs (raw text keeps its newlines and tabs; a
/// tree line must not), and truncate at `cap` cells, appending `…` when
/// anything was cut.
fn clip(s: &str, cap: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    let mut in_ws = false;
    for ch in s.trim().chars() {
        let ch = if ch.is_whitespace() {
            if in_ws {
                continue;
            }
            in_ws = true;
            ' '
        } else {
            in_ws = false;
            ch
        };
        let w = ch.width().unwrap_or(0);
        if width + w > cap {
            // The ellipsis lives *inside* the cap: drop kept chars until it
            // fits, so a clipped result is never wider than `cap` cells.
            while width + 1 > cap {
                let Some(dropped) = out.pop() else { break };
                width -= dropped.width().unwrap_or(0);
            }
            out.push('…');
            return out;
        }
        out.push(ch);
        width += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::parse;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn lines_are_indented_one_node_each() {
        let dom = parse("<p>hi</p>");
        assert_eq!(
            tree_lines(&dom),
            [
                "#document",
                "  <html>",
                "    <head>",
                "    <body>",
                "      <p>",
                "        #text \"hi\"",
            ]
        );
    }

    #[test]
    fn id_and_class_fold_into_a_selector_like_summary() {
        let dom = parse(r#"<div id="main" class="header wide"></div>"#);
        assert!(
            tree_lines(&dom).contains(&"      <div#main.header.wide>".to_string()),
            "got {:?}",
            tree_lines(&dom)
        );
    }

    #[test]
    fn href_shows_truncated_and_other_attrs_elide() {
        let long = format!("https://example.com/{}", "x".repeat(80));
        let dom = parse(&format!(r#"<a href="{long}" data-x="1">go</a>"#));
        let lines = tree_lines(&dom);
        let a = lines.iter().find(|l| l.contains("<a ")).unwrap();
        assert!(a.contains(r#"href="https://example.com/"#), "line: {a}");
        assert!(a.contains('…'), "long href must truncate: {a}");
        assert!(!a.contains("data-x"), "other attrs elide: {a}");
        assert!(a.trim_start().ends_with(" …>"), "elision marker: {a}");
    }

    #[test]
    fn text_snippets_collapse_whitespace_and_truncate_by_cells() {
        let dom = parse("<p>a\n\t b</p>");
        assert!(
            tree_lines(&dom).contains(&"        #text \"a b\"".to_string()),
            "got {:?}",
            tree_lines(&dom)
        );

        // 40 wide chars are 80 cells: the cap must bite by cells, not chars,
        // and the ellipsis must fit inside the cap, not ride past it.
        let dom = parse(&format!("<p>{}</p>", "世".repeat(40)));
        let lines = tree_lines(&dom);
        let text = lines.iter().find(|l| l.contains("#text")).unwrap();
        assert!(text.ends_with("…\""), "wide text must truncate: {text}");
        let snippet: String = text
            .chars()
            .skip_while(|&c| c != '"')
            .filter(|&c| c != '"')
            .collect();
        assert!(
            UnicodeWidthStr::width(snippet.as_str()) <= TEXT_CAP,
            "clipped snippet is {} cells, over the {TEXT_CAP}-cell cap: {text}",
            UnicodeWidthStr::width(snippet.as_str())
        );
        // 29 whole wide chars (58 cells) + the 1-cell ellipsis ≤ 60.
        assert_eq!(snippet.chars().filter(|&c| c == '世').count(), 29);
    }

    #[test]
    fn comments_and_doctype_are_marked() {
        let dom = parse("<!doctype html><!-- note --><p>x</p>");
        let lines = tree_lines(&dom);
        assert!(lines.iter().any(|l| l.contains("<!doctype html>")));
        assert!(lines.iter().any(|l| l.contains("<!-- note -->")));
    }
}
