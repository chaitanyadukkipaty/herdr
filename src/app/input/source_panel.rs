use ratatui::layout::{Direction, Rect};

use crate::app::state::{AppState, Mode, SourcePanelActiveItem, SourcePanelWidthSource};
use crate::layout::PaneId;
use crate::terminal::TerminalRuntimeRegistry;

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.x + rect.width
        && row >= rect.y
        && row < rect.y + rect.height
}

impl AppState {
    /// The Changes section rect within the source panel, or default when the
    /// panel is collapsed/absent.
    pub(super) fn source_panel_changes_rect(&self) -> Rect {
        let panel = self.view.source_panel_rect;
        if self.source_panel_collapsed || panel.width <= 1 || panel.height == 0 {
            return Rect::default();
        }
        crate::ui::source_panel_changes_rect(panel, self.source_panel_section_split)
    }

    /// The Graph section rect within the source panel, or default when the
    /// panel is collapsed/absent.
    pub(super) fn source_panel_graph_rect(&self) -> Rect {
        let panel = self.view.source_panel_rect;
        if self.source_panel_collapsed || panel.width <= 1 || panel.height == 0 {
            return Rect::default();
        }
        crate::ui::source_panel_graph_rect(panel, self.source_panel_section_split)
    }

    /// Scroll the Changes file list by `delta` rows (negative scrolls up).
    pub(super) fn scroll_source_panel_changes(&mut self, delta: i16) {
        let section = self.source_panel_changes_rect();
        let max_scroll =
            crate::ui::source_panel_changes_scroll_metrics(self, section).max_offset_from_bottom;
        if delta.is_negative() {
            self.source_panel_changes_scroll = self
                .source_panel_changes_scroll
                .saturating_sub(delta.unsigned_abs() as usize);
        } else {
            self.source_panel_changes_scroll = self
                .source_panel_changes_scroll
                .saturating_add(delta as usize)
                .min(max_scroll);
        }
    }

    /// Scroll the Graph commit list by `delta` rows (negative scrolls up).
    pub(super) fn scroll_source_panel_log(&mut self, delta: i16) {
        let section = self.source_panel_graph_rect();
        let max_scroll =
            crate::ui::source_panel_log_scroll_metrics(self, section).max_offset_from_bottom;
        if delta.is_negative() {
            self.source_panel_log_scroll = self
                .source_panel_log_scroll
                .saturating_sub(delta.unsigned_abs() as usize);
        } else {
            self.source_panel_log_scroll = self
                .source_panel_log_scroll
                .saturating_add(delta as usize)
                .min(max_scroll);
        }
    }

    /// Route a scroll-wheel notch over the source panel to whichever section
    /// the cursor is over, scrolling only if that section actually overflows.
    pub(super) fn scroll_source_panel_section_at(&mut self, col: u16, row: u16, delta: i16) {
        let graph = self.source_panel_graph_rect();
        if rect_contains(graph, col, row) {
            if crate::ui::should_show_scrollbar(crate::ui::source_panel_log_scroll_metrics(
                self, graph,
            )) {
                self.scroll_source_panel_log(delta);
            }
            return;
        }
        let changes = self.source_panel_changes_rect();
        if crate::ui::should_show_scrollbar(crate::ui::source_panel_changes_scroll_metrics(
            self, changes,
        )) {
            self.scroll_source_panel_changes(delta);
        }
    }

    /// True when `(col, row)` is on the panel's collapse/expand toggle.
    pub(super) fn on_source_panel_toggle(&self, col: u16, row: u16) -> bool {
        let rect = self.view.source_panel_toggle_rect;
        rect.width > 0
            && col >= rect.x
            && col < rect.x + rect.width
            && row >= rect.y
            && row < rect.y + rect.height
    }

    /// True when `(col, row)` is on the Changes header's ↻ refresh glyph.
    pub(super) fn on_source_panel_changes_refresh(&self, col: u16, row: u16) -> bool {
        !self.source_panel_collapsed
            && rect_contains(self.view.source_panel_changes_refresh_rect, col, row)
    }

    /// The `change_idx` of the changed-file row under `(col, row)`, if any.
    pub(super) fn source_panel_changed_file_at(&self, col: u16, row: u16) -> Option<usize> {
        self.view
            .source_panel_changes_card_areas
            .iter()
            .find(|area| rect_contains(area.rect, col, row))
            .map(|area| area.change_idx)
    }

    /// The `(log_idx, file_idx)` of the inline commit-file row under `(col, row)`.
    pub(super) fn source_panel_commit_file_at(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        self.view
            .source_panel_commit_file_card_areas
            .iter()
            .find(|area| rect_contains(area.rect, col, row))
            .map(|area| (area.log_idx, area.file_idx))
    }

    /// Whether the remembered diff pane is still present in the active tab. The
    /// row highlight is suppressed once the pane is gone so a closed diff leaves
    /// no stale highlight behind.
    fn source_panel_diff_pane_is_live(&self) -> bool {
        let Some(pane_id) = self.source_panel_diff_pane else {
            return false;
        };
        self.active
            .and_then(|idx| self.workspaces.get(idx))
            .and_then(|ws| ws.active_tab())
            .is_some_and(|tab| tab.layout.pane_ids().contains(&pane_id))
    }

