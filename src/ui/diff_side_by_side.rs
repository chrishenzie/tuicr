use std::ops::Range;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::app::{
    App, DiffSource, ExpandDirection, FocusedPanel, GAP_EXPAND_BATCH, GapId, InputMode,
};
use crate::model::{
    ChangeBlock, DiffHunk, DiffLine, FileStatus, HunkSegment, LineOrigin, LineRange, LineSide,
};
use crate::theme::Theme;
use crate::ui::comment_panel;
use crate::ui::diff_view::{
    apply_horizontal_scroll, comment_type_presentation, cursor_indicator, cursor_indicator_spaced,
    diff_stat_title, hunk_header_text_and_style, paint_cursor_line_highlight,
    paint_visual_selection_overlay, populate_row_to_annotation, render_expander_line,
    render_hidden_lines, scroll_comment_input_into_view, skip_comment_box,
};
use crate::ui::styles;
use crate::ui::text_utils::{
    apply_search_highlight_pairs, apply_search_highlight_spans, apply_search_highlight_text,
    truncate_or_pad, truncate_or_pad_line, truncate_or_pad_pairs_by_chars, truncate_or_pad_spans,
    wrap_spans,
};
use crate::ui::word_diff::{apply_word_highlight, line_pair_ranges};
use crate::vcs::git::calculate_gap;

#[derive(Clone, Default)]
struct SbsRowMeta {
    left_content: Vec<Span<'static>>,
    right_content: Vec<Span<'static>>,
    left_prefix: Vec<Span<'static>>,
    right_prefix: Vec<Span<'static>>,
    left_pad_style: Style,
    right_pad_style: Style,
}

/// A diff line's content as full-line spans, in layer order: the diff style,
/// syntax spans, word highlight, then search highlight, which wins where the
/// two overlap. The highlights go on the full line, before any truncation,
/// wrapping, or horizontal scroll, so every row path marks the same
/// characters.
fn content_spans_for_diff_line(
    theme: &Theme,
    dl: &DiffLine,
    word_ranges: &[Range<usize>],
    search: Option<(&str, Style)>,
) -> Vec<Span<'static>> {
    let base = match dl.origin {
        LineOrigin::Context => styles::diff_context_style(theme),
        LineOrigin::Addition => styles::diff_add_style(theme),
        LineOrigin::Deletion => styles::diff_del_style(theme),
    };
    let spans: Vec<Span<'static>> = if let Some(ref h) = dl.highlighted_spans {
        h.iter().map(|(s, t)| Span::styled(t.clone(), *s)).collect()
    } else {
        vec![Span::styled(dl.content.clone(), base)]
    };
    let spans = apply_word_highlight(theme, dl, word_ranges, spans);
    match search {
        Some((needle, hl)) => apply_search_highlight_spans(spans, needle, hl),
        None => spans,
    }
}

fn searched_cell_spans(
    pairs: &[(Style, String)],
    width: usize,
    pad_style: Style,
    search: Option<(&str, Style)>,
) -> Vec<Span<'static>> {
    if let Some((needle, hl)) = search
        && let Some(highlighted) = apply_search_highlight_pairs(pairs, needle, hl)
    {
        return truncate_or_pad_spans(&highlighted, width, pad_style);
    }
    truncate_or_pad_spans(pairs, width, pad_style)
}

fn plain_cell_spans(
    content: &str,
    style: Style,
    width: usize,
    search: Option<(&str, Style)>,
) -> Vec<Span<'static>> {
    if let Some((needle, hl)) = search
        && let Some(highlighted) = apply_search_highlight_text(content, style, needle, hl)
    {
        return truncate_or_pad_pairs_by_chars(&highlighted, width, style);
    }
    vec![Span::styled(truncate_or_pad(content, width), style)]
}

fn column_pad_style(theme: &Theme, dl: &DiffLine, origin: LineOrigin) -> Style {
    match origin {
        LineOrigin::Context => styles::diff_context_style(theme),
        LineOrigin::Addition => {
            if dl.highlighted_spans.is_some() {
                Style::default().fg(theme.diff_add).bg(theme.syntax_add_bg)
            } else {
                styles::diff_add_style(theme)
            }
        }
        LineOrigin::Deletion => {
            if dl.highlighted_spans.is_some() {
                Style::default().fg(theme.diff_del).bg(theme.syntax_del_bg)
            } else {
                styles::diff_del_style(theme)
            }
        }
    }
}

fn pad_spans_to_width(
    mut spans: Vec<Span<'static>>,
    width: usize,
    pad_style: Style,
) -> Vec<Span<'static>> {
    let cur: usize = spans.iter().map(|s| s.content.width()).sum();
    if cur < width {
        spans.push(Span::styled(" ".repeat(width - cur), pad_style));
    }
    spans
}

#[derive(Clone, Copy)]
struct SideSpec {
    lineno: Option<u32>,
    marker: &'static str,
    marker_style: Style,
}

/// One column of a change-block row, built once for both row paths: the
/// direct row truncates or pads `content` to the column, and the wrap path
/// wraps it, so both mark the same characters.
struct ChangeColumn {
    content: Vec<Span<'static>>,
    pad_style: Style,
    side: SideSpec,
}

impl ChangeColumn {
    /// The column for `dl`, a line of a change block: a deletion fills the
    /// left column, an addition the right.
    fn for_line(
        ctx: &SideBySideContext,
        dl: &DiffLine,
        word_ranges: &[Range<usize>],
        line_idx: usize,
    ) -> Self {
        let (lineno, marker_style) = match dl.origin {
            LineOrigin::Addition => (dl.new_lineno, styles::diff_add_style(ctx.theme)),
            LineOrigin::Deletion | LineOrigin::Context => {
                (dl.old_lineno, styles::diff_del_style(ctx.theme))
            }
        };
        Self {
            content: content_spans_for_diff_line(
                ctx.theme,
                dl,
                word_ranges,
                ctx.search_for(line_idx),
            ),
            pad_style: column_pad_style(ctx.theme, dl, dl.origin),
            side: SideSpec {
                lineno: ctx.display_lineno(lineno, line_idx),
                marker: "▌",
                marker_style,
            },
        }
    }

    /// The column of the unpaired tail's other side: blank, no marker.
    fn empty() -> Self {
        Self {
            content: Vec::new(),
            pad_style: Style::default(),
            side: SideSpec {
                lineno: None,
                marker: " ",
                marker_style: Style::default(),
            },
        }
    }

    /// The content cut or padded to one row of the column.
    fn cell(&self, width: usize) -> Vec<Span<'static>> {
        truncate_or_pad_line(&self.content, width, self.pad_style)
    }
}

fn sbs_row_prefixes(
    theme: &Theme,
    indicator: &'static str,
    left: SideSpec,
    right: SideSpec,
    lw: usize,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let dim = styles::dim_style(theme);
    let old_num = left
        .lineno
        .map(|n| format!("{n:>lw$}"))
        .unwrap_or_else(|| " ".repeat(lw));
    let new_num = right
        .lineno
        .map(|n| format!("{n:>lw$}"))
        .unwrap_or_else(|| " ".repeat(lw));

    let left_prefix = vec![
        Span::styled(indicator, styles::current_line_indicator_style(theme)),
        Span::styled(format!("{old_num} "), dim),
        Span::styled(left.marker.to_string(), left.marker_style),
    ];
    let right_prefix = vec![
        Span::styled(" │ ", dim),
        Span::styled(format!("{new_num} "), dim),
        Span::styled(right.marker.to_string(), right.marker_style),
    ];
    (left_prefix, right_prefix)
}

/// Continuation-row prefixes shared by every wrapped line: blank in place of
/// the line numbers (same width, so columns stay aligned) with the center
/// divider preserved.
fn sbs_blank_prefixes(theme: &Theme, lw: usize) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let dim = styles::dim_style(theme);
    let left = vec![Span::styled(" ".repeat(lw + 3), Style::default())];
    let right = vec![
        Span::styled(" │ ", dim),
        Span::styled(" ".repeat(lw + 2), Style::default()),
    ];
    (left, right)
}

/// Cursor info for the inline comment input box in side-by-side view:
/// (cursor_logical_line, cursor_column, box_start_line, box_end_line)
type SideBySideCursorInfo = (usize, u16, usize, usize, usize);

/// Context for rendering side-by-side diff lines
struct SideBySideContext<'a> {
    app: &'a App,
    theme: &'a Theme,
    content_width: usize,
    panel_width: usize,
    current_line_idx: usize,
    lineno_width: usize,
    // Comment input state for inline editing
    comment_input_mode: bool,
    comment_line: Option<(u32, LineSide)>,
    comment_type: crate::model::CommentType,
    comment_buffer: &'a str,
    comment_cursor: usize,
    comment_line_range: Option<LineRange>,
    editing_comment_id: Option<&'a str>,
    current_file_idx: usize,
    // RefCell so deeply-nested rendering helpers can push without each
    // intermediate function needing a `&mut Vec` parameter threaded through.
    comment_bars: std::cell::RefCell<Vec<crate::ui::diff_view::CommentBarAnchor>>,
    sbs_meta: std::cell::RefCell<std::collections::HashMap<usize, SbsRowMeta>>,
    // Only fully build spans for diff lines whose `line_idx` falls in this
    // half-open range; off-screen rows push `Line::default()` placeholders.
    visible_start: usize,
    visible_end: usize,
    search_style: Style,
}

impl SideBySideContext<'_> {
    fn is_visible(&self, line_idx: usize) -> bool {
        line_idx >= self.visible_start && line_idx < self.visible_end
    }

    /// Same question for a multi-row comment box: does any of it land on screen?
    fn box_visible(&self, top: usize, rows: usize) -> bool {
        crate::ui::diff_view::comment_box_visible(top, rows, (self.visible_start, self.visible_end))
    }

    fn search_for(&self, line_idx: usize) -> Option<(&str, Style)> {
        let needle = self.app.search_paint_at(line_idx)?;
        Some((needle, self.search_style))
    }

    fn display_lineno(&self, source_line: Option<u32>, line_idx: usize) -> Option<u32> {
        source_line.map(|line| {
            if self.app.relative_line_numbers {
                line_idx.abs_diff(self.current_line_idx) as u32
            } else {
                line
            }
        })
    }
}

