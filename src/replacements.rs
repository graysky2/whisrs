//! User-defined word/phrase replacements (`[replacements]` in `config.toml`).
//!
//! A deterministic fix-up for terms the ASR model reliably mis-hears:
//! `"package build" = "PKGBUILD"` turns "edit the package build file" into
//! "edit the PKGBUILD file". `[general] vocabulary` only hints the model;
//! this rewrites whatever it produced.
//!
//! Matching rules:
//! - case-insensitive, on whole words (no word character directly before or
//!   after the match, whatever the key's own edge characters are);
//! - any run of whitespace or hyphens between the words of a key matches
//!   ("Package-build" matches `"package build"`);
//! - the longest key wins where keys overlap;
//! - a single pass, so a replacement is never itself re-matched.
//!
//! Values are inserted verbatim. An empty value deletes the match together
//! with the spaces after it, so `"um" = ""` leaves no double space behind.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

/// Unicode `\w`: letters, marks, digits, connector punctuation. Wider than
/// `char::is_alphanumeric`, which misses combining marks (a virama would
/// otherwise count as a word edge).
static WORD_CHAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\w$").unwrap());

fn is_word_char(c: char) -> bool {
    let mut buf = [0; 4];
    WORD_CHAR.is_match(c.encode_utf8(&mut buf))
}

/// The words of a key: split on whitespace and hyphens. A hyphen at either
/// end of a key is therefore dropped; `Config::validate` warns about that.
fn words(key: &str) -> impl Iterator<Item = &str> {
    key.split(|c: char| c.is_whitespace() || c == '-')
        .filter(|w| !w.is_empty())
}

