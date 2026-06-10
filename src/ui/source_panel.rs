use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::scrollbar::{render_scrollbar, should_show_scrollbar};
use crate::app::state::{
    Palette, SourcePanelActiveItem, SourcePanelCommitArea, SourcePanelCommitFileArea,
    SourcePanelFileArea,
};
use crate::app::{AppState, Mode};
use crate::pane::ScrollMetrics;
use crate::workspace::{ChangeStatus, ChangedFile, CommitInfo};

/// Rows the Changes section reserves for its header before the file list.
const CHANGES_HEADER_ROWS: u16 = 1;

/// Rows the Graph section reserves for its header before the commit list.
const GRAPH_HEADER_ROWS: u16 = 1;

/// Split a height into (changes, graph) section heights, mirroring the left
/// sidebar's `sidebar_section_heights`.
fn source_panel_section_heights(total_h: u16, split_ratio: f32) -> (u16, u16) {
    if total_h == 0 {
        return (0, 0);
    }

    if total_h < 6 {
        let changes_h = total_h.div_ceil(2);
        return (changes_h, total_h.saturating_sub(changes_h));
    }

    let ratio = split_ratio.clamp(0.1, 0.9);
    let changes_h = ((total_h as f32) * ratio).round() as u16;
    let changes_h = changes_h.clamp(3, total_h.saturating_sub(3));
    let graph_h = total_h.saturating_sub(changes_h);
    (changes_h, graph_h)
}

/// The content area of the panel excludes the left-edge separator column.
fn source_panel_content(area: Rect) -> Rect {
    if area.width == 0 {
        return Rect::default();
    }
    Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(1),
        area.height,
    )
}

/// Whether the panel is tall enough to set aside a dedicated 1-row divider
/// between the changes and graph sections (mirrors `source_panel_section_heights`
/// reserving a full split only above this height).
fn source_panel_has_section_divider(content_height: u16) -> bool {
    content_height >= 6
}

/// Returns `(changes_area, graph_area)` for the expanded panel. When the panel
/// is tall enough, a 1-row divider is carved from the top of the graph section
/// so the two lists read as visually separate.
pub(crate) fn expanded_source_panel_sections(area: Rect, split_ratio: f32) -> (Rect, Rect) {
    let content = source_panel_content(area);
    if content.width == 0 || content.height == 0 {
        return (Rect::default(), Rect::default());
    }

    let (changes_h, graph_h) = source_panel_section_heights(content.height, split_ratio);
    let changes_area = Rect::new(content.x, content.y, content.width, changes_h);

    let divider_rows = u16::from(source_panel_has_section_divider(content.height));
    let graph_area = Rect::new(
        content.x,
        content.y + changes_h + divider_rows,
        content.width,
        graph_h.saturating_sub(divider_rows),
    );
    (changes_area, graph_area)
}

/// The 1-row divider between the changes and graph sections.
pub(crate) fn source_panel_section_divider_rect(area: Rect, split_ratio: f32) -> Rect {
    let content = source_panel_content(area);
    if content.width == 0 || !source_panel_has_section_divider(content.height) {
        return Rect::default();
    }

    let (changes_h, _) = source_panel_section_heights(content.height, split_ratio);
    Rect::new(content.x, content.y + changes_h, content.width, 1)
}

/// The workspace whose changes/graph the panel reflects: the selected workspace
/// while navigating menus, otherwise the active one. Mirrors the agent detail
/// panel's notion of the "current" workspace.
pub(crate) fn source_panel_workspace_idx(app: &AppState) -> Option<usize> {
    if matches!(
        app.mode,
        Mode::Navigate
            | Mode::RenameWorkspace
            | Mode::RenamePane
            | Mode::Resize
            | Mode::ConfirmClose
            | Mode::ContextMenu
            | Mode::Settings
            | Mode::GlobalMenu
            | Mode::KeybindHelp
            | Mode::ProductAnnouncement
    ) {
        Some(app.selected)
    } else {
        app.active
    }
}

/// The Changes section rect (the upper of the two expanded sections).
pub(crate) fn source_panel_changes_rect(area: Rect, split_ratio: f32) -> Rect {
    expanded_source_panel_sections(area, split_ratio).0
}

/// The clickable ↻ refresh glyph in the Changes section header (top-right cell).
pub(crate) fn source_panel_changes_refresh_rect(area: Rect, split_ratio: f32) -> Rect {
    let section = source_panel_changes_rect(area, split_ratio);
    if section.width == 0 || section.height == 0 {
        return Rect::default();
    }
    Rect::new(section.x + section.width.saturating_sub(1), section.y, 1, 1)
}

/// The Graph section rect (the lower of the two expanded sections).
pub(crate) fn source_panel_graph_rect(area: Rect, split_ratio: f32) -> Rect {
    expanded_source_panel_sections(area, split_ratio).1
}

/// The clickable ↻ refresh glyph in the Graph section header (top-right cell).
pub(crate) fn source_panel_log_refresh_rect(area: Rect, split_ratio: f32) -> Rect {
    let section = source_panel_graph_rect(area, split_ratio);
    if section.width == 0 || section.height == 0 {
        return Rect::default();
    }
    Rect::new(section.x + section.width.saturating_sub(1), section.y, 1, 1)
}

/// The commit-list body of the Graph section, below its header.
fn source_panel_graph_body_rect(section: Rect, has_scrollbar: bool) -> Rect {
    if section.width == 0 || section.height <= GRAPH_HEADER_ROWS {
        return Rect::default();
    }
    let body_y = section.y + GRAPH_HEADER_ROWS;
    let body_height = section.height - GRAPH_HEADER_ROWS;
    let body_width = section.width.saturating_sub(u16::from(has_scrollbar));
    Rect::new(section.x, body_y, body_width, body_height)
}

/// A single rendered row of the Graph section's flattened virtual list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphRow {
    /// A commit or graph-connector row, indexed into the workspace's log.
    Commit { log_idx: usize },
    /// A file touched by an expanded commit, shown inline beneath it.
    File { log_idx: usize, file_idx: usize },
    /// The trailing " load more" affordance.
    LoadMore,
}