pub(super) fn render_side_by_side_diff(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focused_panel == FocusedPanel::Diff;

    let title = crate::ui::diff_view::diff_title(app, area.width);

    let block = Block::default()
        .title(title)
        .title_top(diff_stat_title(app).right_aligned())
        .borders(Borders::ALL)
        .style(styles::panel_style(&app.theme))
        .border_style(styles::border_style(&app.theme, focused));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Update viewport height for scroll calculations
    app.diff_state.viewport_height = inner.height as usize;
    app.diff_inner_area = Some(inner);

    // Reset comment input annotation offset (will be set if a comment input box is rendered)
    app.comment_input_annotation_offset = None;

    let lw = app.lineno_width();
    let available_width = inner.width.saturating_sub(crate::app::sbs_overhead(lw)) as usize;
    let content_width = available_width / 2;

    // Determine if we're in line comment mode (not file-level)
    let comment_input_mode = app.input_mode == InputMode::Comment
        && !app.comment_is_file_level
        && !app.comment_is_review_level;

    let (visible_start, visible_end) = crate::ui::diff_view::diff_visible_range(app, inner);

    let ctx = SideBySideContext {
        app,
        theme: &app.theme,
        content_width,
        panel_width: inner.width as usize,
        current_line_idx: app.diff_state.cursor_line,
        lineno_width: lw,
        comment_input_mode,
        comment_line: app.comment_line,
        comment_type: app.comment_type.clone(),
        comment_buffer: &app.comment_buffer,
        comment_cursor: app.comment_cursor,
        comment_line_range: app.comment_line_range.map(|(r, _)| r),
        editing_comment_id: app.editing_comment_id.as_deref(),
        current_file_idx: app.diff_state.current_file_idx,
        comment_bars: std::cell::RefCell::new(Vec::new()),
        sbs_meta: std::cell::RefCell::new(std::collections::HashMap::new()),
        visible_start,
        visible_end,
        search_style: styles::search_match_style(&app.theme),
    };

    // Build all diff lines for side-by-side view
    let mut lines: Vec<Line> = Vec::new();
    let mut line_idx: usize = 0;

    // Track cursor position for IME when in Comment mode
    let mut comment_cursor_logical_line: Option<usize> = None;
    let mut comment_cursor_column: u16 = 0;
    // Track the full extent of the comment input box so we can auto-scroll
    // the viewport to keep it visible while the user types.
    let mut comment_input_box_range: Option<(usize, usize)> = None;
    let mut annotation_offset: Option<(usize, usize, usize)> = None;

    let is_review_comment_mode =
        app.input_mode == InputMode::Comment && app.comment_is_review_level;

    crate::ui::pr_info_panel::append_pr_info_section(
        app,
        &mut lines,
        &mut line_idx,
        ctx.current_line_idx,
    );

    // The `═══ Review Comments ═══` label is redundant in single-file
    // view -- see the matching guard in `src/ui/diff_unified.rs`.
    if app.show_review_comments_header() {
        let general_indicator = cursor_indicator_spaced(line_idx, ctx.current_line_idx);
        lines.push(Line::from(vec![
            Span::styled(
                general_indicator,
                styles::current_line_indicator_style(&app.theme),
            ),
            Span::styled(
                crate::ui::diff_view::REVIEW_COMMENTS_HEADER_PREFIX,
                styles::file_header_style(&app.theme),
            ),
            Span::styled(
                crate::ui::diff_view::HEADER_RULE,
                styles::file_header_style(&app.theme),
            ),
        ]));
        line_idx += 1;
    }

    for summary in &app.forge_review_summaries {
        let summary_lines = comment_panel::format_remote_review_summary_lines(
            &app.theme,
            summary,
            app.forge_kind(),
        );
        for mut summary_line in summary_lines {
            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
            summary_line.spans.insert(
                0,
                Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
            );
            lines.push(summary_line);
            line_idx += 1;
        }
    }

    for comment in &app.session.review_comments {
        let is_being_edited =
            app.editing_comment_id.as_ref() == Some(&comment.id) && is_review_comment_mode;

        if is_being_edited {
            let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
                &app.theme,
                comment_type_presentation(app, &app.comment_type),
                &app.comment_buffer,
                app.comment_cursor,
                None,
                true,
                ctx.panel_width.saturating_sub(1),
                app.comment_vim_mode_label()
                    .as_ref()
                    .map(|(t, w)| (t.as_str(), *w)),
                app.supports_keyboard_enhancement,
            );
            comment_cursor_logical_line = Some(line_idx + cursor_info.line_offset);
            comment_cursor_column = 1 + cursor_info.column;
            comment_input_box_range =
                Some((line_idx, line_idx + input_lines.len().saturating_sub(1)));
            let annotations_replaced = App::comment_display_lines(comment, inner.width as usize);
            annotation_offset = Some((line_idx, input_lines.len(), annotations_replaced));

            for mut input_line in input_lines {
                let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                input_line.spans.insert(
                    0,
                    Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                );
                lines.push(input_line);
                line_idx += 1;
            }
        } else {
            let rows = App::comment_display_lines(comment, ctx.panel_width);
            if !ctx.box_visible(line_idx, rows) {
                skip_comment_box(&mut lines, &mut line_idx, rows);
                continue;
            }
            let comment_lines = comment_panel::format_comment_lines(
                &app.theme,
                comment_type_presentation(app, &comment.comment_type),
                &comment.content,
                None,
                ctx.panel_width.saturating_sub(1),
                (comment.author != app.username).then_some(comment.author.as_str()),
            );
            for mut comment_line in comment_lines {
                let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                comment_line.spans.insert(
                    0,
                    Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                );
                lines.push(comment_line);
                line_idx += 1;
            }
        }
    }

    // Render remote review-level threads (general MR notes, line: None).
    {
        use crate::forge::remote_comments::{PrCommentsVisibility, RemoteCommentSide};
        let _ = RemoteCommentSide::Right; // ensure import is used
        let visibility = app.session.remote_comments_visibility;
        if !matches!(visibility, PrCommentsVisibility::Hide) {
            for thread in &app.forge_review_threads {
                if thread.line.is_some() {
                    continue; // inline threads are rendered in-diff
                }
                let Some(muted) = visibility.render_decision(thread) else {
                    continue;
                };
                let thread_lines = comment_panel::format_remote_thread_lines(
                    &app.theme,
                    thread,
                    muted,
                    app.forge_kind(),
                );
                for mut comment_line in thread_lines {
                    let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                    comment_line.spans.insert(
                        0,
                        Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                    );
                    lines.push(comment_line);
                    line_idx += 1;
                }
            }
        }
    }

    if is_review_comment_mode && app.editing_comment_id.is_none() {
        let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
            &app.theme,
            comment_type_presentation(app, &app.comment_type),
            &app.comment_buffer,
            app.comment_cursor,
            None,
            false,
            ctx.panel_width.saturating_sub(1),
            app.comment_vim_mode_label()
                .as_ref()
                .map(|(t, w)| (t.as_str(), *w)),
            app.supports_keyboard_enhancement,
        );
        comment_cursor_logical_line = Some(line_idx + cursor_info.line_offset);
        comment_cursor_column = 1 + cursor_info.column;
        comment_input_box_range = Some((line_idx, line_idx + input_lines.len().saturating_sub(1)));
        annotation_offset = Some((line_idx, input_lines.len(), 0));

        for mut input_line in input_lines {
            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
            input_line.spans.insert(
                0,
                Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
            );
            lines.push(input_line);
            line_idx += 1;
        }
    }

    crate::ui::pr_info_panel::append_issue_comments_section(
        app,
        &mut lines,
        &mut line_idx,
        ctx.current_line_idx,
        ctx.panel_width.saturating_sub(1),
        (ctx.visible_start, ctx.visible_end),
    );

    for (file_idx, file) in app.diff_files.iter().enumerate() {
        // Single-file view: hide everything except the cursor's file. See
        // src/ui/diff_unified.rs for the matching guard.
        if app.is_single_file_view && file_idx != app.diff_state.current_file_idx {
            continue;
        }
        // See the matching filter guard in src/ui/diff_unified.rs.
        if !app.file_passes_filter(file) {
            continue;
        }
        let path = file.display_path();
        let is_reviewed = app.session.is_file_reviewed(path);

        if !app.is_single_file_view {
            let indicator = cursor_indicator_spaced(line_idx, ctx.current_line_idx);
            let header_text = crate::ui::diff_view::file_header_prefix_text(app, file);
            lines.push(Line::from(vec![
                Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                Span::styled(header_text, styles::file_header_style(&app.theme)),
                Span::styled(
                    crate::ui::diff_view::HEADER_RULE,
                    styles::file_header_style(&app.theme),
                ),
            ]));
            line_idx += 1;
        }

        // Reviewed files normally collapse in continuous view. A summary jump
        // may reveal one target body without changing its reviewed marker.
        if app.should_collapse_file(file_idx) {
            continue;
        }
        if is_reviewed && app.is_single_file_view {
            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
            lines.push(Line::from(vec![
                Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                Span::styled(
                    crate::ui::diff_view::REVIEWED_BANNER_TEXT,
                    Style::default()
                        .fg(app.theme.fg_secondary)
                        .add_modifier(Modifier::DIM),
                ),
            ]));
            line_idx += 1;
        }

        // Check if we're editing/adding a file-level comment for this file
        let is_file_comment_mode = app.input_mode == InputMode::Comment
            && app.comment_is_file_level
            && file_idx == app.diff_state.current_file_idx;

        // Show file-level comments
        if let Some(review) = app.session.files.get(path) {
            for comment in &review.file_comments {
                if !app.comment_visible(comment) {
                    continue;
                }
                // Skip rendering this comment if it's being edited
                let is_being_edited =
                    app.editing_comment_id.as_ref() == Some(&comment.id) && is_file_comment_mode;

                if is_being_edited {
                    // Render the inline input instead
                    let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
                        &app.theme,
                        comment_type_presentation(app, &app.comment_type),
                        &app.comment_buffer,
                        app.comment_cursor,
                        None,
                        true,
                        ctx.panel_width.saturating_sub(1),
                        app.comment_vim_mode_label()
                            .as_ref()
                            .map(|(t, w)| (t.as_str(), *w)),
                        app.supports_keyboard_enhancement,
                    );
                    comment_cursor_logical_line = Some(line_idx + cursor_info.line_offset);
                    comment_cursor_column = 1 + cursor_info.column;
                    comment_input_box_range =
                        Some((line_idx, line_idx + input_lines.len().saturating_sub(1)));
                    let annotations_replaced =
                        App::comment_display_lines(comment, inner.width as usize);
                    annotation_offset = Some((line_idx, input_lines.len(), annotations_replaced));

                    for mut input_line in input_lines {
                        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                        input_line.spans.insert(
                            0,
                            Span::styled(
                                indicator,
                                styles::current_line_indicator_style(&app.theme),
                            ),
                        );
                        lines.push(input_line);
                        line_idx += 1;
                    }
                } else {
                    let rows = App::comment_display_lines(comment, ctx.panel_width);
                    if !ctx.box_visible(line_idx, rows) {
                        skip_comment_box(&mut lines, &mut line_idx, rows);
                        continue;
                    }
                    let comment_lines = comment_panel::format_comment_lines(
                        &app.theme,
                        comment_type_presentation(app, &comment.comment_type),
                        &comment.content,
                        None,
                        ctx.panel_width.saturating_sub(1),
                        (comment.author != app.username).then_some(comment.author.as_str()),
                    );
                    for mut comment_line in comment_lines {
                        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                        comment_line.spans.insert(
                            0,
                            Span::styled(
                                indicator,
                                styles::current_line_indicator_style(&app.theme),
                            ),
                        );
                        lines.push(comment_line);
                        line_idx += 1;
                    }
                }
            }
        }

        // Render inline input for new file-level comment
        if is_file_comment_mode && app.editing_comment_id.is_none() {
            let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
                &app.theme,
                comment_type_presentation(app, &app.comment_type),
                &app.comment_buffer,
                app.comment_cursor,
                None,
                false,
                ctx.panel_width.saturating_sub(1),
                app.comment_vim_mode_label()
                    .as_ref()
                    .map(|(t, w)| (t.as_str(), *w)),
                app.supports_keyboard_enhancement,
            );
            comment_cursor_logical_line = Some(line_idx + cursor_info.line_offset);
            comment_cursor_column = 1 + cursor_info.column;
            comment_input_box_range =
                Some((line_idx, line_idx + input_lines.len().saturating_sub(1)));
            annotation_offset = Some((line_idx, input_lines.len(), 0));

            for mut input_line in input_lines {
                let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                input_line.spans.insert(
                    0,
                    Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                );
                lines.push(input_line);
                line_idx += 1;
            }
        }

        if file.is_too_large || file.is_binary || file.hunks.is_empty() {
            let indicator = cursor_indicator_spaced(line_idx, ctx.current_line_idx);
            lines.push(Line::from(vec![
                Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                Span::styled(
                    crate::ui::diff_view::binary_or_empty_label(file),
                    styles::dim_style(&app.theme),
                ),
            ]));
            line_idx += 1;
        } else {
            let line_comments = app
                .session
                .files
                .get(path)
                .map(|r| &r.line_comments)
                .unwrap_or(&crate::ui::diff_view::EMPTY_LINE_COMMENTS);

            for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
                // Calculate and render gap before this hunk
                let prev_hunk = if hunk_idx > 0 {
                    file.hunks.get(hunk_idx - 1)
                } else {
                    None
                };
                let gap = calculate_gap(
                    prev_hunk.map(|h| (&h.new_start, &h.new_count)),
                    hunk.new_start,
                );

                let gap_id = GapId { file_idx, hunk_idx };

                if gap > 0 && app.should_render_gap_before_hunk(file_idx, hunk_idx) {
                    let top_lines = app.expanded_top.get(&gap_id);
                    let bot_lines = app.expanded_bottom.get(&gap_id);
                    let top_len = top_lines.map_or(0, |v| v.len());
                    let bot_len = bot_lines.map_or(0, |v| v.len());
                    let remaining = (gap as usize).saturating_sub(top_len + bot_len);
                    let is_top_of_file = hunk_idx == 0;

                    // Render top expanded lines
                    if let Some(top) = top_lines {
                        for expanded_line in top {
                            if !ctx.is_visible(line_idx) {
                                lines.push(Line::default());
                                line_idx += 1;
                                continue;
                            }
                            render_sbs_expanded_context_line(
                                &mut lines,
                                &mut line_idx,
                                expanded_line,
                                &ctx,
                            );
                        }
                    }

                    // Render expanders / hidden lines
                    if remaining > 0 {
                        if is_top_of_file {
                            if remaining > GAP_EXPAND_BATCH {
                                render_hidden_lines(
                                    &mut lines,
                                    &mut line_idx,
                                    ctx.current_line_idx,
                                    remaining,
                                    &app.theme,
                                );
                            }
                            render_expander_line(
                                &mut lines,
                                &mut line_idx,
                                ctx.current_line_idx,
                                ExpandDirection::Up,
                                remaining,
                                &app.theme,
                            );
                        } else if remaining >= GAP_EXPAND_BATCH {
                            render_expander_line(
                                &mut lines,
                                &mut line_idx,
                                ctx.current_line_idx,
                                ExpandDirection::Down,
                                remaining,
                                &app.theme,
                            );
                            render_hidden_lines(
                                &mut lines,
                                &mut line_idx,
                                ctx.current_line_idx,
                                remaining,
                                &app.theme,
                            );
                            render_expander_line(
                                &mut lines,
                                &mut line_idx,
                                ctx.current_line_idx,
                                ExpandDirection::Up,
                                remaining,
                                &app.theme,
                            );
                        } else {
                            render_expander_line(
                                &mut lines,
                                &mut line_idx,
                                ctx.current_line_idx,
                                ExpandDirection::Both,
                                remaining,
                                &app.theme,
                            );
                        }
                    }

                    // Render bottom expanded lines
                    if let Some(bot) = bot_lines {
                        for expanded_line in bot {
                            if !ctx.is_visible(line_idx) {
                                lines.push(Line::default());
                                line_idx += 1;
                                continue;
                            }
                            render_sbs_expanded_context_line(
                                &mut lines,
                                &mut line_idx,
                                expanded_line,
                                &ctx,
                            );
                        }
                    }
                }

                // Hunk header
                let is_hunk_reviewed = app.is_hunk_reviewed(file_idx, hunk_idx);
                let (hunk_header_text, hunk_header_style) =
                    hunk_header_text_and_style(&app.theme, hunk, is_hunk_reviewed);
                let indicator = cursor_indicator_spaced(line_idx, ctx.current_line_idx);
                lines.push(Line::from(vec![
                    Span::styled(indicator, styles::current_line_indicator_style(&app.theme)),
                    Span::styled(hunk_header_text, hunk_header_style),
                ]));
                line_idx += 1;
                if app.should_collapse_hunk(file_idx, hunk_idx) {
                    continue;
                }

                // Process diff lines in side-by-side format
                let (new_line_idx, cursor_info) = render_hunk_lines_side_by_side(
                    hunk,
                    line_comments,
                    &ctx,
                    file_idx,
                    line_idx,
                    &mut lines,
                );
                line_idx = new_line_idx;
                if let Some((line, col, box_start, box_end, annotations_replaced)) = cursor_info {
                    comment_cursor_logical_line = Some(line);
                    comment_cursor_column = col;
                    comment_input_box_range = Some((box_start, box_end));
                    let box_len = box_end - box_start + 1;
                    annotation_offset = Some((box_start, box_len, annotations_replaced));
                }
            }
        }

        // End-of-file gap (after all hunks, not for deleted files)
        if file.status != FileStatus::Deleted
            && matches!(
                app.diff_source,
                DiffSource::WorkingTree
                    | DiffSource::Unstaged
                    | DiffSource::StagedAndUnstaged
                    | DiffSource::StagedUnstagedAndCommits(_)
                    | DiffSource::CommitRange(_)
                    | DiffSource::PullRequest(_)
            )
            && let Some(last_hunk) = file.hunks.last()
        {
            let eof_start = last_hunk.new_start + last_hunk.new_count;
            if let Some(&total) = app.file_line_count_cache.get(&file_idx)
                && eof_start <= total
            {
                let gap = (total - eof_start + 1) as usize;
                let eof_gap_id = GapId {
                    file_idx,
                    hunk_idx: file.hunks.len(),
                };
                let top_lines = app.expanded_top.get(&eof_gap_id);
                let bot_lines = app.expanded_bottom.get(&eof_gap_id);
                let top_len = top_lines.map_or(0, |v| v.len());
                let bot_len = bot_lines.map_or(0, |v| v.len());
                let remaining = gap.saturating_sub(top_len + bot_len);

                // Render top expanded lines (↓ direction)
                if let Some(top) = top_lines {
                    for expanded_line in top {
                        render_sbs_expanded_context_line(
                            &mut lines,
                            &mut line_idx,
                            expanded_line,
                            &ctx,
                        );
                    }
                }

                // Expander / hidden lines
                if remaining > 0 {
                    render_expander_line(
                        &mut lines,
                        &mut line_idx,
                        ctx.current_line_idx,
                        ExpandDirection::Down,
                        remaining,
                        &app.theme,
                    );
                    if remaining > GAP_EXPAND_BATCH {
                        render_hidden_lines(
                            &mut lines,
                            &mut line_idx,
                            ctx.current_line_idx,
                            remaining,
                            &app.theme,
                        );
                    }
                }

                // Render bottom expanded lines
                if let Some(bot) = bot_lines {
                    for expanded_line in bot {
                        render_sbs_expanded_context_line(
                            &mut lines,
                            &mut line_idx,
                            expanded_line,
                            &ctx,
                        );
                    }
                }
            }
        }

        // Spacing between files
        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
        lines.push(Line::from(Span::styled(
            indicator,
            styles::current_line_indicator_style(&app.theme),
        )));
        line_idx += 1;
    }

    let comment_bars = {
        let mut bars = ctx.comment_bars.borrow_mut();
        std::mem::take(&mut *bars)
    };
    let sbs_meta = {
        let mut m = ctx.sbs_meta.borrow_mut();
        std::mem::take(&mut *m)
    };
    drop(ctx);
    app.comment_input_annotation_offset = annotation_offset;

    // Auto-scroll so the comment input box stays visible while the user types.
    scroll_comment_input_into_view(
        &mut app.diff_state.scroll_offset,
        comment_input_box_range,
        comment_cursor_logical_line,
        inner.height as usize,
        lines.len(),
    );

    let visible_lines_unscrolled: Vec<Line> = lines
        .into_iter()
        .skip(app.diff_state.scroll_offset)
        .take(inner.height as usize)
        .collect();

    // Calculate the width of each line for max_content_width and visible line count
    let line_widths: Vec<usize> = visible_lines_unscrolled
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.width())
                .sum::<usize>()
        })
        .collect();

    let max_content_width = line_widths.iter().copied().max().unwrap_or(0);

    app.sync_viewport_width(inner.width as usize);
    app.diff_state.max_content_width = max_content_width;

    let scroll_offset = app.diff_state.scroll_offset;
    let wrap = app.diff_state.wrap_lines;
    let viewport_width = inner.width as usize;
    let visible_lines_unscrolled_for_overlay = visible_lines_unscrolled.clone();
    // Single pass: wrap each logical line once, producing both the visual
    // rows to render and the per-line height used by every row-mapping
    // consumer below, so the two can't disagree.
    let (row_heights, wrapped_lines): (Vec<usize>, Option<Vec<Line>>) = if wrap && content_width > 0
    {
        let mut heights = Vec::with_capacity(visible_lines_unscrolled_for_overlay.len());
        let mut out: Vec<Line> = Vec::new();
        let (left_prefix_blank, right_prefix_blank) = sbs_blank_prefixes(&app.theme, lw);
        for (i, line) in visible_lines_unscrolled_for_overlay.iter().enumerate() {
            let logical_idx = scroll_offset + i;
            match sbs_meta.get(&logical_idx) {
                Some(m) => {
                    let left_rows = if m.left_content.is_empty() {
                        vec![Vec::new()]
                    } else {
                        wrap_spans(&m.left_content, content_width)
                    };
                    let right_rows = if m.right_content.is_empty() {
                        vec![Vec::new()]
                    } else {
                        wrap_spans(&m.right_content, content_width)
                    };
                    let n = left_rows.len().max(right_rows.len()).max(1);
                    heights.push(n);
                    let empty_row: Vec<Span> = Vec::new();
                    for k in 0..n {
                        let left_content_row = left_rows.get(k).unwrap_or(&empty_row).clone();
                        let right_content_row = right_rows.get(k).unwrap_or(&empty_row).clone();
                        let left_padded =
                            pad_spans_to_width(left_content_row, content_width, m.left_pad_style);
                        let right_padded =
                            pad_spans_to_width(right_content_row, content_width, m.right_pad_style);
                        let (left_prefix, right_prefix) = if k == 0 {
                            (m.left_prefix.clone(), m.right_prefix.clone())
                        } else {
                            (left_prefix_blank.clone(), right_prefix_blank.clone())
                        };
                        let mut spans = left_prefix;
                        spans.extend(left_padded);
                        spans.extend(right_prefix);
                        spans.extend(right_padded);
                        out.push(Line::from(spans));
                    }
                }
                None => {
                    let rows = wrap_spans(&line.spans, viewport_width);
                    heights.push(rows.len());
                    out.extend(rows.into_iter().map(Line::from));
                }
            }
        }
        (heights, Some(out))
    } else {
        (vec![1; visible_lines_unscrolled_for_overlay.len()], None)
    };
    app.diff_state.visible_line_count = populate_row_to_annotation(
        &mut app.diff_row_to_annotation,
        &row_heights,
        viewport_width,
        inner.height as usize,
        wrap,
        scroll_offset,
    );

    let max_scroll_x = max_content_width.saturating_sub(viewport_width);
    if app.diff_state.scroll_x > max_scroll_x {
        app.diff_state.scroll_x = max_scroll_x;
    }
    if app.diff_state.wrap_lines {
        app.diff_state.scroll_x = 0;
    }

    let scroll_x = app.diff_state.scroll_x;
    let visible_lines: Vec<Line> = match wrapped_lines {
        Some(out) => out,
        None => visible_lines_unscrolled
            .into_iter()
            .map(|line| apply_horizontal_scroll(line, scroll_x))
            .collect(),
    };

    let overlay_ctx = crate::ui::diff_view::DiffOverlayPaint {
        inner,
        visible_lines_unscrolled: &visible_lines_unscrolled_for_overlay,
        line_widths: &line_widths,
        row_heights: &row_heights,
        wrap_lines: app.diff_state.wrap_lines,
        viewport_width: inner.width as usize,
        scroll_x,
        scroll_offset: app.diff_state.scroll_offset,
        theme: &app.theme,
        comment_bars: &comment_bars,
    };

    // Section-marker row tint (hunk headers + expand/hidden stubs).
    crate::ui::diff_view::paint_section_highlight(frame, &overlay_ctx);

    let diff = Paragraph::new(visible_lines).style(styles::panel_style(&app.theme));
    frame.render_widget(diff, inner);

    paint_cursor_line_highlight(
        frame,
        inner,
        &visible_lines_unscrolled_for_overlay,
        &row_heights,
        app,
    );

    // Painted last so the cell overlay wins over cursor-line bg on overlap.
    if let Some(sel) = app.visual_selection {
        paint_visual_selection_overlay(frame, inner, app, sel, &app.theme);
    }

    // File-section header rules extended to the full viewport width.
    crate::ui::diff_view::paint_file_header_fill(frame, &overlay_ctx);

    // Comment-box overlays painted last so the box + bar always win on their
    // single cells.
    crate::ui::diff_view::paint_comment_box_bar(frame, &overlay_ctx);
    crate::ui::diff_view::paint_comment_box_right_border(frame, &overlay_ctx);

    // Calculate screen position for comment cursor if in Comment mode
    if let Some(cursor_logical_line) = comment_cursor_logical_line {
        let scroll_offset = app.diff_state.scroll_offset;
        let visible_lines_count = app.diff_state.visible_line_count.max(1);

        // Check if the cursor line is visible (after scrolling)
        if cursor_logical_line >= scroll_offset
            && cursor_logical_line < scroll_offset + visible_lines_count
        {
            // Calculate screen row - need to account for wrapping
            let logical_offset = cursor_logical_line - scroll_offset;

            let mut visual_row: u16 = 0;
            let viewport_width = inner.width as usize;

            if app.diff_state.wrap_lines && viewport_width > 0 {
                for i in 0..logical_offset {
                    visual_row += row_heights.get(i).copied().unwrap_or(1) as u16;
                }
            } else {
                visual_row = logical_offset as u16;
            }

            let screen_col = inner.x + comment_cursor_column;
            let screen_row_abs = inner.y + visual_row;

            app.comment_cursor_screen_pos = Some((screen_col, screen_row_abs));
        }
    }
}

