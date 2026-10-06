//! Ratatui browser: list both harnesses side by side, preview a transcript, and
//! convert the selected sessions.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};

use crate::transcript::{self, Speaker};
use crate::{convert_one, dsh, paths, pi, shorten, Located, Side};

const ACCENT: Color = Color::Rgb(0x5f, 0xd7, 0xff);
const DIM: Color = Color::Rgb(0x80, 0x80, 0x80);
const OK: Color = Color::Rgb(0x87, 0xd7, 0x87);
const WARN: Color = Color::Rgb(0xff, 0xaf, 0x5f);
const BAD: Color = Color::Rgb(0xff, 0x5f, 0x5f);

/// One session as the browser displays it.
struct Row {
    id: String,
    title: String,
    cwd: String,
    when: String,
    detail: String,
    messages: usize,
    bucket: String,
    size: u64,
    path: PathBuf,
    side: Side,
}

/// Which full-screen panel is covering the browser.
#[derive(PartialEq, Eq)]
enum Overlay {
    /// The browser itself.
    None,
    /// A rendered transcript.
    Preview,
    /// The key map.
    Help,
}

/// Browser state.
struct App {
    side: Side,
    dsh_rows: Vec<Row>,
    pi_rows: Vec<Row>,
    view: Vec<usize>,
    cursor: usize,
    selected_dsh: HashSet<PathBuf>,
    selected_pi: HashSet<PathBuf>,
    filter: String,
    editing_filter: bool,
    overlay: Overlay,
    preview: Vec<transcript::Block>,
    preview_title: String,
    preview_stats: String,
    preview_scroll: u16,
    log: Vec<(String, Color)>,
    dry_run: bool,
    force: bool,
    include_empty: bool,
    quit: bool,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            side: Side::Dsh,
            dsh_rows: Vec::new(),
            pi_rows: Vec::new(),
            view: Vec::new(),
            cursor: 0,
            selected_dsh: HashSet::new(),
            selected_pi: HashSet::new(),
            filter: String::new(),
            editing_filter: false,
            overlay: Overlay::None,
            preview: Vec::new(),
            preview_title: String::new(),
            preview_stats: String::new(),
            preview_scroll: 0,
            log: Vec::new(),
            dry_run: false,
            force: false,
            include_empty: false,
            quit: false,
        };
        app.refresh();
        app
    }

    /// Re-reads both session roots.
    fn refresh(&mut self) {
        self.dsh_rows = dsh::discover(&paths::dsh_root())
            .into_iter()
            .map(|found| Row {
                id: found.id,
                title: found.title.unwrap_or_default(),
                cwd: found.cwd,
                when: crate::convert::iso(found.created_at),
                detail: format!(
                    "v{}{} · {} events",
                    found.generation,
                    if found.other_generations.is_empty() {
                        String::new()
                    } else {
                        format!(" (+{})", found.other_generations.len())
                    },
                    found.events
                ),
                messages: found.messages,
                bucket: found.bucket,
                size: found.size,
                path: found.path,
                side: Side::Dsh,
            })
            .collect();
        self.pi_rows = pi::discover(&paths::pi_root())
            .into_iter()
            .map(|found| Row {
                id: found.id,
                title: found.title,
                cwd: found.cwd,
                when: found.timestamp,
                detail: format!("{} entries", found.entries),
                messages: found.messages,
                bucket: found.bucket,
                size: found.size,
                path: found.path,
                side: Side::Pi,
            })
            .collect();
        self.reindex();
    }

    /// Current side's rows.
    fn rows(&self) -> &[Row] {
        match self.side {
            Side::Dsh => &self.dsh_rows,
            Side::Pi => &self.pi_rows,
        }
    }

    /// Selection set for the current side.
    fn selection(&mut self) -> &mut HashSet<PathBuf> {
        match self.side {
            Side::Dsh => &mut self.selected_dsh,
            Side::Pi => &mut self.selected_pi,
        }
    }

    /// True when `path` is selected on the current side.
    fn is_selected(&self, path: &PathBuf) -> bool {
        match self.side {
            Side::Dsh => self.selected_dsh.contains(path),
            Side::Pi => self.selected_pi.contains(path),
        }
    }

    /// Recomputes the filtered view and clamps the cursor.
    fn reindex(&mut self) {
        let needle = self.filter.to_lowercase();
        self.view = self
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| self.include_empty || row.messages > 0)
            .filter(|(_, row)| {
                needle.is_empty()
                    || row.id.to_lowercase().contains(&needle)
                    || row.title.to_lowercase().contains(&needle)
                    || row.cwd.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();
        if self.cursor >= self.view.len() {
            self.cursor = self.view.len().saturating_sub(1);
        }
    }

    /// Row under the cursor.
    fn current(&self) -> Option<&Row> {
        self.view.get(self.cursor).map(|index| &self.rows()[*index])
    }

    /// Appends one line to the activity log.
    fn say(&mut self, text: impl Into<String>, color: Color) {
        self.log.push((text.into(), color));
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    /// Loads the transcript of the row under the cursor.
    fn open_preview(&mut self) {
        let Some(row) = self.current() else { return };
        let path = row.path.clone();
        let side = row.side;
        let heading = format!("{} · {}", row.id, row.cwd);
        let loaded = match side {
            Side::Dsh => dsh::read_log(&path).map(|log| (transcript::from_dsh(&log), log.histogram())),
            Side::Pi => pi::read_file(&path).map(|file| (transcript::from_pi(&file), file.histogram())),
        };
        match loaded {
            Ok((blocks, histogram)) => {
                self.preview = blocks;
                self.preview_title = heading;
                self.preview_stats = summarize(&histogram);
                self.preview_scroll = 0;
                self.overlay = Overlay::Preview;
            }
            Err(error) => self.say(format!("preview failed: {error:#}"), BAD),
        }
    }

    /// Converts the selected rows, or the cursor row when nothing is selected.
    fn convert_selection(&mut self) {
        let side = self.side;
        let selected: Vec<PathBuf> = match side {
            Side::Dsh => self.selected_dsh.iter().cloned().collect(),
            Side::Pi => self.selected_pi.iter().cloned().collect(),
        };
        let mut targets: Vec<(PathBuf, String, String)> = Vec::new();
        if selected.is_empty() {
            if let Some(row) = self.current() {
                targets.push((row.path.clone(), row.id.clone(), row.cwd.clone()));
            }
        } else {
            for path in selected {
                if let Some(row) = self.rows().iter().find(|row| row.path == path) {
                    targets.push((row.path.clone(), row.id.clone(), row.cwd.clone()));
                }
            }
        }
        if targets.is_empty() {
            self.say("nothing to convert", WARN);
            return;
        }

        let dry_run = self.dry_run;
        let force = self.force;
        let include_empty = self.include_empty;
        let mut converted = 0usize;
        let mut failed = 0usize;
        for (path, id, cwd) in targets {
            let located = Located {
                side,
                path,
                id: id.clone(),
                cwd,
            };
            match convert_one(&located, None, dry_run, force, include_empty) {
                Ok(outcome) => {
                    if outcome.wrote() {
                        converted += 1;
                    }
                    let color = if !outcome.wrote() {
                        DIM
                    } else if dry_run {
                        WARN
                    } else {
                        OK
                    };
                    self.say(outcome.to_string(), color);
                }
                Err(error) => {
                    failed += 1;
                    self.say(format!("{id}: {error:#}"), BAD);
                }
            }
        }
        self.say(
            format!("{converted} converted, {failed} failed"),
            if failed > 0 { BAD } else { OK },
        );
        if !dry_run && converted > 0 {
            self.refresh();
        }
    }

    /// Writes the cursor row as Markdown next to the tool's working directory.
    fn export_markdown(&mut self) {
        let Some(row) = self.current() else { return };
        let (path, side, id, cwd, title) = (
            row.path.clone(),
            row.side,
            row.id.clone(),
            row.cwd.clone(),
            row.title.clone(),
        );
        let blocks = match side {
            Side::Dsh => dsh::read_log(&path).map(|log| transcript::from_dsh(&log)),
            Side::Pi => pi::read_file(&path).map(|file| transcript::from_pi(&file)),
        };
        let blocks = match blocks {
            Ok(blocks) => blocks,
            Err(error) => {
                self.say(format!("export failed: {error:#}"), BAD);
                return;
            }
        };
        let out_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("exports");
        if let Err(error) = std::fs::create_dir_all(&out_dir) {
            self.say(format!("export failed: {error}"), BAD);
            return;
        }
        let target = out_dir.join(format!("{id}.md"));
        let rendered = transcript::to_markdown(&title, &cwd, &blocks);
        match std::fs::write(&target, rendered) {
            Ok(()) => self.say(format!("wrote {}", target.display()), OK),
            Err(error) => self.say(format!("export failed: {error}"), BAD),
        }
    }
}

/// Runs the browser until the user quits.
pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut app = App::new();
    while !app.quit {
        terminal.draw(|frame| draw(frame, &mut app))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if app.editing_filter {
            match key.code {
                KeyCode::Esc => {
                    app.filter.clear();
                    app.editing_filter = false;
                    app.reindex();
                }
                KeyCode::Enter => app.editing_filter = false,
                KeyCode::Backspace => {
                    app.filter.pop();
                    app.reindex();
                }
                KeyCode::Char(c) => {
                    app.filter.push(c);
                    app.reindex();
                }
                _ => {}
            }
            continue;
        }

        match app.overlay {
            Overlay::Preview => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => app.overlay = Overlay::None,
                KeyCode::Char('j') | KeyCode::Down => app.preview_scroll = app.preview_scroll.saturating_add(1),
                KeyCode::Char('k') | KeyCode::Up => app.preview_scroll = app.preview_scroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    app.preview_scroll = app.preview_scroll.saturating_add(20)
                }
                KeyCode::PageUp => app.preview_scroll = app.preview_scroll.saturating_sub(20),
                KeyCode::Char('g') => app.preview_scroll = 0,
                _ => {}
            },
            Overlay::Help => app.overlay = Overlay::None,
            Overlay::None => match key.code {
                KeyCode::Char('q') => app.quit = true,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.quit = true
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    app.side = match app.side {
                        Side::Dsh => Side::Pi,
                        Side::Pi => Side::Dsh,
                    };
                    app.cursor = 0;
                    app.reindex();
                }
                KeyCode::Char('1') => {
                    app.side = Side::Dsh;
                    app.cursor = 0;
                    app.reindex();
                }
                KeyCode::Char('2') => {
                    app.side = Side::Pi;
                    app.cursor = 0;
                    app.reindex();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if app.cursor + 1 < app.view.len() {
                        app.cursor += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => app.cursor = app.cursor.saturating_sub(1),
                KeyCode::PageDown => {
                    app.cursor = (app.cursor + 10).min(app.view.len().saturating_sub(1))
                }
                KeyCode::PageUp => app.cursor = app.cursor.saturating_sub(10),
                KeyCode::Char('g') | KeyCode::Home => app.cursor = 0,
                KeyCode::Char('G') | KeyCode::End => {
                    app.cursor = app.view.len().saturating_sub(1)
                }
                KeyCode::Char(' ') => {
                    if let Some(path) = app.current().map(|row| row.path.clone()) {
                        let selection = app.selection();
                        if !selection.insert(path.clone()) {
                            selection.remove(&path);
                        }
                    }
                }
                KeyCode::Char('a') => {
                    let paths: Vec<PathBuf> = app
                        .view
                        .iter()
                        .map(|index| app.rows()[*index].path.clone())
                        .collect();
                    let selection = app.selection();
                    for path in paths {
                        selection.insert(path);
                    }
                }
                KeyCode::Char('A') => {
                    app.selection().clear();
                }
                KeyCode::Char('/') => {
                    app.editing_filter = true;
                    app.filter.clear();
                }
                KeyCode::Enter => app.open_preview(),
                KeyCode::Char('x') => app.convert_selection(),
                KeyCode::Char('m') => app.export_markdown(),
                KeyCode::Char('d') => {
                    app.dry_run = !app.dry_run;
                    let state = app.dry_run;
                    app.say(format!("dry-run {}", if state { "on" } else { "off" }), WARN);
                }
                KeyCode::Char('f') => {
                    app.force = !app.force;
                    let state = app.force;
                    app.say(format!("overwrite {}", if state { "on" } else { "off" }), WARN);
                }
                KeyCode::Char('e') => {
                    app.include_empty = !app.include_empty;
                    app.cursor = 0;
                    app.reindex();
                    let state = app.include_empty;
                    app.say(
                        format!(
                            "sessions without a conversation {}",
                            if state { "shown" } else { "hidden" }
                        ),
                        WARN,
                    );
                }
                KeyCode::Char('r') => {
                    app.refresh();
                    app.say("rescanned both roots", ACCENT);
                }
                KeyCode::Char('?') => app.overlay = Overlay::Help,
                _ => {}
            },
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(7),
            Constraint::Length(1),
        ])
        .split(area);

    draw_tabs(frame, chunks[0], app);

    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(chunks[1]);
    draw_list(frame, middle[0], app);
    draw_detail(frame, middle[1], app);

    draw_log(frame, chunks[2], app);
    draw_footer(frame, chunks[3], app);

    match app.overlay {
        Overlay::Preview => draw_preview(frame, area, app),
        Overlay::Help => draw_help(frame, area),
        Overlay::None => {}
    }
}

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let titles = vec![
        Line::from(format!(" DSH · {} ", app.dsh_rows.len())),
        Line::from(format!(" Pi · {} ", app.pi_rows.len())),
    ];
    let index = match app.side {
        Side::Dsh => 0,
        Side::Pi => 1,
    };
    let flags = format!(
        "{}{}{}",
        if app.dry_run { " [dry-run]" } else { "" },
        if app.force { " [overwrite]" } else { "" },
        if app.include_empty { " [+empty]" } else { "" }
    );
    let tabs = Tabs::new(titles)
        .select(index)
        .style(Style::default().fg(DIM))
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .divider("│")
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(Span::styled(
                    " dshpi — DSH ⇄ Pi sessions ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ))
                .title_top(Line::from(Span::styled(flags, Style::default().fg(WARN))).right_aligned()),
        );
    frame.render_widget(tabs, area);
}

