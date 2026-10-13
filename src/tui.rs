//! Ratatui browser: list every harness's sessions, preview a transcript, and
//! convert the selected sessions.
//!
//! The Claude Code tab groups sessions by project. Projects and sessions both
//! carry checkboxes, sessions Pi already holds are marked and start unchecked,
//! and importing asks for confirmation since the checked set starts populated.

use std::collections::{HashMap, HashSet};
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
use crate::{claude, clip, convert_one, dedup, dsh, paths, pi, shorten, Located, Side};

const ACCENT: Color = Color::Rgb(0x5f, 0xd7, 0xff);
const DIM: Color = Color::Rgb(0x80, 0x80, 0x80);
const OK: Color = Color::Rgb(0x87, 0xd7, 0x87);
const WARN: Color = Color::Rgb(0xff, 0xaf, 0x5f);
const BAD: Color = Color::Rgb(0xff, 0x5f, 0x5f);

/// Tab order; `tab` walks it forward and `shift-tab` backward.
const SIDES: [Side; 3] = [Side::Dsh, Side::Pi, Side::Claude];

/// One session as the browser displays it.
struct Row {
    id: String,
    /// Display name. For an unnamed Claude Code session this is its first
    /// prompt, or its id when it has none.
    title: String,
    /// False when `title` stands in for a missing name.
    named: bool,
    cwd: String,
    when: String,
    detail: String,
    messages: usize,
    /// True when the session holds no conversation: no message at all, or
    /// for Claude Code no model reply.
    empty: bool,
    bucket: String,
    size: u64,
    path: PathBuf,
    side: Side,
    /// How a Claude Code session relates to what Pi already holds.
    verdict: Option<dedup::Verdict>,
}

impl Row {
    /// True when Pi already holds a copy of this Claude Code session.
    fn duplicate(&self) -> bool {
        self.verdict.as_ref().is_some_and(dedup::Verdict::is_duplicate)
    }

    fn located(&self) -> Located {
        Located {
            side: self.side,
            path: self.path.clone(),
            id: self.id.clone(),
            cwd: self.cwd.clone(),
        }
    }
}

/// One Claude Code project: a working directory and its sessions, newest first.
struct Project {
    cwd: String,
    rows: Vec<usize>,
}

/// One line of the session list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// A Claude Code project heading.
    Project(usize),
    /// A session, by index into the current side's rows.
    Row(usize),
}

/// How many of a project's shown sessions are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    None,
    Some,
    All,
}

impl Check {
    fn glyph(self) -> &'static str {
        match self {
            Check::None => "[ ]",
            Check::Some => "[-]",
            Check::All => "[x]",
        }
    }
}

/// Which full-screen panel is covering the browser.
enum Overlay {
    /// The browser itself.
    None,
    /// A rendered transcript.
    Preview,
    /// The key map.
    Help,
    /// Asks before importing these Claude Code sessions.
    Confirm(Vec<PathBuf>),
}

/// Browser state.
struct App {
    side: Side,
    dsh_rows: Vec<Row>,
    pi_rows: Vec<Row>,
    claude_rows: Vec<Row>,
    projects: Vec<Project>,
    /// Project of each Claude row.
    project_of: Vec<usize>,
    /// Working directories of the folded projects.
    collapsed: HashSet<String>,
    view: Vec<Item>,
    cursor: usize,
    selected_dsh: HashSet<PathBuf>,
    selected_pi: HashSet<PathBuf>,
    selected_claude: HashSet<PathBuf>,
    /// Claude transcripts already given their default check, so a rescan only
    /// pre-checks sessions that appeared since.
    seen_claude: HashSet<PathBuf>,
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
            claude_rows: Vec::new(),
            projects: Vec::new(),
            project_of: Vec::new(),
            collapsed: HashSet::new(),
            view: Vec::new(),
            cursor: 0,
            selected_dsh: HashSet::new(),
            selected_pi: HashSet::new(),
            selected_claude: HashSet::new(),
            seen_claude: HashSet::new(),
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