/// Render a single expanded context line in side-by-side mode
fn render_sbs_expanded_context_line(
    lines: &mut Vec<Line<'_>>,
    line_idx: &mut usize,
    expanded_line: &crate::model::DiffLine,
    ctx: &SideBySideContext,
) {
    let theme = ctx.theme;
    let lw = ctx.lineno_width;
    let content_width = ctx.content_width;
    let indicator = cursor_indicator(*line_idx, ctx.current_line_idx);
    let old_line_num = ctx
        .display_lineno(expanded_line.old_lineno, *line_idx)
        .map(|n| format!("{n:>lw$} "))
        .unwrap_or_else(|| " ".repeat(lw + 1));
    let new_line_num = ctx
        .display_lineno(expanded_line.new_lineno, *line_idx)
        .map(|n| format!("{n:>lw$} "))
        .unwrap_or_else(|| " ".repeat(lw + 1));
    let ec_style = styles::expanded_context_style(theme);
    let content_cell = plain_cell_spans(
        &expanded_line.content,
        ec_style,
        content_width,
        ctx.search_for(*line_idx),
    );
    let mut line_spans = vec![
        Span::styled(indicator, styles::current_line_indicator_style(theme)),
        Span::styled(old_line_num.clone(), ec_style),
        Span::styled(" ", ec_style),
    ];
    line_spans.extend(content_cell.clone());
    line_spans.extend([
        Span::styled(" │ ", styles::dim_style(theme)),
        Span::styled(new_line_num.clone(), ec_style),
        Span::styled(" ", ec_style),
    ]);
    line_spans.extend(content_cell);
    lines.push(Line::from(line_spans));

    let dim = styles::dim_style(theme);
    let left_prefix = vec![
        Span::styled(indicator, styles::current_line_indicator_style(theme)),
        Span::styled(old_line_num, ec_style),
        Span::styled(" ", ec_style),
    ];
    let right_prefix = vec![
        Span::styled(" │ ", dim),
        Span::styled(new_line_num, ec_style),
        Span::styled(" ", ec_style),
    ];
    let mut content = vec![Span::styled(expanded_line.content.clone(), ec_style)];
    if let Some((needle, hl)) = ctx.search_for(*line_idx) {
        content = apply_search_highlight_spans(content, needle, hl);
    }
    ctx.sbs_meta.borrow_mut().insert(
        *line_idx,
        SbsRowMeta {
            left_content: content.clone(),
            right_content: content,
            left_prefix,
            right_prefix,
            left_pad_style: ec_style,
            right_pad_style: ec_style,
        },
    );
    *line_idx += 1;
}

