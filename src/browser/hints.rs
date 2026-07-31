//! Link-hint label generation (PLAN.md M6, vimium-style).

use crate::layout::LinkHit;

/// Home-row first, then the rest of the alphabet — short labels for the links
/// the fingers already rest on.
const ALPHABET: &[u8] = b"asdfjklghqwertyuiopzxcvbnm";

/// Assign unique 1–2 character labels to `links` (document order). Label
/// length grows only as needed for the count.
pub fn label_links(links: &[LinkHit]) -> Vec<(String, LinkHit)> {
    let n = links.len();
    if n == 0 {
        return Vec::new();
    }
    let labels = generate_labels(n);
    labels.into_iter().zip(links.iter().cloned()).collect()
}

fn generate_labels(n: usize) -> Vec<String> {
    let alpha = ALPHABET.len();
    // One character covers `alpha` links; two cover `alpha²`, etc.
    let mut len = 1;
    let mut capacity = alpha;
    while capacity < n {
        len += 1;
        capacity = capacity.saturating_mul(alpha);
        if len > 4 {
            break; // absurd; still produce something
        }
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(index_to_label(i, len));
    }
    out
}

fn index_to_label(mut i: usize, len: usize) -> String {
    let alpha = ALPHABET.len();
    let mut chars = vec![b'a'; len];
    for pos in (0..len).rev() {
        chars[pos] = ALPHABET[i % alpha];
        i /= alpha;
    }
    String::from_utf8(chars).expect("alphabet is ascii")
}

/// Filter labeled links whose label starts with `prefix` (case-insensitive on
/// the typed side — labels are lowercase).
pub fn filter_prefix<'a>(
    labeled: &'a [(String, LinkHit)],
    prefix: &str,
) -> Vec<&'a (String, LinkHit)> {
    let prefix = prefix.to_ascii_lowercase();
    labeled
        .iter()
        .filter(|(label, _)| label.starts_with(&prefix))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::NodeId;

    fn dummy(n: usize) -> Vec<LinkHit> {
        (0..n)
            .map(|i| LinkHit {
                node: NodeId(i as u32),
                href: format!("/{i}"),
                x: 0,
                y: i as i32,
            })
            .collect()
    }

    #[test]
    fn labels_are_unique_and_home_row_first() {
        let labeled = label_links(&dummy(3));
        assert_eq!(labeled[0].0, "a");
        assert_eq!(labeled[1].0, "s");
        assert_eq!(labeled[2].0, "d");
        let set: std::collections::HashSet<_> = labeled.iter().map(|(l, _)| l.clone()).collect();
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn many_links_get_two_char_labels() {
        let labeled = label_links(&dummy(30));
        assert!(labeled.iter().all(|(l, _)| l.len() == 2));
        let set: std::collections::HashSet<_> = labeled.iter().map(|(l, _)| l.clone()).collect();
        assert_eq!(set.len(), 30);
    }

    #[test]
    fn filter_prefix_narrows() {
        let labeled = label_links(&dummy(3));
        let hits = filter_prefix(&labeled, "a");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "a");
    }
}
