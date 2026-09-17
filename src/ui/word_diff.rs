//! Word diff: highlights the tokens that differ between the two lines of a
//! line pair. This module owns the token rule; the renderers do not.

use std::borrow::Cow;
use std::ops::Range;

use ratatui::text::Span;
use similar::{Algorithm, DiffTag, capture_diff_slices};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::App;
use crate::model::{DiffHunk, DiffLine, LineOrigin};
use crate::theme::Theme;
use crate::ui::styles;
use crate::ui::text_utils::{apply_highlight_ranges_spans, merge_touching_ranges};
use crate::vcs::DiffWhitespaceMode;

/// The word ranges of one line pair. Each side's ranges index that side's
/// content, in ascending order, and never overlap. A dissimilar pair has
/// none, like an identical one.
#[derive(Debug, Default)]
pub(crate) struct WordRanges {
    pub deletion: Vec<Range<usize>>,
    pub addition: Vec<Range<usize>>,
}

/// Patch the word background over `ranges` of `line`'s content spans. The
/// style is chosen by the line's origin and whether the line is syntax
/// highlighted. It is background-only, so syntax foreground survives. Empty
/// ranges return the spans unchanged.
pub(crate) fn apply_word_highlight(
    theme: &Theme,
    line: &DiffLine,
    ranges: &[Range<usize>],
    spans: Vec<Span<'static>>,
) -> Vec<Span<'static>> {
    // Only deletion and addition lines carry ranges.
    let syntax_highlighted = line.highlighted_spans.is_some();
    let word_style = if line.origin == LineOrigin::Addition {
        styles::word_add_style(theme, syntax_highlighted)
    } else {
        styles::word_del_style(theme, syntax_highlighted)
    };
    apply_highlight_ranges_spans(spans, ranges, word_style)
}

/// The largest share of a line pair's non-whitespace characters that word
/// diff will mark. Above it the pair is dissimilar and left plain, so a
/// rewritten line is not painted nearly end to end.
///
/// On positional pairs from this repository's history, pairs below 0.6 are
/// mostly one edit inside a kept line, such as a rename to a longer name or
/// an added argument, and above it unrelated lines become common.
const MAX_CHANGED_SHARE: f64 = 0.6;

/// Word ranges for a line pair under the session's settings; empty when the
/// pair is dissimilar. The renderers compute a pair's ranges here, the one
/// place the session's settings are applied.
///
/// A syntax-highlighted line is displayed as its spans' text, so that text is
/// diffed and the ranges index it; a plain line is displayed as its content.
pub(crate) fn line_pair_ranges(app: &App, deletion: &DiffLine, addition: &DiffLine) -> WordRanges {
    word_ranges(
        &displayed_text(deletion),
        &displayed_text(addition),
        app.diff_whitespace_mode(),
    )
}

fn displayed_text(line: &DiffLine) -> Cow<'_, str> {
    match &line.highlighted_spans {
        Some(spans) => spans.iter().map(|(_, text)| text.as_str()).collect(),
        None => Cow::Borrowed(&line.content),
    }
}

/// Word ranges for the lines of one hunk, for a renderer that walks the
/// hunk's lines flat. Lines are paired through the hunk's change blocks, and
/// a pair is diffed the first time either of its lines is asked for, so
/// off-screen rows cost nothing and a visible pair is diffed once.
pub(crate) struct HunkWordRanges<'a> {
    hunk: &'a DiffHunk,
    /// Each line's partner in its line pair, indexed like the hunk's lines.
    partners: Vec<Option<usize>>,
    /// Each line's ranges once its pair has been diffed. Unpaired lines and
    /// the lines of a dissimilar pair get an empty vector.
    ranges: Vec<Option<Vec<Range<usize>>>>,
}

impl<'a> HunkWordRanges<'a> {
    pub(crate) fn new(hunk: &'a DiffHunk) -> Self {
        let mut partners = vec![None; hunk.lines.len()];
        for block in hunk.change_blocks() {
            for (deletion, addition) in block.rows() {
                if let (Some(deletion), Some(addition)) = (deletion, addition) {
                    partners[deletion] = Some(addition);
                    partners[addition] = Some(deletion);
                }
            }
        }
        Self {
            hunk,
            partners,
            ranges: vec![None; hunk.lines.len()],
        }
    }

