use crate::db::{HistoryEntry, history_path, load_commands, log_error};
use crate::util::{elide_start_to_width, format_time_ago, shorten_path, truncate_to_width};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

const INPUT_DRAIN_TIMEOUT_MS: u64 = 20;

enum SearchAction {
    Run(String),
    Replay(i64),
}

#[derive(PartialEq)]
enum FocusedPanel {
    Input,
    Summary,
}

pub fn search(initial_query: Option<String>) {
    if !history_path().join("history.db").exists() {
        return;
    }

    let entries = match load_commands() {
        Ok(e) => e,
        Err(_) => return,
    };
    if entries.is_empty() {
        return;
    }

    match run_tui(&entries, initial_query) {
        Ok(Some(SearchAction::Run(cmd))) => print!("{cmd}"),
        Ok(Some(SearchAction::Replay(id))) => print!("__DEJINY_REPLAY__{id}"),
        Ok(None) => {}
        Err(e) => {
            log_error(&format!("search: {e}"));
        }
    }
}

struct IndexedEntry<'a> {
    index: usize,
    entry: &'a HistoryEntry,
}

impl AsRef<str> for IndexedEntry<'_> {
    fn as_ref(&self) -> &str {
        &self.entry.command
    }
}

struct SearchState {
    input: String,
    all_entries: Vec<HistoryEntry>,
    filtered: Vec<usize>,
    list_state: ListState,
    page_size: usize,
    filter_recorded: bool,
    focus: FocusedPanel,
    summary_scroll: u16,
    /// Cells the id column needs for the widest id in the history.
    id_width: usize,
}

impl SearchState {
    fn new(entries: Vec<HistoryEntry>) -> Self {
        let filtered: Vec<usize> = (0..entries.len()).collect();
        // The id is a rowid, so its width is a property of the history, not a
        // constant: a six-cell column silently overflows past 999,999 commands.
        let id_width = entries
            .iter()
            .map(|e| e.id)
            .max()
            .unwrap_or(0)
            .to_string()
            .len()
            .max(ID_MIN_WIDTH);
        let mut list_state = ListState::default();
        if !filtered.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            input: String::new(),
            all_entries: entries,
            filtered,
            list_state,
            page_size: 20,
            filter_recorded: false,
            focus: FocusedPanel::Input,
            summary_scroll: 0,
            id_width,
        }
    }

    fn refilter(&mut self) {
        if self.input.is_empty() {
            self.filtered = (0..self.all_entries.len())
                .filter(|&i| !self.filter_recorded || self.all_entries[i].has_recording)
                .collect();
        } else {
            let mut matcher = Matcher::new(Config::DEFAULT);
            let pattern = Pattern::new(
                &self.input,
                CaseMatching::Smart,
                Normalization::Smart,
                AtomKind::Fuzzy,
            );
            let indexed: Vec<IndexedEntry> = self
                .all_entries
                .iter()
                .enumerate()
                .map(|(i, e)| IndexedEntry { index: i, entry: e })
                .collect();
            let matches = pattern.match_list(indexed, &mut matcher);
            self.filtered = matches
                .into_iter()
                .filter(|(ie, _)| !self.filter_recorded || self.all_entries[ie.index].has_recording)
                .map(|(ie, _)| ie.index)
                .collect();
        }
        if self.filtered.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
        self.summary_scroll = 0;
    }

    fn move_up(&mut self) {
        if let Some(i) = self.list_state.selected()
            && i > 0
        {
            self.list_state.select(Some(i - 1));
            self.summary_scroll = 0;
        }
    }

    fn move_down(&mut self) {
        if let Some(i) = self.list_state.selected()
            && i + 1 < self.filtered.len()
        {
            self.list_state.select(Some(i + 1));
            self.summary_scroll = 0;
        }
    }

    fn page_up(&mut self) {
        if let Some(i) = self.list_state.selected() {
            self.list_state
                .select(Some(i.saturating_sub(self.page_size)));
            self.summary_scroll = 0;
        }
    }

    fn page_down(&mut self) {
        if let Some(i) = self.list_state.selected() {
            let last = self.filtered.len().saturating_sub(1);
            self.list_state.select(Some((i + self.page_size).min(last)));
            self.summary_scroll = 0;
        }
    }

    fn selected_command(&self) -> Option<&str> {
        let i = self.list_state.selected()?;
        let &idx = self.filtered.get(i)?;
        self.all_entries.get(idx).map(|e| e.command.as_str())
    }

    fn selected_entry(&self) -> Option<&HistoryEntry> {
        let i = self.list_state.selected()?;
        let &idx = self.filtered.get(i)?;
        self.all_entries.get(idx)
    }
}