/// Process and render all diff lines in a hunk for side-by-side view
/// Returns (new_line_idx, optional cursor info for inline comment input)
fn render_hunk_lines_side_by_side(
    hunk: &DiffHunk,
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, Option<SideBySideCursorInfo>) {
    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;

    // A commit message is a synthetic "added" file; its lines are Context so
    // the unified view renders them neutrally. In side-by-side that would
    // duplicate the message across both columns, so render it right-side only
    // as an addition instead.
    let is_commit_msg = ctx
        .app
        .diff_files
        .get(file_idx)
        .is_some_and(|f| f.is_commit_message);

    for segment in hunk.segments() {
        let (new_line_idx, cursor_info) = match segment {
            HunkSegment::Context(i) if is_commit_msg => render_commit_message_line_side_by_side(
                &hunk.lines[i],
                line_comments,
                ctx,
                file_idx,
                line_idx,
                lines,
            ),
            HunkSegment::Context(i) => render_context_line_side_by_side(
                &hunk.lines[i],
                line_comments,
                ctx,
                file_idx,
                line_idx,
                lines,
            ),
            HunkSegment::ChangeBlock(block) => render_change_block_side_by_side(
                hunk,
                &block,
                line_comments,
                ctx,
                file_idx,
                line_idx,
                lines,
            ),
        };
        line_idx = new_line_idx;
        if cursor_info.is_some() {
            cursor_info_out = cursor_info;
        }
    }
    (line_idx, cursor_info_out)
}