    /// Re-reads every session root and re-judges Claude Code sessions against Pi.
    fn refresh(&mut self) {
        self.dsh_rows = dsh::discover(&paths::dsh_root())
            .into_iter()
            .map(|found| Row {
                id: found.id,
                title: found.title.unwrap_or_default(),
                named: true,
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
                empty: found.messages == 0,
                bucket: found.bucket,
                size: found.size,
                path: found.path,
                side: Side::Dsh,
                verdict: None,
            })
            .collect();

        let pi_found = pi::discover(&paths::pi_root());
        let index = dedup::Index::new(&pi_found);
        self.pi_rows = pi_found
            .into_iter()
            .map(|found| Row {
                id: found.id,
                title: found.title,
                named: true,
                cwd: found.cwd,
                when: found.timestamp,
                detail: format!("{} entries", found.entries),
                messages: found.messages,
                empty: found.messages == 0,
                bucket: found.bucket,
                size: found.size,
                path: found.path,
                side: Side::Pi,
                verdict: None,
            })
            .collect();

        self.claude_rows = claude::discover(&paths::claude_root())
            .into_iter()
            .map(|found| {
                let verdict = index.judge(&found.id, &found.prompts);
                Row {
                    title: found.label().to_string(),
                    named: found.name.is_some(),
                    id: found.id,
                    cwd: found.cwd,
                    when: crate::convert::iso(found.started),
                    detail: format!("{} subagent(s)", found.subagents),
                    messages: found.messages,
                    empty: found.replies == 0,
                    bucket: found.bucket,
                    size: found.size,
                    path: found.path,
                    side: Side::Claude,
                    verdict: Some(verdict),
                }
            })
            .collect();

        // Rows arrive newest first, so projects order by their newest session.
        let mut slots: HashMap<&str, usize> = HashMap::new();
        let mut projects: Vec<Project> = Vec::new();
        let mut project_of = Vec::with_capacity(self.claude_rows.len());
        for (index, row) in self.claude_rows.iter().enumerate() {
            let slot = *slots.entry(row.cwd.as_str()).or_insert_with(|| {
                projects.push(Project {
                    cwd: row.cwd.clone(),
                    rows: Vec::new(),
                });
                projects.len() - 1
            });
            projects[slot].rows.push(index);
            project_of.push(slot);
        }
        self.projects = projects;
        self.project_of = project_of;

        let present: HashSet<&PathBuf> = self.claude_rows.iter().map(|row| &row.path).collect();
        self.selected_claude.retain(|path| present.contains(path));
        for row in &self.claude_rows {
            if self.seen_claude.insert(row.path.clone()) && !row.empty && !row.duplicate() {
                self.selected_claude.insert(row.path.clone());
            }
        }
        self.reindex();
    }

    /// Current side's rows.
    fn rows(&self) -> &[Row] {
        match self.side {
            Side::Dsh => &self.dsh_rows,
            Side::Pi => &self.pi_rows,
            Side::Claude => &self.claude_rows,
        }
    }

    /// Selection set for the current side.
    fn selection(&mut self) -> &mut HashSet<PathBuf> {
        match self.side {
            Side::Dsh => &mut self.selected_dsh,
            Side::Pi => &mut self.selected_pi,
            Side::Claude => &mut self.selected_claude,
        }
    }

    /// True when `path` is selected on the current side.
    fn is_selected(&self, path: &PathBuf) -> bool {
        match self.side {
            Side::Dsh => self.selected_dsh.contains(path),
            Side::Pi => self.selected_pi.contains(path),
            Side::Claude => self.selected_claude.contains(path),
        }
    }

    /// True when a row passes the empty-session rule and the filter.
    fn visible(&self, row: &Row) -> bool {
        if !self.include_empty && row.empty {
            return false;
        }
        let needle = self.filter.to_lowercase();
        needle.is_empty()
            || [&row.id, &row.title, &row.cwd]
                .iter()
                .any(|field| field.to_lowercase().contains(&needle))
    }