/// The files of an expanded commit, or an empty slice when it is collapsed or
/// its file list has not been fetched yet.
fn expanded_commit_files<'a>(app: &'a AppState, commit: &CommitInfo) -> &'a [ChangedFile] {
    commit
        .sha
        .as_ref()
        .filter(|sha| app.source_panel_expanded_commits.contains(*sha))
        .and_then(|sha| app.source_panel_commit_files.get(sha))
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Flatten the commit graph into the virtual rows the Graph section scrolls
/// over: each commit, the files of any expanded commit immediately beneath it,
/// and a trailing "load more" row when more history is available.
fn source_panel_graph_rows(app: &AppState) -> Vec<GraphRow> {
    let Some(ws) = source_panel_workspace_idx(app).and_then(|idx| app.workspaces.get(idx)) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for (log_idx, commit) in ws.commits().iter().enumerate() {
        rows.push(GraphRow::Commit { log_idx });
        for file_idx in 0..expanded_commit_files(app, commit).len() {
            rows.push(GraphRow::File { log_idx, file_idx });
        }
    }
    if ws.has_more_commits() {
        rows.push(GraphRow::LoadMore);
    }
    rows
}

/// Total scrollable rows in the Graph section: commits, the inline file rows of
/// expanded commits, and the trailing "load more" row.
fn source_panel_log_row_count(app: &AppState) -> usize {
    source_panel_graph_rows(app).len()
}

/// The visible virtual rows of the Graph section paired with the 1-row rect each
/// occupies, honoring the current scroll offset. Shared by the card-area and
/// load-more computations and used implicitly when rendering.
fn source_panel_graph_visible_rows(app: &AppState, panel_area: Rect) -> Vec<(GraphRow, Rect)> {
    if app.source_panel_collapsed {
        return Vec::new();
    }
    let section = source_panel_graph_rect(panel_area, app.source_panel_section_split);
    if section == Rect::default() {
        return Vec::new();
    }
    let rows = source_panel_graph_rows(app);
    if rows.is_empty() {
        return Vec::new();
    }

    let metrics = source_panel_log_scroll_metrics(app, section);
    let body = source_panel_graph_body_rect(section, should_show_scrollbar(metrics));
    if body.width == 0 || body.height == 0 {
        return Vec::new();
    }

    let body_bottom = body.y + body.height;
    rows.into_iter()
        .skip(app.source_panel_log_scroll)
        .zip(body.y..body_bottom)
        .map(|(row, y)| (row, Rect::new(body.x, y, body.width, 1)))
        .collect()
}

pub(crate) fn source_panel_log_scroll_metrics(app: &AppState, section: Rect) -> ScrollMetrics {
    let viewport_rows = source_panel_graph_body_rect(section, false).height as usize;
    let total_rows = source_panel_log_row_count(app);
    let max_offset_from_bottom = total_rows.saturating_sub(viewport_rows);
    let offset_from_bottom = total_rows
        .saturating_sub(app.source_panel_log_scroll)
        .saturating_sub(viewport_rows);
    ScrollMetrics {
        offset_from_bottom,
        max_offset_from_bottom,
        viewport_rows,
    }
}

fn source_panel_log_scrollbar_rect(app: &AppState, section: Rect) -> Option<Rect> {
    let metrics = source_panel_log_scroll_metrics(app, section);
    let body = source_panel_graph_body_rect(section, true);
    (should_show_scrollbar(metrics) && body.width > 0 && body.height > 0).then_some(Rect::new(
        section.x + section.width.saturating_sub(1),
        body.y,
        1,
        body.height,
    ))
}

/// Clickable row rects for the visible commit-graph entries.
pub(crate) fn compute_source_panel_log_card_areas(
    app: &AppState,
    panel_area: Rect,
) -> Vec<SourcePanelCommitArea> {
    source_panel_graph_visible_rows(app, panel_area)
        .into_iter()
        .filter_map(|(row, rect)| match row {
            GraphRow::Commit { log_idx } => Some(SourcePanelCommitArea { rect, log_idx }),
            _ => None,
        })
        .collect()
}

/// Clickable row rects for the file rows shown inline beneath expanded commits.
pub(crate) fn compute_source_panel_commit_file_card_areas(
    app: &AppState,
    panel_area: Rect,
) -> Vec<SourcePanelCommitFileArea> {
    source_panel_graph_visible_rows(app, panel_area)
        .into_iter()
        .filter_map(|(row, rect)| match row {
            GraphRow::File { log_idx, file_idx } => Some(SourcePanelCommitFileArea {
                rect,
                log_idx,
                file_idx,
            }),
            _ => None,
        })
        .collect()
}

/// The clickable " load more" row, shown as the trailing virtual row of the
/// Graph section when the repository has history beyond the loaded window.
/// Returns the default rect when there is no more history or the row is
/// currently scrolled out of view.
pub(crate) fn compute_source_panel_load_more_rect(app: &AppState, panel_area: Rect) -> Rect {
    source_panel_graph_visible_rows(app, panel_area)
        .into_iter()
        .find_map(|(row, rect)| matches!(row, GraphRow::LoadMore).then_some(rect))
        .unwrap_or_default()
}

/// The file-list body of the Changes section, below its header.
fn source_panel_changes_body_rect(section: Rect, has_scrollbar: bool) -> Rect {
    if section.width == 0 || section.height <= CHANGES_HEADER_ROWS {
        return Rect::default();
    }
    let body_y = section.y + CHANGES_HEADER_ROWS;
    let body_height = section.height - CHANGES_HEADER_ROWS;
    let body_width = section.width.saturating_sub(u16::from(has_scrollbar));
    Rect::new(section.x, body_y, body_width, body_height)
}

fn source_panel_changes_count(app: &AppState) -> usize {
    source_panel_workspace_idx(app)
        .and_then(|idx| app.workspaces.get(idx))
        .map(|ws| ws.changed_files().len())
        .unwrap_or(0)
}

pub(crate) fn source_panel_changes_scroll_metrics(app: &AppState, section: Rect) -> ScrollMetrics {
    let viewport_rows = source_panel_changes_body_rect(section, false).height as usize;
    let total_rows = source_panel_changes_count(app);
    let max_offset_from_bottom = total_rows.saturating_sub(viewport_rows);
    let offset_from_bottom = total_rows
        .saturating_sub(app.source_panel_changes_scroll)
        .saturating_sub(viewport_rows);
    ScrollMetrics {
        offset_from_bottom,
        max_offset_from_bottom,
        viewport_rows,
    }
}