/// Render a context line (appears on both sides)
/// Returns (new_line_idx, optional cursor info for inline comment input)
fn render_context_line_side_by_side(
    diff_line: &crate::model::DiffLine,
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, Option<SideBySideCursorInfo>) {
    if ctx.is_visible(line_idx) {
        let w = ctx.lineno_width;
        let old_line_num = ctx
            .display_lineno(diff_line.old_lineno, line_idx)
            .map(|n| format!("{n:>w$}"))
            .unwrap_or_else(|| " ".repeat(w));
        let new_line_num = ctx
            .display_lineno(diff_line.new_lineno, line_idx)
            .map(|n| format!("{n:>w$}"))
            .unwrap_or_else(|| " ".repeat(w));

        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);

        let mut spans = vec![
            Span::styled(indicator, styles::current_line_indicator_style(ctx.theme)),
            Span::styled(format!("{old_line_num} "), styles::dim_style(ctx.theme)),
            Span::styled(" ".to_string(), styles::diff_context_style(ctx.theme)),
        ];

        let search = ctx.search_for(line_idx);
        let content_cell = if let Some(ref highlighted) = diff_line.highlighted_spans {
            searched_cell_spans(
                highlighted,
                ctx.content_width,
                styles::diff_context_style(ctx.theme),
                search,
            )
        } else {
            plain_cell_spans(
                &diff_line.content,
                styles::diff_context_style(ctx.theme),
                ctx.content_width,
                search,
            )
        };

        // Left side content - use syntax highlighting if available
        spans.extend(content_cell.clone());

        // Separator
        spans.push(Span::styled(" │ ", styles::dim_style(ctx.theme)));
        spans.push(Span::styled(
            format!("{new_line_num} "),
            styles::dim_style(ctx.theme),
        ));
        spans.push(Span::styled(
            " ".to_string(),
            styles::diff_context_style(ctx.theme),
        ));

        // Right side content - use same highlighting
        spans.extend(content_cell);

        lines.push(Line::from(spans));

        let content = content_spans_for_diff_line(ctx.theme, diff_line, &[], search);
        let ctx_style = styles::diff_context_style(ctx.theme);
        let (lp, rp) = sbs_row_prefixes(
            ctx.theme,
            indicator,
            SideSpec {
                lineno: ctx.display_lineno(diff_line.old_lineno, line_idx),
                marker: " ",
                marker_style: ctx_style,
            },
            SideSpec {
                lineno: ctx.display_lineno(diff_line.new_lineno, line_idx),
                marker: " ",
                marker_style: ctx_style,
            },
            w,
        );
        ctx.sbs_meta.borrow_mut().insert(
            line_idx,
            SbsRowMeta {
                left_content: content.clone(),
                right_content: content,
                left_prefix: lp,
                right_prefix: rp,
                left_pad_style: ctx_style,
                right_pad_style: ctx_style,
            },
        );
    } else {
        lines.push(Line::default());
    }
    line_idx += 1;

    // Add comments if any
    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;
    if let Some(new_ln) = diff_line.new_lineno {
        let (new_line_idx, cursor_info) = add_comments_to_line(
            new_ln,
            line_comments,
            LineSide::New,
            ctx,
            file_idx,
            line_idx,
            lines,
        );
        line_idx = new_line_idx;
        cursor_info_out = cursor_info;
        if let Some(file) = ctx.app.diff_files.get(file_idx) {
            line_idx = add_remote_threads_to_line(
                new_ln,
                LineSide::New,
                ctx,
                file.display_path(),
                line_idx,
                lines,
            );
        }
    }

    (line_idx, cursor_info_out)
}

/// Render a change block as side-by-side rows: each line pair on one row,
/// then the unpaired tail with the other column empty.
/// Returns (line_idx, optional cursor info for inline comment input)
fn render_change_block_side_by_side(
    hunk: &DiffHunk,
    block: &ChangeBlock,
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, Option<SideBySideCursorInfo>) {
    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;

    for (del_idx, add_idx) in block.rows() {
        let del_opt = del_idx.map(|idx| &hunk.lines[idx]);
        let add_opt = add_idx.map(|idx| &hunk.lines[idx]);
        if ctx.is_visible(line_idx) {
            let word_ranges = del_opt
                .zip(add_opt)
                .map(|(del, add)| line_pair_ranges(ctx.app, del, add))
                .unwrap_or_default();
            let left = del_opt.map_or_else(ChangeColumn::empty, |dl| {
                ChangeColumn::for_line(ctx, dl, &word_ranges.deletion, line_idx)
            });
            let right = add_opt.map_or_else(ChangeColumn::empty, |al| {
                ChangeColumn::for_line(ctx, al, &word_ranges.addition, line_idx)
            });

            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
            let (left_prefix, right_prefix) = sbs_row_prefixes(
                ctx.theme,
                indicator,
                left.side,
                right.side,
                ctx.lineno_width,
            );

            let mut spans = left_prefix.clone();
            spans.extend(left.cell(ctx.content_width));
            spans.extend(right_prefix.clone());
            spans.extend(right.cell(ctx.content_width));
            lines.push(Line::from(spans));

            ctx.sbs_meta.borrow_mut().insert(
                line_idx,
                SbsRowMeta {
                    left_content: left.content,
                    right_content: right.content,
                    left_prefix,
                    right_prefix,
                    left_pad_style: left.pad_style,
                    right_pad_style: right.pad_style,
                },
            );
        } else {
            lines.push(Line::default());
        }
        line_idx += 1;

        // Add comments for deletion
        if let Some(del_line) = del_opt
            && let Some(old_ln) = del_line.old_lineno
        {
            let (new_line_idx, cursor_info) = add_comments_to_line(
                old_ln,
                line_comments,
                LineSide::Old,
                ctx,
                file_idx,
                line_idx,
                lines,
            );
            line_idx = new_line_idx;
            if cursor_info.is_some() {
                cursor_info_out = cursor_info;
            }
            if let Some(file) = ctx.app.diff_files.get(file_idx) {
                line_idx = add_remote_threads_to_line(
                    old_ln,
                    LineSide::Old,
                    ctx,
                    file.display_path(),
                    line_idx,
                    lines,
                );
            }
        }

        // Add comments for addition
        if let Some(add_line) = add_opt
            && let Some(new_ln) = add_line.new_lineno
        {
            let (new_line_idx, cursor_info) = add_comments_to_line(
                new_ln,
                line_comments,
                LineSide::New,
                ctx,
                file_idx,
                line_idx,
                lines,
            );
            line_idx = new_line_idx;
            if cursor_info.is_some() {
                cursor_info_out = cursor_info;
            }
            if let Some(file) = ctx.app.diff_files.get(file_idx) {
                line_idx = add_remote_threads_to_line(
                    new_ln,
                    LineSide::New,
                    ctx,
                    file.display_path(),
                    line_idx,
                    lines,
                );
            }
        }
    }

    (line_idx, cursor_info_out)
}

/// Render a commit-message line in side-by-side mode. The commit message is a
/// synthetic "added" file, but visually it is prose, not code: delta renders it
/// as a full-width block, not confined to a diff column. So we emit a single
/// full-width, left-aligned, neutrally-styled line with no column split, diff
/// coloring, or per-column line numbers. It is deliberately NOT inserted into
/// `sbs_meta`, so the wrap path falls through to the full-width wrapping branch.
fn render_commit_message_line_side_by_side(
    diff_line: &crate::model::DiffLine,
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, Option<SideBySideCursorInfo>) {
    let ctx_style = styles::diff_context_style(ctx.theme);

    if ctx.is_visible(line_idx) {
        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
        let mut spans = vec![Span::styled(
            indicator,
            styles::current_line_indicator_style(ctx.theme),
        )];
        // Git-style two-space indent, then the message text at full width.
        spans.push(Span::styled("  ".to_string(), ctx_style));
        spans.push(Span::styled(diff_line.content.clone(), ctx_style));

        lines.push(Line::from(spans));
    } else {
        lines.push(Line::default());
    }
    line_idx += 1;

    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;
    if let Some(new_ln) = diff_line.new_lineno {
        let (new_line_idx, cursor_info) = add_comments_to_line(
            new_ln,
            line_comments,
            LineSide::New,
            ctx,
            file_idx,
            line_idx,
            lines,
        );
        line_idx = new_line_idx;
        cursor_info_out = cursor_info;
        if let Some(file) = ctx.app.diff_files.get(file_idx) {
            line_idx = add_remote_threads_to_line(
                new_ln,
                LineSide::New,
                ctx,
                file.display_path(),
                line_idx,
                lines,
            );
        }
    }

    (line_idx, cursor_info_out)
}

