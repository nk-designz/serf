//! Visual link hinting (the `f` overlay).
//!
//! Links come from the engine as a list of [`LinkBox`]es (URL, visible text
//! and viewport rectangle, obtained from the DOM by the engine backend).
//! [`assign`] labels them with short, unambiguous, **prefix-free** hint
//! labels drawn from a home-row alphabet, and [`HintSession`] tracks the
//! partially-typed label so the overlay can narrow the visible set as you type.

use serde::{Deserialize, Serialize};

/// The default home-row alphabet — all on the ASDF/JKL home row, which is
/// what makes the hints fast to type without looking.
pub const HOME_ALPHABET: &str = "asdfghjkl";

/// One link discovered in the page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkBox {
    pub url: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub w: f64,
    #[serde(default)]
    pub h: f64,
}

/// A link paired with its hint label.
#[derive(Debug, Clone, PartialEq)]
pub struct Hinted {
    pub label: String,
    pub link: LinkBox,
}

/// Generates prefix-free hint labels.
#[derive(Debug, Clone)]
pub struct HintGenerator {
    alphabet: Vec<char>,
}

impl Default for HintGenerator {
    fn default() -> Self {
        Self::with_alphabet(HOME_ALPHABET)
    }
}

impl HintGenerator {
    pub fn with_alphabet(alphabet: &str) -> Self {
        let mut alphabet: Vec<char> = alphabet.chars().collect();
        alphabet.dedup_by(|a, b| a == b); // keep order, drop immediate dups
        Self { alphabet }
    }

    pub fn alphabet(&self) -> &[char] {
        &self.alphabet
    }

    /// Produce `count` labels.
    ///
    /// Every label has the same length `L = ceil(log_k(count))`, which makes
    /// the set prefix-free by construction: no label can be a strict prefix
    /// of another, so typing a label always resolves unambiguously.
    pub fn generate(&self, count: usize) -> Vec<String> {
        if count == 0 || self.alphabet.is_empty() {
            return Vec::new();
        }
        let k = self.alphabet.len() as u64;
        let mut len = 1usize;
        let mut capacity = k;
        while capacity < count as u64 {
            len += 1;
            match capacity.checked_mul(k) {
                Some(c) => capacity = c,
                None => break,
            }
        }

        (0..count as u64)
            .map(|mut n| {
                let mut label = vec![self.alphabet[0]; len];
                for slot in label.iter_mut().rev() {
                    *slot = self.alphabet[(n % k) as usize];
                    n /= k;
                }
                label.into_iter().collect()
            })
            .collect()
    }
}

/// Label a set of links.
pub fn assign(links: &[LinkBox], generator: &HintGenerator) -> Vec<Hinted> {
    generator
        .generate(links.len())
        .into_iter()
        .zip(links.iter().cloned())
        .map(|(label, link)| Hinted { label, link })
        .collect()
}

/// The result of typing another character into an active hint session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintOutcome {
    /// Still ambiguous — keep typing.
    Pending,
    /// Exactly one label matched and was completed.
    Selected(usize),
    /// What was typed matches no hint; the session is over.
    NoMatch,
}

/// An in-progress hint selection.
#[derive(Debug, Clone)]
pub struct HintSession {
    hints: Vec<Hinted>,
    typed: String,
}

impl HintSession {
    pub fn new(links: &[LinkBox], generator: &HintGenerator) -> Self {
        Self {
            hints: assign(links, generator),
            typed: String::new(),
        }
    }

    pub fn from_hints(hints: Vec<Hinted>) -> Self {
        Self {
            hints,
            typed: String::new(),
        }
    }

    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// The hints still reachable with the current prefix, in label order.
    pub fn visible(&self) -> Vec<&Hinted> {
        let mut v: Vec<&Hinted> = self
            .hints
            .iter()
            .filter(|h| h.label.starts_with(&self.typed))
            .collect();
        v.sort_by(|a, b| a.label.cmp(&b.label));
        v
    }

    /// Feed one character.
    pub fn push(&mut self, c: char) -> HintOutcome {
        self.typed.push(c);
        let matches: Vec<usize> = self
            .hints
            .iter()
            .enumerate()
            .filter(|(_, h)| h.label.starts_with(&self.typed))
            .map(|(i, _)| i)
            .collect();
        match matches.as_slice() {
            [] => HintOutcome::NoMatch,
            [only] if self.hints[*only].label == self.typed => HintOutcome::Selected(*only),
            _ => HintOutcome::Pending,
        }
    }