/// Lowercase `key` and collapse its separators to one space. Two keys with
/// the same normalized form match exactly the same text.
pub fn normalize(key: &str) -> String {
    words(key)
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A compiled replacement table. Build once per dictation, apply per chunk.
#[derive(Debug, Clone)]
pub struct Replacer {
    /// `(?i)(key0)|(key1)|...` followed by a capture of trailing spaces.
    re: Regex,
    /// `values[i]` replaces a match of capture group `i + 1`.
    values: Vec<String>,
}

impl Replacer {
    /// Compile `table`. Returns `Ok(None)` when it holds no usable key (empty,
    /// or only blank keys), so callers can skip the pass entirely.
    pub fn new(table: &BTreeMap<String, String>) -> Result<Option<Self>, regex::Error> {
        // Keys that normalize alike would match the same text; keep the last,
        // as a TOML-order reader would expect. `validate` warns about these.
        let mut unique: BTreeMap<String, (&str, &str)> = BTreeMap::new();
        for (key, value) in table {
            let norm = normalize(key);
            if !norm.is_empty() {
                unique.insert(norm, (key.as_str(), value.as_str()));
            }
        }
        if unique.is_empty() {
            return Ok(None);
        }

        let mut entries: Vec<(String, &str, &str)> = unique
            .into_iter()
            .map(|(norm, (key, value))| (norm, key, value))
            .collect();
        // Longest first, so leftmost-first alternation prefers
        // "package build file" over "package build".
        entries.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));

        // Built from the original key's words, not the lowercased form:
        // `(?i)` uses simple case folding, which full `to_lowercase` does not
        // always agree with ("İ" lowercases to two chars).
        let alternatives: Vec<String> = entries
            .iter()
            .map(|(_, key, _)| {
                let body = words(key)
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(r"[\s-]+");
                format!("({body})")
            })
            .collect();

        let re = Regex::new(&format!("(?i)(?:{})([ \\t]*)", alternatives.join("|")))?;
        let values = entries.into_iter().map(|(_, _, v)| v.to_string()).collect();
        Ok(Some(Self { re, values }))
    }

    /// Apply every replacement to `text` in one pass.
    pub fn apply(&self, text: &str) -> String {
        let trailing = self.values.len() + 1;
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        let mut pos = 0;
        while let Some(caps) = self.re.captures_at(text, pos) {
            let (index, key) = caps
                .iter()
                .enumerate()
                .skip(1)
                .take(self.values.len())
                .find_map(|(i, m)| m.map(|m| (i - 1, m)))
                .expect("one alternative matched");

            // Whole words only. Checked here rather than with `\b` because
            // `\b` next to a non-word key edge (`c#`, `.net`) would demand the
            // opposite of a boundary.
            let before = text[..key.start()].chars().next_back();
            let after = text[key.end()..].chars().next();
            if before.is_some_and(is_word_char) || after.is_some_and(is_word_char) {
                // Retry one character further on, so a shorter key starting
                // inside the rejected match can still be found.
                let step = text[key.start()..].chars().next().map_or(1, char::len_utf8);
                pos = key.start() + step;
                continue;
            }

            let value = &self.values[index];
            let end = if value.is_empty() {
                caps.get(trailing).map_or(key.end(), |m| m.end())
            } else {
                key.end()
            };
            out.push_str(&text[copied..key.start()]);
            out.push_str(value);
            copied = end;
            pos = end;
            if pos >= text.len() {
                break;
            }
        }
        out.push_str(&text[copied..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replacer(pairs: &[(&str, &str)]) -> Replacer {
        let table = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Replacer::new(&table).unwrap().expect("non-empty table")
    }

    #[test]
    fn replaces_phrase() {
        let r = replacer(&[("package build", "PKGBUILD")]);
        assert_eq!(
            r.apply("edit the package build file"),
            "edit the PKGBUILD file"
        );
    }

    #[test]
    fn case_insensitive_and_hyphen_or_space_runs() {
        let r = replacer(&[("package build", "PKGBUILD")]);
        assert_eq!(r.apply("Package Build"), "PKGBUILD");
        assert_eq!(r.apply("package-build"), "PKGBUILD");
        assert_eq!(r.apply("package   build"), "PKGBUILD");
    }

    #[test]
    fn respects_word_boundaries() {
        let r = replacer(&[("package build", "PKGBUILD")]);
        assert_eq!(r.apply("prepackage builder"), "prepackage builder");
    }

    #[test]
    fn keeps_surrounding_punctuation() {
        let r = replacer(&[("package build", "PKGBUILD")]);
        assert_eq!(r.apply("Check the package build."), "Check the PKGBUILD.");
        assert_eq!(r.apply("(package build)"), "(PKGBUILD)");
    }

    #[test]
    fn longest_key_wins() {
        let r = replacer(&[
            ("package build", "PKGBUILD"),
            ("package build file", "the PKGBUILD"),
        ]);
        assert_eq!(r.apply("open package build file"), "open the PKGBUILD");
        assert_eq!(r.apply("open package build"), "open PKGBUILD");
    }

    #[test]
    fn does_not_chain() {
        let r = replacer(&[("foo", "bar"), ("bar", "baz")]);
        assert_eq!(r.apply("foo bar"), "bar baz");
    }

    #[test]
    fn key_with_non_word_edge_still_matches_whole_words_only() {
        let r = replacer(&[("c#", "C#"), (".net", "DOTNET")]);
        assert_eq!(r.apply("I like c# a lot"), "I like C# a lot");
        assert_eq!(r.apply("c#d"), "c#d");
        assert_eq!(r.apply("asp.net"), "asp.net");
        assert_eq!(r.apply("use .net now"), "use DOTNET now");
    }

    #[test]
    fn rejected_match_does_not_hide_a_later_one() {
        let r = replacer(&[("ab", "X")]);
        assert_eq!(r.apply("aab ab"), "aab X");
    }

    #[test]
    fn empty_value_deletes_word_and_its_trailing_space() {
        let r = replacer(&[("um", "")]);
        assert_eq!(r.apply("so um yes"), "so yes");
        assert_eq!(r.apply("um"), "");
        assert_eq!(r.apply("yes um"), "yes ");
    }

    #[test]
    fn case_folding_does_not_lose_matches() {
        let r = replacer(&[("İstanbul", "Istanbul")]);
        assert_eq!(r.apply("to İstanbul"), "to Istanbul");
    }

    #[test]
    fn combining_mark_is_not_a_word_edge() {
        let r = replacer(&[("नमस्", "X")]);
        assert_eq!(r.apply("नमस्ते"), "नमस्ते");
    }

    #[test]
    fn blank_keys_ignored_and_empty_table_is_none() {
        let mut table = BTreeMap::new();
        assert!(Replacer::new(&table).unwrap().is_none());
        table.insert("   ".to_string(), "x".to_string());
        assert!(Replacer::new(&table).unwrap().is_none());
        table.insert("whisper s".to_string(), "whisrs".to_string());
        let r = Replacer::new(&table).unwrap().unwrap();
        assert_eq!(r.apply("run whisper s now"), "run whisrs now");
    }

    #[test]
    fn regex_metacharacters_are_literal() {
        let r = replacer(&[("a.b", "X")]);
        assert_eq!(r.apply("a.b axb"), "X axb");
    }

    #[test]
    fn dollar_in_value_is_literal() {
        let r = replacer(&[("dollar one", "$1")]);
        assert_eq!(r.apply("dollar one"), "$1");
    }
}