/// Add comments for a specific line.
/// Returns (new_line_idx, optional cursor info for inline comment input)
/// Render remote review threads anchored at this `(file, line, side)`
/// position into the side-by-side rendering. Mirrors the unified-view
/// helper but uses the side-by-side cursor indicator path.
fn add_remote_threads_to_line(
    line_num: u32,
    side: LineSide,
    ctx: &SideBySideContext,
    file_path: &std::path::Path,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> usize {
    use crate::forge::remote_comments::{PrCommentsVisibility, RemoteCommentSide};
    let visibility = ctx.app.session.remote_comments_visibility;
    if matches!(visibility, PrCommentsVisibility::Hide) {
        return line_idx;
    }
    let target_path = file_path.to_string_lossy();
    for thread in &ctx.app.forge_review_threads {
        let Some(muted) = visibility.render_decision(thread) else {
            continue;
        };
        if thread.path != *target_path {
            continue;
        }
        let Some(thread_line) = thread.line else {
            continue;
        };
        if thread_line != line_num {
            continue;
        }
        let matches_side = matches!(
            (thread.side, side),
            (RemoteCommentSide::Right, LineSide::New) | (RemoteCommentSide::Left, LineSide::Old)
        );
        if !matches_side {
            continue;
        }
        let thread_lines = comment_panel::format_remote_thread_lines(
            ctx.theme,
            thread,
            muted,
            ctx.app.forge_kind(),
        );
        let box_top_row = line_idx;
        for mut comment_line in thread_lines {
            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
            comment_line.spans.insert(
                0,
                Span::styled(indicator, styles::current_line_indicator_style(ctx.theme)),
            );
            lines.push(comment_line);
            line_idx += 1;
        }
        crate::ui::diff_view::push_comment_bar(
            &mut ctx.comment_bars.borrow_mut(),
            box_top_row,
            Some(crate::model::LineRange::single(thread_line)),
        );
    }
    line_idx
}

fn add_comments_to_line(
    line_num: u32,
    line_comments: &std::collections::HashMap<u32, Vec<crate::model::Comment>>,
    side: LineSide,
    ctx: &SideBySideContext,
    file_idx: usize,
    mut line_idx: usize,
    lines: &mut Vec<Line>,
) -> (usize, Option<SideBySideCursorInfo>) {
    // Check if we're adding/editing a comment on this line and side
    let is_line_comment_mode = ctx.comment_input_mode
        && file_idx == ctx.current_file_idx
        && ctx.comment_line == Some((line_num, side));
    let mut cursor_info_out: Option<SideBySideCursorInfo> = None;

    if let Some(comments) = line_comments.get(&line_num) {
        for comment in comments {
            let comment_side = comment.side.unwrap_or(LineSide::New);
            if ((side == LineSide::Old && comment_side == LineSide::Old)
                || (side == LineSide::New && comment_side != LineSide::Old))
                && ctx.app.comment_visible(comment)
            {
                // Check if this comment is being edited
                let is_being_edited =
                    is_line_comment_mode && ctx.editing_comment_id == Some(comment.id.as_str());

                if is_being_edited {
                    // Render inline input instead
                    let line_range = ctx
                        .comment_line_range
                        .or_else(|| Some(LineRange::single(line_num)));
                    let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
                        ctx.theme,
                        comment_type_presentation(ctx.app, &ctx.comment_type),
                        ctx.comment_buffer,
                        ctx.comment_cursor,
                        line_range,
                        true,
                        ctx.panel_width.saturating_sub(1),
                        ctx.app
                            .comment_vim_mode_label()
                            .as_ref()
                            .map(|(t, w)| (t.as_str(), *w)),
                        ctx.app.supports_keyboard_enhancement,
                    );
                    let box_top_row = line_idx;
                    let box_end = line_idx + input_lines.len().saturating_sub(1);
                    let annotations_replaced = App::comment_display_lines(comment, ctx.panel_width);
                    cursor_info_out = Some((
                        line_idx + cursor_info.line_offset,
                        1 + cursor_info.column,
                        line_idx,
                        box_end,
                        annotations_replaced,
                    ));

                    for mut input_line in input_lines {
                        let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                        input_line.spans.insert(
                            0,
                            Span::styled(
                                indicator,
                                styles::current_line_indicator_style(ctx.theme),
                            ),
                        );
                        lines.push(input_line);
                        line_idx += 1;
                    }
                    crate::ui::diff_view::push_comment_bar(
                        &mut ctx.comment_bars.borrow_mut(),
                        box_top_row,
                        line_range,
                    );
                } else {
                    let line_range = comment
                        .line_range
                        .or_else(|| Some(LineRange::single(line_num)));
                    let box_top_row = line_idx;
                    let rows = App::comment_display_lines(comment, ctx.panel_width);
                    // The bar is recorded either way: it is painted above the
                    // box, so it can be on screen while the box itself is not.
                    if !ctx.box_visible(line_idx, rows) {
                        skip_comment_box(lines, &mut line_idx, rows);
                    } else {
                        let comment_lines = comment_panel::format_comment_lines(
                            ctx.theme,
                            comment_type_presentation(ctx.app, &comment.comment_type),
                            &comment.content,
                            line_range,
                            ctx.panel_width.saturating_sub(1),
                            (comment.author != ctx.app.username).then_some(comment.author.as_str()),
                        );
                        for mut comment_line in comment_lines {
                            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
                            comment_line.spans.insert(
                                0,
                                Span::styled(
                                    indicator,
                                    styles::current_line_indicator_style(ctx.theme),
                                ),
                            );
                            lines.push(comment_line);
                            line_idx += 1;
                        }
                    }
                    crate::ui::diff_view::push_comment_bar(
                        &mut ctx.comment_bars.borrow_mut(),
                        box_top_row,
                        line_range,
                    );
                }
            }
        }
    }

    // Render inline input for new line comment
    if is_line_comment_mode && ctx.editing_comment_id.is_none() {
        let line_range = ctx
            .comment_line_range
            .or_else(|| Some(LineRange::single(line_num)));
        let (input_lines, cursor_info) = comment_panel::format_comment_input_lines(
            ctx.theme,
            comment_type_presentation(ctx.app, &ctx.comment_type),
            ctx.comment_buffer,
            ctx.comment_cursor,
            line_range,
            false,
            ctx.panel_width.saturating_sub(1),
            ctx.app
                .comment_vim_mode_label()
                .as_ref()
                .map(|(t, w)| (t.as_str(), *w)),
            ctx.app.supports_keyboard_enhancement,
        );
        let box_top_row = line_idx;
        let box_end = line_idx + input_lines.len().saturating_sub(1);
        cursor_info_out = Some((
            line_idx + cursor_info.line_offset,
            1 + cursor_info.column,
            line_idx,
            box_end,
            0,
        ));

        for mut input_line in input_lines {
            let indicator = cursor_indicator(line_idx, ctx.current_line_idx);
            input_line.spans.insert(
                0,
                Span::styled(indicator, styles::current_line_indicator_style(ctx.theme)),
            );
            lines.push(input_line);
            line_idx += 1;
        }
        crate::ui::diff_view::push_comment_bar(
            &mut ctx.comment_bars.borrow_mut(),
            box_top_row,
            line_range,
        );
    }

    (line_idx, cursor_info_out)
}

#[cfg(test)]
mod remote_comments_side_by_side_snapshot_tests {
    //! Render-snapshot tests for inline remote review threads in the
    //! side-by-side diff view. Confirms the badge appears at least once
    //! when a thread is active and is hidden under `:comments hide`.
    use crate::app::{App, DiffSource, DiffViewMode, InputMode, PullRequestDiffSource};
    use crate::error::Result as TuicrResult;
    use crate::error::TuicrError;
    use crate::forge::remote_comments::{
        PrCommentsVisibility, RemoteCommentSide, RemoteReviewComment, RemoteReviewThread,
    };
    use crate::forge::traits::{ForgeRepository, PrSessionKey};
    use crate::model::{
        DiffFile, DiffHunk, DiffLine, FileStatus, LineOrigin, ReviewSession, SessionDiffSource,
    };
    use crate::syntax::SyntaxHighlighter;
    use crate::theme::Theme;
    use crate::ui::render;
    use crate::vcs::traits::{VcsBackend, VcsChangeStatus, VcsInfo, VcsType};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use std::path::{Path, PathBuf};

    struct SnapshotVcs {
        info: VcsInfo,
    }

    impl VcsBackend for SnapshotVcs {
        fn info(&self) -> &VcsInfo {
            &self.info
        }
        fn get_working_tree_diff(
            &self,
            _highlighter: &SyntaxHighlighter,
        ) -> TuicrResult<Vec<DiffFile>> {
            Err(TuicrError::NoChanges)
        }
        fn fetch_context_lines(
            &self,
            _file_path: &Path,
            _file_status: FileStatus,
            _ref_commit: Option<&str>,
            _start_line: u32,
            _end_line: u32,
        ) -> TuicrResult<Vec<DiffLine>> {
            Ok(Vec::new())
        }
        fn get_change_status(&self) -> TuicrResult<VcsChangeStatus> {
            Ok(VcsChangeStatus {
                staged: false,
                unstaged: false,
            })
        }
        fn file_line_count(
            &self,
            _file_path: &Path,
            _file_status: FileStatus,
            _ref_commit: Option<&str>,
        ) -> TuicrResult<u32> {
            Ok(0)
        }
    }

    fn repo() -> ForgeRepository {
        ForgeRepository::github("github.com", "agavra", "tuicr")
    }