    /// The source-panel item to highlight: the row whose content fills the diff
    /// pane, but only while that pane is still open.
    pub(crate) fn source_panel_highlighted_item(&self) -> Option<&SourcePanelActiveItem> {
        self.source_panel_diff_pane_is_live()
            .then_some(self.source_panel_active_item.as_ref())
            .flatten()
    }

    /// Pick the pane — and the direction to split it — that a source-panel
    /// diff/show pane should be spawned from. Splitting the *focused* pane
    /// shreds the panel into ever-thinner slivers when several commits are
    /// opened in a row, because each freshly spawned git pane becomes focused
    /// and is then halved by the next click. Instead we split the largest pane
    /// in the active tab along its longer visual axis, so repeated opens spread
    /// across the tab and stay readable: panes split side-by-side while they are
    /// wide enough, then start stacking vertically once they are not.
    fn source_panel_split_target(&self) -> Option<(PaneId, Direction)> {
        let ws = self.workspaces.get(self.active?)?;
        let tab = ws.active_tab()?;
        let target = tab
            .layout
            .panes(self.view.terminal_area)
            .into_iter()
            .max_by_key(|pane| u32::from(pane.rect.width) * u32::from(pane.rect.height))?;
        // Terminal cells are roughly twice as tall as they are wide, so weight
        // the height to compare physical proportions and split the longer side.
        let direction = if u32::from(target.rect.width) >= u32::from(target.rect.height) * 2 {
            Direction::Horizontal
        } else {
            Direction::Vertical
        };
        Some((target.id, direction))
    }

    /// Replace the source panel's diff pane content with `argv` (a `git
    /// diff`/`git show` command) when that pane is still open in the active tab,
    /// or spawn a fresh pane beside the largest one otherwise. `source_cwd` is
    /// the clicked workspace's repo, used only as a fallback working directory
    /// for a freshly spawned pane. Returns `true` when a pane showed the output.
    fn show_in_source_panel_diff_pane(
        &mut self,
        terminal_runtimes: &mut TerminalRuntimeRegistry,
        argv: &[String],
        source_cwd: &std::path::Path,
    ) -> bool {
        self.reuse_source_panel_diff_pane(terminal_runtimes, argv)
            || self.spawn_source_panel_diff_pane(terminal_runtimes, argv, source_cwd)
    }

    /// Re-run `argv` inside the existing source-panel diff pane, replacing its
    /// content in place. Fails (so the caller spawns a fresh pane) when there is
    /// no remembered diff pane, it has been closed, or it no longer lives in the
    /// active workspace's active tab.
    fn reuse_source_panel_diff_pane(
        &mut self,
        terminal_runtimes: &mut TerminalRuntimeRegistry,
        argv: &[String],
    ) -> bool {
        let Some(pane_id) = self.source_panel_diff_pane else {
            return false;
        };
        let Some(active_idx) = self.active else {
            return false;
        };

        // Gather everything needed under an immutable borrow, as owned values, so
        // the borrow ends before we spawn the runtime and mutate state below.
        let Some((terminal_id, cwd, rows, cols, events, render_notify, render_dirty)) = (|| {
            let ws = self.workspaces.get(active_idx)?;
            let tab = ws.active_tab()?;
            if !tab.layout.pane_ids().contains(&pane_id) {
                return None;
            }
            let terminal_id = ws.terminal_id(pane_id)?.clone();
            let cwd = self.terminals.get(&terminal_id)?.cwd.clone();
            let (rows, cols) = terminal_runtimes
                .get(&terminal_id)
                .map(|runtime| runtime.current_size())
                .unwrap_or_else(|| self.estimate_pane_size());
            Some((
                terminal_id,
                cwd,
                rows,
                cols,
                tab.events.clone(),
                tab.render_notify.clone(),
                tab.render_dirty.clone(),
            ))
        })(
        ) else {
            return false;
        };

        let runtime = match crate::terminal::TerminalRuntime::spawn_argv_command(
            pane_id,
            rows,
            cols,
            cwd,
            argv,
            self.pane_scrollback_limit_bytes,
            self.host_terminal_theme,
            events,
            render_notify,
            render_dirty,
        ) {
            Ok(runtime) => runtime,
            Err(_) => return false,
        };

        // Swapping the runtime drops the old process (git/pager) and its grid, so
        // the pane shows the new command's output on a clean screen. Dropping the
        // outgoing runtime kills its child, which fires `PaneDied` for `pane_id`;
        // mark it so `handle_pane_died` ignores that one spurious death and keeps
        // the pane (which is alive with its freshly inserted runtime).
        self.suppress_pane_death.insert(pane_id);
        terminal_runtimes.insert(terminal_id.clone(), runtime);
        if let Some(terminal) = self.terminals.get_mut(&terminal_id) {
            terminal.launch_argv = Some(argv.to_vec());
        }
        self.focus_pane_in_workspace(active_idx, pane_id);
        self.mark_session_dirty();
        self.mode = Mode::Terminal;
        true
    }