    /// The word ranges of the hunk's line at `idx`, indexing its displayed
    /// text.
    pub(crate) fn for_line(&mut self, app: &App, idx: usize) -> &[Range<usize>] {
        if self.ranges[idx].is_none() {
            self.diff_pair_of(app, idx);
        }
        self.ranges[idx]
            .as_deref()
            .expect("diff_pair_of fills the line's ranges")
    }

    fn diff_pair_of(&mut self, app: &App, idx: usize) {
        let Some(partner) = self.partners[idx] else {
            self.ranges[idx] = Some(Vec::new());
            return;
        };
        let (deletion, addition) = match self.hunk.lines[idx].origin {
            LineOrigin::Deletion => (idx, partner),
            _ => (partner, idx),
        };
        let pair = line_pair_ranges(app, &self.hunk.lines[deletion], &self.hunk.lines[addition]);
        self.ranges[deletion] = Some(pair.deletion);
        self.ranges[addition] = Some(pair.addition);
    }
}

/// Word ranges for a line pair; empty when the pair is dissimilar.
///
/// The two contents are tokenized with [`tokenize`] and the token sequences
/// are diffed; every token outside an equal run is marked, and adjacent
/// marked tokens form one range. Whitespace tokens compare like any other
/// token unless `whitespace_mode` ignores whitespace, in which case they are
/// left out of the comparison and never marked.
///
/// The pair is dissimilar when the non-whitespace characters marked on both
/// sides exceed [`MAX_CHANGED_SHARE`] of the pair's non-whitespace characters.
/// Identical lines yield empty ranges; an empty side against a non-empty
/// side is dissimilar.
fn word_ranges(deletion: &str, addition: &str, whitespace_mode: DiffWhitespaceMode) -> WordRanges {
    let ignore_whitespace = whitespace_mode.ignores_all();
    let deletion_tokens = Tokens::of(deletion, ignore_whitespace);
    let addition_tokens = Tokens::of(addition, ignore_whitespace);

    let ops = capture_diff_slices(
        Algorithm::Myers,
        &deletion_tokens.texts,
        &addition_tokens.texts,
    );
    let changed_ops = ops.iter().filter(|op| op.tag() != DiffTag::Equal);
    let ranges = WordRanges {
        deletion: merge_touching_ranges(
            changed_ops
                .clone()
                .flat_map(|op| deletion_tokens.ranges[op.old_range()].iter().cloned()),
        ),
        addition: merge_touching_ranges(
            changed_ops.flat_map(|op| addition_tokens.ranges[op.new_range()].iter().cloned()),
        ),
    };

    let changed = marked_len(deletion, &ranges.deletion) + marked_len(addition, &ranges.addition);
    let total = non_whitespace_len(deletion) + non_whitespace_len(addition);
    if changed as f64 > MAX_CHANGED_SHARE * total as f64 {
        return WordRanges::default();
    }
    ranges
}

/// One side's tokens as parallel slices: the text of each token, which the
/// diff compares, and its byte range, which the result reports.
struct Tokens<'a> {
    texts: Vec<&'a str>,
    ranges: Vec<Range<usize>>,
}

impl<'a> Tokens<'a> {
    fn of(content: &'a str, ignore_whitespace: bool) -> Self {
        let (texts, ranges) = tokenize(content)
            .into_iter()
            .map(|range| (&content[range.clone()], range))
            .filter(|(text, _)| !(ignore_whitespace && is_whitespace_token(text)))
            .unzip();
        Self { texts, ranges }
    }
}

