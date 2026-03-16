use crate::db::{HistoryEntry, history_path, load_commands, log_error};
use crate::util::{format_time_ago, shorten_path, truncate_to_width};
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
}

impl SearchState {
    fn new(entries: Vec<HistoryEntry>) -> Self {
        let filtered: Vec<usize> = (0..entries.len()).collect();
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

fn run_tui(entries: &[HistoryEntry], initial_query: Option<String>) -> anyhow::Result<Option<SearchAction>> {
    let _guard = TuiGuard::new()?;

    let backend = CrosstermBackend::new(std::io::stderr());
    let mut terminal = Terminal::new(backend)?;

    let mut state = SearchState::new(entries.to_vec());

    if let Some(query) = initial_query {
        if !query.is_empty() {
            state.input = query;
            state.refilter();
        }
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

fn draw(f: &mut ratatui::Frame, state: &mut SearchState) {
    let summary_text = state
        .selected_entry()
        .and_then(|e| e.summary.clone())
        .unwrap_or_default();
    let has_summary = !summary_text.is_empty();

    let chunks = if has_summary {
        Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(8),
        ])
        .split(f.area())
    } else {
        Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(f.area())
    };

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

    // Available width inside the list block (minus borders and highlight symbol)
    let inner_width = chunks[1].width.saturating_sub(2 + 2) as usize; // borders + "> "

    // Results list
    let items: Vec<ListItem> = state
        .filtered
        .iter()
        .map(|&idx| {
            let entry = &state.all_entries[idx];
            let status = if entry.exit_code == 0 {
                Span::styled("  ok", Style::default().fg(Color::Green))
            } else {
                Span::styled(
                    format!("{:>4}", entry.exit_code),
                    Style::default().fg(Color::Red),
                )
            };

            let time = format_time_ago(entry.start);
            let cwd = shorten_path(&entry.cwd);

            let rec_indicator = if entry.has_recording {
                Span::styled("\u{25CF}", Style::default().fg(Color::Magenta))
            } else {
                Span::raw(" ")
            };

            let id_str = format!("{:>6}", entry.id);

            // Layout: id(6) + gap(1) + status(4) + gap(1) + rec(1) + gap(1) + command + gap(2) + cwd + gap(2) + time
            let fixed = 6 + 1 + 4 + 1 + 1 + 1 + 2 + cwd.width() + 2 + time.width();
            let cmd_width = inner_width.saturating_sub(fixed);
            let cmd_display = {
                let cmd_w = entry.command.width();
                if cmd_w > cmd_width {
                    truncate_to_width(&entry.command, cmd_width)
                } else {
                    let pad = cmd_width - cmd_w;
                    format!("{}{}", entry.command, " ".repeat(pad))
                }
            };

            ListItem::new(Line::from(vec![
                Span::styled(id_str, Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                status,
                Span::raw(" "),
                rec_indicator,
                Span::raw(" "),
                Span::raw(cmd_display),
                Span::styled(format!("  {cwd}  "), Style::default().fg(Color::Cyan)),
                Span::styled(time, Style::default().fg(Color::DarkGray)),
            ]))
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
        f.render_widget(summary_widget, chunks[2]);
    }
}