    /// Remove the last typed character.
    pub fn backspace(&mut self) -> bool {
        self.typed.pop().is_some()
    }

    /// The index of the link a completed label refers to, if any.
    pub fn selected_index(&self) -> Option<usize> {
        self.hints
            .iter()
            .position(|h| h.label == self.typed && !h.label.is_empty())
    }

    pub fn link_at(&self, index: usize) -> Option<&Hinted> {
        self.hints.get(index)
    }

    pub fn hints(&self) -> &[Hinted] {
        &self.hints
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(url: &str) -> LinkBox {
        LinkBox {
            url: url.into(),
            text: url.into(),
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        }
    }

    #[test]
    fn labels_are_short_and_unambiguous() {
        let g = HintGenerator::default();
        // 9 links, 9-letter alphabet -> single characters.
        let labels = g.generate(9);
        assert_eq!(labels.len(), 9);
        assert!(labels.iter().all(|l| l.chars().count() == 1));
        assert_eq!(labels[0], "a");
        assert_eq!(labels[1], "s");
        assert_eq!(labels[8], "l");
    }

    #[test]
    fn ten_links_need_two_characters() {
        let g = HintGenerator::default();
        let labels = g.generate(10);
        assert!(labels.iter().all(|l| l.chars().count() == 2), "{labels:?}");
    }

    #[test]
    fn labels_are_unique_and_prefix_free() {
        let g = HintGenerator::default();
        for count in [1usize, 5, 9, 10, 81, 82, 500] {
            let labels = g.generate(count);
            assert_eq!(labels.len(), count);
            let unique: std::collections::HashSet<_> = labels.iter().collect();
            assert_eq!(unique.len(), count, "duplicate labels for count={count}");
            for a in &labels {
                for b in &labels {
                    if a != b {
                        assert!(
                            !b.starts_with(a.as_str()),
                            "{a:?} is a prefix of {b:?} (count={count})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn empty_alphabet_and_zero_count_are_safe() {
        assert!(HintGenerator::with_alphabet("").generate(5).is_empty());
        assert!(HintGenerator::default().generate(0).is_empty());
    }

    #[test]
    fn assignment_keeps_links_in_order() {
        let links: Vec<LinkBox> = (0..3).map(|i| link(&format!("https://h/{i}"))).collect();
        let hinted = assign(&links, &HintGenerator::default());
        assert_eq!(hinted.len(), 3);
        assert_eq!(hinted[0].link.url, "https://h/0");
        assert_eq!(hinted[2].link.url, "https://h/2");
        assert_eq!(hinted[0].label, "a");
    }

    #[test]
    fn session_narrows_then_selects() {
        let links: Vec<LinkBox> = (0..10).map(|i| link(&format!("https://h/{i}"))).collect();
        let mut s = HintSession::new(&links, &HintGenerator::default());
        assert_eq!(s.visible().len(), 10);

        let first_char = s.hints()[5].label.chars().next().unwrap();
        assert_eq!(s.push(first_char), HintOutcome::Pending);
        assert!(s.visible().len() < 10);
        assert!(s.visible().iter().all(|h| h.label.starts_with(first_char)));

        let label = s.hints()[5].label.clone();
        s.push(label.chars().nth(1).unwrap());
        assert_eq!(s.selected_index(), Some(5));
        assert_eq!(s.link_at(5).unwrap().link.url, "https://h/5");
    }

    #[test]
    fn session_reports_no_match_for_a_bad_key() {
        let links: Vec<LinkBox> = (0..3).map(|i| link(&format!("https://h/{i}"))).collect();
        let mut s = HintSession::new(&links, &HintGenerator::default());
        // 'z' is not in the home-row alphabet.
        assert_eq!(s.push('z'), HintOutcome::NoMatch);
    }

    #[test]
    fn single_link_selects_immediately_on_first_key() {
        let links = vec![link("https://only/")];
        let mut s = HintSession::new(&links, &HintGenerator::default());
        // One link -> one-character label 'a' -> typing it selects at once.
        assert_eq!(s.push('a'), HintOutcome::Selected(0));
    }

    #[test]
    fn backspace_widens_the_visible_set() {
        let links: Vec<LinkBox> = (0..10).map(|i| link(&format!("https://h/{i}"))).collect();
        let mut s = HintSession::new(&links, &HintGenerator::default());
        let c = s.hints()[0].label.chars().next().unwrap();
        s.push(c);
        let narrowed = s.visible().len();
        assert!(s.backspace());
        assert!(s.visible().len() > narrowed);
        assert_eq!(s.typed(), "");
    }
}