    /// Spawn a fresh pane running `argv`, splitting the largest pane in the
    /// active tab, and remember it as the source panel's diff pane so later
    /// clicks replace its content instead of splitting again.
    fn spawn_source_panel_diff_pane(
        &mut self,
        terminal_runtimes: &mut TerminalRuntimeRegistry,
        argv: &[String],
        source_cwd: &std::path::Path,
    ) -> bool {
        let Some(active_idx) = self.active else {
            return false;
        };
        let (rows, cols) = self.estimate_pane_size();
        let new_rows = (rows / 2).max(4);
        let new_cols = (cols / 2).max(10);

        // Anchor the new pane to the active tab's focused cwd, falling back to the
        // clicked workspace's repo so it lands somewhere sensible on an empty tab.
        let pane_cwd = self
            .workspaces
            .get(active_idx)
            .and_then(|ws| {
                let tab = ws.active_tab()?;
                tab.cwd_for_pane(tab.layout.focused(), &self.terminals, terminal_runtimes)
            })
            .or_else(|| Some(source_cwd.to_path_buf()));

        let Some((target_pane, direction)) = self.source_panel_split_target() else {
            return false;
        };
        let previous_focus = self.current_pane_focus_target();
        let Some(ws) = self.workspaces.get_mut(active_idx) else {
            return false;
        };
        let new_pane = match ws.split_pane_argv_command(
            target_pane,
            direction,
            new_rows,
            new_cols,
            pane_cwd,
            argv,
            self.pane_scrollback_limit_bytes,
            self.host_terminal_theme,
            true,
        ) {
            Some(Ok((_, new_pane))) => new_pane,
            _ => return false,
        };
        let new_id = new_pane.pane_id;
        terminal_runtimes.insert(new_pane.terminal.id.clone(), new_pane.runtime);
        self.remove_alias_shadowed_by_new_pane(new_id);
        self.terminals
            .insert(new_pane.terminal.id.clone(), new_pane.terminal);
        self.record_pane_focus_change(previous_focus, active_idx, new_id);
        self.source_panel_diff_pane = Some(new_id);
        self.mark_session_dirty();
        self.mode = Mode::Terminal;
        true
    }

    /// Open a changed file's diff. The diff is computed against the workspace
    /// whose change was clicked (`ws_idx`), via `git -C <identity_cwd>`, so it is
    /// independent of the pane's working directory. The output reuses the source
    /// panel's diff pane when one is open, else spawns a new pane. Returns `true`
    /// when a pane showed the diff.
    pub(super) fn open_changed_file_diff(
        &mut self,
        terminal_runtimes: &mut TerminalRuntimeRegistry,
        ws_idx: usize,
        change_idx: usize,
    ) -> bool {
        let Some(source_ws) = self.workspaces.get(ws_idx) else {
            return false;
        };
        let Some(change) = source_ws.changed_files().get(change_idx).cloned() else {
            return false;
        };
        let Some(cwd) = source_ws.resolved_identity_cwd_from(&self.terminals, terminal_runtimes)
        else {
            return false;
        };
        let argv = crate::workspace::changed_file_diff_argv(&cwd, &change);
        let shown = self.show_in_source_panel_diff_pane(terminal_runtimes, &argv, &cwd);
        if shown {
            self.source_panel_active_item = Some(SourcePanelActiveItem::WorkingFile(change.path));
        }
        shown
    }

    /// True when `(col, row)` is on the Graph header's ↻ refresh glyph.
    pub(super) fn on_source_panel_log_refresh(&self, col: u16, row: u16) -> bool {
        !self.source_panel_collapsed
            && rect_contains(self.view.source_panel_log_refresh_rect, col, row)
    }

    /// True when `(col, row)` is on the Graph section's " load more" row.
    pub(super) fn on_source_panel_load_more(&self, col: u16, row: u16) -> bool {
        !self.source_panel_collapsed
            && rect_contains(self.view.source_panel_load_more_rect, col, row)
    }

    /// The `log_idx` of the commit row under `(col, row)`, if any.
    pub(super) fn source_panel_commit_at(&self, col: u16, row: u16) -> Option<usize> {
        self.view
            .source_panel_log_card_areas
            .iter()
            .find(|area| rect_contains(area.rect, col, row))
            .map(|area| area.log_idx)
    }

    /// Handle a left-click on a commit row. A collapsed commit expands to reveal
    /// its files inline (fetching them the first time); clicking an
    /// already-expanded commit's leading chevron collapses it again, while
    /// clicking elsewhere on it opens its full `git show` in the diff pane.
    /// Connector-only rows (no sha) are ignored.
    pub(super) fn click_source_panel_commit(
        &mut self,
        terminal_runtimes: &mut TerminalRuntimeRegistry,
        ws_idx: usize,
        log_idx: usize,
        col: u16,
    ) {
        let Some(source_ws) = self.workspaces.get(ws_idx) else {
            return;
        };
        let Some(commit) = source_ws.commits().get(log_idx) else {
            return;
        };
        let Some(sha) = commit.sha.clone() else {
            return;
        };

        if !self.source_panel_expanded_commits.contains(&sha) {
            self.expand_source_panel_commit(terminal_runtimes, ws_idx, &sha);
            return;
        }

        // Already expanded: the leading chevron (the cell right after the graph
        // glyphs) collapses; the rest of the row opens the commit.
        let chevron_col = self
            .source_panel_graph_rect()
            .x
            .saturating_add(commit.graph_cell.chars().count() as u16);
        if col == chevron_col {
            self.source_panel_expanded_commits.remove(&sha);
            return;
        }
        self.open_commit_show(terminal_runtimes, ws_idx, log_idx);
    }