    fn sample_diff_file() -> DiffFile {
        let lines = vec![
            DiffLine {
                origin: LineOrigin::Context,
                content: "first".to_string(),
                old_lineno: Some(1),
                new_lineno: Some(1),
                highlighted_spans: None,
            },
            DiffLine {
                origin: LineOrigin::Addition,
                content: "second".to_string(),
                old_lineno: None,
                new_lineno: Some(2),
                highlighted_spans: None,
            },
        ];
        let hunk = DiffHunk {
            header: "@@ -1,1 +1,2 @@".to_string(),
            lines,
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
        };
        let hunks = vec![hunk];
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

    fn thread() -> RemoteReviewThread {
        RemoteReviewThread {
            id: "T".to_string(),
            path: "src/lib.rs".to_string(),
            line: Some(2),
            side: RemoteCommentSide::Right,
            is_resolved: false,
            is_outdated: false,
            comments: vec![RemoteReviewComment {
                id: "C".to_string(),
                author: Some("alice".to_string()),
                body: "sbs hello".to_string(),
                created_at: None,
                in_reply_to: None,
                url: "https://example.com".to_string(),
            }],
        }
    }

    fn make_pr_app() -> App {
        make_pr_app_with(vec![sample_diff_file()])
    }

    pub(super) fn make_pr_app_with(diff_files: Vec<DiffFile>) -> App {
        let pr = PullRequestDiffSource {
            key: PrSessionKey::new(repo(), 125, "headsha".to_string()),
            base_sha: "basesha".to_string(),
            title: "test pr".to_string(),
            url: "https://example.com".to_string(),
            head_ref_name: "feat".to_string(),
            base_ref_name: "main".to_string(),
            state: "OPEN".to_string(),
            closed: false,
            merged: false,
        };
        let vcs_info = VcsInfo {
            root_path: PathBuf::from("forge:github.com/agavra/tuicr"),
            head_commit: "headsha".to_string(),
            branch_name: Some("feat".to_string()),
            vcs_type: VcsType::File,
        };
        let mut session = ReviewSession::new(
            vcs_info.root_path.clone(),
            "headsha".to_string(),
            Some("feat".to_string()),
            SessionDiffSource::PullRequest,
        );
        session.pr_session_key = Some(pr.key.clone());
        let mut app = App::build(
            Box::new(SnapshotVcs {
                info: vcs_info.clone(),
            }),
            vcs_info,
            Theme::dark(),
            None,
            false,
            diff_files,
            session,
            DiffSource::PullRequest(Box::new(pr)),
            InputMode::Normal,
            Vec::new(),
            None,
            None,
        )
        .expect("build app");
        app.diff_view_mode = DiffViewMode::SideBySide;
        app
    }

    fn draw(app: &mut App) -> Buffer {
        let backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, app))
            .expect("draw frame");
        terminal.backend().buffer().clone()
    }

    fn body_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Side-by-side mirror of the unified culling test: the skip/emit wiring
    /// here goes through `ctx.box_visible` and a by-value `line_idx`, so it
    /// needs its own coverage.
    #[test]
    fn should_cull_comment_boxes_outside_the_viewport() {
        use crate::app::AnnotatedLine;
        use crate::model::{Comment, CommentType};

        const NEEDLE: &str = "far-below-the-fold";

        let lines: Vec<DiffLine> = (1..=120)
            .map(|n| DiffLine {
                origin: LineOrigin::Addition,
                content: format!("line {n}"),
                old_lineno: None,
                new_lineno: Some(n),
                highlighted_spans: None,
            })
            .collect();
        let hunks = vec![DiffHunk {
            header: "@@ -0,0 +1,120 @@".to_string(),
            lines,
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 120,
        }];
        let content_hash = DiffFile::compute_content_hash(&hunks);
        let path = PathBuf::from("src/lib.rs");
        let file = DiffFile {
            old_path: Some(path.clone()),
            new_path: Some(path.clone()),
            status: FileStatus::Modified,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: false,
            content_hash,
        };

        let mut app = make_pr_app_with(vec![file]);
        app.session
            .get_file_mut(&path)
            .expect("file registered in session")
            .add_line_comment(
                100,
                Comment::new(NEEDLE.to_string(), CommentType::from_id("note"), None),
            );
        app.rebuild_annotations();

        let body = body_text(&draw(&mut app));
        assert!(
            !body.contains(NEEDLE),
            "off-screen comment should not be visible:\n{body}"
        );

        let comment_row = app
            .line_annotations
            .iter()
            .position(|a| matches!(a, AnnotatedLine::LineComment { .. }))
            .expect("comment annotated in the document");
        app.diff_state.scroll_offset = comment_row;
        app.diff_state.cursor_line = comment_row;

        let body = body_text(&draw(&mut app));
        assert!(
            body.contains(NEEDLE),
            "comment scrolled into view should render at its annotated row:\n{body}"
        );
    }

    #[test]
    fn should_render_remote_comment_inline_in_side_by_side_diff() {
        // given
        let mut app = make_pr_app();
        app.forge_review_threads = vec![thread()];
        app.rebuild_annotations();
        // when
        let buffer = draw(&mut app);
        // then
        let body = body_text(&buffer);
        assert!(
            body.contains("[github @alice]"),
            "expected badge in side-by-side render:\n{body}"
        );
    }

    #[test]
    fn should_hide_remote_comments_under_comments_hide_in_side_by_side() {
        // given
        let mut app = make_pr_app();
        app.forge_review_threads = vec![thread()];
        app.set_remote_comments_visibility(PrCommentsVisibility::Hide);
        // when
        let buffer = draw(&mut app);
        // then
        let body = body_text(&buffer);
        assert!(
            !body.contains("[github @alice"),
            "remote comment leaked under Hide:\n{body}"
        );
    }

    fn diff_file_with_pair(left: &str, right: &str) -> DiffFile {
        let lines = vec![
            DiffLine {
                origin: LineOrigin::Deletion,
                content: left.to_string(),
                old_lineno: Some(1),
                new_lineno: None,
                highlighted_spans: None,
            },
            DiffLine {
                origin: LineOrigin::Addition,
                content: right.to_string(),
                old_lineno: None,
                new_lineno: Some(1),
                highlighted_spans: None,
            },
        ];
        let hunks = vec![DiffHunk {
            header: "@@ -1,1 +1,1 @@".to_string(),
            lines,
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
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

    fn diff_file_with_standalone_deletion(left: &str) -> DiffFile {
        let lines = vec![DiffLine {
            origin: LineOrigin::Deletion,
            content: left.to_string(),
            old_lineno: Some(1),
            new_lineno: None,
            highlighted_spans: None,
        }];
        let hunks = vec![DiffHunk {
            header: "@@ -1,1 +0,0 @@".to_string(),
            lines,
            old_start: 1,
            old_count: 1,
            new_start: 0,
            new_count: 0,
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

    pub(super) fn draw_sbs(app: &mut App, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render_side_by_side_diff(frame, app, Rect::new(0, 0, w, h)))
            .expect("draw sbs");
        terminal.backend().buffer().clone()
    }

    fn char_at(buf: &Buffer, x: u16, y: u16) -> String {
        buf[(x, y)].symbol().to_string()
    }

    #[test]
    fn should_wrap_long_line_in_side_by_side_view_when_wrap_enabled() {
        let long_left = "L".repeat(200);
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_pair(&long_left, "short")];
        app.set_diff_wrap(true);
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);

        let mut rows_with_l = 0u16;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
            if row.contains("LLLLLLLLLL") {
                rows_with_l += 1;
            }
        }
        assert!(
            rows_with_l >= 2,
            "expected long left content to span >=2 visual rows, got {rows_with_l}"
        );
    }

    #[test]
    fn should_not_wrap_when_wrap_disabled_in_side_by_side() {
        let long_left = "L".repeat(200);
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_pair(&long_left, "short")];
        app.set_diff_wrap(false);
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);

        let rows_with_l: u16 = (0..buf.area.height)
            .filter(|&y| {
                (0..buf.area.width)
                    .map(|x| char_at(&buf, x, y))
                    .collect::<String>()
                    .contains("LLLLLLLLLL")
            })
            .count() as u16;
        assert_eq!(
            rows_with_l, 1,
            "wrap-off should produce exactly one row of L, got {rows_with_l}"
        );
    }

    #[test]
    fn should_align_divider_on_wrapped_rows_in_side_by_side() {
        let long_left = "L".repeat(200);
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_pair(&long_left, "short")];
        app.set_diff_wrap(true);
        app.rebuild_annotations();

        let lw = 1usize;
        let inner_w = 158usize;
        let content_width = (inner_w - crate::app::sbs_overhead(lw) as usize) / 2;
        let divider_x_inner = crate::app::sbs_left_gutter(lw) as usize + content_width;
        let divider_glyph_x = 1 + divider_x_inner + 1;

        let buf = draw_sbs(&mut app, 160, 20);

        let mut rows_with_l: Vec<u16> = Vec::new();
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
            if row.contains("LLLLLLLLLL") {
                rows_with_l.push(y);
            }
        }
        assert!(
            rows_with_l.len() >= 2,
            "expected ≥2 wrapped rows, got {}",
            rows_with_l.len()
        );
        for y in &rows_with_l {
            let glyph = char_at(&buf, divider_glyph_x as u16, *y);
            assert_eq!(
                glyph, "│",
                "expected │ at col {divider_glyph_x} on row {y}, got {glyph:?}"
            );
        }
    }

    #[test]
    fn should_pad_shorter_column_on_wrapped_rows_in_side_by_side() {
        let long_left = "L".repeat(200);
        let mut app = make_pr_app();
        app.diff_files = vec![diff_file_with_standalone_deletion(&long_left)];
        app.set_diff_wrap(true);
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);

        let lw = 1usize;
        let inner_w = 158usize;
        let content_width = (inner_w - crate::app::sbs_overhead(lw) as usize) / 2;
        let divider_glyph_x = 1 + crate::app::sbs_left_gutter(lw) as usize + content_width + 1;
        let right_content_start = divider_glyph_x + 2 + lw + 1 + 1;
        let right_content_end = right_content_start + content_width;

        let mut checked = 0;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
            if !row.contains("LLLLLLLLLL") {
                continue;
            }
            checked += 1;
            let right: String = (right_content_start..right_content_end)
                .map(|x| char_at(&buf, x as u16, y))
                .collect();
            assert!(
                right.trim().is_empty(),
                "right column should be blank on wrapped L row {y}, got {right:?}"
            );
        }
        assert!(
            checked >= 2,
            "expected ≥2 wrapped rows to check, got {checked}"
        );
    }

    fn commit_message_file(message: &str) -> DiffFile {
        let lines: Vec<DiffLine> = message
            .lines()
            .enumerate()
            .map(|(i, line)| DiffLine {
                origin: LineOrigin::Context,
                content: line.to_string(),
                old_lineno: None,
                new_lineno: Some(i as u32 + 1),
                highlighted_spans: None,
            })
            .collect();
        let new_count = lines.len() as u32;
        let hunks = vec![DiffHunk {
            header: String::new(),
            lines,
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count,
        }];
        let content_hash = DiffFile::compute_content_hash(&hunks);
        DiffFile {
            old_path: None,
            new_path: Some(PathBuf::from("Commit Message (abc1234)")),
            status: FileStatus::Added,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: true,
            content_hash,
        }
    }

    #[test]
    fn should_render_commit_message_full_width_in_side_by_side() {
        let mut app = make_pr_app();
        app.diff_files = vec![commit_message_file("COMMITMSG summary line")];
        app.rebuild_annotations();

        let buf = draw_sbs(&mut app, 160, 20);

        let mut checked = 0;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| char_at(&buf, x, y)).collect();
            let Some(col) = row.find("COMMITMSG") else {
                continue;
            };
            checked += 1;
            // Full-width prose: rendered near the left edge (small indent), not
            // pushed into the right diff column, and with no column divider.
            assert!(
                col < 8,
                "commit message should start near the left edge, got col {col} on row {y}: {row:?}"
            );
            assert!(
                !row.contains(" │ "),
                "commit message row should not have a column divider on row {y}: {row:?}"
            );
        }
        assert_eq!(
            checked, 1,
            "expected the commit message body to render exactly once, got {checked}"
        );
    }
}