pub(crate) fn source_panel_changes_scrollbar_rect(app: &AppState, section: Rect) -> Option<Rect> {
    let metrics = source_panel_changes_scroll_metrics(app, section);
    let body = source_panel_changes_body_rect(section, true);
    (should_show_scrollbar(metrics) && body.width > 0 && body.height > 0).then_some(Rect::new(
        section.x + section.width.saturating_sub(1),
        body.y,
        1,
        body.height,
    ))
}

/// Clickable row rects for the visible changed files.
pub(crate) fn compute_source_panel_changes_card_areas(
    app: &AppState,
    panel_area: Rect,
) -> Vec<SourcePanelFileArea> {
    if app.source_panel_collapsed {
        return Vec::new();
    }
    let section = source_panel_changes_rect(panel_area, app.source_panel_section_split);
    if section == Rect::default() {
        return Vec::new();
    }
    let count = source_panel_changes_count(app);
    if count == 0 {
        return Vec::new();
    }

    let metrics = source_panel_changes_scroll_metrics(app, section);
    let body = source_panel_changes_body_rect(section, should_show_scrollbar(metrics));
    if body.width == 0 || body.height == 0 {
        return Vec::new();
    }

    let body_bottom = body.y + body.height;
    (app.source_panel_changes_scroll..count)
        .zip(body.y..body_bottom)
        .map(|(change_idx, row_y)| SourcePanelFileArea {
            rect: Rect::new(body.x, row_y, body.width, 1),
            change_idx,
        })
        .collect()
}

/// Bottom-right collapse toggle for the expanded panel.
pub(crate) fn expanded_source_panel_toggle_rect(area: Rect) -> Rect {
    if area.width <= 1 || area.height == 0 {
        return Rect::default();
    }
    Rect::new(
        area.x + area.width.saturating_sub(1),
        area.y + area.height.saturating_sub(1),
        1,
        1,
    )
}

/// Toggle for the collapsed narrow strip, centered in the content column(s).
pub(crate) fn collapsed_source_panel_toggle_rect(area: Rect) -> Rect {
    let content = source_panel_content(area);
    if content.width == 0 || content.height == 0 {
        return Rect::default();
    }
    let bottom_y = area.y + area.height.saturating_sub(1);
    let x = content.x + content.width / 2;
    Rect::new(x.min(area.x + area.width.saturating_sub(1)), bottom_y, 1, 1)
}

fn render_left_separator(frame: &mut Frame, area: Rect, p: &Palette) {
    let sep_x = area.x;
    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        buf[(sep_x, y)].set_symbol("│");
        buf[(sep_x, y)].set_style(Style::default().fg(p.surface_dim));
    }
}

/// The Changes header: ` changes (N)` on the left, a clickable ↻ on the right.
fn render_changes_header(frame: &mut Frame, section: Rect, count: usize, p: &Palette) {
    if section.width == 0 || section.height == 0 {
        return;
    }
    let header_style = Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD);
    // Refresh glyph occupies the rightmost cell; the label fills the rest.
    let label_width = section.width.saturating_sub(1);
    if label_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                format!(" changes ({count})"),
                header_style,
            )])),
            Rect::new(section.x, section.y, label_width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(Span::styled("↻", Style::default().fg(p.overlay0))),
        Rect::new(section.x + section.width.saturating_sub(1), section.y, 1, 1),
    );
}

fn render_source_panel_toggle(frame: &mut Frame, area: Rect, collapsed: bool, p: &Palette) {
    let toggle_area = if collapsed {
        collapsed_source_panel_toggle_rect(area)
    } else {
        expanded_source_panel_toggle_rect(area)
    };
    if toggle_area == Rect::default() {
        return;
    }
    // The panel lives on the right edge, so the collapse arrow points right and
    // the expand arrow (shown while collapsed) points left.
    let icon = if collapsed { "«" } else { "»" };
    frame.render_widget(
        Paragraph::new(Span::styled(icon, Style::default().fg(p.overlay0))),
        toggle_area,
    );
}

/// Whether the panel's current workspace is a git repository. Defaults to true
/// when no workspace is selected, so the panel only shows its non-git empty
/// state for a workspace we have positively determined is not a repo.
fn source_panel_is_git_repo(app: &AppState) -> bool {
    source_panel_workspace_idx(app)
        .and_then(|idx| app.workspaces.get(idx))
        .map(|ws| ws.is_git_repo())
        .unwrap_or(true)
}

/// Whether the panel has a git workspace worth reserving columns for. Unlike
/// [`source_panel_is_git_repo`], this is `false` when no workspace is selected:
/// an absent or non-git workspace has nothing to show, so the layout hides the
/// panel entirely rather than reserving columns for an empty placeholder.
pub(super) fn source_panel_has_git_workspace(app: &AppState) -> bool {
    source_panel_workspace_idx(app)
        .and_then(|idx| app.workspaces.get(idx))
        .map(|ws| ws.is_git_repo())
        .unwrap_or(false)
}

/// Expanded source control panel: left separator, the changes and graph
/// sections (or a non-git empty state), and the collapse toggle.
pub(super) fn render_source_panel(app: &AppState, frame: &mut Frame, area: Rect) {
    let p = &app.palette;
    render_left_separator(frame, area, p);

    if source_panel_is_git_repo(app) {
        let (changes_area, graph_area) =
            expanded_source_panel_sections(area, app.source_panel_section_split);
        render_source_panel_changes(app, frame, changes_area);
        render_source_panel_section_divider(frame, area, app.source_panel_section_split, p);
        render_source_panel_graph(app, frame, graph_area);
    } else {
        render_source_panel_non_git(app, frame, area);
    }

    render_source_panel_toggle(frame, area, false, p);
}

/// Draw the horizontal rule that separates the Changes and Graph sections,
/// matching the sidebar's section divider. Absent on panels too short to carve
/// out a dedicated divider row.
fn render_source_panel_section_divider(
    frame: &mut Frame,
    area: Rect,
    split_ratio: f32,
    p: &Palette,
) {
    let divider = source_panel_section_divider_rect(area, split_ratio);
    if divider == Rect::default() {
        return;
    }
    let buf = frame.buffer_mut();
    for x in divider.x..divider.x + divider.width {
        buf[(x, divider.y)].set_symbol("─");
        buf[(x, divider.y)].set_style(Style::default().fg(p.surface_dim));
    }
}