    /// Fetch (once) and cache the files a commit touched, then mark it expanded so
    /// the Graph section renders them inline beneath the commit row.
    fn expand_source_panel_commit(
        &mut self,
        terminal_runtimes: &mut TerminalRuntimeRegistry,
        ws_idx: usize,
        sha: &str,
    ) {
        if !self.source_panel_commit_files.contains_key(sha) {
            let Some(cwd) = self
                .workspaces
                .get(ws_idx)
                .and_then(|ws| ws.resolved_identity_cwd_from(&self.terminals, terminal_runtimes))
            else {
                return;
            };
            let files = crate::workspace::commit_files_for_cwd(&cwd, sha);
            self.source_panel_commit_files
                .insert(sha.to_string(), files);
        }
        self.source_panel_expanded_commits.insert(sha.to_string());
    }

    /// Open a commit's `git show`. The command runs via `git -C <identity_cwd>`
    /// against the workspace whose commit was clicked (`ws_idx`), so it is
    /// independent of the pane's working directory. Connector-only rows (no sha)
    /// are ignored. The output reuses the source panel's diff pane when one is
    /// open, else spawns a new pane. Returns `true` when a pane showed the commit.
    pub(super) fn open_commit_show(
        &mut self,
        terminal_runtimes: &mut TerminalRuntimeRegistry,
        ws_idx: usize,
        log_idx: usize,
    ) -> bool {
        let Some(source_ws) = self.workspaces.get(ws_idx) else {
            return false;
        };
        let Some(sha) = source_ws
            .commits()
            .get(log_idx)
            .and_then(|commit| commit.sha.clone())
        else {
            return false;
        };
        let Some(cwd) = source_ws.resolved_identity_cwd_from(&self.terminals, terminal_runtimes)
        else {
            return false;
        };
        let argv = crate::workspace::commit_show_argv(&cwd, &sha);
        let shown = self.show_in_source_panel_diff_pane(terminal_runtimes, &argv, &cwd);
        if shown {
            self.source_panel_active_item = Some(SourcePanelActiveItem::Commit(sha));
        }
        shown
    }

    /// Open the diff of a single file within an expanded commit via `git show
    /// <sha> -- <path>`, reusing the source panel's diff pane when one is open.
    pub(super) fn open_commit_file_diff(
        &mut self,
        terminal_runtimes: &mut TerminalRuntimeRegistry,
        ws_idx: usize,
        log_idx: usize,
        file_idx: usize,
    ) -> bool {
        let Some(source_ws) = self.workspaces.get(ws_idx) else {
            return false;
        };
        let Some(sha) = source_ws
            .commits()
            .get(log_idx)
            .and_then(|commit| commit.sha.clone())
        else {
            return false;
        };
        let Some(change) = self
            .source_panel_commit_files
            .get(&sha)
            .and_then(|files| files.get(file_idx))
            .cloned()
        else {
            return false;
        };
        let Some(cwd) = source_ws.resolved_identity_cwd_from(&self.terminals, terminal_runtimes)
        else {
            return false;
        };
        let argv = crate::workspace::commit_file_diff_argv(&cwd, &sha, &change);
        let shown = self.show_in_source_panel_diff_pane(terminal_runtimes, &argv, &cwd);
        if shown {
            self.source_panel_active_item = Some(SourcePanelActiveItem::CommitFile {
                sha,
                path: change.path,
            });
        }
        shown
    }