struct TuiGuard;

impl TuiGuard {
    fn new() -> anyhow::Result<Self> {
        crossterm::execute!(std::io::stderr(), EnterAlternateScreen)?;
        let guard = Self;
        terminal::enable_raw_mode()?;
        Ok(guard)
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stderr(), LeaveAlternateScreen);
    }
}

fn run_tui(
    entries: &[HistoryEntry],
    initial_query: Option<String>,
) -> anyhow::Result<Option<SearchAction>> {
    let _guard = TuiGuard::new()?;

    let backend = CrosstermBackend::new(std::io::stderr());
    let mut terminal = Terminal::new(backend)?;

    let mut state = SearchState::new(entries.to_vec());

    if let Some(query) = initial_query
        && !query.is_empty()
    {
        state.input = query;
        state.refilter();
    }

    // Drain any buffered input (e.g. leftover bytes from the Ctrl+R keypress).
    while event::poll(std::time::Duration::from_millis(INPUT_DRAIN_TIMEOUT_MS))? {
        let _ = event::read();
    }

    let result;

    loop {
        terminal.draw(|f| draw(f, &mut state))?;

        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => continue,
        };

        if let Event::Key(KeyEvent {
            kind: KeyEventKind::Press,
            code,
            modifiers,
            ..
        }) = ev
        {
            let has_summary = state
                .selected_entry()
                .and_then(|e| e.summary.as_ref())
                .is_some_and(|s| !s.is_empty());

            // If summary disappeared while focused, return to input
            if state.focus == FocusedPanel::Summary && !has_summary {
                state.focus = FocusedPanel::Input;
            }

            match (code, modifiers) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    result = None;
                    break;
                }
                (KeyCode::Enter, _) => {
                    result = state
                        .selected_command()
                        .map(|s| SearchAction::Run(s.to_string()));
                    break;
                }
                (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
                    if let Some(entry) = state.selected_entry()
                        && entry.has_recording
                    {
                        result = Some(SearchAction::Replay(entry.id));
                        break;
                    }
                }
                (KeyCode::Tab | KeyCode::BackTab, _) => {
                    if has_summary {
                        state.focus = match state.focus {
                            FocusedPanel::Input => FocusedPanel::Summary,
                            FocusedPanel::Summary => FocusedPanel::Input,
                        };
                    }
                }
                _ if state.focus == FocusedPanel::Summary => match (code, modifiers) {
                    (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                        state.summary_scroll = state.summary_scroll.saturating_sub(1);
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                        state.summary_scroll = state.summary_scroll.saturating_add(1);
                    }
                    (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        state.focus = FocusedPanel::Input;
                        state.input.push(c);
                        state.refilter();
                    }
                    (KeyCode::Backspace, _) => {
                        state.focus = FocusedPanel::Input;
                        state.input.pop();
                        state.refilter();
                    }
                    _ => {}
                },
                (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                    state.move_up();
                }
                (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                    state.move_down();
                }
                (KeyCode::PageUp, _) => {
                    state.page_up();
                }
                (KeyCode::PageDown, _) => {
                    state.page_down();
                }
                (KeyCode::Backspace, _) => {
                    state.input.pop();
                    state.refilter();
                }
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    state.input.clear();
                    state.refilter();
                }
                (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                    state.filter_recorded = !state.filter_recorded;
                    state.refilter();
                }
                (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                    let trimmed = state.input.trim_end();
                    if let Some(pos) = trimmed.rfind(' ') {
                        state.input.truncate(pos + 1);
                    } else {
                        state.input.clear();
                    }
                    state.refilter();
                }
                (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    state.input.push(c);
                    state.refilter();
                }
                _ => {}
            }
        }
    }

    Ok(result)
}