fn draw_list(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .view
        .iter()
        .map(|index| {
            let row = &app.rows()[*index];
            let mark = if app.is_selected(&row.path) { "●" } else { "·" };
            let empty = row.messages == 0;
            let title = if empty {
                "(no messages)".to_string()
            } else if row.title.is_empty() {
                "(untitled)".to_string()
            } else {
                row.title.clone()
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{mark} "),
                    Style::default().fg(if app.is_selected(&row.path) { OK } else { DIM }),
                ),
                Span::styled(format!("{:<10} ", &row.when[..10.min(row.when.len())]), Style::default().fg(DIM)),
                Span::styled(
                    format!("{:<30} ", shorten(&title, 30)),
                    Style::default().fg(if empty { DIM } else { Color::White }),
                ),
                Span::styled(shorten(&row.cwd, 34), Style::default().fg(DIM)),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    if !app.view.is_empty() {
        state.select(Some(app.cursor));
    }
    let heading = if app.filter.is_empty() {
        format!(" sessions · {} ", app.view.len())
    } else {
        format!(" sessions · {} matching {:?} ", app.view.len(), app.filter)
    };
    let list = List::new(items)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(heading),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(0x26, 0x30, 0x38))
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let text = match app.current() {
        None => Text::from("no session selected"),
        Some(row) => {
            let destination = crate::destination(
                &Located {
                    side: row.side,
                    path: row.path.clone(),
                    id: row.id.clone(),
                    cwd: row.cwd.clone(),
                },
                None,
            );
            let mut lines = vec![
                field("id", &row.id),
                field("cwd", &row.cwd),
                field("when", &row.when),
                field("proj", &row.bucket),
                field("rows", &format!("{} · {} messages", row.detail, row.messages)),
                field("size", &human_size(row.size)),
            ];
            lines.extend([
                Line::from(""),
                Line::from(Span::styled("source", Style::default().fg(ACCENT))),
                Line::from(Span::styled(
                    row.path.display().to_string(),
                    Style::default().fg(DIM),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    match row.side {
                        Side::Dsh => "converts to Pi",
                        Side::Pi => "converts to DSH",
                    },
                    Style::default().fg(ACCENT),
                )),
            ]);
            match destination {
                Ok(path) => {
                    let exists = path.exists();
                    lines.push(Line::from(Span::styled(
                        path.display().to_string(),
                        Style::default().fg(if exists { WARN } else { DIM }),
                    )));
                    if exists {
                        lines.push(Line::from(Span::styled(
                            "destination exists — press f to allow overwrite",
                            Style::default().fg(WARN),
                        )));
                    }
                }
                Err(error) => lines.push(Line::from(Span::styled(
                    format!("{error:#}"),
                    Style::default().fg(BAD),
                ))),
            }
            Text::from(lines)
        }
    };
    let paragraph = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" detail "),
        );
    frame.render_widget(paragraph, area);
}