    /// A project's sessions that pass [`App::visible`], whether or not it is folded.
    fn project_rows(&self, project: usize) -> Vec<usize> {
        self.projects[project]
            .rows
            .iter()
            .copied()
            .filter(|index| self.visible(&self.claude_rows[*index]))
            .collect()
    }

    /// True when a project shows its sessions. A filter unfolds every project
    /// so matches are never hidden.
    fn expanded(&self, project: usize) -> bool {
        !self.filter.is_empty() || !self.collapsed.contains(&self.projects[project].cwd)
    }

    /// How many of a project's shown sessions are checked.
    fn check_state(&self, project: usize) -> Check {
        let rows = self.project_rows(project);
        let checked = rows
            .iter()
            .filter(|index| self.selected_claude.contains(&self.claude_rows[**index].path))
            .count();
        match checked {
            0 => Check::None,
            n if n == rows.len() => Check::All,
            _ => Check::Some,
        }
    }

    /// Recomputes the list and clamps the cursor.
    fn reindex(&mut self) {
        self.view = match self.side {
            Side::Claude => {
                let mut view = Vec::new();
                for project in 0..self.projects.len() {
                    let rows = self.project_rows(project);
                    if rows.is_empty() {
                        continue;
                    }
                    view.push(Item::Project(project));
                    if self.expanded(project) {
                        view.extend(rows.into_iter().map(Item::Row));
                    }
                }
                view
            }
            _ => self
                .rows()
                .iter()
                .enumerate()
                .filter(|(_, row)| self.visible(row))
                .map(|(index, _)| Item::Row(index))
                .collect(),
        };
        if self.cursor >= self.view.len() {
            self.cursor = self.view.len().saturating_sub(1);
        }
    }

    /// Switches tabs and starts at the top of the new list.
    fn switch(&mut self, side: Side) {
        self.side = side;
        self.cursor = 0;
        self.reindex();
    }

    /// Steps through the tabs, forward or backward.
    fn cycle(&mut self, forward: bool) {
        let at = SIDES.iter().position(|s| *s == self.side).unwrap_or(0);
        let next = if forward {
            (at + 1) % SIDES.len()
        } else {
            (at + SIDES.len() - 1) % SIDES.len()
        };
        self.switch(SIDES[next]);
    }

    /// Row under the cursor, when the cursor is on a session.
    fn current(&self) -> Option<&Row> {
        match self.view.get(self.cursor)? {
            Item::Row(index) => self.rows().get(*index),
            Item::Project(_) => None,
        }
    }

    /// Project under the cursor, or the project of the session under it.
    fn current_project(&self) -> Option<usize> {
        match self.view.get(self.cursor)? {
            Item::Project(project) => Some(*project),
            Item::Row(index) if self.side == Side::Claude => self.project_of.get(*index).copied(),
            Item::Row(_) => None,
        }
    }