/// Minimum width, in display cells, the command column keeps before any
/// optional column is allowed in.
const CMD_MIN_WIDTH: usize = 24;
/// Upper bound on the cwd column so a deep path can't crowd out the command.
const CWD_MAX_WIDTH: usize = 32;
/// Smallest cwd worth showing at all.
const CWD_MIN_WIDTH: usize = 10;
/// Widest string `format_time_ago` can produce in practice ("123mo ago").
const TIME_MAX_WIDTH: usize = 9;

/// Cells the id column keeps for a short id. It grows with the history's
/// widest id, so this is only the floor.
const ID_MIN_WIDTH: usize = 6;
const ID_GAP: usize = 1;
const STATUS_COST: usize = 5; // 4-wide status + 1 gap
const REC_COST: usize = 2; // 1-wide dot + 1 gap
const CWD_GAP: usize = 2;
const TIME_GAP: usize = 2;

/// Which columns a result row can afford at a given width. Computed once per
/// draw so every row drops the same columns.
#[derive(Debug, PartialEq)]
struct RowLayout {
    /// Cells the id may use, excluding its gap. 0 means hidden.
    id_width: usize,
    show_status: bool,
    show_rec: bool,
    show_time: bool,
    /// Cells the path may use, excluding its gap. 0 means hidden.
    cwd_width: usize,
    /// Cells left for the command once every admitted column is reserved. The
    /// same on every row, so the columns after it line up.
    cmd_width: usize,
}

/// Admit columns in order of decreasing importance, each only if the command
/// still keeps `CMD_MIN_WIDTH` afterwards. Admission is a strict prefix: once
/// one column doesn't fit, every less important one is dropped too, so the
/// drop order as the window narrows is id, then cwd, then time, then status,
/// then rec. Without that, rejecting the cwd would free enough budget for the
/// id to reappear in a *narrower* window.
///
/// Every admitted column is charged its full width, and what's left over is the
/// command's. Charging what a row's text happens to need instead would hand the
/// slack to the command and start each column wherever the row above ended.
fn row_layout(inner_width: usize, id_width: usize) -> RowLayout {
    /// Charge `cost` to the budget if the command still keeps its floor.
    fn admit(budget: &mut usize, cost: usize) -> bool {
        if budget.saturating_sub(cost) >= CMD_MIN_WIDTH {
            *budget -= cost;
            true
        } else {
            false
        }
    }

    let mut budget = inner_width;
    let show_rec = admit(&mut budget, REC_COST);
    let show_status = show_rec && admit(&mut budget, STATUS_COST);
    let show_time = show_status && admit(&mut budget, TIME_GAP + TIME_MAX_WIDTH);

    // The cwd has no natural bound, so it gets a clamp rather than a flat cost.
    let cwd_avail = if show_time {
        budget.saturating_sub(CMD_MIN_WIDTH + CWD_GAP)
    } else {
        0
    };
    let cwd_width = if cwd_avail >= CWD_MIN_WIDTH {
        let w = cwd_avail.min(CWD_MAX_WIDTH);
        budget -= CWD_GAP + w;
        w
    } else {
        0
    };

    let show_id = cwd_width > 0 && admit(&mut budget, ID_GAP + id_width);

    RowLayout {
        id_width: if show_id { id_width } else { 0 },
        show_status,
        show_rec,
        show_time,
        cwd_width,
        // Whatever the admitted columns didn't claim.
        cmd_width: budget,
    }
}

