//! Screens.
//!
//! ratatui owns the things the Python version had to do by hand: the layout
//! solver places the panel, the buffer diffs frames so there is no flicker,
//! and every widget measures text in display cells rather than bytes. What
//! remains here is what the tool actually decides -- what to show, and when.

use crate::llm::{Narration, Narrator};
use crate::model::*;
use crate::sigil;
use crate::source::{Source, SourceError};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::DefaultTerminal;
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};

/// The panel is a luxury of width: below this the list takes every column.
const PANEL_MIN: u16 = 78;
const PANEL_W: u16 = 24;

const ACCENT: Color = Color::Indexed(39);
const ACTIVE: Color = Color::Indexed(120);
const IDLE: Color = Color::Indexed(250);
const WARN: Color = Color::Indexed(214);
const ERRC: Color = Color::Indexed(203);
const ADD: Color = Color::Indexed(114);
const DEL: Color = Color::Indexed(203);

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn frame_block<'a>(title: Line<'a>, right: Line<'a>) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(title)
        .title_top(right.right_aligned())
}

// --- screens ------------------------------------------------------------

pub enum Screen {
    Repos(RepoList),
    Commits(CommitList),
    Detail(DetailView),
    Week(WeekView),
}

pub struct RepoList {
    pub repos: Vec<Repo>,
    pub state: ListState,
    pub filter: String,
    pub filtering: bool,
}

pub struct CommitList {
    pub repo: String,
    pub commits: Vec<Commit>,
    pub state: ListState,
    pub filter: String,
    pub filtering: bool,
    pub more: bool,
    pub page: usize,
}

pub struct DetailView {
    pub repo: String,
    pub commit: Commit,
    pub detail: Option<Detail>,
    pub scroll: u16,
}

pub struct WeekView {
    pub digest: WeekDigest,
    pub scroll: u16,
    /// Prose from a local model, when one was asked and answered. Kept
    /// separate from the digest so it can never be mistaken for it.
    pub narration: Option<Result<Narration, String>>,
    pub narrating: bool,
}

impl RepoList {
    fn visible(&self) -> Vec<&Repo> {
        self.repos
            .iter()
            .filter(|r| {
                self.filter.is_empty()
                    || format!("{} {}", r.name, r.subtitle)
                        .to_lowercase()
                        .contains(&self.filter.to_lowercase())
            })
            .collect()
    }
}

impl CommitList {
    fn visible(&self) -> Vec<&Commit> {
        self.commits
            .iter()
            .filter(|c| {
                self.filter.is_empty()
                    || format!("{} {} {}", c.short(), c.subject, c.author)
                        .to_lowercase()
                        .contains(&self.filter.to_lowercase())
            })
            .collect()
    }
}

// --- app ----------------------------------------------------------------

pub enum Action {
    None,
    Quit,
    Pop,
    OpenCommits(String),
    OpenWeek(String),
    OpenDetail(usize),
    StepWeek(i64),
    Narrate,
    PageCommits,
}

pub struct App {
    pub source: Box<dyn Source>,
    pub stack: Vec<Screen>,
    pub narrator: Option<Narrator>,
    pub status: Option<String>,
    pub running: bool,
}

