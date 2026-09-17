//! Measures one `ui::render` frame against diffs of increasing size, so the
//! per-frame cost can be compared across branches and checked for O(total
//! diff) rather than O(visible rows) scaling.
//!
//! `cargo test --release render_perf -- --ignored --nocapture`

use crate::app::*;
use crate::model::{
    Comment, CommentType, DiffFile, DiffHunk, DiffLine, FileStatus, LineOrigin, LineRange, LineSide,
};
use crate::vcs::traits::{VcsBackend, VcsInfo, VcsType};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Style;
use std::path::PathBuf;
use std::time::Instant;

struct StubVcs(VcsInfo);
impl VcsBackend for StubVcs {
    fn info(&self) -> &VcsInfo {
        &self.0
    }
    fn get_working_tree_diff(
        &self,
        _hl: &crate::syntax::SyntaxHighlighter,
    ) -> crate::error::Result<Vec<DiffFile>> {
        Ok(Vec::new())
    }
    fn fetch_context_lines(
        &self,
        _path: &std::path::Path,
        _status: FileStatus,
        _ref_commit: Option<&str>,
        _start: u32,
        _end: u32,
    ) -> crate::error::Result<Vec<DiffLine>> {
        Ok(Vec::new())
    }
    fn file_line_count(
        &self,
        _path: &std::path::Path,
        _status: FileStatus,
        _ref_commit: Option<&str>,
    ) -> crate::error::Result<u32> {
        Ok(0)
    }
}

/// Lines cycle through a twelve-line pattern: six context lines, a change
/// block of two deletions paired with two additions, a context line, and a
/// standalone addition. Each line pair differs in one token, so word diff has
/// a range to compute on every block; the standalone addition is unpaired and
/// renders plain.
///
/// Four spans per line, matching what the syntax highlighter produces for
/// ordinary code: a bare `content` string would understate real frame cost.
fn line(idx: usize) -> DiffLine {
    let (origin, name_idx, arg) = match idx % 12 {
        6 | 7 => (LineOrigin::Deletion, idx, "input"),
        8 | 9 => (LineOrigin::Addition, idx - 2, "output"),
        11 => (LineOrigin::Addition, idx, "input"),
        _ => (LineOrigin::Context, idx, "input"),
    };
    let content =
        format!("    let value_{name_idx} = compute({arg}, {name_idx}); // measured line");
    let spans = vec![
        (Style::default(), "    let ".to_string()),
        (Style::default(), format!("value_{name_idx}")),
        (Style::default(), format!(" = compute({arg}, {name_idx});")),
        (Style::default(), " // measured line".to_string()),
    ];
    DiffLine {
        origin,
        content,
        old_lineno: None,
        new_lineno: None,
        highlighted_spans: Some(spans),
    }
}

/// Number the lines of one hunk that starts at line 1 on both sides: a
/// deletion advances the old side, an addition the new side, and context
/// advances both.
fn number_lines(lines: &mut [DiffLine]) {
    let (mut old, mut new) = (0, 0);
    for line in lines {
        if line.origin != LineOrigin::Addition {
            old += 1;
            line.old_lineno = Some(old);
        }
        if line.origin != LineOrigin::Deletion {
            new += 1;
            line.new_lineno = Some(new);
        }
    }
}

fn file(path: &str, lines_per_file: usize) -> DiffFile {
    let mut lines: Vec<DiffLine> = (0..lines_per_file).map(line).collect();
    number_lines(&mut lines);
    let old_count = lines.iter().filter(|l| l.old_lineno.is_some()).count() as u32;
    let new_count = lines.iter().filter(|l| l.new_lineno.is_some()).count() as u32;
    let hunks = vec![DiffHunk {
        header: format!("@@ -1,{old_count} +1,{new_count} @@"),
        lines,
        old_start: 1,
        old_count,
        new_start: 1,
        new_count,
    }];
    let content_hash = DiffFile::compute_content_hash(&hunks);
    DiffFile {
        old_path: Some(PathBuf::from(path)),
        new_path: Some(PathBuf::from(path)),
        status: FileStatus::Modified,
        hunks,
        is_binary: false,
        is_too_large: false,
        is_commit_message: false,
        content_hash,
    }
}

/// `file_count` files of `lines_per_file` lines each.
fn files(file_count: usize, lines_per_file: usize) -> Vec<DiffFile> {
    (0..file_count)
        .map(|i| file(&format!("src/module_{i}/file_{i}.rs"), lines_per_file))
        .collect()
}

fn app_with(files: Vec<DiffFile>) -> App {
    let vcs_info = VcsInfo {
        root_path: PathBuf::from("/tmp"),
        head_commit: "head".into(),
        branch_name: Some("main".into()),
        vcs_type: VcsType::Git,
    };
    let session = ReviewSession::new(
        vcs_info.root_path.clone(),
        vcs_info.head_commit.clone(),
        vcs_info.branch_name.clone(),
        SessionDiffSource::WorkingTree,
    );
    App::build(
        Box::new(StubVcs(vcs_info.clone())),
        vcs_info,
        crate::theme::Theme::dark(),
        None,
        false,
        files,
        session,
        DiffSource::WorkingTree,
        InputMode::Normal,
        Vec::new(),
        None,
        None,
    )
    .expect("build app")
}