/// Height of the summary pane, including its borders.
const SUMMARY_HEIGHT: usize = 8;
/// Most wrapped lines the full-command pane will grow to. A command that needs
/// more is clipped, and the pane's title says so.
const FULL_CMD_MAX_ROWS: usize = 4;
/// Rows the results list keeps before the full-command pane is suppressed.
const LIST_MIN_HEIGHT: usize = 5; // 2 borders + 3 visible rows

/// Build the full-command pane and the exact height it needs, borders included.
///
/// The height has to come from the widget that renders: the pane word-wraps, so
/// a character-wrap estimate undercounts it (every word break wastes up to a
/// line's worth of cells) and `Paragraph` then clips the overflow with nothing
/// to show for it. `line_count` runs the same wrapper the render does. It is a
/// ratatui API marked unstable (`unstable-rendered-line-info` in Cargo.toml), so
/// a version bump could rename it -- that would be a build error, not a silent
/// return of the bug.
fn full_cmd_pane(cmd: &str, area_width: u16) -> (Paragraph<'_>, usize) {
    let wrap_width = area_width.saturating_sub(2).max(1); // borders
    // Measure before attaching the block: the title depends on the count, and
    // `line_count` would otherwise add the block's own two rows to it.
    let body = Paragraph::new(Span::raw(cmd)).wrap(Wrap { trim: false });
    let rows = body.line_count(wrap_width);
    let shown = rows.clamp(1, FULL_CMD_MAX_ROWS);
    // Ratatui doesn't expose its wrap points, so a clipped command is marked in
    // the title rather than at the cut -- but it is marked. This pane is the one
    // place that promises the whole command.
    let title = if rows > shown {
        " full command \u{2026} "
    } else {
        " full command "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    (body.block(block), shown + 2)
}

fn draw(f: &mut ratatui::Frame, state: &mut SearchState) {
    let summary_text = state
        .selected_entry()
        .and_then(|e| e.summary.clone())
        .unwrap_or_default();
    let has_summary = !summary_text.is_empty();

    // A vertical split preserves width, so the row layout can be settled before
    // the panes are sized -- adding a pane only costs the list height.
    let area = f.area();
    let inner_width = area.width.saturating_sub(2 + 2) as usize; // borders + "> "
    let layout = row_layout(inner_width, state.id_width);

    // Reveal the selected command in full, but only while it is actually cut.
    let full_command = state
        .selected_entry()
        .and_then(|e| (e.command.width() > layout.cmd_width).then(|| e.command.clone()));
    let full_cmd = full_command
        .as_ref()
        .map(|cmd| full_cmd_pane(cmd, area.width));
    let full_cmd_height = full_cmd.as_ref().map_or(0, |(_, h)| *h);
    // Don't starve the list in a short terminal.
    let summary_height = if has_summary { SUMMARY_HEIGHT } else { 0 };
    let show_full_cmd = full_cmd.is_some()
        && area.height as usize >= 3 + summary_height + full_cmd_height + LIST_MIN_HEIGHT;

    let mut constraints = vec![Constraint::Length(3), Constraint::Min(1)];
    if show_full_cmd {
        constraints.push(Constraint::Length(full_cmd_height as u16));
    }
    if has_summary {
        constraints.push(Constraint::Length(SUMMARY_HEIGHT as u16));
    }
    let chunks = Layout::vertical(constraints).split(area);
    let summary_idx = 2 + usize::from(show_full_cmd);

    // Input field
    let title = if state.filter_recorded {
        Line::from(vec![
            Span::raw(" search [rec "),
            Span::styled("\u{25CF}", Style::default().fg(Color::Magenta)),
            Span::raw("] "),
        ])
    } else {
        Line::from(" search ")
    };
    let input = Paragraph::new(Span::raw(&state.input))
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(input, chunks[0]);

    // Place cursor after input text, clamped to visible area (only when input is focused)
    if state.focus == FocusedPanel::Input {
        let input_width = state.input.width() as u16;
        let max_x = chunks[0].x + chunks[0].width.saturating_sub(2); // stay inside border
        let cursor_x = (chunks[0].x + 1 + input_width).min(max_x);
        f.set_cursor_position((cursor_x, chunks[0].y + 1));
    }

    // Update page size based on visible list height (minus borders)
    state.page_size = chunks[1].height.saturating_sub(2) as usize;

    // Results list
    let items: Vec<ListItem> = state
        .filtered
        .iter()
        .map(|&idx| {
            let entry = &state.all_entries[idx];

            // Layout: id + gap(1) + status + gap(1) + rec + gap(1) + command
            //         + gap(2) + cwd + gap(2) + time, with each optional column
            //         and its gap present only if `layout` admitted it. Every
            //         cell is padded to the width `layout` reserved for it, so
            //         the columns hold the same x on every row.
            let mut spans: Vec<Span> = Vec::new();

            if layout.id_width > 0 {
                spans.push(Span::styled(
                    format!("{:>width$}", entry.id, width = layout.id_width),
                    Style::default().fg(Color::DarkGray),
                ));
                spans.push(Span::raw(" "));
            }
            if layout.show_status {
                spans.push(if entry.exit_code == 0 {
                    Span::styled("  ok", Style::default().fg(Color::Green))
                } else {
                    Span::styled(
                        format!("{:>4}", entry.exit_code),
                        Style::default().fg(Color::Red),
                    )
                });
                spans.push(Span::raw(" "));
            }
            if layout.show_rec {
                spans.push(if entry.has_recording {
                    Span::styled("\u{25CF}", Style::default().fg(Color::Magenta))
                } else {
                    Span::raw(" ")
                });
                spans.push(Span::raw(" "));
            }

            let cmd_w = entry.command.width();
            if layout.cmd_width == 0 {
                // Degenerate width; a marker alone would overflow the row.
            } else if cmd_w > layout.cmd_width {
                // Reserve the last cell for the marker. `truncate_to_width` pads
                // to exactly its argument, so head + marker is exactly
                // `cmd_width` and the next column starts where it should.
                spans.push(Span::raw(truncate_to_width(
                    &entry.command,
                    layout.cmd_width - 1,
                )));
                spans.push(Span::styled(
                    "\u{2026}",
                    Style::default().fg(Color::DarkGray),
                ));
            } else {
                let pad = layout.cmd_width - cmd_w;
                spans.push(Span::raw(format!("{}{}", entry.command, " ".repeat(pad))));
            }

            if layout.cwd_width > 0 {
                // `truncate_to_width` pads to exactly its argument, so a short
                // path still ends where the time column begins.
                let cwd = elide_start_to_width(&shorten_path(&entry.cwd), layout.cwd_width);
                spans.push(Span::styled(
                    format!("  {}", truncate_to_width(&cwd, layout.cwd_width)),
                    Style::default().fg(Color::Cyan),
                ));
            }
            if layout.show_time {
                // Right-aligned so the row ends flush. `format_time_ago` is
                // ASCII, so padding by chars pads by cells.
                let time = elide_start_to_width(&format_time_ago(entry.start), TIME_MAX_WIDTH);
                spans.push(Span::styled(
                    format!("  {time:>width$}", width = TIME_MAX_WIDTH),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" history "))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    f.render_stateful_widget(list, chunks[1], &mut state.list_state);

    // Full-command pane: reveals the selected command when the row cut it off.
    if show_full_cmd
        && let Some((widget, _)) = full_cmd
    {
        f.render_widget(widget, chunks[2]);
    }

    // Summary pane
    if has_summary {
        let border_style = if state.focus == FocusedPanel::Summary {
            Style::default().fg(Color::Blue)
        } else {
            Style::default()
        };
        let summary_widget = Paragraph::new(Span::raw(&summary_text))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" summary ")
                    .border_style(border_style),
            )
            .wrap(Wrap { trim: false })
            .scroll((state.summary_scroll, 0));
        f.render_widget(summary_widget, chunks[summary_idx]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cells the optional columns claim, i.e. the worst case the command floor
    /// is computed against.
    fn reserved(l: &RowLayout) -> usize {
        let mut n = 0;
        if l.id_width > 0 {
            n += ID_GAP + l.id_width;
        }
        if l.show_status {
            n += STATUS_COST;
        }
        if l.show_rec {
            n += REC_COST;
        }
        if l.show_time {
            n += TIME_GAP + TIME_MAX_WIDTH;
        }
        if l.cwd_width > 0 {
            n += CWD_GAP + l.cwd_width;
        }
        n
    }

    #[test]
    fn wide_window_shows_every_column() {
        let l = row_layout(100, ID_MIN_WIDTH);
        assert!(l.id_width > 0 && l.show_status && l.show_rec && l.show_time);
        assert_eq!(l.cwd_width, CWD_MAX_WIDTH);
    }

    #[test]
    fn ladder_drops_id_then_cwd_then_time_then_status() {
        let l = row_layout(64, ID_MIN_WIDTH);
        assert!(l.id_width == 0, "id is the first to go");
        assert!(l.cwd_width > 0 && l.show_time && l.show_status && l.show_rec);

        let l = row_layout(50, ID_MIN_WIDTH);
        assert!(l.id_width == 0 && l.cwd_width == 0, "cwd goes next");
        assert!(l.show_time && l.show_status && l.show_rec);

        let l = row_layout(40, ID_MIN_WIDTH);
        assert!(!l.show_time, "time goes next");
        assert!(l.show_status && l.show_rec);

        let l = row_layout(30, ID_MIN_WIDTH);
        assert!(!l.show_status, "status goes next");
        assert!(l.show_rec, "the recording dot is the last to go");

        let l = row_layout(20, ID_MIN_WIDTH);
        assert_eq!(reserved(&l), 0, "the command gets the whole row");
    }

    #[test]
    fn command_is_never_squeezed_out() {
        // Sweep a wide id too: the column grows with the history's largest id.
        for id_width in [ID_MIN_WIDTH, 8] {
            for inner_width in 0..=200usize {
                let l = row_layout(inner_width, id_width);
                assert!(
                    reserved(&l) <= inner_width,
                    "columns overflow the row at width {inner_width}"
                );
                if reserved(&l) > 0 {
                    assert!(
                        l.cmd_width >= CMD_MIN_WIDTH,
                        "command got {} cells at width {inner_width}",
                        l.cmd_width
                    );
                } else {
                    assert_eq!(l.cmd_width, inner_width);
                }
            }
        }
    }

    #[test]
    fn the_command_gets_every_cell_the_columns_did_not_reserve() {
        // The command's width is a property of the layout, not of the row's
        // text: that is what keeps the columns after it in a straight line.
        for id_width in [ID_MIN_WIDTH, 8] {
            for inner_width in 0..=200usize {
                let l = row_layout(inner_width, id_width);
                assert_eq!(
                    l.cmd_width,
                    inner_width - reserved(&l),
                    "at width {inner_width} with a {id_width}-wide id"
                );
            }
        }
    }

    fn entry(id: i64, command: &str, cwd: &str) -> HistoryEntry {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        HistoryEntry {
            id,
            command: command.to_string(),
            exit_code: 0,
            start: now - 3600.0,
            cwd: cwd.to_string(),
            has_recording: true,
            summary: None,
        }
    }

    /// Render `draw` into an off-screen buffer and return its rows as strings.
    fn render(state: &mut SearchState, width: u16, height: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| draw(f, state)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_narrow_window_hands_the_dropped_columns_to_the_command() {
        let wide = row_layout(100, ID_MIN_WIDTH);
        let narrow = row_layout(40, ID_MIN_WIDTH);
        assert_eq!(narrow.cwd_width, 0);
        assert!(!narrow.show_time);
        assert_eq!(narrow.cmd_width, 40 - STATUS_COST - REC_COST);
        // The command keeps a bigger share of the row once the columns go.
        let share = |cmd: usize, inner: usize| cmd * 100 / inner;
        assert!(share(narrow.cmd_width, 40) > share(wide.cmd_width, 100));
    }

    #[test]
    fn columns_start_at_the_same_column_on_every_row() {
        let mut state = SearchState::new(vec![
            entry(1, "cargo test", "~/a"),
            entry(2, "ls", "~/some/deep/path/src"),
        ]);
        let rows = render(&mut state, 100, 12);
        let starts: Vec<usize> = rows
            .iter()
            .filter(|r| r.contains("cargo test") || r.contains("ls "))
            .map(|r| r.chars().position(|c| c == '~').unwrap())
            .collect();
        assert_eq!(starts.len(), 2, "expected both rows in {rows:?}");
        assert_eq!(
            starts[0], starts[1],
            "the cwd column starts in a different place on each row: {rows:?}"
        );
    }

    #[test]
    fn id_column_widens_for_large_ids() {
        // The id is a rowid; a six-cell column overflows the row past 999,999.
        let mut state = SearchState::new(vec![entry(12_345_678, "cargo test", "~/dejiny")]);
        let rows = render(&mut state, 100, 12);
        let row = rows.iter().find(|r| r.contains("cargo test")).unwrap();

        assert!(row.contains("12345678"), "id was cut: {row:?}");
        assert!(row.contains("1h ago"), "the time column was pushed off: {row:?}");
        assert!(row.ends_with('\u{2502}'), "row overflowed its block: {row:?}");
        assert_eq!(row.chars().count(), 100);
    }

    #[test]
    fn truncated_command_is_marked_and_row_stays_inside_the_border() {
        let long = "ssh deploy@prod-web-01 'systemctl restart the-service --now'";
        let mut state = SearchState::new(vec![entry(1, long, "~/dejiny")]);
        let rows = render(&mut state, 60, 12);
        let row = rows.iter().find(|r| r.contains("ssh deploy")).unwrap();

        assert!(row.contains('\u{2026}'), "expected a truncation marker in {row:?}");
        assert!(!row.contains(long), "the command should have been cut");
        // The border is still the last cell: nothing overflowed.
        assert!(row.ends_with('\u{2502}'), "row overflowed its block: {row:?}");
        assert_eq!(row.chars().count(), 60);
    }

    #[test]
    fn command_that_fits_gets_no_marker_and_no_pane() {
        let mut state = SearchState::new(vec![entry(1, "cargo test", "~/dejiny")]);
        let rows = render(&mut state, 100, 12);
        let row = rows.iter().find(|r| r.contains("cargo test")).unwrap();
        assert!(!row.contains('\u{2026}'), "unexpected marker in {row:?}");
        assert!(
            !rows.iter().any(|r| r.contains("full command")),
            "pane should be absent when the command fits"
        );
    }

    #[test]
    fn full_command_pane_shows_the_whole_command() {
        let long = "ssh deploy@prod-web-01 'systemctl restart the-service --now'";
        let mut state = SearchState::new(vec![entry(1, long, "~/dejiny")]);
        let rows = render(&mut state, 60, 16);

        assert!(
            rows.iter().any(|r| r.contains("full command")),
            "pane should open for a truncated selection"
        );
        // The pane wraps, so read its rows back without borders. Compare on
        // collapsed whitespace so the assertion doesn't depend on where the
        // wrap happens to fall.
        let start = rows.iter().position(|r| r.contains("full command")).unwrap();
        let pane: String = rows[start + 1..]
            .iter()
            .take_while(|r| !r.contains('\u{2514}'))
            .map(|r| r.trim_matches('\u{2502}').trim())
            .collect::<Vec<_>>()
            .join(" ");
        let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            collapse(&pane),
            collapse(long),
            "pane should reveal the whole command"
        );
    }

    #[test]
    fn full_command_pane_is_suppressed_in_a_short_terminal() {
        let long = "ssh deploy@prod-web-01 'systemctl restart the-service --now'";
        let mut state = SearchState::new(vec![entry(1, long, "~/dejiny")]);
        let rows = render(&mut state, 60, 9);
        assert!(
            !rows.iter().any(|r| r.contains("full command")),
            "pane must not starve the list in a short window"
        );
    }

    #[test]
    fn pane_follows_the_selection() {
        let long = "ssh deploy@prod-web-01 'systemctl restart the-service --now'";
        let mut state = SearchState::new(vec![
            entry(1, long, "~/dejiny"),
            entry(2, "ls", "~/dejiny"),
        ]);
        assert!(
            render(&mut state, 60, 16).iter().any(|r| r.contains("full command")),
            "pane open on the truncated row"
        );
        state.move_down();
        assert!(
            !render(&mut state, 60, 16).iter().any(|r| r.contains("full command")),
            "pane closes once a row that fits is selected"
        );
    }

    #[test]
    fn pane_reveals_the_whole_command_at_every_width() {
        // Sizing the pane by a character-wrap estimate leaves it too short: the
        // pane wraps on words, and every break wastes cells the estimate never
        // charged for. `Paragraph` then clips the tail with nothing to show it.
        let long = "cargo build --release --target x86_64-unknown-linux-musl --features full,extra";
        let mut state = SearchState::new(vec![entry(1, long, "~/dejiny")]);
        let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");

        for width in 40..=120u16 {
            let rows = render(&mut state, width, 20);
            let start = rows
                .iter()
                .position(|r| r.contains("full command"))
                .unwrap_or_else(|| panic!("pane should be open at width {width}: {rows:?}"));
            assert!(
                !rows[start].contains('\u{2026}'),
                "this command fits in {FULL_CMD_MAX_ROWS} rows at width {width}"
            );
            let pane = rows[start + 1..]
                .iter()
                .take_while(|r| !r.contains('\u{2514}'))
                .map(|r| r.trim_matches('\u{2502}'))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(
                collapse(&pane),
                collapse(long),
                "pane clipped the command at width {width}"
            );
        }
    }

    #[test]
    fn pane_marks_a_command_it_had_to_clip() {
        let long = "docker run --rm -it -v /Users/me/projects/dejiny:/work -w /work \
                    -e RUST_LOG=debug -e CARGO_TERM_COLOR=always rust:1.83-bookworm \
                    cargo test --all-features -- --nocapture --test-threads 1";
        let mut state = SearchState::new(vec![entry(1, long, "~/dejiny")]);
        let rows = render(&mut state, 40, 24);
        let start = rows.iter().position(|r| r.contains("full command")).unwrap();

        assert!(
            rows[start].contains('\u{2026}'),
            "a pane that clipped the command must say so: {:?}",
            rows[start]
        );
        let height = rows[start..]
            .iter()
            .position(|r| r.contains('\u{2514}'))
            .expect("pane should have a bottom border")
            + 1;
        assert_eq!(height, FULL_CMD_MAX_ROWS + 2, "pane grew past its cap");
    }

    #[test]
    fn wider_window_never_shows_less() {
        // The ladder is monotonic: growing the window never drops a column.
        for inner_width in 0..200usize {
            let a = row_layout(inner_width, ID_MIN_WIDTH);
            let b = row_layout(inner_width + 1, ID_MIN_WIDTH);
            assert!(a.id_width == 0 || b.id_width > 0);
            assert!(!a.show_status || b.show_status);
            assert!(!a.show_rec || b.show_rec);
            assert!(!a.show_time || b.show_time);
            assert!(b.cwd_width >= a.cwd_width);
        }
    }
}