#[cfg(test)]
mod word_diff_render_tests {
    //! Render tests for word diff in the side-by-side view. Each draws into a
    //! `TestBackend` and asserts which cells carry the word background, so
    //! the assertions read as the marked text of a row. A line pair shares
    //! one row, so a row yields both sides' marks.
    use super::remote_comments_side_by_side_snapshot_tests::{draw_sbs, make_pr_app_with};
    use crate::app::{App, sbs_left_gutter, sbs_overhead};
    use crate::ui::word_diff::test_support::{
        body_text, cells_with_bg, change_block_file, row_containing, row_text, text_with_bg,
    };
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Style};

    fn pair_app(deletion: &str, addition: &str) -> App {
        make_pr_app_with(vec![change_block_file("context", &[deletion], &[addition])])
    }

    /// The width of each content column when the frame is `frame_width` wide.
    fn content_width(app: &App, frame_width: usize) -> usize {
        (frame_width - 2 - sbs_overhead(app.lineno_width()) as usize) / 2
    }

    /// The x of the column divider glyph when the frame is `frame_width` wide.
    fn divider_x(app: &App, frame_width: usize) -> u16 {
        let gutter = sbs_left_gutter(app.lineno_width()) as usize;
        (1 + gutter + content_width(app, frame_width) + 1) as u16
    }

    /// The text of the row containing `needle` that carries the deletion and
    /// the addition word background.
    fn marked(buffer: &Buffer, app: &App, needle: &str) -> (String, String) {
        let row = row_containing(buffer, needle);
        (
            text_with_bg(buffer, row, app.theme.word_del_bg()),
            text_with_bg(buffer, row, app.theme.word_add_bg()),
        )
    }

    #[test]
    fn should_mark_the_changed_token_in_both_columns_of_a_pair() {
        let mut app = pair_app("let x = foo;", "let x = bar;");
        let buffer = draw_sbs(&mut app, 60, 10);

        assert_eq!(
            marked(&buffer, &app, "let x = foo;"),
            ("foo".to_string(), "bar".to_string()),
            "{}",
            body_text(&buffer)
        );
        // The mark adds no characters: the indicator cell, both columns,
        // the divider, and the row below are as before.
        let row = row_containing(&buffer, "let x = foo;");
        let width = content_width(&app, 60);
        let lw = app.lineno_width();
        assert_eq!(
            row_text(&buffer, row).trim_matches('│'),
            format!(
                " {:>lw$} ▌{:width$} │ {:>lw$} ▌{:width$}",
                2, "let x = foo;", 2, "let x = bar;"
            )
        );
        assert_eq!(buffer[(divider_x(&app, 60), row)].symbol(), "│");
        assert!(
            row_text(&buffer, row + 1)
                .trim_matches('│')
                .trim()
                .is_empty(),
            "{}",
            body_text(&buffer)
        );
        assert!(
            row_text(&buffer, 1).starts_with("│▶ ═══ src/lib.rs"),
            "cursor stays on the file header:\n{}",
            body_text(&buffer)
        );
    }

    #[test]
    fn should_keep_the_cursor_row_as_before() {
        // The cursor-row paint covers every cell but a search match, so the
        // mark is hidden there like the diff background is; the row itself,
        // its indicator, and its text are unchanged.
        let mut app = pair_app("let x = foo;", "let x = bar;");
        // With wrap off, screen row `r` inside the border is line `r - 1`.
        app.set_diff_wrap(false);
        let plain = draw_sbs(&mut app, 60, 10);
        let row = row_containing(&plain, "let x = foo;");
        app.diff_state.cursor_line = row as usize - 1;
        let buffer = draw_sbs(&mut app, 60, 10);

        assert_eq!(row_containing(&buffer, "let x = foo;"), row);
        assert_eq!(
            row_text(&buffer, row),
            row_text(&plain, row).replacen(' ', "▶", 1),
            "{}",
            body_text(&buffer)
        );
        assert_eq!(
            marked(&buffer, &app, "let x = foo;"),
            (String::new(), String::new())
        );
    }

    #[test]
    fn should_mark_the_same_characters_with_wrap_on_and_off() {
        let mut app = pair_app(
            "let total = compute_total(items);",
            "let total = compute_sum(items);",
        );
        app.set_diff_wrap(false);
        let unwrapped = draw_sbs(&mut app, 120, 10);
        app.set_diff_wrap(true);
        app.rebuild_annotations();
        let wrapped = draw_sbs(&mut app, 120, 10);

        let expected = ("compute_total".to_string(), "compute_sum".to_string());
        assert_eq!(marked(&unwrapped, &app, "let total"), expected);
        assert_eq!(marked(&wrapped, &app, "let total"), expected);
        let row = row_containing(&unwrapped, "let total");
        assert_eq!(
            cells_with_bg(&unwrapped, row, app.theme.word_add_bg()),
            cells_with_bg(&wrapped, row, app.theme.word_add_bg())
        );
    }

    #[test]
    fn should_pair_an_uneven_block_by_position_and_leave_the_padded_tail_plain() {
        let mut app = make_pr_app_with(vec![change_block_file(
            "context",
            &["a = 1;", "b = 2;"],
            &["a = 10;", "b = 20;", "c = 30;"],
        )]);
        let buffer = draw_sbs(&mut app, 60, 10);

        assert_eq!(
            marked(&buffer, &app, "a = 1;"),
            ("1".to_string(), "10".to_string()),
            "{}",
            body_text(&buffer)
        );
        assert_eq!(
            marked(&buffer, &app, "b = 2;"),
            ("2".to_string(), "20".to_string())
        );
        assert_eq!(
            marked(&buffer, &app, "c = 30;"),
            (String::new(), String::new())
        );
    }

    #[test]
    fn should_leave_every_pair_plain_when_word_diff_is_off() {
        let mut app = pair_app("let x = foo;", "let x = bar;");
        app.set_word_diff(false);
        let buffer = draw_sbs(&mut app, 60, 10);

        assert_eq!(
            marked(&buffer, &app, "let x = foo;"),
            (String::new(), String::new()),
            "{}",
            body_text(&buffer)
        );

        // Turning it back on takes effect on the next frame.
        app.set_word_diff(true);
        let buffer = draw_sbs(&mut app, 60, 10);
        assert_eq!(
            marked(&buffer, &app, "let x = foo;"),
            ("foo".to_string(), "bar".to_string()),
            "{}",
            body_text(&buffer)
        );
    }

    #[test]
    fn should_truncate_the_mark_with_the_text_at_the_column_edge() {
        // With wrap off a column shows `width - 3` characters and an
        // ellipsis. The changed token starts inside the column and runs past
        // it; the unchanged head keeps the pair under the dissimilar guard.
        let head = "abcdefghijklmnopqrstuvwxyz01234567";
        let token = |prefix: &str| format!("{prefix}{}", "b".repeat(36));
        let mut app = pair_app(
            &format!("{head} {}", token("old_")),
            &format!("{head} {}", token("new_")),
        );
        app.set_diff_wrap(false);
        let buffer = draw_sbs(&mut app, 100, 10);

        let shown = content_width(&app, 100) - 3 - head.len() - 1;
        let row = row_containing(&buffer, head);
        assert_eq!(
            marked(&buffer, &app, head),
            (
                token("old_")[..shown].to_string(),
                token("new_")[..shown].to_string()
            ),
            "{}",
            body_text(&buffer)
        );
        assert_eq!(row_text(&buffer, row).matches("...").count(), 2);
        assert_eq!(buffer[(divider_x(&app, 100), row)].symbol(), "│");
    }

    #[test]
    fn should_move_the_mark_with_horizontal_scroll() {
        let mut app = pair_app("let x = foo;", "let x = bar;");
        app.set_diff_wrap(false);
        let unscrolled = draw_sbs(&mut app, 50, 10);
        app.diff_state.scroll_x = 3;
        let scrolled = draw_sbs(&mut app, 50, 10);
        assert_eq!(
            app.diff_state.scroll_x, 3,
            "the file header allows the scroll"
        );

        let row = row_containing(&unscrolled, "let x = foo;");
        for bg in [app.theme.word_del_bg(), app.theme.word_add_bg()] {
            let before = cells_with_bg(&unscrolled, row, bg);
            let after = cells_with_bg(&scrolled, row, bg);
            assert_eq!(before.len(), 3, "{}", body_text(&unscrolled));
            assert_eq!(
                after,
                before.iter().map(|x| x - 3).collect::<Vec<_>>(),
                "{}",
                body_text(&scrolled)
            );
        }
        assert_eq!(
            marked(&scrolled, &app, "x = foo;"),
            ("foo".to_string(), "bar".to_string())
        );
    }

    #[test]
    fn should_keep_the_mark_on_both_visual_rows_of_a_wrapped_column() {
        // Wrapping keeps a token whole when it fits, so the changed token is
        // wider than a column and must split. The unchanged head keeps the
        // pair under the dissimilar guard.
        let head = "a".repeat(100);
        let token = |prefix: &str| format!("{prefix}{}", "b".repeat(90));
        let mut app = pair_app(
            &format!("{head} {}", token("old_")),
            &format!("{head} {}", token("new_")),
        );
        app.set_diff_wrap(true);
        app.rebuild_annotations();
        let buffer = draw_sbs(&mut app, 160, 12);

        let first = row_containing(&buffer, "aaaaaaaaaa");
        let rows_marked = |bg: Color| -> (usize, String) {
            let rows: Vec<String> = (first..buffer.area.height)
                .map(|y| text_with_bg(&buffer, y, bg))
                .filter(|text| !text.is_empty())
                .collect();
            (rows.len(), rows.concat())
        };
        assert_eq!(
            rows_marked(app.theme.word_del_bg()),
            (2, token("old_")),
            "{}",
            body_text(&buffer)
        );
        assert_eq!(rows_marked(app.theme.word_add_bg()), (2, token("new_")));
    }

    #[test]
    fn should_show_a_search_match_inside_a_changed_token_in_the_search_background() {
        // The first match is on the context line, so the cursor row, whose
        // paint covers everything but a search match, is not the pair's row.
        let mut app = make_pr_app_with(vec![change_block_file(
            "bar context",
            &["let x = foobar;"],
            &["let x = bazbar;"],
        )]);
        app.search_buffer = "bar".to_string();
        assert!(app.search_in_diff_from_cursor());
        let buffer = draw_sbs(&mut app, 80, 10);

        let row = row_containing(&buffer, "let x = foobar;");
        assert_eq!(
            text_with_bg(&buffer, row, app.theme.search_match_bg),
            "barbar",
            "{}",
            body_text(&buffer)
        );
        assert_eq!(
            marked(&buffer, &app, "let x = foobar;"),
            ("foo".to_string(), "baz".to_string())
        );
    }

    #[test]
    fn should_mark_wide_characters_and_keep_the_divider_aligned() {
        // Columns are cut and padded by display width, so a wide character
        // does not push the divider.
        let mut app = pair_app("name = 値;", "name = 値段;");
        app.set_diff_wrap(false);
        let buffer = draw_sbs(&mut app, 60, 10);

        assert_eq!(
            marked(&buffer, &app, "name = "),
            ("値".to_string(), "値段".to_string()),
            "{}",
            body_text(&buffer)
        );
        let row = row_containing(&buffer, "name = ");
        assert_eq!(buffer[(divider_x(&app, 60), row)].symbol(), "│");
    }

    #[test]
    fn should_mark_a_syntax_highlighted_line_and_keep_its_foreground() {
        let mut file = change_block_file("context", &["let x = foo;"], &["let x = bar;"]);
        let theme = crate::theme::Theme::dark();
        let syntax = |bg: Color, tail: &str| {
            Some(vec![
                (
                    Style::default().fg(Color::Yellow).bg(bg),
                    "let ".to_string(),
                ),
                (Style::default().fg(Color::Blue).bg(bg), tail.to_string()),
            ])
        };
        file.hunks[0].lines[1].highlighted_spans = syntax(theme.syntax_del_bg, "x = foo;");
        file.hunks[0].lines[2].highlighted_spans = syntax(theme.syntax_add_bg, "x = bar;");
        let mut app = make_pr_app_with(vec![file]);
        let buffer = draw_sbs(&mut app, 60, 10);

        let row = row_containing(&buffer, "let x = foo;");
        assert_eq!(
            text_with_bg(&buffer, row, theme.syntax_word_del_bg()),
            "foo",
            "{}",
            body_text(&buffer)
        );
        assert_eq!(
            text_with_bg(&buffer, row, theme.syntax_word_add_bg()),
            "bar"
        );
        let marked_fg: Vec<Color> = cells_with_bg(&buffer, row, theme.syntax_word_add_bg())
            .into_iter()
            .map(|x| buffer[(x, row)].fg)
            .collect();
        assert_eq!(marked_fg, vec![Color::Blue; 3]);
    }
}