    /// Appends one line to the activity log.
    fn say(&mut self, text: impl Into<String>, color: Color) {
        self.log.push((text.into(), color));
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    /// Toggles the session under the cursor, or every shown session of the
    /// project under it: all become checked unless all already were.
    fn toggle(&mut self) {
        match self.view.get(self.cursor).copied() {
            Some(Item::Row(index)) => {
                let path = self.rows()[index].path.clone();
                let selection = self.selection();
                if !selection.insert(path.clone()) {
                    selection.remove(&path);
                }
            }
            Some(Item::Project(project)) => {
                let check = self.check_state(project) != Check::All;
                for index in self.project_rows(project) {
                    let path = self.claude_rows[index].path.clone();
                    if check {
                        self.selected_claude.insert(path);
                    } else {
                        self.selected_claude.remove(&path);
                    }
                }
            }
            None => {}
        }
    }

    /// Every shown session of the current side, folded projects included.
    fn shown_paths(&self) -> Vec<PathBuf> {
        match self.side {
            Side::Claude => (0..self.projects.len())
                .flat_map(|project| self.project_rows(project))
                .map(|index| self.claude_rows[index].path.clone())
                .collect(),
            _ => self
                .view
                .iter()
                .filter_map(|item| match item {
                    Item::Row(index) => Some(self.rows()[*index].path.clone()),
                    Item::Project(_) => None,
                })
                .collect(),
        }
    }

    /// Folds or unfolds the project under the cursor; `None` flips it.
    fn fold(&mut self, collapse: Option<bool>) {
        let Some(project) = self.current_project() else {
            return;
        };
        let cwd = self.projects[project].cwd.clone();
        let collapse = collapse.unwrap_or(!self.collapsed.contains(&cwd));
        if collapse {
            self.collapsed.insert(cwd);
        } else {
            self.collapsed.remove(&cwd);
        }
        self.reindex();
        if let Some(at) = self.view.iter().position(|item| *item == Item::Project(project)) {
            self.cursor = at;
        }
        if !self.filter.is_empty() {
            self.say("projects stay unfolded while a filter is active", DIM);
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
            Side::Claude => claude::read_records(&path)
                .map(|records| (transcript::from_claude(&records, false), claude::histogram(&records))),
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

    /// What `x` acts on: the checked sessions, else the session under the
    /// cursor, else the shown sessions of the project under it.
    fn targets(&self) -> Vec<PathBuf> {
        let selected: Vec<PathBuf> = self
            .rows()
            .iter()
            .filter(|row| self.is_selected(&row.path))
            .map(|row| row.path.clone())
            .collect();
        if !selected.is_empty() {
            return selected;
        }
        match self.view.get(self.cursor) {
            Some(Item::Row(index)) => vec![self.rows()[*index].path.clone()],
            Some(Item::Project(project)) => self
                .project_rows(*project)
                .into_iter()
                .map(|index| self.claude_rows[index].path.clone())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Converts right away, except that Claude Code imports ask first.
    fn request_convert(&mut self) {
        let targets = self.targets();
        if targets.is_empty() {
            self.say("nothing to convert", WARN);
            return;
        }
        if self.side == Side::Claude {
            self.overlay = Overlay::Confirm(targets);
        } else {
            self.convert(targets);
        }
    }

    /// Converts the given sessions of the current side, in list order.
    fn convert(&mut self, paths: Vec<PathBuf>) {
        let wanted: HashSet<PathBuf> = paths.into_iter().collect();
        let targets: Vec<Located> = self
            .rows()
            .iter()
            .filter(|row| wanted.contains(&row.path))
            .map(Row::located)
            .collect();

        let dry_run = self.dry_run;
        let force = self.force;
        let include_empty = self.include_empty;
        let mut converted = 0usize;
        let mut failed = 0usize;
        let mut written = Vec::new();
        for located in targets {
            match convert_one(&located, None, dry_run, force, include_empty) {
                Ok(outcome) => {
                    if outcome.wrote() {
                        converted += 1;
                        written.push(located.path.clone());
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
                    self.say(format!("{}: {error:#}", located.id), BAD);
                }
            }
        }
        self.say(
            format!("{converted} converted, {failed} failed"),
            if failed > 0 { BAD } else { OK },
        );
        if !dry_run && converted > 0 {
            // An imported Claude session is in Pi now; leaving it checked
            // would import it a second time on the next `x`.
            if self.side == Side::Claude {
                for path in &written {
                    self.selected_claude.remove(path);
                }
            }
            self.refresh();
        }
    }

    /// Writes the cursor row as Markdown next to the tool's working directory.
    fn export_markdown(&mut self) {
        let Some(row) = self.current() else {
            self.say("move to a session to export it", WARN);
            return;
        };
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
            Side::Claude => {
                claude::read_records(&path).map(|records| transcript::from_claude(&records, false))
            }
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
            Overlay::Confirm(_) => {
                let overlay = std::mem::replace(&mut app.overlay, Overlay::None);
                match (key.code, overlay) {
                    (KeyCode::Char('y') | KeyCode::Enter, Overlay::Confirm(paths)) => app.convert(paths),
                    _ => app.say("import cancelled", DIM),
                }
            }
            Overlay::None => match key.code {
                KeyCode::Char('q') => app.quit = true,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.quit = true
                }
                KeyCode::Tab => app.cycle(true),
                KeyCode::BackTab => app.cycle(false),
                KeyCode::Char('1') => app.switch(Side::Dsh),
                KeyCode::Char('2') => app.switch(Side::Pi),
                KeyCode::Char('3') => app.switch(Side::Claude),
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
                KeyCode::Char('h') | KeyCode::Left => app.fold(Some(true)),
                KeyCode::Char('l') | KeyCode::Right => app.fold(Some(false)),
                KeyCode::Char(' ') => app.toggle(),
                KeyCode::Char('a') => {
                    let paths = app.shown_paths();
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
                KeyCode::Enter => match app.view.get(app.cursor) {
                    Some(Item::Project(_)) => app.fold(None),
                    _ => app.open_preview(),
                },
                KeyCode::Char('x') => app.request_convert(),
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
                    app.say("rescanned every session root", ACCENT);
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

    match &app.overlay {
        Overlay::Preview => draw_preview(frame, area, app),
        Overlay::Help => draw_help(frame, area),
        Overlay::Confirm(paths) => draw_confirm(frame, area, app, paths),
        Overlay::None => {}
    }
}

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let titles = vec![
        Line::from(format!(" DSH · {} ", app.dsh_rows.len())),
        Line::from(format!(" Pi · {} ", app.pi_rows.len())),
        Line::from(format!(" Claude Code · {} ", app.claude_rows.len())),
    ];
    let index = SIDES.iter().position(|s| *s == app.side).unwrap_or(0);
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
                    " pi-transfuse — DSH ⇄ Pi ← Claude Code ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ))
                .title_top(Line::from(Span::styled(flags, Style::default().fg(WARN))).right_aligned()),
        );
    frame.render_widget(tabs, area);
}

/// Marker and color for a Claude Code session's standing in Pi.
fn verdict_mark(verdict: Option<&dedup::Verdict>) -> (&'static str, Color) {
    match verdict {
        Some(verdict) if verdict.is_duplicate() => ("=", WARN),
        Some(dedup::Verdict::Overlap { .. }) => ("≈", WARN),
        _ => (" ", DIM),
    }
}

/// One list line for a DSH or Pi session.
fn harness_line(app: &App, row: &Row) -> Line<'static> {
    let selected = app.is_selected(&row.path);
    let empty = row.empty;
    let title = if empty {
        "(no messages)".to_string()
    } else if row.title.is_empty() {
        "(untitled)".to_string()
    } else {
        row.title.clone()
    };
    Line::from(vec![
        Span::styled(
            format!("{} ", if selected { "●" } else { "·" }),
            Style::default().fg(if selected { OK } else { DIM }),
        ),
        Span::styled(format!("{:<10} ", &row.when[..10.min(row.when.len())]), Style::default().fg(DIM)),
        Span::styled(
            format!("{:<30} ", shorten(&title, 30)),
            Style::default().fg(if empty { DIM } else { Color::White }),
        ),
        Span::styled(shorten(&row.cwd, 34), Style::default().fg(DIM)),
    ])
}

/// One list line for a Claude Code project heading.
fn project_line(app: &App, project: usize, width: usize) -> Line<'static> {
    let rows = app.project_rows(project);
    let held = rows
        .iter()
        .filter(|index| app.claude_rows[**index].duplicate())
        .count();
    let check = app.check_state(project);
    let summary = format!(
        "  {} session{}{}",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        if held > 0 { format!(" · {held} in Pi") } else { String::new() }
    );
    let cwd_width = width.saturating_sub(8 + summary.chars().count()).max(12);
    Line::from(vec![
        Span::styled(
            format!("{} ", if app.expanded(project) { "▾" } else { "▸" }),
            Style::default().fg(DIM),
        ),
        Span::styled(
            format!("{} ", check.glyph()),
            Style::default().fg(if check == Check::None { DIM } else { OK }),
        ),
        Span::styled(
            shorten(&app.projects[project].cwd, cwd_width),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(summary, Style::default().fg(DIM)),
    ])
}

/// One list line for a Claude Code session, indented under its project.
fn claude_line(app: &App, row: &Row, width: usize) -> Line<'static> {
    let selected = app.is_selected(&row.path);
    let (mark, mark_color) = verdict_mark(row.verdict.as_ref());
    let empty = row.empty;
    let title = if empty && !row.named {
        format!("(no reply) {}", row.title)
    } else {
        row.title.clone()
    };
    let style = if empty {
        Style::default().fg(DIM)
    } else if row.named {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(DIM).add_modifier(Modifier::ITALIC)
    };
    Line::from(vec![
        Span::raw("    "),
        Span::styled(
            format!("{} ", if selected { "[x]" } else { "[ ]" }),
            Style::default().fg(if selected { OK } else { DIM }),
        ),
        Span::styled(format!("{:<10} ", &row.when[..10.min(row.when.len())]), Style::default().fg(DIM)),
        Span::styled(format!("{mark} "), Style::default().fg(mark_color).add_modifier(Modifier::BOLD)),
        Span::styled(clip(&title, width.saturating_sub(23).max(10)), style),
    ])
}

fn draw_list(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .view
        .iter()
        .map(|item| {
            ListItem::new(match *item {
                Item::Project(project) => project_line(app, project, width),
                Item::Row(index) if app.side == Side::Claude => {
                    claude_line(app, &app.claude_rows[index], width)
                }
                Item::Row(index) => harness_line(app, &app.rows()[index]),
            })
        })
        .collect();

    let mut state = ListState::default();
    if !app.view.is_empty() {
        state.select(Some(app.cursor));
    }
    let count = app.view.iter().filter(|item| matches!(item, Item::Row(_))).count();
    let checked = match app.side {
        Side::Claude => format!(" · {} checked", app.selected_claude.len()),
        _ => String::new(),
    };
    let heading = if app.filter.is_empty() {
        format!(" sessions · {count}{checked} ")
    } else {
        format!(" sessions · {count} matching {:?}{checked} ", app.filter)
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

/// Detail lines for a Claude Code project heading.
fn project_detail(app: &App, project: usize) -> Vec<Line<'static>> {
    let all = &app.projects[project].rows;
    let rows = app.project_rows(project);
    let count = |keep: &dyn Fn(&Row) -> bool| {
        rows.iter().filter(|index| keep(&app.claude_rows[**index])).count()
    };
    let checked = count(&|row| app.selected_claude.contains(&row.path));
    let held = count(&|row| row.duplicate());
    let partial = count(&|row| matches!(row.verdict, Some(dedup::Verdict::Overlap { .. })));
    let buckets: HashSet<&str> = all.iter().map(|i| app.claude_rows[*i].bucket.as_str()).collect();
    let mut buckets: Vec<&str> = buckets.into_iter().collect();
    buckets.sort_unstable();
    let mut lines = vec![
        field("cwd", &app.projects[project].cwd),
        field("shown", &format!("{} of {} sessions", rows.len(), all.len())),
        field("check", &format!("{checked} checked")),
        field("in Pi", &format!("{held} already, {partial} partly")),
        field("proj", &buckets.join(", ")),
        Line::from(""),
    ];
    if held > 0 {
        lines.push(Line::from(Span::styled(
            "= sessions start unchecked: Pi already holds them",
            Style::default().fg(WARN),
        )));
    }
    lines.push(Line::from(Span::styled(
        "space checks or unchecks every shown session",
        Style::default().fg(DIM),
    )));
    lines
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = match (app.view.get(app.cursor), app.current()) {
        (Some(Item::Project(project)), _) => project_detail(app, *project),
        (_, None) => vec![Line::from("no session selected")],
        (_, Some(row)) => {
            let destination = crate::destination(&row.located(), None);
            let mut lines = vec![
                field("id", &row.id),
                field("cwd", &row.cwd),
                field("when", &row.when),
                field("proj", &row.bucket),
                field("rows", &format!("{} · {} messages", row.detail, row.messages)),
                field("size", &human_size(row.size)),
            ];
            if row.side == Side::Claude && !row.named {
                lines.push(field("name", "(none — showing the first prompt)"));
            }
            if let Some(verdict) = &row.verdict {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("in Pi", Style::default().fg(ACCENT))));
                match verdict.describe() {
                    None => lines.push(Line::from(Span::styled(
                        "no copy found",
                        Style::default().fg(DIM),
                    ))),
                    Some(reason) => {
                        lines.push(Line::from(Span::styled(reason, Style::default().fg(WARN))));
                        if let Some(twin) = verdict.twin() {
                            lines.push(Line::from(Span::styled(
                                twin.path.display().to_string(),
                                Style::default().fg(DIM),
                            )));
                        }
                    }
                }
            }
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
                        Side::Claude => "imports to Pi",
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
            lines
        }
    };
    let paragraph = Paragraph::new(Text::from(lines))
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
    } else if app.side == Side::Claude {
        "tab switch · space check · ←/→ fold · a all · A none · x import checked · enter preview · m markdown · d dry-run · f overwrite · e empty · r rescan · ? help · q quit".to_string()
    } else {
        "tab switch · space select · a all · x convert · enter preview · m markdown · d dry-run · f overwrite · e empty · r rescan · ? help · q quit".to_string()
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(DIM))),
        area,
    );
}