/// Whether `text` is a run of plain whitespace. A whitespace token that
/// carries a combining mark is not, so it stays in the comparison and a
/// change inside it is still marked.
fn is_whitespace_token(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

/// Non-whitespace characters of `content` inside `ranges`.
fn marked_len(content: &str, ranges: &[Range<usize>]) -> usize {
    ranges
        .iter()
        .map(|range| non_whitespace_len(&content[range.clone()]))
        .sum()
}

fn non_whitespace_len(text: &str) -> usize {
    text.chars().filter(|ch| !ch.is_whitespace()).count()
}

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
fn tokenize(content: &str) -> Vec<Range<usize>> {
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

    use DiffWhitespaceMode::{IgnoreAll, Normal};

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

    /// The marked text of each side, so assertions read as words, not offsets.
    fn marked<'a>(
        deletion: &'a str,
        addition: &'a str,
        mode: DiffWhitespaceMode,
    ) -> (Vec<&'a str>, Vec<&'a str>) {
        let ranges = word_ranges(deletion, addition, mode);
        assert_within(deletion, &ranges.deletion);
        assert_within(addition, &ranges.addition);
        let slice = |content: &'a str, ranges: &[Range<usize>]| {
            ranges.iter().map(|r| &content[r.clone()]).collect()
        };
        (
            slice(deletion, &ranges.deletion),
            slice(addition, &ranges.addition),
        )
    }

    /// Nothing marked on either side.
    const PLAIN: (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());

    /// Ranges are non-empty, ascending, non-overlapping, inside `content`, and
    /// start and end on character boundaries.
    fn assert_within(content: &str, ranges: &[Range<usize>]) {
        let mut previous_end = 0;
        for range in ranges {
            assert!(range.start < range.end, "empty range in {content:?}");
            assert!(range.start >= previous_end, "overlap in {content:?}");
            assert!(range.end <= content.len(), "past the end of {content:?}");
            assert!(
                content.is_char_boundary(range.start),
                "{content:?} {range:?}"
            );
            assert!(content.is_char_boundary(range.end), "{content:?} {range:?}");
            previous_end = range.end;
        }
    }

    #[test]
    fn one_changed_token_marks_it_on_both_sides() {
        assert_eq!(
            marked("let x = foo;", "let x = bar;", Normal),
            (vec!["foo"], vec!["bar"])
        );
    }

    #[test]
    fn rename_inside_a_longer_line_marks_only_the_name() {
        assert_eq!(
            marked(
                "    let total = compute_total(items, options)?;",
                "    let total = compute_sum(items, options)?;",
                Normal
            ),
            (vec!["compute_total"], vec!["compute_sum"])
        );
    }

    #[test]
    fn several_changed_tokens_produce_several_ranges() {
        assert_eq!(
            marked("let a = foo(bar, baz);", "let a = qux(bar, quux);", Normal),
            (vec!["foo", "baz"], vec!["qux", "quux"])
        );
    }

    #[test]
    fn adjacent_changed_tokens_form_one_range() {
        assert_eq!(
            marked("let value = foo(x);", "let value = bar[x];", Normal),
            (vec!["foo(", ")"], vec!["bar[", "]"])
        );
    }

    #[test]
    fn whitespace_only_change_is_marked() {
        assert_eq!(
            marked("let x = 1;", "let x  = 1;", Normal),
            (vec![" "], vec!["  "])
        );
    }

    #[test]
    fn whitespace_only_change_is_not_marked_when_whitespace_is_ignored() {
        assert_eq!(marked("let x = 1;", "let x  = 1;", IgnoreAll), PLAIN);
        assert_eq!(marked("foo( x, y )", "foo(x, y)", IgnoreAll), PLAIN);
    }

    #[test]
    fn word_and_indent_change_marks_only_the_word_when_whitespace_is_ignored() {
        assert_eq!(
            marked("  let x = foo;", "    let x = bar;", IgnoreAll),
            (vec!["foo"], vec!["bar"])
        );
        assert_eq!(
            marked("  let x = foo;", "    let x = bar;", Normal),
            (vec!["  ", "foo"], vec!["    ", "bar"])
        );
    }

    #[test]
    fn identical_lines_yield_empty_ranges() {
        assert_eq!(marked("let x = 1;", "let x = 1;", Normal), PLAIN);
        assert_eq!(marked("", "", Normal), PLAIN);
    }

    #[test]
    fn rewritten_line_is_a_dissimilar_pair() {
        assert_eq!(marked("let x = foo;", "return bar(baz)", Normal), PLAIN);
    }

    #[test]
    fn guard_splits_pairs_around_the_changed_share() {
        // 20 of 36 non-whitespace characters are marked: 0.56, under the guard.
        assert_eq!(
            marked("fn foo(a: u32) -> u32 {", "fn bar(b: i64) -> i64 {", Normal),
            (
                vec!["foo", "a", "u32", "u32"],
                vec!["bar", "b", "i64", "i64"]
            )
        );
        // 13 of 19 are marked: 0.68, over the guard.
        assert_eq!(marked("foo bar baz", "qux bar quux", Normal), PLAIN);
    }

    #[test]
    fn empty_side_against_non_empty_side_is_a_dissimilar_pair() {
        assert_eq!(marked("", "added", Normal), PLAIN);
        assert_eq!(marked("removed", "", Normal), PLAIN);
        assert_eq!(marked("   ", "added", Normal), PLAIN);
    }

    #[test]
    fn unicode_pairs_produce_boundary_safe_ranges() {
        // The first two pairs are ported from NikolayXHD/tuicr@970db13.
        assert_eq!(
            marked("let x = wörld;", "let x = wörldx;", Normal),
            (vec!["wörld"], vec!["wörldx"])
        );
        assert_eq!(
            marked("let b = wörld;", "let b = wörld!", Normal),
            (vec![";"], vec!["!"])
        );
        assert_eq!(
            marked("日本語 = 値", "日本語 = 値段", Normal),
            (vec!["値"], vec!["値段"])
        );
        assert_eq!(
            marked("x = cafe\u{301} + 1", "x = cafe\u{301} + 2", Normal),
            (vec!["1"], vec!["2"])
        );
        // A changed skin tone marks the whole emoji, never the modifier alone.
        assert_eq!(
            marked(
                "wave \u{1F44B}\u{1F3FB} hi",
                "wave \u{1F44B}\u{1F3FF} hi",
                Normal
            ),
            (vec!["\u{1F44B}\u{1F3FB}"], vec!["\u{1F44B}\u{1F3FF}"])
        );
    }

    #[test]
    fn tab_expanded_pairs_index_the_expanded_text() {
        let deletion = crate::vcs::tabify("\tfoo\tbar");
        let addition = crate::vcs::tabify("\tfoo\tbaz");
        let ranges = word_ranges(&deletion, &addition, Normal);
        assert_eq!(ranges.deletion, vec![11..14]);
        assert_eq!(ranges.addition, vec![11..14]);
        assert_eq!(&deletion[ranges.deletion[0].clone()], "bar");
    }
}