/// A multi-paragraph body with a fenced code block, matching what a real
/// review thread carries: the markdown highlighter's cost tracks body lines,
/// not comment count.
fn comment_body(idx: usize) -> String {
    format!(
        "This looks wrong to me (#{idx}).\n\n\
         The `compute` call ignores its second argument, so every branch\n\
         collapses to the same value.\n\n\
         ```rust\n\
         let value = compute(input, {idx});\n\
         assert_eq!(value, expected);\n\
         ```\n\n\
         Can you confirm before we merge?"
    )
}

/// Median frame time in microseconds for a diff of `file_count` ×
/// `lines_per_file`, with `comments_per_file` review comments attached,
/// drawn at a realistic terminal size.
fn frame_micros_with_comments(
    file_count: usize,
    lines_per_file: usize,
    comments_per_file: usize,
) -> u128 {
    let files = files(file_count, lines_per_file);
    let mut app = app_with(files.clone());

    for (i, f) in files.iter().enumerate() {
        let path = f.display_path().clone();
        app.session.add_diff_file(f);
        let Some(review) = app.session.get_file_mut(&path) else {
            continue;
        };
        for c in 0..comments_per_file {
            let mut comment = Comment::new(
                comment_body(i * comments_per_file + c),
                CommentType::default(),
                Some(LineSide::New),
            );
            comment.line_range = Some(LineRange::single(c as u32 + 1));
            review.add_line_comment(c as u32 + 1, comment);
        }
    }
    app.rebuild_annotations();

    median_frame_micros(&mut app)
}

/// Median frame time in microseconds for a diff of `file_count` ×
/// `lines_per_file`, drawn at a realistic terminal size.
fn frame_micros(file_count: usize, lines_per_file: usize) -> u128 {
    let mut app = app_with(files(file_count, lines_per_file));
    median_frame_micros(&mut app)
}

/// Median frame time in microseconds for `app`, drawn at a realistic
/// terminal size.
fn median_frame_micros(app: &mut App) -> u128 {
    let mut terminal = Terminal::new(TestBackend::new(180, 50)).unwrap();
    let mut samples = Vec::new();
    for _ in 0..21 {
        let start = Instant::now();
        terminal
            .draw(|frame| crate::ui::render(frame, app))
            .expect("draw frame");
        samples.push(start.elapsed().as_micros());
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
#[ignore = "timing measurement, run explicitly"]
fn render_perf_scaling() {
    for (files, lines) in [(1, 200), (10, 200), (50, 200), (100, 200), (200, 200)] {
        let micros = frame_micros(files, lines);
        println!(
            "{files:>4} files x {lines} lines = {:>7} diff lines: {:>8.2} ms/frame",
            files * lines,
            micros as f64 / 1000.0
        );
    }
}

#[test]
#[ignore = "timing measurement, run explicitly"]
fn render_perf_with_comments() {
    for (files, comments) in [(20, 0), (20, 1), (20, 3), (20, 10)] {
        let micros = frame_micros_with_comments(files, 200, comments);
        println!(
            "{files} files x 200 lines, {:>4} comments total: {:>8.2} ms/frame",
            files * comments,
            micros as f64 / 1000.0
        );
    }
}

/// Word diff runs per visible line pair in Normal mode and, because the
/// renderers build every row there, per line pair in the whole diff in
/// Comment mode. Each case is drawn with the feature on and off, so the
/// difference is its per-frame cost.
#[test]
#[ignore = "timing measurement, run explicitly"]
fn render_perf_word_diff() {
    for view in [DiffViewMode::Unified, DiffViewMode::SideBySide] {
        for (mode, file_count) in [
            (InputMode::Normal, 20),
            (InputMode::Normal, 100),
            (InputMode::Comment, 20),
            (InputMode::Comment, 100),
        ] {
            let frame_micros = |word_diff: bool| {
                let mut app = app_with(files(file_count, 200));
                if view == DiffViewMode::SideBySide {
                    app.toggle_diff_view_mode();
                }
                app.set_word_diff(word_diff);
                if mode == InputMode::Comment {
                    app.enter_comment_mode(false, Some((1, LineSide::New)));
                }
                median_frame_micros(&mut app)
            };
            let on = frame_micros(true);
            let off = frame_micros(false);
            println!(
                "{view:?} view, {mode:?} mode, {file_count:>3} files x 200 lines: \
                 word diff on {:>8.2} ms/frame, off {:>8.2} ms/frame",
                on as f64 / 1000.0,
                off as f64 / 1000.0
            );
        }
    }
}
