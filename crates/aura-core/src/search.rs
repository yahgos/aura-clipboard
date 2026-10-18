//! Fuzzy search over clipboard entries.
//!
//! Every word of the query has to match, either with its letters in order
//! (fzf style, scored by `nucleo-matcher`: `cgtst` finds "cargo test") or, as a
//! fallback, a word of the text within a small edit distance (`tset` finds
//! "test"). In-order matches always outrank typo matches, and ties go to the
//! newer entry. Results carry a short snippet already split into highlighted
//! and plain parts, so every frontend renders matches the same way.

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};

use crate::{Content, DeviceId, Entry, EntryId};

/// Only the start of very long texts is searched.
const MAX_CHARS: usize = 20_000;
/// Characters of context shown around the first match.
const SNIPPET_CHARS: usize = 120;
const SNIPPET_LEAD: usize = 40;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnippetPart {
    pub text: String,
    /// True for the characters the query matched.
    pub hit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: EntryId,
    pub score: u32,
    pub snippet: Vec<SnippetPart>,
}

/// How one query word matched an entry.
struct TermMatch {
    score: u32,
    /// Char positions in the searchable text to highlight.
    positions: Vec<usize>,
}

pub fn search<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
    origin_name: impl Fn(&DeviceId) -> String,
    query: &str,
    limit: usize,
) -> Vec<SearchHit> {
    let terms: Vec<&str> = query.split_whitespace().collect();
    if terms.is_empty() || limit == 0 {
        return Vec::new();
    }
    let atoms: Vec<Atom> = terms
        .iter()
        .map(|t| {
            Atom::new(
                t,
                CaseMatching::Smart,
                Normalization::Smart,
                AtomKind::Fuzzy,
                false,
            )
        })
        .collect();
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();

    let mut hits: Vec<SearchHit> = Vec::new();
    for entry in entries {
        let chars = searchable_chars(&entry.content);
        let origin: Vec<char> = origin_name(&entry.id.origin).chars().collect();
        let mut total = 0u32;
        let mut positions = Vec::new();
        let mut all = true;
        for (term, atom) in terms.iter().zip(&atoms) {
            let m = in_order(atom, &chars, &mut matcher, &mut buf)
                .or_else(|| typo(term, &chars))
                .or_else(|| {
                    // The origin PC's name counts too, but well below the content.
                    in_order(atom, &origin, &mut matcher, &mut buf).map(|m| TermMatch {
                        score: (m.score / 4).max(1),
                        positions: Vec::new(),
                    })
                });
            match m {
                Some(m) => {
                    total += m.score;
                    positions.extend(m.positions);
                }
                None => {
                    all = false;
                    break;
                }
            }
        }
        if all {
            hits.push(SearchHit {
                id: entry.id,
                score: total,
                snippet: snippet(&chars, &positions),
            });
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(b.id.cmp(&a.id)));
    hits.truncate(limit);
    hits
}

/// The entry's text with whitespace runs collapsed, as chars (positions index into this).
fn searchable_chars(content: &Content) -> Vec<char> {
    let raw = match content {
        Content::Text(t) | Content::Html { text: t, .. } => t.as_str(),
        Content::ImagePng { width, height, .. } => {
            return format!("image {width}×{height}").chars().collect();
        }
    };
    let mut out = Vec::new();
    let mut space = false;
    for c in raw.chars() {
        if out.len() >= MAX_CHARS {
            break;
        }
        if c.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

/// Letters of the term in order. The matched letters may not be spread too far
/// apart, or long texts would match almost anything.
fn in_order(
    atom: &Atom,
    chars: &[char],
    matcher: &mut Matcher,
    buf: &mut Vec<char>,
) -> Option<TermMatch> {
    let text: String = chars.iter().collect();
    let haystack = Utf32Str::new(&text, buf);
    let mut idx = Vec::new();
    let score = atom.indices(haystack, matcher, &mut idx)?;
    idx.sort_unstable();
    idx.dedup();
    let (first, last) = (*idx.first()?, *idx.last()?);
    let needle = idx.len() as u32;
    // At most about twice the term's length: "cgtst" fits in "cargo test", but
    // letters scattered across several words fall through to the typo check.
    if last - first + 1 > needle * 2 + 1 {
        return None;
    }
    // In-order matches always outrank typo matches (which score at most 10).
    Some(TermMatch {
        score: u32::from(score) + 100,
        positions: idx.into_iter().map(|i| i as usize).collect(),
    })
}

/// A word of the text, or the start of one, within a small edit distance.
fn typo(term: &str, chars: &[char]) -> Option<TermMatch> {
    let term: Vec<char> = term.to_lowercase().chars().collect();
    if term.len() < 4 {
        return None;
    }
    let allowed = if term.len() >= 8 { 2 } else { 1 };
    let term_str: String = term.iter().collect();
    let mut best: Option<(usize, usize, usize)> = None; // (distance, start, len)
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect::<String>().to_lowercase();
        let wlen = i - start;
        let mut d = strsim::damerau_levenshtein(&term_str, &word);
        // Also accept a typo in the beginning of a longer word ("pasw" for "password").
        if wlen > term.len() {
            let prefix: String = word.chars().take(term.len()).collect();
            d = d.min(strsim::damerau_levenshtein(&term_str, &prefix).max(1));
        }
        if d <= allowed && best.is_none_or(|(bd, _, _)| d < bd) {
            best = Some((d, start, wlen));
        }
    }
    let (d, start, len) = best?;
    Some(TermMatch {
        score: 10 - 3 * d as u32,
        positions: (start..start + len).collect(),
    })
}

/// About `SNIPPET_CHARS` characters around the first highlighted position, split
/// into highlighted and plain parts.
fn snippet(chars: &[char], positions: &[usize]) -> Vec<SnippetPart> {
    let first = positions.iter().copied().min().unwrap_or(0);
    let mut start = first.saturating_sub(SNIPPET_LEAD);
    let end = (start + SNIPPET_CHARS).min(chars.len());
    start = end.saturating_sub(SNIPPET_CHARS).min(start);
    let marked: std::collections::HashSet<usize> = positions.iter().copied().collect();

    let mut body: Vec<SnippetPart> = Vec::new();
    for (i, c) in chars.iter().enumerate().take(end).skip(start) {
        let hit = marked.contains(&i);
        match body.last_mut() {
            Some(p) if p.hit == hit => p.text.push(*c),
            _ => body.push(SnippetPart {
                text: c.to_string(),
                hit,
            }),
        }
    }
    let ellipsis = || SnippetPart {
        text: "…".into(),
        hit: false,
    };
    let mut parts = Vec::with_capacity(body.len() + 2);
    if start > 0 {
        parts.push(ellipsis());
    }
    parts.extend(body);
    if end < chars.len() {
        parts.push(ellipsis());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hlc;
    use proptest::prelude::*;

    fn entry(wall: u64, text: &str) -> Entry {
        Entry {
            id: EntryId {
                hlc: Hlc {
                    wall_ms: wall,
                    counter: 0,
                },
                origin: DeviceId([wall as u8; 16]),
            },
            content: Content::Text(text.into()),
        }
    }

    fn names(_: &DeviceId) -> String {
        "MacBook".into()
    }

    /// Texts of the hits, best first.
    fn find(entries: &[Entry], q: &str) -> Vec<String> {
        search(entries, names, q, 10)
            .into_iter()
            .map(|h| {
                entries
                    .iter()
                    .find(|e| e.id == h.id)
                    .unwrap()
                    .content
                    .preview(200)
            })
            .collect()
    }

    fn highlighted(hit: &SearchHit) -> String {
        hit.snippet
            .iter()
            .filter(|p| p.hit)
            .map(|p| p.text.as_str())
            .collect()
    }

    #[test]
    fn letters_in_order() {
        let e = [entry(1, "cargo test -p aura-web"), entry(2, "git status")];
        assert_eq!(find(&e, "cgtst"), ["cargo test -p aura-web"]);
        let hit = &search(&e, names, "cgtst", 10)[0];
        assert_eq!(highlighted(hit), "cgtst");
    }

    #[test]
    fn typos() {
        let e = [
            entry(1, "the wifi password is hunter2"),
            entry(2, "unit test results"),
        ];
        assert_eq!(find(&e, "pasword"), ["the wifi password is hunter2"]);
        assert_eq!(find(&e, "tset"), ["unit test results"]);
        // A missing letter still matches in order; only the typed letters light up.
        let hit = &search(&e, names, "pasword", 10)[0];
        assert_eq!(highlighted(hit), "pasword");
        // A swapped letter needs the typo fallback, which highlights the whole word.
        let hit = &search(&e, names, "tset", 10)[0];
        assert_eq!(highlighted(hit), "test");
        // Short words need to be typed in order; no typo guessing.
        assert!(find(&e, "tsx").is_empty());
    }

    #[test]
    fn accents_and_case() {
        let e = [entry(1, "Café da manhã")];
        assert_eq!(find(&e, "cafe"), ["Café da manhã"]);
        assert_eq!(find(&e, "manha"), ["Café da manhã"]);
        // Caps lock still finds it, through the case-insensitive typo fallback.
        assert_eq!(find(&e, "MANHA"), ["Café da manhã"]);
    }

    #[test]
    fn every_word_must_match() {
        let e = [
            entry(1, "docker compose up"),
            entry(2, "docker ps"),
            entry(3, "compose a letter"),
        ];
        assert_eq!(find(&e, "docker compose"), ["docker compose up"]);
    }

    #[test]
    fn ranking() {
        // In-order beats typo; ties go to the newer entry.
        let e = [
            entry(1, "tset fixtures"),
            entry(2, "test runner"),
            entry(3, "same text"),
            entry(4, "same text"),
        ];
        assert_eq!(find(&e, "tset")[0], "tset fixtures");
        let hits = search(&e, names, "same", 10);
        assert_eq!(hits[0].id.hlc.wall_ms, 4);
    }

    #[test]
    fn scattered_letters_in_long_text_do_not_match() {
        let long = "a".repeat(50) + "c" + &"x".repeat(200) + "g t s t";
        assert!(find(&[entry(1, &long)], "cgtst").is_empty());
    }

    #[test]
    fn images_and_origins() {
        let img = Entry {
            id: entry(5, "").id,
            content: Content::ImagePng {
                width: 800,
                height: 600,
                png: vec![],
            },
        };
        let e = [img, entry(1, "hello")];
        assert_eq!(search(&e, names, "image", 10).len(), 1);
        // The origin name matches too, but below content matches.
        let hits = search(
            &[entry(1, "macbook pro"), entry(2, "hello")],
            names,
            "macbook",
            10,
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id.hlc.wall_ms, 1);
    }

    #[test]
    fn snippet_shows_context_around_the_match() {
        let text = "x ".repeat(200) + "needle" + &" y".repeat(200);
        let hit = &search(&[entry(1, &text)], names, "needle", 10)[0];
        let shown: String = hit.snippet.iter().map(|p| p.text.as_str()).collect();
        assert!(shown.starts_with('…') && shown.ends_with('…'));
        assert!(shown.chars().count() <= SNIPPET_CHARS + 2);
        assert_eq!(highlighted(hit), "needle");
    }

    proptest! {
        #[test]
        fn hits_are_consistent(
            texts in prop::collection::vec("[a-zA-Z0-9 éãç\\n]{0,60}", 1..20),
            query in "[a-zA-Zéã ]{1,12}",
        ) {
            let entries: Vec<Entry> =
                texts.iter().enumerate().map(|(i, t)| entry(i as u64 + 1, t)).collect();
            let hits = search(&entries, names, &query, 50);
            for pair in hits.windows(2) {
                prop_assert!(
                    (pair[0].score, pair[0].id) >= (pair[1].score, pair[1].id),
                    "sorted best first"
                );
            }
            for hit in &hits {
                // The snippet is a contiguous piece of the searchable text.
                let e = entries.iter().find(|e| e.id == hit.id).unwrap();
                let full: String = searchable_chars(&e.content).into_iter().collect();
                let shown: String = hit
                    .snippet
                    .iter()
                    .map(|p| p.text.as_str())
                    .collect::<String>()
                    .trim_matches('…')
                    .to_string();
                prop_assert!(full.contains(&shown));
                prop_assert!(hit.snippet.iter().all(|p| !p.text.is_empty()));
            }
        }
    }
}