/// Fixtures and buffer probes shared by the two views' word-diff render
/// tests, so both assert on the same marked text.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    use ratatui::buffer::Buffer;
    use ratatui::style::Color;

    use crate::model::{DiffFile, DiffHunk, DiffLine, FileStatus, LineOrigin};

    /// One hunk: a context line, then the deletions, then the additions.
    pub(crate) fn change_block_file(
        context: &str,
        deletions: &[&str],
        additions: &[&str],
    ) -> DiffFile {
        let mut lines = vec![DiffLine {
            origin: LineOrigin::Context,
            content: context.to_string(),
            old_lineno: Some(1),
            new_lineno: Some(1),
            highlighted_spans: None,
        }];
        lines.extend(deletions.iter().enumerate().map(|(i, content)| DiffLine {
            origin: LineOrigin::Deletion,
            content: content.to_string(),
            old_lineno: Some(i as u32 + 2),
            new_lineno: None,
            highlighted_spans: None,
        }));
        lines.extend(additions.iter().enumerate().map(|(i, content)| DiffLine {
            origin: LineOrigin::Addition,
            content: content.to_string(),
            old_lineno: None,
            new_lineno: Some(i as u32 + 2),
            highlighted_spans: None,
        }));
        let hunks = vec![DiffHunk {
            header: "@@ -1,3 +1,3 @@".to_string(),
            old_count: deletions.len() as u32 + 1,
            new_count: additions.len() as u32 + 1,
            lines,
            old_start: 1,
            new_start: 1,
        }];
        let content_hash = DiffFile::compute_content_hash(&hunks);
        DiffFile {
            old_path: Some(PathBuf::from("src/lib.rs")),
            new_path: Some(PathBuf::from("src/lib.rs")),
            status: FileStatus::Modified,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: false,
            content_hash,
        }
    }

    /// Every row's text, one per line, for assertion messages.
    pub(crate) fn body_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| row_text(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    /// The row whose text contains `needle`.
    pub(crate) fn row_containing(buffer: &Buffer, needle: &str) -> u16 {
        (0..buffer.area.height)
            .find(|&y| row_text(buffer, y).contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}:\n{}", body_text(buffer)))
    }

    /// The x of each cell in row `y` whose background is `bg`.
    pub(crate) fn cells_with_bg(buffer: &Buffer, y: u16, bg: Color) -> Vec<u16> {
        (0..buffer.area.width)
            .filter(|&x| buffer[(x, y)].bg == bg)
            .collect()
    }

    /// The text spelled by the cells of row `y` whose background is `bg`.
    pub(crate) fn text_with_bg(buffer: &Buffer, y: u16, bg: Color) -> String {
        cells_with_bg(buffer, y, bg)
            .into_iter()
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }
}