fn draw_log(frame: &mut Frame, area: Rect, app: &App) {
    let height = area.height.saturating_sub(2) as usize;
    let start = app.log.len().saturating_sub(height);
    let lines: Vec<Line> = app.log[start..]
        .iter()
        .map(|(text, color)| Line::from(Span::styled(text.clone(), Style::default().fg(*color))))
        .collect();
    let paragraph = Paragraph::new(Text::from(lines)).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" activity "),
    );
    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let text = if app.editing_filter {
        format!("filter: {}▏ (enter to apply, esc to clear)", app.filter)
    } else {
        "tab switch · space select · a all · x convert · enter preview · m markdown · d dry-run · f overwrite · e empty · r rescan · ? help · q quit".to_string()
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(DIM))),
        area,
    );
}

fn draw_preview(frame: &mut Frame, area: Rect, app: &App) {
    let area = centered(area, 90, 90);
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();
    for block in &app.preview {
        let (color, prefix) = match block.speaker {
            Speaker::User => (Color::Rgb(0x87, 0xaf, 0xff), "▌ user"),
            Speaker::Assistant => (Color::White, "▌ assistant"),
            Speaker::Thinking => (DIM, "▌ thinking"),
            Speaker::ToolCall => (ACCENT, "▌ →"),
            Speaker::ToolResult => (OK, "▌ ←"),
            Speaker::System => (WARN, "▌ system"),
            Speaker::Meta => (Color::Rgb(0xaf, 0x87, 0xff), "▌ meta"),
        };
        let heading = if block.heading.is_empty() {
            prefix.to_string()
        } else {
            format!("{prefix} {}", block.heading)
        };
        lines.push(Line::from(Span::styled(
            heading,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        for raw in block.body.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {raw}"),
                Style::default().fg(color),
            )));
        }
        lines.push(Line::from(""));
    }
    let total = lines.len();

    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((app.preview_scroll, 0))
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(format!(" {} ", app.preview_title))
                .title_top(
                    Line::from(Span::styled(
                        format!(" {} ", app.preview_stats),
                        Style::default().fg(DIM),
                    ))
                    .right_aligned(),
                )
                .title_bottom(
                    Line::from(Span::styled(
                        " j/k scroll · space page · g top · esc close ",
                        Style::default().fg(DIM),
                    ))
                    .centered(),
                ),
        );
    frame.render_widget(paragraph, area);

    let mut scrollbar_state = ScrollbarState::new(total).position(app.preview_scroll as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        area,
        &mut scrollbar_state,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let area = centered(area, 60, 70);
    frame.render_widget(Clear, area);
    let rows = [
        ("tab / 1 / 2", "switch between DSH and Pi"),
        ("j k ↑ ↓", "move"),
        ("g G", "first / last"),
        ("space", "toggle selection"),
        ("a / A", "select all shown / clear selection"),
        ("/", "filter by id, title, or cwd"),
        ("enter", "preview the transcript"),
        ("x", "convert selection (or cursor) to the other harness"),
        ("m", "write ./exports/<id>.md"),
        ("d", "toggle dry-run"),
        ("f", "toggle overwrite of an existing destination"),
        ("e", "show sessions that hold no conversation (hidden by default)"),
        ("r", "rescan both session roots"),
        ("q", "quit"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!("  {key:<12}"), Style::default().fg(ACCENT)),
                Span::raw(*what),
            ])
        })
        .collect();
    let paragraph = Paragraph::new(Text::from(lines)).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" keys ")
            .title_alignment(Alignment::Center),
    );
    frame.render_widget(paragraph, area);
}

/// Renders the busiest row kinds of a session, most frequent first.
fn summarize(histogram: &std::collections::BTreeMap<String, usize>) -> String {
    let mut pairs: Vec<(&String, &usize)> = histogram.iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(a.1));
    pairs
        .iter()
        .take(6)
        .map(|(kind, count)| format!("{kind} {count}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Labelled detail line.
fn field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<6}"), Style::default().fg(DIM)),
        Span::raw(value.to_string()),
    ])
}

/// Byte count rendered for the detail pane.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Centers a percentage-sized rectangle inside `area`.
fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_scales_to_the_right_unit() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