impl App {
    pub fn new(source: Box<dyn Source>, repos: Vec<Repo>, narrator: Option<Narrator>) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            source,
            stack: vec![Screen::Repos(RepoList {
                repos,
                state,
                filter: String::new(),
                filtering: false,
            })],
            narrator,
            status: None,
            running: true,
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        while self.running && !self.stack.is_empty() {
            terminal.draw(|f| self.draw(f))?;
            if let Event::Key(key) = event::read()? {
                // Windows reports press and release; acting on both would
                // move the cursor twice for one keystroke.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let action = self.handle(key);
                self.act(action, terminal)?;
            }
        }
        Ok(())
    }

    fn act(&mut self, action: Action, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        match action {
            Action::None => {}
            Action::Quit => self.running = false,
            Action::Pop => {
                self.stack.pop();
            }
            Action::OpenCommits(repo) => {
                self.splash(terminal, &format!("reading {repo}…"))?;
                match self.source.commits(&repo, 1) {
                    Ok((commits, more)) if commits.is_empty() => {
                        self.status = Some(format!("{repo} has no commits yet"));
                        let _ = more;
                    }
                    Ok((commits, more)) => {
                        let mut state = ListState::default();
                        state.select(Some(0));
                        self.stack.push(Screen::Commits(CommitList {
                            repo,
                            commits,
                            state,
                            filter: String::new(),
                            filtering: false,
                            more,
                            page: 1,
                        }));
                    }
                    Err(e) => self.status = Some(e.to_string()),
                }
            }
            Action::OpenWeek(repo) => {
                self.splash(terminal, &format!("reading the week in {repo}…"))?;
                match self.source.week(&repo, week_start(0), 0) {
                    Ok(digest) => self.stack.push(Screen::Week(WeekView {
                        digest,
                        scroll: 0,
                        narration: None,
                        narrating: false,
                    })),
                    Err(e) => self.status = Some(e.to_string()),
                }
            }
            Action::StepWeek(delta) => {
                if let Some(Screen::Week(w)) = self.stack.last_mut() {
                    let back = w.digest.back + delta;
                    if back < 0 {
                        return Ok(());
                    }
                    let repo = w.digest.repo.clone();
                    let label = week_label(back);
                    self.splash(terminal, &format!("reading {label} in {repo}…"))?;
                    match self.source.week(&repo, week_start(back), back) {
                        Ok(digest) => {
                            if let Some(Screen::Week(w)) = self.stack.last_mut() {
                                // Recomputed on every step. A digest is a claim
                                // about the present; a cached one ages into a lie.
                                w.digest = digest;
                                w.scroll = 0;
                                w.narration = None;
                            }
                        }
                        Err(e) => self.status = Some(e.to_string()),
                    }
                }
            }
            Action::Narrate => {
                let Some(narrator) = self.narrator.clone() else {
                    self.status = Some(
                        "no local model found -- start ollama or llama-server".into(),
                    );
                    return Ok(());
                };
                if let Some(Screen::Week(w)) = self.stack.last_mut() {
                    w.narrating = true;
                }
                self.splash(terminal, &format!("asking {}…", narrator.model))?;
                if let Some(Screen::Week(w)) = self.stack.last_mut() {
                    w.narration = Some(narrator.narrate(&w.digest));
                    w.narrating = false;
                }
            }
            Action::OpenDetail(idx) => {
                if let Some(Screen::Commits(c)) = self.stack.last() {
                    let Some(commit) = c.visible().get(idx).map(|c| (*c).clone()) else {
                        return Ok(());
                    };
                    let repo = c.repo.clone();
                    self.splash(terminal, &format!("reading {}…", commit.short()))?;
                    let detail = self.source.detail(&repo, &commit).ok();
                    self.stack.push(Screen::Detail(DetailView {
                        repo,
                        commit,
                        detail,
                        scroll: 0,
                    }));
                }
            }
            Action::PageCommits => {
                if let Some(Screen::Commits(c)) = self.stack.last() {
                    if !c.more {
                        return Ok(());
                    }
                    let (repo, page) = (c.repo.clone(), c.page + 1);
                    match self.source.commits(&repo, page) {
                        Ok((batch, more)) => {
                            if let Some(Screen::Commits(c)) = self.stack.last_mut() {
                                c.commits.extend(batch);
                                c.more = more;
                                c.page = page;
                            }
                        }
                        Err(e) => {
                            if let Some(Screen::Commits(c)) = self.stack.last_mut() {
                                c.more = false;
                            }
                            self.status = Some(e.to_string());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn splash(&self, terminal: &mut DefaultTerminal, message: &str) -> std::io::Result<()> {
        terminal.draw(|f| {
            let area = centred(f.area(), 60, 3);
            f.render_widget(Clear, area);
            let block = frame_block(Line::from(""), Line::from(""));
            let text = Line::from(vec![
                Span::styled(" ◐ ", Style::default().fg(WARN)),
                Span::styled(message.to_string(), dim()),
            ]);
            f.render_widget(Paragraph::new(text).block(block), area);
        })?;
        Ok(())
    }

    // -- input
    fn handle(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if self.status.is_some() {
            self.status = None; // any key dismisses the error card
            return Action::None;
        }
        match self.stack.last_mut() {
            Some(Screen::Repos(r)) => Self::handle_repos(r, key),
            Some(Screen::Commits(c)) => Self::handle_commits(c, key),
            Some(Screen::Detail(d)) => Self::handle_scroll(&mut d.scroll, key),
            Some(Screen::Week(w)) => {
                let scroll = &mut w.scroll;
                match key.code {
                    KeyCode::Char('p') | KeyCode::Char('P') => Action::StepWeek(1),
                    KeyCode::Char('n') | KeyCode::Char('N') => Action::StepWeek(-1),
                    KeyCode::Char('m') | KeyCode::Char('M') => Action::Narrate,
                    _ => Self::handle_scroll(scroll, key),
                }
            }
            None => Action::Quit,
        }
    }

    fn handle_scroll(scroll: &mut u16, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
            KeyCode::PageDown => *scroll = scroll.saturating_add(10),
            KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Enter => {
                return Action::Pop
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    fn move_cursor(state: &mut ListState, len: usize, delta: isize) {
        if len == 0 {
            return;
        }
        let cur = state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(len as isize);
        state.select(Some(next as usize));
    }

    fn filter_key(filter: &mut String, filtering: &mut bool, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter => *filtering = false,
            KeyCode::Esc => {
                *filtering = false;
                filter.clear();
            }
            KeyCode::Backspace => {
                filter.pop();
            }
            KeyCode::Char(c) => filter.push(c),
            _ => return false,
        }
        true
    }

    fn handle_repos(r: &mut RepoList, key: KeyEvent) -> Action {
        if r.filtering {
            Self::filter_key(&mut r.filter, &mut r.filtering, key);
            r.state.select(Some(0));
            return Action::None;
        }
        let len = r.visible().len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => Self::move_cursor(&mut r.state, len, -1),
            KeyCode::Down | KeyCode::Char('j') => Self::move_cursor(&mut r.state, len, 1),
            KeyCode::PageUp => Self::move_cursor(&mut r.state, len, -10),
            KeyCode::PageDown => Self::move_cursor(&mut r.state, len, 10),
            KeyCode::Home | KeyCode::Char('g') => r.state.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') => {
                r.state.select(Some(len.saturating_sub(1)))
            }
            KeyCode::Char('/') => {
                r.filtering = true;
                r.filter.clear();
            }
            KeyCode::Char('w') | KeyCode::Char('W') => {
                if let Some(repo) = r.visible().get(r.state.selected().unwrap_or(0)) {
                    return Action::OpenWeek(repo.name.clone());
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                if let Some(repo) = r.visible().get(r.state.selected().unwrap_or(0)) {
                    return Action::OpenCommits(repo.name.clone());
                }
            }
            KeyCode::Esc | KeyCode::Backspace => {
                if r.filter.is_empty() {
                    return Action::Quit;
                }
                r.filter.clear();
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    fn handle_commits(c: &mut CommitList, key: KeyEvent) -> Action {
        if c.filtering {
            Self::filter_key(&mut c.filter, &mut c.filtering, key);
            c.state.select(Some(0));
            return Action::None;
        }
        let len = c.visible().len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => Self::move_cursor(&mut c.state, len, -1),
            KeyCode::Down | KeyCode::Char('j') => {
                Self::move_cursor(&mut c.state, len, 1);
                // The scroll position is the trigger: reaching the end asks
                // for the next page, so there is no "load more" to press.
                if c.state.selected() == Some(len.saturating_sub(1)) && c.more {
                    return Action::PageCommits;
                }
            }
            KeyCode::PageUp => Self::move_cursor(&mut c.state, len, -10),
            KeyCode::PageDown => {
                Self::move_cursor(&mut c.state, len, 10);
                if c.more {
                    return Action::PageCommits;
                }
            }
            KeyCode::Home | KeyCode::Char('g') => c.state.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') => {
                c.state.select(Some(len.saturating_sub(1)))
            }
            KeyCode::Char('/') => {
                c.filtering = true;
                c.filter.clear();
            }
            KeyCode::Enter | KeyCode::Right => {
                return Action::OpenDetail(c.state.selected().unwrap_or(0))
            }
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => {
                if c.filter.is_empty() {
                    return Action::Pop;
                }
                c.filter.clear();
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    // -- drawing
    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let label = self.source.label();
        let has_model = self.narrator.is_some();
        match self.stack.last_mut() {
            Some(Screen::Repos(r)) => draw_repos(f, area, r, &label),
            Some(Screen::Commits(c)) => draw_commits(f, area, c),
            Some(Screen::Detail(d)) => draw_detail(f, area, d),
            Some(Screen::Week(w)) => draw_week(f, area, w, has_model),
            None => {}
        }
        if let Some(msg) = &self.status {
            draw_error(f, area, msg);
        }
    }
}

fn centred(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_error(f: &mut Frame, area: Rect, message: &str) {
    let area = centred(area, 60.min(area.width), 7);
    f.render_widget(Clear, area);
    let block = frame_block(
        Line::from(Span::styled(" something went wrong ", Style::default().fg(ERRC))),
        Line::from(""),
    );
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(message.to_string(), Style::default().fg(IDLE))),
        Line::from(""),
        Line::from(Span::styled("press any key", dim())),
    ];
    f.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn hint_bar(f: &mut Frame, area: Rect, hint: &str, counter: &str) {
    let line = Line::from(vec![
        Span::styled(format!(" {hint}"), dim()),
        Span::raw(" "),
    ]);
    f.render_widget(Paragraph::new(line), area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!("{counter} "), dim())).right_aligned()),
        area,
    );
}

/// The mark, over the facts that identify the selection.
fn sigil_panel(name: &str, language: &str, facts: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let colour = sigil::tint(name, language);
    let gap = " ".repeat(
        (PANEL_W as usize).saturating_sub(sigil::WIDTH) / 2,
    );
    let mut lines = vec![Line::from("")];
    for art in sigil::lines(name) {
        lines.push(Line::from(Span::styled(
            format!("{gap}{art}"),
            Style::default().fg(colour),
        )));
    }
    lines.push(Line::from(""));
    lines.extend(facts);
    lines
}

fn split_for_panel(area: Rect) -> (Rect, Option<Rect>) {
    if area.width < PANEL_MIN {
        return (area, None);
    }
    let parts = Layout::horizontal([Constraint::Min(0), Constraint::Length(PANEL_W)])
        .split(area);
    (parts[0], Some(parts[1]))
}

fn draw_repos(f: &mut Frame, area: Rect, r: &mut RepoList, label: &str) {
    let outer = frame_block(
        Line::from(vec![
            Span::raw(" "),
            Span::styled("git history", Style::default().fg(ACCENT).bold()),
            Span::raw(" "),
        ]),
        Line::from(Span::styled(format!(" {label} "), dim())),
    );
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let (list_area, panel_area) = split_for_panel(rows[0]);

    // Move the list state aside so the immutable borrow for `visible` and
    // the mutable borrow the stateful widget needs do not overlap.
    let mut state = std::mem::take(&mut r.state);
    let visible = r.visible();
    let selected = state.selected().unwrap_or(0);

    let items: Vec<ListItem> = visible
        .iter()
        .map(|repo| {
            let right = format!("{}  {}", repo.extra, ago(repo.updated));
            let name_w = list_area.width.saturating_sub(right.len() as u16 + 8) as usize;
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(
                    truncate(&repo.name, name_w),
                    Style::default().fg(IDLE),
                ),
            ];
            if !repo.subtitle.is_empty() && list_area.width > 50 {
                spans.push(Span::styled(format!("  {}", repo.subtitle), dim()));
            }
            spans.push(Span::styled(format!("  {}", right.trim()), dim()));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().fg(ACTIVE).bold())
        .highlight_symbol("▍");
    f.render_stateful_widget(list, list_area, &mut state);

    if let Some(panel) = panel_area {
        if let Some(repo) = visible.get(selected) {
            let mut facts = vec![Line::from(Span::styled(
                repo.name.clone(),
                Style::default().bold(),
            ))];
            if !repo.subtitle.is_empty() {
                facts.push(Line::from(Span::styled(repo.subtitle.clone(), dim())));
            }
            if !repo.extra.is_empty() {
                facts.push(Line::from(Span::styled(repo.extra.clone(), dim())));
            }
            facts.push(Line::from(""));
            facts.push(Line::from(Span::styled(stamp(repo.updated), dim())));
            let text = sigil_panel(&repo.name, &repo.subtitle, facts);
            f.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::LEFT).border_style(dim())),
                panel,
            );
        }
    }

    let counter = format!("{}/{}", selected + 1, visible.len());
    if r.filtering {
        hint_bar(f, rows[1], &format!("/{}▏", r.filter), &counter);
    } else {
        let hint = if area.width >= 64 {
            "↑↓ move   ⏎ commits   w week   / filter   q quit"
        } else {
            "↑↓  ⏎  w  /  q"
        };
        let shown = if r.filter.is_empty() {
            counter
        } else {
            format!("/{}  {counter}", r.filter)
        };
        hint_bar(f, rows[1], hint, &shown);
    }
    r.state = state;
}

fn draw_commits(f: &mut Frame, area: Rect, c: &mut CommitList) {
    let mut state = std::mem::take(&mut c.state);
    let visible = c.visible();
    let count = if c.more {
        format!("{}+ commits", visible.len())
    } else {
        format!("{} commits", visible.len())
    };
    let outer = frame_block(
        Line::from(vec![
            Span::raw(" "),
            Span::styled(c.repo.clone(), Style::default().fg(ACCENT).bold()),
            Span::raw(" "),
        ]),
        Line::from(Span::styled(format!(" {count} "), dim())),
    );
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let (list_area, panel_area) = split_for_panel(rows[0]);
    let selected = state.selected().unwrap_or(0);

    let items: Vec<ListItem> = visible
        .iter()
        .map(|commit| {
            let meta = format!("{}  {}", commit.short(), stamp(commit.date));
            let subj_w = list_area.width.saturating_sub(meta.len() as u16 + 6) as usize;
            ListItem::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(truncate(&commit.subject, subj_w), Style::default().fg(IDLE)),
                Span::styled(format!("  {meta}"), dim()),
            ]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().fg(ACTIVE).bold())
        .highlight_symbol("●");
    f.render_stateful_widget(list, list_area, &mut state);

    if let Some(panel) = panel_area {
        if let Some(commit) = visible.get(selected) {
            let facts = vec![
                Line::from(Span::styled(commit.short().to_string(), Style::default().bold())),
                Line::from(Span::styled(commit.author.clone(), dim())),
                Line::from(Span::styled(ago(commit.date), dim())),
            ];
            let text = sigil_panel(&c.repo, "", facts);
            f.render_widget(
                Paragraph::new(text)
                    .block(Block::default().borders(Borders::LEFT).border_style(dim())),
                panel,
            );
        }
    }

    let counter = format!("{}/{}", selected + 1, visible.len());
    if c.filtering {
        hint_bar(f, rows[1], &format!("/{}▏", c.filter), &counter);
    } else {
        hint_bar(f, rows[1], "↑↓ move   ⏎ detail   / filter   esc back", &counter);
    }
    c.state = state;
}

fn draw_detail(f: &mut Frame, area: Rect, d: &mut DetailView) {
    let sub = match &d.detail {
        None => "no diff".to_string(),
        Some(det) => format!(
            "{} {}  +{} −{}",
            det.files.len(),
            if det.files.len() == 1 { "file" } else { "files" },
            det.additions,
            det.deletions
        ),
    };
    let outer = frame_block(
        Line::from(vec![
            Span::raw(" "),
            Span::styled(d.repo.clone(), Style::default().fg(ACCENT).bold()),
            Span::styled(format!(" @ {} ", d.commit.short()), dim()),
        ]),
        Line::from(Span::styled(format!(" {sub} "), dim())),
    );
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

    let c = &d.commit;
    let mut text = vec![
        Line::from(Span::styled(c.subject.clone(), Style::default().bold())),
        Line::from(Span::styled(
            format!("{}  ·  {}  ·  {}", c.author, stamp(c.date), ago(c.date)),
            dim(),
        )),
        Line::from(Span::styled(c.sha.clone(), dim())),
    ];
    if !c.body.is_empty() {
        text.push(Line::from(""));
        for para in c.body.lines() {
            text.push(Line::from(Span::styled(para.to_string(), Style::default().fg(IDLE))));
        }
    }
    if let Some(det) = &d.detail {
        text.push(Line::from(""));
        text.push(Line::from(Span::styled("files", dim())));
        for (name, a, del) in &det.files {
            text.push(Line::from(vec![
                Span::styled(format!("  {name}  "), Style::default().fg(IDLE)),
                Span::styled(format!("+{a}"), Style::default().fg(ADD)),
                Span::raw(" "),
                Span::styled(format!("−{del}"), Style::default().fg(DEL)),
            ]));
        }
    }

    let total = text.len() as u16;
    d.scroll = d.scroll.min(total.saturating_sub(1));
    f.render_widget(
        Paragraph::new(text).scroll((d.scroll, 0)).wrap(Wrap { trim: false }),
        rows[0],
    );
    hint_bar(f, rows[1], "↑↓ scroll   esc back   q quit",
             &format!("{}/{}", d.scroll + 1, total));
}

fn draw_week(f: &mut Frame, area: Rect, w: &mut WeekView, has_model: bool) {
    let d = &w.digest;
    let state_colour = match d.state() {
        "quiet" => Color::DarkGray,
        "sustained" | "concentrated" => ACTIVE,
        "moving" => ACCENT,
        _ => WARN,
    };
    let outer = frame_block(
        Line::from(vec![
            Span::raw(" "),
            Span::styled(d.repo.clone(), Style::default().fg(ACCENT).bold()),
            Span::styled(" · ", dim()),
            Span::styled(d.state().to_string(), Style::default().fg(state_colour)),
            Span::raw(" "),
        ]),
        Line::from(Span::styled(
            format!(" {}  ·  {} ", week_label(d.back), d.start.format("%b %d")),
            dim(),
        )),
    );
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

    // The week as a shape before the week as a sentence: seven days, one
    // column each, Sunday on the left.
    let today = (chrono::Local::now() - d.start).num_days();
    let bars = spark(&d.by_day);
    let mut spark_spans = vec![Span::raw(" ")];
    let mut label_spans = vec![Span::raw(" ")];
    for i in 0..7 {
        let is_today = today == i as i64 && d.back == 0;
        let style = if is_today {
            Style::default().fg(ACTIVE)
        } else {
            Style::default().fg(IDLE)
        };
        spark_spans.push(Span::styled(format!("{} ", bars[i]), style));
        label_spans.push(Span::styled(
            format!("{} ", DAY_INITIALS[i]),
            if is_today { Style::default().fg(ACCENT) } else { dim() },
        ));
    }
    spark_spans.push(Span::styled(
        format!("  {} {}", plural(d.commits.len(), "commit"), week_label(d.back)),
        dim(),
    ));

    let mut text = vec![Line::from(spark_spans), Line::from(label_spans), Line::from("")];

    for sentence in d.sentences() {
        text.push(Line::from(Span::styled(sentence, Style::default().fg(IDLE))));
        text.push(Line::from(""));
    }

    if !d.kinds.is_empty() {
        text.push(Line::from(Span::styled("kind of work", dim())));
        let widest = d.kinds.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        for (kind, count) in &d.kinds {
            text.push(Line::from(vec![
                Span::styled(format!("  {:widest$} ", kind, widest = widest),
                             Style::default().fg(IDLE)),
                Span::styled("█".repeat((*count).min(30)), Style::default().fg(ACCENT)),
                Span::styled(format!(" {count}"), dim()),
            ]));
        }
        text.push(Line::from(""));
    }

    if !d.areas.is_empty() {
        text.push(Line::from(Span::styled("where it landed", dim())));
        for (area_name, (a, del, n)) in d.areas.iter().take(8) {
            text.push(Line::from(vec![
                Span::styled(format!("  {area_name}  "), Style::default().fg(IDLE)),
                Span::styled(format!("{}  ", plural(*n, "file")), dim()),
                Span::styled(format!("+{a}"), Style::default().fg(ADD)),
                Span::raw(" "),
                Span::styled(format!("−{del}"), Style::default().fg(DEL)),
            ]));
        }
        text.push(Line::from(""));
    }

    if d.files.is_empty() && !d.commits.is_empty() {
        text.push(Line::from(Span::styled(
            "file counts need a local clone; over the API they would cost a request per commit",
            dim(),
        )));
        text.push(Line::from(""));
    }

    // Kept visibly apart from the digest above it, and labelled, because
    // everything above is counted and this is not.
    match &w.narration {
        Some(Ok(n)) => {
            text.push(Line::from(Span::styled(
                format!("── narrated by {} ──", n.model),
                dim(),
            )));
            text.push(Line::from(""));
            text.push(Line::from(Span::styled(
                n.week.clone(),
                Style::default().fg(WARN),
            )));
            if !n.days.is_empty() {
                text.push(Line::from(""));
                for day in &n.days {
                    // A reused line cost no inference: the day was already
                    // finished when it was first summarised.
                    let mark = if day.reused { "·" } else { "▸" };
                    text.push(Line::from(vec![
                        Span::styled(format!("  {mark} {:<12}", day.label), dim()),
                        Span::styled(day.text.clone(), Style::default().fg(IDLE)),
                    ]));
                }
            }
            text.push(Line::from(""));
            text.push(Line::from(Span::styled(
                format!(
                    "generated text; the digest above is counted from the log \
                     ({} inferred, {} from cache)",
                    n.computed, n.reused
                ),
                dim(),
            )));
        }
        Some(Err(e)) => {
            text.push(Line::from(Span::styled(format!("model: {e}"), Style::default().fg(ERRC))));
        }
        None if w.narrating => {
            text.push(Line::from(Span::styled("asking the model…", dim())));
        }
        None => {}
    }

    let total = text.len() as u16;
    w.scroll = w.scroll.min(total.saturating_sub(1));
    f.render_widget(
        Paragraph::new(text).scroll((w.scroll, 0)).wrap(Wrap { trim: false }),
        rows[0],
    );

    let hint = if has_model {
        "↑↓ scroll   p/n week   m narrate   esc back"
    } else {
        "↑↓ scroll   p/n week   esc back   q quit"
    };
    hint_bar(f, rows[1], hint, &format!("{}/{}", w.scroll + 1, total));
}

fn truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn report(e: &SourceError) {
    eprintln!("\n  ✕ {e}\n");
}