    /// Grow the loaded commit window by one page and request a git refresh so the
    /// Graph section repopulates with the larger history. `cached_log_has_more` is
    /// cleared until the refresh completes so the "load more" row hides during the
    /// in-flight fetch.
    pub(super) fn load_more_commits(&mut self, ws_idx: usize) {
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.loaded_commit_count = ws
                .loaded_commit_count
                .saturating_add(crate::workspace::DEFAULT_LOADED_COMMIT_COUNT);
            ws.cached_log_has_more = false;
        }
        self.request_source_panel_git_refresh = true;
    }

    /// True when `(col, row)` is on the draggable divider between the Changes and
    /// Graph sections. The divider shares its row with the Graph header, so the
    /// header's refresh glyph takes precedence over starting a drag.
    pub(super) fn on_source_panel_section_divider(&self, col: u16, row: u16) -> bool {
        if self.source_panel_collapsed {
            return false;
        }
        rect_contains(self.view.source_panel_section_divider_rect, col, row)
            && !self.on_source_panel_log_refresh(col, row)
    }

    /// Update the section split from a divider drag. Dragging down grows the
    /// Changes section; the ratio is clamped to `[0.1, 0.9]`.
    pub(super) fn set_source_panel_section_split_from_drag(
        &mut self,
        start_y: u16,
        start_ratio: f32,
        row: u16,
    ) {
        let panel = self.view.source_panel_rect;
        if panel.height < 6 {
            return;
        }
        let delta_rows = i32::from(row) - i32::from(start_y);
        let delta_ratio = delta_rows as f32 / panel.height as f32;
        self.source_panel_section_split = (start_ratio + delta_ratio).clamp(0.1, 0.9);
        self.mark_session_dirty();
    }

    /// True when `(col, row)` is on the panel's draggable left edge (and not on
    /// the toggle). The source panel resizes from its left edge.
    pub(super) fn on_source_panel_resize_edge(&self, col: u16, row: u16) -> bool {
        if self.source_panel_collapsed {
            return false;
        }
        let panel = self.view.source_panel_rect;
        if panel.width == 0 {
            return false;
        }
        let toggle = self.view.source_panel_toggle_rect;
        let on_toggle = toggle.width > 0
            && col >= toggle.x
            && col < toggle.x + toggle.width
            && row >= toggle.y
            && row < toggle.y + toggle.height;
        !on_toggle && col == panel.x && row >= panel.y && row < panel.y + panel.height
    }

    /// Update the panel width from a left-edge drag. Dragging the edge left
    /// (smaller column) widens the panel.
    pub(super) fn set_source_panel_width_from_drag(
        &mut self,
        start_x: u16,
        start_width: u16,
        col: u16,
    ) {
        let delta = i32::from(start_x) - i32::from(col);
        let new_width = (i32::from(start_width) + delta).clamp(
            i32::from(self.source_panel_min_width),
            i32::from(self.source_panel_max_width),
        ) as u16;
        self.source_panel_width = new_width;
        self.source_panel_width_source = SourcePanelWidthSource::Manual;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn width_drag_widens_when_dragging_left_edge_left() {
        let mut app = AppState::test_new();
        app.source_panel_min_width = 18;
        app.source_panel_max_width = 36;

        // Drag the left edge from column 80 to 76 (4 columns left) → +4 width.
        app.set_source_panel_width_from_drag(80, 26, 76);

        assert_eq!(app.source_panel_width, 30);
        assert_eq!(
            app.source_panel_width_source,
            SourcePanelWidthSource::Manual
        );
    }

    #[test]
    fn width_drag_clamps_to_configured_bounds() {
        let mut app = AppState::test_new();
        app.source_panel_min_width = 18;
        app.source_panel_max_width = 36;

        app.set_source_panel_width_from_drag(80, 30, 40); // far left → max
        assert_eq!(app.source_panel_width, 36);

        app.set_source_panel_width_from_drag(80, 30, 200); // far right → min
        assert_eq!(app.source_panel_width, 18);
    }

    #[test]
    fn toggle_hit_test_uses_computed_view_rect() {
        let mut app = AppState::test_new();
        app.view.source_panel_toggle_rect = Rect::new(95, 19, 1, 1);

        assert!(app.on_source_panel_toggle(95, 19));
        assert!(!app.on_source_panel_toggle(94, 19));
    }

    #[test]
    fn split_target_splits_wide_pane_side_by_side() {
        let mut app = AppState::test_new();
        app.workspaces = vec![crate::workspace::Workspace::test_new("ws")];
        app.active = Some(0);
        // A single wide pane: split horizontally so the diff lands beside it.
        app.view.terminal_area = Rect::new(0, 0, 200, 50);

        let (pane, direction) = app
            .source_panel_split_target()
            .expect("a target pane should exist");
        let focused = app.workspaces[0].active_tab().unwrap().layout.focused();
        assert_eq!(pane, focused, "the only pane is both largest and focused");
        assert_eq!(direction, Direction::Horizontal);
    }

    #[test]
    fn split_target_stacks_when_pane_is_no_longer_wide() {
        let mut app = AppState::test_new();
        app.workspaces = vec![crate::workspace::Workspace::test_new("ws")];
        app.active = Some(0);
        // A tall, narrow pane should split vertically rather than shrink width.
        app.view.terminal_area = Rect::new(0, 0, 40, 60);

        let (_, direction) = app
            .source_panel_split_target()
            .expect("a target pane should exist");
        assert_eq!(direction, Direction::Vertical);
    }

    #[test]
    fn split_target_avoids_the_small_focused_pane() {
        let mut app = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("ws");
        // Split once (focus moves to the right pane), then split that right pane
        // again so the focused pane is one of the small slivers and the left
        // pane is the largest. Opening a diff must target the large left pane,
        // not keep halving the tiny focused one — that is the shredding bug.
        ws.test_split(Direction::Horizontal);
        ws.test_split(Direction::Horizontal);
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.view.terminal_area = Rect::new(0, 0, 200, 50);

        let (target, _) = app
            .source_panel_split_target()
            .expect("a target pane should exist");
        let focused = app.workspaces[0].active_tab().unwrap().layout.focused();
        assert_ne!(
            target, focused,
            "should split the largest pane, not the small focused one"
        );
    }

    #[test]
    fn reuse_diff_pane_declines_when_none_is_tracked() {
        let mut app = AppState::test_new();
        app.workspaces = vec![crate::workspace::Workspace::test_new("ws")];
        app.active = Some(0);
        let mut runtimes = TerminalRuntimeRegistry::new();

        // No diff pane remembered → the caller falls through to spawning one.
        assert!(!app.reuse_source_panel_diff_pane(&mut runtimes, &["git".to_string()]));
    }

    #[test]
    fn reuse_diff_pane_declines_when_tracked_pane_is_gone() {
        let mut app = AppState::test_new();
        app.workspaces = vec![crate::workspace::Workspace::test_new("ws")];
        app.active = Some(0);
        // A diff pane id that was never part of any tab (e.g. since closed).
        app.source_panel_diff_pane = Some(crate::layout::PaneId::from_raw(99_999));
        let mut runtimes = TerminalRuntimeRegistry::new();

        assert!(!app.reuse_source_panel_diff_pane(&mut runtimes, &["git".to_string()]));
    }

    #[test]
    fn scroll_changes_advances_then_clamps_to_top() {
        let mut app = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("ws");
        ws.cached_changes = (0..40)
            .map(|i| crate::workspace::ChangedFile {
                path: std::path::PathBuf::from(format!("f{i}.rs")),
                status: crate::workspace::ChangeStatus::Modified,
            })
            .collect();
        app.workspaces = vec![ws];
        app.selected = 0;
        app.view.source_panel_rect = Rect::new(70, 0, 26, 20);

        app.scroll_source_panel_changes(3);
        assert_eq!(app.source_panel_changes_scroll, 3);

        app.scroll_source_panel_changes(-10);
        assert_eq!(app.source_panel_changes_scroll, 0);
    }

    #[test]
    fn scroll_log_advances_then_clamps_to_top() {
        let mut app = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("ws");
        ws.cached_log = (0..40)
            .map(|i| crate::workspace::CommitInfo {
                graph_cell: "* ".into(),
                sha: Some(format!("sha{i}")),
                subject: format!("commit {i}"),
                author: "Ada".into(),
                decorations: Vec::new(),
            })
            .collect();
        app.workspaces = vec![ws];
        app.selected = 0;
        app.view.source_panel_rect = Rect::new(70, 0, 26, 20);

        app.scroll_source_panel_log(3);
        assert_eq!(app.source_panel_log_scroll, 3);

        app.scroll_source_panel_log(-10);
        assert_eq!(app.source_panel_log_scroll, 0);
    }

    #[test]
    fn wheel_over_graph_section_scrolls_log_not_changes() {
        let mut app = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("ws");
        ws.cached_log = (0..40)
            .map(|i| crate::workspace::CommitInfo {
                graph_cell: "* ".into(),
                sha: Some(format!("sha{i}")),
                subject: format!("commit {i}"),
                author: "Ada".into(),
                decorations: Vec::new(),
            })
            .collect();
        app.workspaces = vec![ws];
        app.selected = 0;
        app.view.source_panel_rect = Rect::new(70, 0, 26, 20);

        let graph = app.source_panel_graph_rect();
        app.scroll_source_panel_section_at(graph.x, graph.y + 1, 2);

        assert_eq!(app.source_panel_log_scroll, 2);
        assert_eq!(app.source_panel_changes_scroll, 0);
    }

    #[test]
    fn resize_edge_matches_left_column_but_not_toggle() {
        let mut app = AppState::test_new();
        app.source_panel_collapsed = false;
        app.view.source_panel_rect = Rect::new(70, 0, 26, 20);
        app.view.source_panel_toggle_rect = Rect::new(95, 19, 1, 1);

        assert!(app.on_source_panel_resize_edge(70, 5)); // left edge column
        assert!(!app.on_source_panel_resize_edge(71, 5)); // interior
        assert!(!app.on_source_panel_resize_edge(95, 19)); // on the toggle

        app.source_panel_collapsed = true;
        assert!(!app.on_source_panel_resize_edge(70, 5)); // no resize when collapsed
    }

    #[test]
    fn changed_file_hit_test_resolves_change_idx() {
        use crate::app::state::SourcePanelFileArea;
        let mut app = AppState::test_new();
        app.view.source_panel_changes_card_areas = vec![
            SourcePanelFileArea {
                rect: Rect::new(71, 1, 24, 1),
                change_idx: 0,
            },
            SourcePanelFileArea {
                rect: Rect::new(71, 2, 24, 1),
                change_idx: 1,
            },
        ];

        assert_eq!(app.source_panel_changed_file_at(75, 1), Some(0));
        assert_eq!(app.source_panel_changed_file_at(75, 2), Some(1));
        assert_eq!(app.source_panel_changed_file_at(75, 5), None); // below the rows
        assert_eq!(app.source_panel_changed_file_at(120, 1), None); // outside columns
    }

    #[test]
    fn refresh_hit_test_honors_collapsed_state() {
        let mut app = AppState::test_new();
        app.source_panel_collapsed = false;
        app.view.source_panel_changes_refresh_rect = Rect::new(95, 0, 1, 1);

        assert!(app.on_source_panel_changes_refresh(95, 0));
        assert!(!app.on_source_panel_changes_refresh(94, 0));

        app.source_panel_collapsed = true;
        assert!(!app.on_source_panel_changes_refresh(95, 0)); // ignored when collapsed
    }

    #[test]
    fn commit_hit_test_resolves_log_idx() {
        use crate::app::state::SourcePanelCommitArea;
        let mut app = AppState::test_new();
        app.view.source_panel_log_card_areas = vec![
            SourcePanelCommitArea {
                rect: Rect::new(71, 5, 24, 1),
                log_idx: 0,
            },
            SourcePanelCommitArea {
                rect: Rect::new(71, 6, 24, 1),
                log_idx: 1,
            },
        ];

        assert_eq!(app.source_panel_commit_at(75, 5), Some(0));
        assert_eq!(app.source_panel_commit_at(75, 6), Some(1));
        assert_eq!(app.source_panel_commit_at(75, 9), None);
        assert_eq!(app.source_panel_commit_at(120, 5), None);
    }

    #[test]
    fn commit_file_hit_test_resolves_log_and_file_indices() {
        use crate::app::state::SourcePanelCommitFileArea;
        let mut app = AppState::test_new();
        app.view.source_panel_commit_file_card_areas = vec![
            SourcePanelCommitFileArea {
                rect: Rect::new(71, 6, 24, 1),
                log_idx: 0,
                file_idx: 0,
            },
            SourcePanelCommitFileArea {
                rect: Rect::new(71, 7, 24, 1),
                log_idx: 0,
                file_idx: 1,
            },
        ];

        assert_eq!(app.source_panel_commit_file_at(75, 6), Some((0, 0)));
        assert_eq!(app.source_panel_commit_file_at(75, 7), Some((0, 1)));
        assert_eq!(app.source_panel_commit_file_at(75, 9), None);
        assert_eq!(app.source_panel_commit_file_at(120, 6), None);
    }

    /// An app with a single workspace whose log holds one commit, ready for
    /// source-panel click tests. `sha`'s files are pre-cached so expanding it
    /// never shells out to git.
    fn app_with_one_commit(sha: &str) -> AppState {
        let mut app = AppState::test_new();
        app.mode = Mode::Terminal;
        let mut ws = crate::workspace::Workspace::test_new("ws");
        ws.cached_log = vec![crate::workspace::CommitInfo {
            graph_cell: "* ".into(),
            sha: Some(sha.into()),
            subject: "subject".into(),
            author: "Ada".into(),
            decorations: Vec::new(),
        }];
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;
        app.view.source_panel_rect = Rect::new(70, 0, 26, 20);
        app.view.source_panel_log_card_areas = vec![crate::app::state::SourcePanelCommitArea {
            rect: Rect::new(71, 12, 24, 1),
            log_idx: 0,
        }];
        app.source_panel_commit_files.insert(sha.into(), Vec::new());
        app
    }

    #[test]
    fn clicking_collapsed_commit_expands_it() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = app_with_one_commit("aaa1111");
        let mut runtimes = TerminalRuntimeRegistry::new();

        // Click the row body (not the leading chevron column).
        app.handle_mouse(
            &mut runtimes,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 85,
                row: 12,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );

        assert!(app.source_panel_expanded_commits.contains("aaa1111"));
    }

    #[test]
    fn clicking_chevron_collapses_expanded_commit() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = app_with_one_commit("aaa1111");
        app.source_panel_expanded_commits.insert("aaa1111".into());

        // The chevron sits at the graph section's left edge plus the graph-cell
        // width ("* " → 2 columns).
        let chevron_col = app.source_panel_graph_rect().x + 2;
        let mut runtimes = TerminalRuntimeRegistry::new();

        app.handle_mouse(
            &mut runtimes,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: chevron_col,
                row: 12,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );

        assert!(!app.source_panel_expanded_commits.contains("aaa1111"));
    }

    #[test]
    fn highlighted_item_suppressed_when_diff_pane_absent() {
        let mut app = AppState::test_new();
        app.workspaces = vec![crate::workspace::Workspace::test_new("ws")];
        app.active = Some(0);
        app.source_panel_active_item = Some(SourcePanelActiveItem::Commit("aaa1111".into()));

        // No diff pane → no highlight, even with an active item set.
        assert!(app.source_panel_highlighted_item().is_none());

        // A diff pane that still lives in the active tab → the item highlights.
        let pane = app.workspaces[0].tabs[0].root_pane;
        app.source_panel_diff_pane = Some(pane);
        assert_eq!(
            app.source_panel_highlighted_item(),
            Some(&SourcePanelActiveItem::Commit("aaa1111".into()))
        );
    }

    #[test]
    fn log_refresh_hit_test_honors_collapsed_state() {
        let mut app = AppState::test_new();
        app.source_panel_collapsed = false;
        app.view.source_panel_log_refresh_rect = Rect::new(95, 10, 1, 1);

        assert!(app.on_source_panel_log_refresh(95, 10));
        assert!(!app.on_source_panel_log_refresh(94, 10));

        app.source_panel_collapsed = true;
        assert!(!app.on_source_panel_log_refresh(95, 10));
    }

    #[test]
    fn load_more_hit_test_honors_collapsed_state() {
        let mut app = AppState::test_new();
        app.source_panel_collapsed = false;
        app.view.source_panel_load_more_rect = Rect::new(71, 18, 24, 1);

        assert!(app.on_source_panel_load_more(75, 18));
        assert!(!app.on_source_panel_load_more(75, 17));

        app.source_panel_collapsed = true;
        assert!(!app.on_source_panel_load_more(75, 18));
    }

    #[test]
    fn load_more_commits_bumps_count_and_requests_refresh() {
        let mut app = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("ws");
        ws.cached_log_has_more = true;
        let before = ws.loaded_commit_count();
        app.workspaces = vec![ws];
        app.selected = 0;

        app.load_more_commits(0);

        assert_eq!(
            app.workspaces[0].loaded_commit_count(),
            before + crate::workspace::DEFAULT_LOADED_COMMIT_COUNT
        );
        // Cleared until the refresh repopulates the log.
        assert!(!app.workspaces[0].has_more_commits());
        assert!(app.request_source_panel_git_refresh);
    }

    #[test]
    fn section_divider_hit_test_excludes_refresh_cell() {
        let mut app = AppState::test_new();
        app.source_panel_collapsed = false;
        app.view.source_panel_section_divider_rect = Rect::new(71, 10, 24, 1);
        // The Graph refresh glyph shares the divider row's right edge.
        app.view.source_panel_log_refresh_rect = Rect::new(94, 10, 1, 1);

        assert!(app.on_source_panel_section_divider(75, 10));
        assert!(!app.on_source_panel_section_divider(94, 10)); // refresh glyph wins

        app.source_panel_collapsed = true;
        assert!(!app.on_source_panel_section_divider(75, 10));
    }

    #[test]
    fn section_split_drag_updates_and_clamps_ratio() {
        let mut app = AppState::test_new();
        app.view.source_panel_rect = Rect::new(70, 0, 26, 20);
        app.source_panel_section_split = 0.5;

        // Drag the divider down 4 rows of a 20-row panel → +0.2 ratio.
        app.set_source_panel_section_split_from_drag(10, 0.5, 14);
        assert!((app.source_panel_section_split - 0.7).abs() < 1e-3);

        // Dragging far down clamps to 0.9.
        app.set_source_panel_section_split_from_drag(0, 0.5, 200);
        assert!((app.source_panel_section_split - 0.9).abs() < 1e-3);

        // Dragging far up clamps to 0.1.
        app.set_source_panel_section_split_from_drag(200, 0.5, 0);
        assert!((app.source_panel_section_split - 0.1).abs() < 1e-3);
    }

    #[test]
    fn clicking_log_refresh_requests_git_refresh() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = AppState::test_new();
        app.mode = Mode::Terminal;
        app.view.source_panel_rect = Rect::new(70, 0, 26, 20);
        app.view.source_panel_log_refresh_rect = Rect::new(95, 10, 1, 1);
        let mut runtimes = TerminalRuntimeRegistry::new();

        app.handle_mouse(
            &mut runtimes,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 95,
                row: 10,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );

        assert!(app.request_source_panel_git_refresh);
    }

    #[test]
    fn clicking_load_more_bumps_loaded_commit_count() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = AppState::test_new();
        app.mode = Mode::Terminal;
        let mut ws = crate::workspace::Workspace::test_new("ws");
        ws.cached_log_has_more = true;
        let before = ws.loaded_commit_count();
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;
        app.view.source_panel_rect = Rect::new(70, 0, 26, 20);
        app.view.source_panel_load_more_rect = Rect::new(71, 18, 24, 1);
        let mut runtimes = TerminalRuntimeRegistry::new();

        app.handle_mouse(
            &mut runtimes,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 75,
                row: 18,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );

        assert_eq!(
            app.workspaces[0].loaded_commit_count(),
            before + crate::workspace::DEFAULT_LOADED_COMMIT_COUNT
        );
        assert!(app.request_source_panel_git_refresh);
    }

    #[test]
    fn section_divider_drag_start_sets_drag_target() {
        use crate::app::state::DragTarget;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = AppState::test_new();
        app.mode = Mode::Terminal;
        app.source_panel_collapsed = false;
        app.view.source_panel_rect = Rect::new(70, 0, 26, 20);
        app.view.source_panel_section_divider_rect = Rect::new(71, 10, 24, 1);
        app.source_panel_section_split = 0.5;
        let mut runtimes = TerminalRuntimeRegistry::new();

        app.handle_mouse(
            &mut runtimes,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 75,
                row: 10,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );

        assert!(matches!(
            app.drag.as_ref().map(|d| &d.target),
            Some(DragTarget::SourcePanelSectionResize {
                start_y: 10,
                start_ratio,
            }) if (start_ratio - 0.5).abs() < 1e-6
        ));
    }

    #[test]
    fn clicking_changes_refresh_requests_git_refresh() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = AppState::test_new();
        app.mode = Mode::Terminal;
        app.view.source_panel_rect = Rect::new(70, 0, 26, 20);
        app.view.source_panel_changes_refresh_rect = Rect::new(95, 0, 1, 1);
        let mut runtimes = TerminalRuntimeRegistry::new();

        app.handle_mouse(
            &mut runtimes,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 95,
                row: 0,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );

        assert!(app.request_source_panel_git_refresh);
    }
}