fn draw_confirm(frame: &mut Frame, area: Rect, app: &App, paths: &[PathBuf]) {
    let area = centered(area, 56, 30);
    frame.render_widget(Clear, area);
    let wanted: HashSet<&PathBuf> = paths.iter().collect();
    let chosen: Vec<&Row> = app
        .claude_rows
        .iter()
        .filter(|row| wanted.contains(&row.path))
        .collect();
    let held = chosen.iter().filter(|row| row.duplicate()).count();
    let projects: HashSet<&str> = chosen.iter().map(|row| row.cwd.as_str()).collect();
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "Import {} Claude Code session(s) from {} project(s) into Pi?",
                chosen.len(),
                projects.len()
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if held > 0 {
        lines.push(Line::from(Span::styled(
            format!("{held} of them Pi already holds; importing adds a second copy"),
            Style::default().fg(WARN),
        )));
    }
    if app.dry_run {
        lines.push(Line::from(Span::styled(
            "dry-run is on: nothing will be written",
            Style::default().fg(WARN),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "y / enter import · any other key cancels",
        Style::default().fg(DIM),
    )));
    let paragraph = Paragraph::new(Text::from(lines))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" import ")
                .title_alignment(Alignment::Center),
        );
    frame.render_widget(paragraph, area);
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
    let area = centered(area, 64, 80);
    frame.render_widget(Clear, area);
    let rows = [
        ("tab / 1 2 3", "switch between DSH, Pi, and Claude Code"),
        ("j k ↑ ↓", "move"),
        ("g G", "first / last"),
        ("space", "toggle selection; on a project, every shown session"),
        ("a / A", "select all shown / clear selection"),
        ("/", "filter by id, title, or cwd"),
        ("enter", "preview the transcript; on a project, fold it"),
        ("← → / h l", "fold / unfold a Claude Code project"),
        ("x", "convert selection (or cursor) to the other harness"),
        ("", "on Claude Code: import the checked sessions into Pi"),
        ("= ≈", "Claude Code session already / partly in Pi"),
        ("m", "write ./exports/<id>.md"),
        ("d", "toggle dry-run"),
        ("f", "toggle overwrite of an existing destination"),
        ("e", "show sessions that hold no conversation (hidden by default)"),
        ("r", "rescan every session root"),
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
