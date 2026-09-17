//! Word diff: highlights the tokens that differ between the two lines of a
//! line pair. This module owns the token rule; the renderers do not.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// Split a line's content into token byte ranges.
///
/// The unit is the grapheme cluster, classified by its first character: a run
/// of identifier clusters (alphanumeric or underscore, Unicode-aware) is one
/// token, every other non-whitespace cluster is its own token, and a run of
/// whitespace is one token. So a combining mark stays with the letter before
/// it, a flag, a skin-toned emoji, or a joiner sequence is one token, and a
/// token never ends inside a cluster. The ranges cover the whole input in
/// order with no gaps or overlaps, and every boundary falls on a character
/// boundary.
///
/// Content reaches the renderers tab-expanded and newline-free, so the ranges
/// index the displayed text.
pub fn tokenize(content: &str) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut previous: Option<TokenKind> = None;
    for (index, grapheme) in content.grapheme_indices(true) {
        let kind = TokenKind::of(grapheme);
        let extends_run = previous == Some(kind) && kind.forms_runs();
        if previous.is_some() && !extends_run {
            tokens.push(start..index);
            start = index;
        }
        previous = Some(kind);
    }
    if !content.is_empty() {
        tokens.push(start..content.len());
    }
    tokens
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Identifier,
    Whitespace,
    Punctuation,
}

impl TokenKind {
    /// The kind of a grapheme cluster, decided by its first character; the
    /// marks that follow do not change it.
    fn of(grapheme: &str) -> Self {
        match grapheme.chars().next() {
            Some(ch) if ch.is_alphanumeric() || ch == '_' => Self::Identifier,
            Some(ch) if ch.is_whitespace() => Self::Whitespace,
            _ => Self::Punctuation,
        }
    }

    /// Whether adjacent characters of this kind belong to one token.
    fn forms_runs(self) -> bool {
        self != Self::Punctuation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(content: &str) -> Vec<&str> {
        tokenize(content).into_iter().map(|r| &content[r]).collect()
    }

    /// Slicing `content` by each range panics on a non-character boundary,
    /// and a rebuilt copy equal to `content` rules out gaps and overlaps.
    fn assert_covers(content: &str) {
        let ranges = tokenize(content);
        let mut expected_start = 0;
        for range in &ranges {
            assert_eq!(range.start, expected_start, "gap or overlap in {content:?}");
            assert!(range.start < range.end, "empty token in {content:?}");
            expected_start = range.end;
        }
        assert_eq!(
            expected_start,
            content.len(),
            "tail not covered in {content:?}"
        );
        assert_eq!(tokens(content).concat(), content);
    }

    #[test]
    fn identifier_is_one_token() {
        assert_eq!(tokens("foo_bar_baz"), ["foo_bar_baz"]);
    }

    #[test]
    fn punctuation_is_one_token_per_character() {
        assert_eq!(tokens("foo.bar(baz)"), ["foo", ".", "bar", "(", "baz", ")"]);
    }

    #[test]
    fn number_is_one_token() {
        assert_eq!(tokens("5000"), ["5000"]);
        assert_eq!(tokens("5_000"), ["5_000"]);
    }

    #[test]
    fn indentation_and_trailing_whitespace_are_single_tokens() {
        assert_eq!(
            tokens("    let x = 1;  "),
            ["    ", "let", " ", "x", " ", "=", " ", "1", ";", "  "]
        );
    }

    #[test]
    fn unicode_identifier_is_one_token() {
        assert_eq!(tokens("naïve_café"), ["naïve_café"]);
    }

    #[test]
    fn cjk_run_is_one_token_with_character_boundaries() {
        let content = "日本語 = 値";
        assert_eq!(tokens(content), ["日本語", " ", "=", " ", "値"]);
        assert_covers(content);
    }

    #[test]
    fn grapheme_clusters_never_split_across_tokens() {
        // Combining acute accent in decomposed form.
        assert_eq!(tokens("cafe\u{301} ok"), ["cafe\u{301}", " ", "ok"]);
        // Devanagari virama joining two consonants.
        assert_eq!(tokens("\u{915}\u{94D}\u{937}"), ["\u{915}\u{94D}\u{937}"]);
        // Emoji presentation selector on a punctuation character.
        assert_eq!(tokens("\u{2764}\u{FE0F} x"), ["\u{2764}\u{FE0F}", " ", "x"]);
        // Zero-width joiner sequence.
        assert_eq!(
            tokens("\u{1F468}\u{200D}\u{1F469}"),
            ["\u{1F468}\u{200D}\u{1F469}"]
        );
        // Regional indicator pair: one flag, and two flags are two tokens.
        assert_eq!(
            tokens("\u{1F1FA}\u{1F1F8}\u{1F1EB}\u{1F1F7}"),
            ["\u{1F1FA}\u{1F1F8}", "\u{1F1EB}\u{1F1F7}"]
        );
        // Skin-tone modifier on an emoji base.
        assert_eq!(
            tokens("\u{1F44B}\u{1F3FB} \u{1F44B}\u{1F3FF}"),
            ["\u{1F44B}\u{1F3FB}", " ", "\u{1F44B}\u{1F3FF}"]
        );
    }

    #[test]
    fn tab_expanded_content_tokenizes_on_the_expanded_text() {
        let content = crate::vcs::tabify("\tfoo\tbar");
        assert_eq!(tokens(&content), ["    ", "foo", "    ", "bar"]);
    }

    #[test]
    fn empty_line_yields_no_tokens() {
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn tokens_cover_the_whole_input_without_gaps_or_overlaps() {
        for content in [
            "",
            " ",
            "x",
            "foo.bar(baz)",
            "  indented = \"string\";  ",
            "naïve_café → 日本語",
            "🦀 crab_🦀",
        ] {
            assert_covers(content);
        }
    }
}