/// Replaces both section bodies with a single dim line when the active
/// workspace's identity directory is not a git repository.
fn render_source_panel_non_git(app: &AppState, frame: &mut Frame, area: Rect) {
    let p = &app.palette;
    let content = source_panel_content(area);
    if content.width == 0 || content.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            " not a git repository",
            Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
        )),
        Rect::new(content.x, content.y, content.width, 1),
    );
}

/// Map a change status to its single-letter glyph and accent color.
fn change_status_glyph(status: &ChangeStatus, p: &Palette) -> (&'static str, Color) {
    match status {
        ChangeStatus::Modified => ("M", p.red),
        ChangeStatus::Added => ("A", p.green),
        ChangeStatus::Deleted => ("D", p.red),
        ChangeStatus::Renamed { .. } => ("R", p.yellow),
        ChangeStatus::Unmerged => ("U", p.peach),
        ChangeStatus::Untracked => ("?", p.overlay0),
    }
}

/// Split a changed-file path into `(basename, parent-dir)` display strings.
fn change_path_parts(path: &std::path::Path) -> (String, String) {
    let basename = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let parent = path
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .filter(|parent| !parent.is_empty())
        .unwrap_or_default();
    (basename, parent)
}

/// Fill a 1-row rect with a background color, used to highlight the row whose
/// content currently fills the diff pane. Rendered before the row's fg content,
/// which leaves cell backgrounds untouched so the highlight shows through.
fn highlight_row(frame: &mut Frame, row: Rect, bg: Color) {
    let buf = frame.buffer_mut();
    for y in row.y..row.y + row.height {
        for x in row.x..row.x + row.width {
            buf[(x, y)].set_style(Style::default().bg(bg));
        }
    }
}

/// Render one changed-file row: `<indent><basename> <parent>` on the left,
/// clipped so the right-edge status letter stays visible. Shared by the Changes
/// section (`indent` 1) and the file rows inline under expanded commits.
fn render_changed_file_row(
    frame: &mut Frame,
    row: Rect,
    change: &ChangedFile,
    indent: usize,
    p: &Palette,
) {
    if row.width == 0 {
        return;
    }
    let (glyph, glyph_color) = change_status_glyph(&change.status, p);
    let (basename, parent) = change_path_parts(&change.path);

    let mut spans = vec![
        Span::styled(" ".repeat(indent), Style::default()),
        Span::styled(basename, Style::default().fg(p.subtext0)),
    ];
    if !parent.is_empty() {
        spans.push(Span::styled(" ", Style::default()));
        spans.push(Span::styled(
            parent,
            Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
        ));
    }
    let left_width = row.width.saturating_sub(2);
    if left_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(row.x, row.y, left_width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(Span::styled(glyph, Style::default().fg(glyph_color))),
        Rect::new(row.x + row.width.saturating_sub(1), row.y, 1, 1),
    );
}

fn render_source_panel_changes(app: &AppState, frame: &mut Frame, section: Rect) {
    let p = &app.palette;

    let changes = source_panel_workspace_idx(app)
        .and_then(|idx| app.workspaces.get(idx))
        .map(|ws| ws.changed_files())
        .unwrap_or(&[]);

    render_changes_header(frame, section, changes.len(), p);

    let metrics = source_panel_changes_scroll_metrics(app, section);
    let scrollbar_rect = source_panel_changes_scrollbar_rect(app, section);
    let body = source_panel_changes_body_rect(section, scrollbar_rect.is_some());
    if body.width == 0 || body.height == 0 {
        return;
    }

    if changes.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " no changes",
                Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
            )),
            Rect::new(body.x, body.y, body.width, 1),
        );
        return;
    }

    let active = app.source_panel_highlighted_item();
    for area in &app.view.source_panel_changes_card_areas {
        let Some(change) = changes.get(area.change_idx) else {
            continue;
        };
        let highlighted = matches!(
            active,
            Some(SourcePanelActiveItem::WorkingFile(path)) if *path == change.path
        );
        if highlighted {
            highlight_row(frame, area.rect, p.surface0);
        }
        render_changed_file_row(frame, area.rect, change, 1, p);
    }

    if let Some(track) = scrollbar_rect {
        render_scrollbar(frame, metrics, track, p.surface_dim, p.overlay0, "▕");
    }
}

/// The Graph header: ` graph · <branch> ↑n ↓n`.
fn render_graph_header(
    frame: &mut Frame,
    section: Rect,
    branch: Option<&str>,
    ahead_behind: Option<(usize, usize)>,
    p: &Palette,
) {
    if section.width == 0 || section.height == 0 {
        return;
    }
    let header_style = Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled(" graph", header_style)];
    if let Some(branch) = branch {
        spans.push(Span::styled(" · ", Style::default().fg(p.overlay0)));
        spans.push(Span::styled(
            branch.to_string(),
            Style::default().fg(p.mauve).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some((ahead, behind)) = ahead_behind {
        if ahead > 0 {
            spans.push(Span::styled(
                format!(" ↑{ahead}"),
                Style::default().fg(p.green),
            ));
        }
        if behind > 0 {
            spans.push(Span::styled(
                format!(" ↓{behind}"),
                Style::default().fg(p.red),
            ));
        }
    }
    // Refresh glyph occupies the rightmost cell; the label fills the rest.
    let label_width = section.width.saturating_sub(1);
    if label_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(section.x, section.y, label_width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(Span::styled("↻", Style::default().fg(p.overlay0))),
        Rect::new(section.x + section.width.saturating_sub(1), section.y, 1, 1),
    );
}

/// Build the styled spans for one commit-graph row. Connector-only lines render
/// just their graph cell; commit lines add a collapse/expand chevron then their
/// decorations, subject, and author.
fn commit_row_spans<'a>(commit: &'a CommitInfo, expanded: bool, p: &Palette) -> Vec<Span<'a>> {
    let mut spans = vec![Span::styled(
        commit.graph_cell.clone(),
        Style::default().fg(p.overlay1),
    )];
    if commit.sha.is_none() {
        return spans;
    }
    // Chevron occupies the cell right after the graph glyphs (the collapse
    // hit-target); a trailing space separates it from the subject.
    spans.push(Span::styled(
        if expanded { "▾" } else { "▸" },
        Style::default().fg(p.overlay0),
    ));
    spans.push(Span::styled(" ", Style::default()));
    for decoration in &commit.decorations {
        spans.push(Span::styled(
            decoration.clone(),
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ", Style::default()));
    }
    spans.push(Span::styled(
        commit.subject.clone(),
        Style::default().fg(p.text),
    ));
    if !commit.author.is_empty() {
        spans.push(Span::styled(
            " · ",
            Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
        ));
        spans.push(Span::styled(
            commit.author.clone(),
            Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
        ));
    }
    spans
}

fn render_source_panel_graph(app: &AppState, frame: &mut Frame, section: Rect) {
    let p = &app.palette;

    let ws = source_panel_workspace_idx(app).and_then(|idx| app.workspaces.get(idx));
    let commits = ws.map(|ws| ws.commits()).unwrap_or(&[]);
    let branch = ws.and_then(|ws| ws.branch());
    let ahead_behind = ws.and_then(|ws| ws.git_ahead_behind());

    render_graph_header(frame, section, branch.as_deref(), ahead_behind, p);

    let metrics = source_panel_log_scroll_metrics(app, section);
    let scrollbar_rect = source_panel_log_scrollbar_rect(app, section);
    let body = source_panel_graph_body_rect(section, scrollbar_rect.is_some());
    if body.width == 0 || body.height == 0 {
        return;
    }

    if commits.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " no commits",
                Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
            )),
            Rect::new(body.x, body.y, body.width, 1),
        );
        return;
    }

    let active = app.source_panel_highlighted_item();
    for area in &app.view.source_panel_log_card_areas {
        let Some(commit) = commits.get(area.log_idx) else {
            continue;
        };
        let row = area.rect;
        if row.width == 0 {
            continue;
        }
        let expanded = commit
            .sha
            .as_ref()
            .is_some_and(|sha| app.source_panel_expanded_commits.contains(sha));
        let highlighted = matches!(
            (active, &commit.sha),
            (Some(SourcePanelActiveItem::Commit(active_sha)), Some(sha)) if active_sha == sha
        );
        if highlighted {
            highlight_row(frame, row, p.surface0);
        }
        frame.render_widget(
            Paragraph::new(Line::from(commit_row_spans(commit, expanded, p))),
            Rect::new(row.x, row.y, row.width, 1),
        );
    }

    // File rows inline beneath expanded commits.
    for area in &app.view.source_panel_commit_file_card_areas {
        let Some(commit) = commits.get(area.log_idx) else {
            continue;
        };
        let Some(sha) = commit.sha.as_ref() else {
            continue;
        };
        let Some(change) = app
            .source_panel_commit_files
            .get(sha)
            .and_then(|files| files.get(area.file_idx))
        else {
            continue;
        };
        let highlighted = matches!(
            active,
            Some(SourcePanelActiveItem::CommitFile { sha: active_sha, path })
                if active_sha == sha && *path == change.path
        );
        if highlighted {
            highlight_row(frame, area.rect, p.surface0);
        }
        // Indent to line up beneath the commit subject, which starts after the
        // graph glyphs, the chevron, and a separating space.
        let indent = commit.graph_cell.chars().count() + 2;
        render_changed_file_row(frame, area.rect, change, indent, p);
    }

    let load_more = app.view.source_panel_load_more_rect;
    if load_more.width > 0 && load_more.height > 0 {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " load more",
                Style::default().fg(p.accent).add_modifier(Modifier::DIM),
            )),
            load_more,
        );
    }

    if let Some(track) = scrollbar_rect {
        render_scrollbar(frame, metrics, track, p.surface_dim, p.overlay0, "▕");
    }
}

/// Collapsed strip: the separator, a changed-file count badge, and the expand
/// toggle.
pub(super) fn render_source_panel_collapsed(app: &AppState, frame: &mut Frame, area: Rect) {
    let p = &app.palette;
    render_left_separator(frame, area, p);
    render_collapsed_change_badge(app, frame, area);
    render_source_panel_toggle(frame, area, true, p);
}

/// A compact changed-file count near the top of the collapsed strip. Hidden
/// when the active workspace has no changes or is not a git repository.
fn render_collapsed_change_badge(app: &AppState, frame: &mut Frame, area: Rect) {
    if !source_panel_is_git_repo(app) {
        return;
    }
    let count = source_panel_changes_count(app);
    if count == 0 {
        return;
    }
    let content = source_panel_content(area);
    if content.width == 0 || content.height == 0 {
        return;
    }
    let label = if count > 99 {
        "99+".to_string()
    } else {
        count.to_string()
    };
    let label_width = (label.chars().count() as u16).min(content.width);
    let x = content.x + content.width.saturating_sub(label_width) / 2;
    frame.render_widget(
        Paragraph::new(Span::styled(
            label,
            Style::default()
                .fg(app.palette.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Rect::new(x, content.y, label_width, 1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::ChangedFile;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn expanded_source_panel_sections_split_height_excluding_separator() {
        let area = Rect::new(70, 0, 26, 20);
        let (changes, graph) = expanded_source_panel_sections(area, 0.5);

        // Content starts one column in (past the left separator).
        assert_eq!(changes.x, area.x + 1);
        assert_eq!(changes.width, area.width - 1);
        assert_eq!(changes.y, area.y);
        // A 1-row divider sits between the sections, so the graph starts one row
        // below the changes section and the two cover all but that divider row.
        assert_eq!(graph.y, changes.y + changes.height + 1);
        assert_eq!(changes.height + graph.height, area.height - 1);
    }

    #[test]
    fn expanded_source_panel_sections_handle_tiny_heights() {
        // Below the divider threshold the sections cover the full height with no
        // dedicated divider row carved out.
        let (changes, graph) = expanded_source_panel_sections(Rect::new(70, 0, 20, 5), 0.9);
        assert!(changes.height >= 1);
        assert_eq!(changes.height + graph.height, 5);
    }

    #[test]
    fn section_divider_sits_between_sections() {
        let area = Rect::new(70, 0, 26, 20);
        let (changes, _) = expanded_source_panel_sections(area, 0.5);
        let divider = source_panel_section_divider_rect(area, 0.5);
        assert_eq!(divider.y, changes.y + changes.height);
        assert_eq!(divider.height, 1);
        assert_eq!(divider.x, area.x + 1);
    }

    #[test]
    fn expanded_toggle_sits_in_bottom_right_corner() {
        let area = Rect::new(70, 0, 26, 20);
        let toggle = expanded_source_panel_toggle_rect(area);
        assert_eq!(toggle.x, area.x + area.width - 1);
        assert_eq!(toggle.y, area.y + area.height - 1);
    }

    #[test]
    fn render_source_panel_draws_headers_separator_and_toggle() {
        let app = crate::app::state::AppState::test_new();
        let area = Rect::new(0, 0, 26, 20);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        // Left-edge separator.
        assert_eq!(buf[(area.x, area.y)].symbol(), "│");
        // " changes" header on the first content row (starts past the separator).
        let header: String = (area.x + 1..area.x + area.width)
            .map(|x| buf[(x, area.y)].symbol())
            .collect();
        assert!(header.starts_with(" changes"), "header was {header:?}");
        // Collapse toggle in the bottom-right corner.
        let toggle = expanded_source_panel_toggle_rect(area);
        assert_eq!(buf[(toggle.x, toggle.y)].symbol(), "»");
    }

    #[test]
    fn render_collapsed_strip_draws_expand_toggle() {
        let app = crate::app::state::AppState::test_new();
        let area = Rect::new(0, 0, 3, 20);
        let mut terminal =
            Terminal::new(TestBackend::new(3, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel_collapsed(&app, frame, area))
            .expect("collapsed source panel should render");

        let buf = terminal.backend().buffer().clone();
        assert_eq!(buf[(area.x, area.y)].symbol(), "│");
        let toggle = collapsed_source_panel_toggle_rect(area);
        assert_eq!(buf[(toggle.x, toggle.y)].symbol(), "«");
    }

    fn app_with_changes(changes: Vec<ChangedFile>) -> crate::app::state::AppState {
        use crate::workspace::Workspace;
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("ws");
        ws.cached_changes = changes;
        app.workspaces = vec![ws];
        app.selected = 0;
        app
    }

    fn modified(path: &str) -> ChangedFile {
        ChangedFile {
            path: std::path::PathBuf::from(path),
            status: ChangeStatus::Modified,
        }
    }

    #[test]
    fn compute_changes_card_areas_map_rows_to_indices() {
        let app = app_with_changes((0..3).map(|i| modified(&format!("f{i}.rs"))).collect());
        let area = Rect::new(0, 0, 26, 20);

        let cards = compute_source_panel_changes_card_areas(&app, area);

        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].change_idx, 0);
        assert_eq!(cards[1].change_idx, 1);
        // Rows stack directly under the header.
        assert_eq!(cards[1].rect.y, cards[0].rect.y + 1);
        assert_eq!(cards[0].rect.x, area.x + 1);
    }

    #[test]
    fn compute_changes_card_areas_honor_scroll_offset() {
        let mut app = app_with_changes((0..40).map(|i| modified(&format!("f{i}.rs"))).collect());
        app.source_panel_changes_scroll = 5;
        let area = Rect::new(0, 0, 26, 20);

        let cards = compute_source_panel_changes_card_areas(&app, area);

        assert!(!cards.is_empty());
        assert_eq!(cards[0].change_idx, 5);
    }

    #[test]
    fn render_changes_places_basename_left_and_status_on_right() {
        let mut app = app_with_changes(vec![modified("src/app/foo.rs")]);
        let area = Rect::new(0, 0, 26, 20);
        app.view.source_panel_changes_card_areas =
            compute_source_panel_changes_card_areas(&app, area);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        let card = app.view.source_panel_changes_card_areas[0];
        let row: String = (card.rect.x..card.rect.x + card.rect.width)
            .map(|x| buf[(x, card.rect.y)].symbol())
            .collect();
        assert!(row.contains("foo.rs"), "row was {row:?}");
        assert!(row.contains("src/app"), "row was {row:?}");
        // Status letter sits on the row's right edge.
        let status_x = card.rect.x + card.rect.width - 1;
        assert_eq!(buf[(status_x, card.rect.y)].symbol(), "M");
    }

    #[test]
    fn render_changes_header_shows_count_and_refresh_glyph() {
        let app = app_with_changes((0..3).map(|i| modified(&format!("f{i}.rs"))).collect());
        let area = Rect::new(0, 0, 26, 20);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        let section = source_panel_changes_rect(area, app.source_panel_section_split);
        let header: String = (section.x..section.x + section.width)
            .map(|x| buf[(x, section.y)].symbol())
            .collect();
        assert!(header.contains("changes (3)"), "header was {header:?}");
        // Refresh glyph sits on the header's right edge.
        let refresh = source_panel_changes_refresh_rect(area, app.source_panel_section_split);
        assert_eq!(buf[(refresh.x, refresh.y)].symbol(), "↻");
    }

    #[test]
    fn render_changes_shows_placeholder_when_empty() {
        let app = app_with_changes(Vec::new());
        let area = Rect::new(0, 0, 26, 20);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        let section = source_panel_changes_rect(area, app.source_panel_section_split);
        let body_y = section.y + 1;
        let row: String = (section.x..section.x + section.width)
            .map(|x| buf[(x, body_y)].symbol())
            .collect();
        assert!(row.starts_with(" no changes"), "row was {row:?}");
    }

    #[test]
    fn no_change_rows_when_workspace_has_none() {
        // A workspace with no cached changes (e.g. a non-git cwd, whose refresh
        // yields an empty list) produces no clickable file rows.
        let app = app_with_changes(Vec::new());
        let area = Rect::new(0, 0, 26, 20);

        assert!(compute_source_panel_changes_card_areas(&app, area).is_empty());
    }

    #[test]
    fn renamed_and_untracked_status_glyphs() {
        let app = crate::app::state::AppState::test_new();
        let p = &app.palette;
        assert_eq!(
            change_status_glyph(
                &ChangeStatus::Renamed {
                    from: std::path::PathBuf::from("old")
                },
                p
            )
            .0,
            "R"
        );
        assert_eq!(change_status_glyph(&ChangeStatus::Untracked, p).0, "?");
        assert_eq!(change_status_glyph(&ChangeStatus::Added, p).0, "A");
        assert_eq!(change_status_glyph(&ChangeStatus::Deleted, p).0, "D");
        assert_eq!(change_status_glyph(&ChangeStatus::Unmerged, p).0, "U");
    }

    fn app_with_commits(commits: Vec<CommitInfo>) -> crate::app::state::AppState {
        use crate::workspace::Workspace;
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("ws");
        ws.cached_log = commits;
        app.workspaces = vec![ws];
        app.selected = 0;
        app
    }

    fn commit(sha: &str, subject: &str) -> CommitInfo {
        CommitInfo {
            graph_cell: "* ".into(),
            sha: Some(sha.into()),
            subject: subject.into(),
            author: "Ada".into(),
            decorations: Vec::new(),
        }
    }

    #[test]
    fn compute_log_card_areas_map_rows_to_indices() {
        let app = app_with_commits((0..3).map(|i| commit(&format!("sha{i}"), "s")).collect());
        let area = Rect::new(0, 0, 26, 20);

        let cards = compute_source_panel_log_card_areas(&app, area);

        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].log_idx, 0);
        assert_eq!(cards[1].log_idx, 1);
        // Rows stack directly under the header.
        assert_eq!(cards[1].rect.y, cards[0].rect.y + 1);
        assert_eq!(cards[0].rect.x, area.x + 1);
    }

    #[test]
    fn compute_log_card_areas_honor_scroll_offset() {
        let mut app = app_with_commits((0..40).map(|i| commit(&format!("sha{i}"), "s")).collect());
        app.source_panel_log_scroll = 4;
        let area = Rect::new(0, 0, 26, 20);

        let cards = compute_source_panel_log_card_areas(&app, area);

        assert!(!cards.is_empty());
        assert_eq!(cards[0].log_idx, 4);
    }

    #[test]
    fn render_graph_header_shows_branch_and_subject() {
        let mut app = app_with_commits(vec![commit("abc1234", "initial commit")]);
        app.workspaces[0].cached_git_branch = Some("main".into());
        app.workspaces[0].cached_git_ahead_behind = Some((2, 1));
        let area = Rect::new(0, 0, 26, 20);
        app.view.source_panel_log_card_areas = compute_source_panel_log_card_areas(&app, area);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        let section = source_panel_graph_rect(area, app.source_panel_section_split);
        let header: String = (section.x..section.x + section.width)
            .map(|x| buf[(x, section.y)].symbol())
            .collect();
        assert!(header.contains("graph"), "header was {header:?}");
        assert!(header.contains("main"), "header was {header:?}");
        assert!(header.contains("↑2"), "header was {header:?}");
        assert!(header.contains("↓1"), "header was {header:?}");

        // The commit subject renders on the first body row.
        let card = app.view.source_panel_log_card_areas[0];
        let row: String = (card.rect.x..card.rect.x + card.rect.width)
            .map(|x| buf[(x, card.rect.y)].symbol())
            .collect();
        assert!(row.contains("initial commit"), "row was {row:?}");
    }

    #[test]
    fn render_graph_shows_placeholder_when_empty() {
        let app = app_with_commits(Vec::new());
        let area = Rect::new(0, 0, 26, 20);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        let section = source_panel_graph_rect(area, app.source_panel_section_split);
        let body_y = section.y + 1;
        let row: String = (section.x..section.x + section.width)
            .map(|x| buf[(x, body_y)].symbol())
            .collect();
        assert!(row.starts_with(" no commits"), "row was {row:?}");
    }

    #[test]
    fn load_more_rect_present_only_when_more_history_exists() {
        let area = Rect::new(0, 0, 26, 20);

        let app = app_with_commits((0..3).map(|i| commit(&format!("sha{i}"), "s")).collect());
        assert_eq!(
            compute_source_panel_load_more_rect(&app, area),
            Rect::default(),
            "no load-more row without more history"
        );

        let mut app = app;
        app.workspaces[0].cached_log_has_more = true;
        let load_more = compute_source_panel_load_more_rect(&app, area);
        assert_ne!(load_more, Rect::default());
        // It sits on the row right after the last commit.
        let cards = compute_source_panel_log_card_areas(&app, area);
        let last = cards.last().expect("commit rows present");
        assert_eq!(load_more.y, last.rect.y + 1);
    }

    #[test]
    fn render_graph_header_shows_refresh_glyph() {
        let app = app_with_commits(vec![commit("abc1234", "initial commit")]);
        let area = Rect::new(0, 0, 26, 20);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        let refresh = source_panel_log_refresh_rect(area, app.source_panel_section_split);
        assert_eq!(buf[(refresh.x, refresh.y)].symbol(), "↻");
    }

    #[test]
    fn render_graph_shows_load_more_row() {
        let mut app = app_with_commits((0..3).map(|i| commit(&format!("sha{i}"), "s")).collect());
        app.workspaces[0].cached_log_has_more = true;
        let area = Rect::new(0, 0, 26, 20);
        app.view.source_panel_log_card_areas = compute_source_panel_log_card_areas(&app, area);
        app.view.source_panel_load_more_rect = compute_source_panel_load_more_rect(&app, area);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        let row = app.view.source_panel_load_more_rect;
        let text: String = (row.x..row.x + row.width)
            .map(|x| buf[(x, row.y)].symbol())
            .collect();
        assert!(text.contains("load more"), "row was {text:?}");
    }

    #[test]
    fn connector_only_rows_render_their_graph_cell() {
        let connector = CommitInfo {
            graph_cell: "|\\".into(),
            sha: None,
            subject: String::new(),
            author: String::new(),
            decorations: Vec::new(),
        };
        let app = app_with_commits(vec![commit("abc1234", "merge"), connector]);
        let area = Rect::new(0, 0, 26, 20);
        let cards = compute_source_panel_log_card_areas(&app, area);

        // Both the commit and the connector get a clickable row.
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[1].log_idx, 1);
    }

    /// Mark `sha` expanded and cache `files` as its touched-file list.
    fn expand_commit(app: &mut crate::app::state::AppState, sha: &str, files: Vec<ChangedFile>) {
        app.source_panel_expanded_commits.insert(sha.to_string());
        app.source_panel_commit_files.insert(sha.to_string(), files);
    }

    #[test]
    fn expanded_commit_inserts_inline_file_rows_beneath_it() {
        let mut app = app_with_commits(vec![
            commit("aaa1111", "first"),
            commit("bbb2222", "second"),
        ]);
        expand_commit(
            &mut app,
            "aaa1111",
            vec![modified("src/a.rs"), modified("src/b.rs")],
        );
        let area = Rect::new(0, 0, 26, 20);

        let commits = compute_source_panel_log_card_areas(&app, area);
        let files = compute_source_panel_commit_file_card_areas(&app, area);

        // Two file rows belong to the expanded commit (log_idx 0).
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.log_idx == 0));
        assert_eq!(files[0].file_idx, 0);
        assert_eq!(files[1].file_idx, 1);
        // They sit directly beneath the first commit, ahead of the second commit.
        let first = commits.iter().find(|c| c.log_idx == 0).unwrap();
        let second = commits.iter().find(|c| c.log_idx == 1).unwrap();
        assert_eq!(files[0].rect.y, first.rect.y + 1);
        assert_eq!(files[1].rect.y, first.rect.y + 2);
        assert_eq!(second.rect.y, files[1].rect.y + 1);
    }

    #[test]
    fn collapsed_commit_has_no_inline_file_rows() {
        // Files are cached but the commit is not in the expanded set.
        let mut app = app_with_commits(vec![commit("aaa1111", "first")]);
        app.source_panel_commit_files
            .insert("aaa1111".into(), vec![modified("src/a.rs")]);
        let area = Rect::new(0, 0, 26, 20);

        assert!(compute_source_panel_commit_file_card_areas(&app, area).is_empty());
    }

    #[test]
    fn render_inline_commit_file_shows_basename_and_status() {
        let mut app = app_with_commits(vec![commit("aaa1111", "first")]);
        expand_commit(&mut app, "aaa1111", vec![modified("skills/SKILL.md")]);
        let area = Rect::new(0, 0, 26, 20);
        app.view.source_panel_log_card_areas = compute_source_panel_log_card_areas(&app, area);
        app.view.source_panel_commit_file_card_areas =
            compute_source_panel_commit_file_card_areas(&app, area);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        let file_row = app.view.source_panel_commit_file_card_areas[0].rect;
        let row: String = (file_row.x..file_row.x + file_row.width)
            .map(|x| buf[(x, file_row.y)].symbol())
            .collect();
        assert!(row.contains("SKILL.md"), "row was {row:?}");
        // Status letter sits on the row's right edge.
        let status_x = file_row.x + file_row.width - 1;
        assert_eq!(buf[(status_x, file_row.y)].symbol(), "M");

        // The expanded commit row carries the open chevron.
        let commit_row = app.view.source_panel_log_card_areas[0].rect;
        let commit_text: String = (commit_row.x..commit_row.x + commit_row.width)
            .map(|x| buf[(x, commit_row.y)].symbol())
            .collect();
        assert!(commit_text.contains('▾'), "commit row was {commit_text:?}");
    }

    #[test]
    fn section_divider_line_renders_between_sections() {
        let app = app_with_commits(vec![commit("aaa1111", "first")]);
        let area = Rect::new(0, 0, 26, 20);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        let divider = source_panel_section_divider_rect(area, app.source_panel_section_split);
        assert_ne!(divider, Rect::default());
        let line: String = (divider.x..divider.x + divider.width)
            .map(|x| buf[(x, divider.y)].symbol())
            .collect();
        assert!(line.chars().all(|c| c == '─'), "divider was {line:?}");
    }

    #[test]
    fn non_git_workspace_renders_empty_state_and_no_sections() {
        let mut app = app_with_changes(vec![modified("src/app/foo.rs")]);
        // Positively mark the workspace as a non-git directory.
        app.workspaces[0].cached_is_git_repo = false;
        let area = Rect::new(0, 0, 26, 20);
        app.view.source_panel_changes_card_areas =
            compute_source_panel_changes_card_areas(&app, area);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel(&app, frame, area))
            .expect("source panel should render");

        let buf = terminal.backend().buffer().clone();
        // The single dim empty-state line replaces both section bodies.
        let top: String = (area.x + 1..area.x + area.width)
            .map(|x| buf[(x, area.y)].symbol())
            .collect();
        assert!(
            top.starts_with(" not a git repository"),
            "top row was {top:?}"
        );
        // No section headers or file rows are rendered.
        let whole: String = (area.y..area.y + area.height)
            .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(!whole.contains("changes"), "panel was {whole:?}");
        assert!(!whole.contains("graph"), "panel was {whole:?}");
        assert!(!whole.contains("foo.rs"), "panel was {whole:?}");
    }

    #[test]
    fn collapsed_strip_renders_change_count_badge() {
        let app = app_with_changes((0..3).map(|i| modified(&format!("f{i}.rs"))).collect());
        let area = Rect::new(0, 0, 3, 20);
        let mut terminal =
            Terminal::new(TestBackend::new(3, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel_collapsed(&app, frame, area))
            .expect("collapsed source panel should render");

        let buf = terminal.backend().buffer().clone();
        // The expand toggle is still present.
        let toggle = collapsed_source_panel_toggle_rect(area);
        assert_eq!(buf[(toggle.x, toggle.y)].symbol(), "«");
        // The changed-file count appears somewhere in the strip.
        let whole: String = (area.y..area.y + area.height)
            .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(whole.contains('3'), "strip was {whole:?}");
    }

    #[test]
    fn collapsed_strip_has_no_badge_without_changes() {
        let app = app_with_changes(Vec::new());
        let area = Rect::new(0, 0, 3, 20);
        let mut terminal =
            Terminal::new(TestBackend::new(3, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_source_panel_collapsed(&app, frame, area))
            .expect("collapsed source panel should render");

        let buf = terminal.backend().buffer().clone();
        let whole: String = (area.y..area.y + area.height)
            .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(
            !whole.chars().any(|c| c.is_ascii_digit()),
            "strip should have no count badge, was {whole:?}"
        );
    }
}
