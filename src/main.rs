use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use reqwest::blocking::Client;
use rss::Channel;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use viuer::{Config as ViuerConfig, print_from_file};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    feeds: Vec<Feed>,
    #[serde(default)]
    settings: Settings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    #[serde(default = "default_image_width")]
    image_width: u32,
    #[serde(default = "default_image_height")]
    image_height: u32,
    #[serde(default = "default_page_size")]
    page_size: usize,
}

fn default_image_width() -> u32 {
    60
}
fn default_image_height() -> u32 {
    20
}
fn default_page_size() -> usize {
    20
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            image_width: default_image_width(),
            image_height: default_image_height(),
            page_size: default_page_size(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Feed {
    name: String,
    url: String,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Debug, Clone)]
struct Article {
    title: String,
    link: String,
    pub_date: Option<DateTime<Utc>>,
    content: String,
    image_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum View {
    Feeds,
    Articles,
    ArticleContent,
}

struct App {
    config: Config,
    articles: Vec<Article>,
    current_feed_idx: usize,
    current_article_idx: usize,
    current_view: View,
    scroll_offset: usize,
    config_path: PathBuf,
    client: Client,
    search_mode: bool,
    search_buffer: String,
    status_message: Option<String>,
}

impl App {
    fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .context("Could not find config directory")?
            .join("rss-reader");

        fs::create_dir_all(&config_dir)?;
        let config_path = config_dir.join("config.toml");

        let config = if config_path.exists() {
            let data = fs::read_to_string(&config_path)?;
            toml::from_str(&data).unwrap_or_else(|_| Self::default_config())
        } else {
            let default = Self::default_config();
            let toml_str = toml::to_string_pretty(&default)?;
            fs::write(&config_path, toml_str)?;
            default
        };

        Ok(Self {
            config,
            articles: Vec::new(),
            current_feed_idx: 0,
            current_article_idx: 0,
            current_view: View::Feeds,
            scroll_offset: 0,
            config_path: config_path.clone(),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
            search_mode: false,
            search_buffer: String::new(),
            status_message: Some(format!("Config loaded from: {}", config_path.display())),
        })
    }

    fn default_config() -> Config {
        Config {
            feeds: vec![
                Feed {
                    name: "Hacker News".to_string(),
                    url: "https://hnrss.org/frontpage".to_string(),
                    category: Some("Tech".to_string()),
                },
                Feed {
                    name: "Rust Blog".to_string(),
                    url: "https://blog.rust-lang.org/feed.xml".to_string(),
                    category: Some("Programming".to_string()),
                },
                Feed {
                    name: "Lobsters".to_string(),
                    url: "https://lobste.rs/rss".to_string(),
                    category: Some("Tech".to_string()),
                },
            ],
            settings: Settings::default(),
        }
    }

    fn reload_config(&mut self) -> Result<()> {
        if self.config_path.exists() {
            let data = fs::read_to_string(&self.config_path)?;
            self.config = toml::from_str(&data)?;
            self.status_message = Some("Config reloaded successfully".to_string());
            self.current_feed_idx = 0;
            self.articles.clear();
        } else {
            self.status_message = Some("Config file not found".to_string());
        }
        Ok(())
    }

    fn fetch_feed(&mut self) -> Result<()> {
        if self.config.feeds.is_empty() {
            return Ok(());
        }

        let feed = &self.config.feeds[self.current_feed_idx];
        self.status_message = Some(format!("Fetching: {}...", feed.name));

        let response = self.client.get(&feed.url).send()?;
        let content = response.bytes()?;
        let channel = Channel::read_from(&content[..])?;

        self.articles = channel
            .items()
            .iter()
            .map(|item| {
                let description = item.description().unwrap_or("").to_string();
                let content = item.content().unwrap_or(&description).to_string();

                let image_url = item.enclosure().and_then(|e| {
                    if e.mime_type().starts_with("image/") {
                        Some(e.url().to_string())
                    } else {
                        None
                    }
                });

                let pub_date = item
                    .pub_date()
                    .and_then(|d| DateTime::parse_from_rfc2822(d).ok())
                    .map(|d| d.with_timezone(&Utc));

                Article {
                    title: item.title().unwrap_or("No title").to_string(),
                    link: item.link().unwrap_or("").to_string(),
                    pub_date,
                    content: html2text::from_read(content.as_bytes(), 80),
                    image_url,
                }
            })
            .collect();

        self.current_article_idx = 0;
        self.scroll_offset = 0;
        self.status_message = Some(format!("Loaded {} articles", self.articles.len()));
        Ok(())
    }

    fn search_forward(&mut self, query: &str) {
        let start = self.current_article_idx + 1;
        for i in start..self.articles.len() {
            if self.articles[i]
                .title
                .to_lowercase()
                .contains(&query.to_lowercase())
            {
                self.current_article_idx = i;
                self.adjust_scroll();
                self.status_message = Some(format!("Found: {}", query));
                return;
            }
        }
        self.status_message = Some(format!("Not found: {}", query));
    }

    fn search_backward(&mut self, query: &str) {
        if self.current_article_idx == 0 {
            self.status_message = Some("No more matches".to_string());
            return;
        }

        for i in (0..self.current_article_idx).rev() {
            if self.articles[i]
                .title
                .to_lowercase()
                .contains(&query.to_lowercase())
            {
                self.current_article_idx = i;
                self.adjust_scroll();
                self.status_message = Some(format!("Found: {}", query));
                return;
            }
        }
        self.status_message = Some(format!("Not found: {}", query));
    }

    fn adjust_scroll(&mut self) {
        let page_size = self.config.settings.page_size;
        if self.current_article_idx < self.scroll_offset {
            self.scroll_offset = self.current_article_idx;
        } else if self.current_article_idx >= self.scroll_offset + page_size {
            self.scroll_offset = self.current_article_idx - page_size + 1;
        }
    }

    fn goto_top(&mut self) {
        match self.current_view {
            View::Feeds => {
                self.current_feed_idx = 0;
            }
            View::Articles => {
                self.current_article_idx = 0;
                self.scroll_offset = 0;
            }
            _ => {}
        }
    }

    fn goto_bottom(&mut self) {
        match self.current_view {
            View::Feeds => {
                self.current_feed_idx = self.config.feeds.len().saturating_sub(1);
            }
            View::Articles => {
                self.current_article_idx = self.articles.len().saturating_sub(1);
                self.adjust_scroll();
            }
            _ => {}
        }
    }

    fn page_down(&mut self) {
        let page_size = self.config.settings.page_size;
        if self.current_view == View::Articles {
            let new_idx =
                (self.current_article_idx + page_size).min(self.articles.len().saturating_sub(1));
            self.current_article_idx = new_idx;
            self.adjust_scroll();
        }
    }

    fn page_up(&mut self) {
        let page_size = self.config.settings.page_size;
        if self.current_view == View::Articles {
            self.current_article_idx = self.current_article_idx.saturating_sub(page_size);
            self.adjust_scroll();
        }
    }
}

fn draw_row_and_return_cursor_position(
    content: &str,
    cursor_row: u16,
    stdout: &mut io::Stdout,
) -> u16 {
    execute!(
        stdout,
        crossterm::style::Print(content),
        crossterm::cursor::MoveTo(0, cursor_row)
    );

    cursor_row + 1
}

fn draw_ui(app: &App) -> Result<()> {
    let mut stdout = io::stdout();
    let mut cursor_row_idx = 1;

    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, cursor_row_idx)
    )?;
    cursor_row_idx += 1;

    // Header
    cursor_row_idx = draw_row_and_return_cursor_position(
        "┌────────────────────────────────────────────────────────────────────────────┐",
        cursor_row_idx,
        &mut stdout,
    );
    cursor_row_idx = draw_row_and_return_cursor_position(
        "│ RSS Reader - [q]uit [r]efresh [R]eload config [h/j/k/l] vim nav [/]search  │",
        cursor_row_idx,
        &mut stdout,
    );
    cursor_row_idx = draw_row_and_return_cursor_position(
        "│ [gg]top [G]bottom [Ctrl-d/u]page down/up [n/N]next/prev match              │",
        cursor_row_idx,
        &mut stdout,
    );
    cursor_row_idx = draw_row_and_return_cursor_position(
        "└────────────────────────────────────────────────────────────────────────────┘",
        cursor_row_idx,
        &mut stdout,
    );

    cursor_row_idx = match app.current_view {
        View::Feeds => draw_feeds(app, cursor_row_idx, &mut stdout)?,
        View::Articles => draw_articles(app, cursor_row_idx, &mut stdout)?,
        View::ArticleContent => draw_article_content(app, cursor_row_idx, &mut stdout)?,
    };

    // Status line
    if let Some(msg) = &app.status_message {
        cursor_row_idx = draw_row_and_return_cursor_position(
            format!("\n📌 {}", msg).as_str(),
            cursor_row_idx,
            &mut stdout,
        );
    }

    if app.search_mode {
        draw_row_and_return_cursor_position(
            format!("\n🔍 Search: {}_", app.search_buffer).as_str(),
            cursor_row_idx,
            &mut stdout,
        );
    }

    stdout.flush()?;
    Ok(())
}

fn draw_feeds(app: &App, cursor_row_idx: u16, stdout: &mut io::Stdout) -> Result<u16> {
    let mut local_cursor_idx = cursor_row_idx;

    local_cursor_idx = draw_row_and_return_cursor_position(
        format!("\n📰 Feeds ({})\n", app.config.feeds.len()).as_str(),
        local_cursor_idx,
        stdout,
    );

    for (idx, feed) in app.config.feeds.iter().enumerate() {
        let marker = if idx == app.current_feed_idx {
            "→"
        } else {
            " "
        };
        let category = feed
            .category
            .as_ref()
            .map(|c| format!(" [{}]", c))
            .unwrap_or_default();
        local_cursor_idx = draw_row_and_return_cursor_position(
            format!("{} {:<3} {}{}", marker, idx + 1, feed.name, category).as_str(),
            local_cursor_idx,
            stdout,
        )
    }

    if app.config.feeds.is_empty() {
        local_cursor_idx = draw_row_and_return_cursor_position(
            format!(
                "  No feeds. Edit {} to add feeds.",
                app.config_path.display()
            )
            .as_str(),
            local_cursor_idx,
            stdout,
        );
    }

    Ok(local_cursor_idx)
}

fn draw_articles(app: &App, cursor_row_idx: u16, stdout: &mut io::Stdout) -> Result<u16> {
    if app.config.feeds.is_empty() {
        return Ok(cursor_row_idx);
    }

    let mut local_cursor_idx = cursor_row_idx;

    local_cursor_idx = draw_row_and_return_cursor_position(
        format!(
            "\n📄 {} - {} articles\n",
            app.config.feeds[app.current_feed_idx].name,
            app.articles.len()
        )
        .as_str(),
        local_cursor_idx,
        stdout,
    );

    let page_size = app.config.settings.page_size;
    let visible_start = app.scroll_offset;
    let visible_end = (app.scroll_offset + page_size).min(app.articles.len());

    for (idx, article) in app.articles[visible_start..visible_end].iter().enumerate() {
        let actual_idx = visible_start + idx;
        let marker = if actual_idx == app.current_article_idx {
            "→"
        } else {
            " "
        };

        let date_str = article
            .pub_date
            .map(|d| d.format("%m/%d %H:%M").to_string())
            .unwrap_or_else(|| "??/??".to_string());

        local_cursor_idx = draw_row_and_return_cursor_position(
            format!(
                "{} {:<4} {} | {}",
                marker,
                actual_idx + 1,
                date_str,
                truncate(&article.title, 60)
            )
            .as_str(),
            local_cursor_idx,
            stdout,
        );
    }

    if app.articles.is_empty() {
        local_cursor_idx = draw_row_and_return_cursor_position(
            "  No articles. Press 'r' to refresh.",
            local_cursor_idx,
            stdout,
        );
    }

    if app.articles.len() > page_size {
        local_cursor_idx = draw_row_and_return_cursor_position(
            format!(
                "\n  Showing {}-{} of {}",
                visible_start + 1,
                visible_end,
                app.articles.len()
            )
            .as_str(),
            local_cursor_idx,
            stdout,
        );
    }

    Ok(local_cursor_idx)
}

fn draw_article_content(app: &App, cursor_row_idx: u16, stdout: &mut io::Stdout) -> Result<u16> {
    if app.articles.is_empty() {
        return Ok(cursor_row_idx);
    }

    let mut local_cursor_idx = cursor_row_idx;
    let article = &app.articles[app.current_article_idx];

    local_cursor_idx = draw_row_and_return_cursor_position(
        "═══════════════════════════════════════════════════════════════════════════════",
        local_cursor_idx,
        stdout,
    );
    local_cursor_idx += 1;
    local_cursor_idx = draw_row_and_return_cursor_position(
        format!("📰 {}", article.title).as_str(),
        local_cursor_idx,
        stdout,
    );
    local_cursor_idx = draw_row_and_return_cursor_position(
        format!("📰 {}", article.title).as_str(),
        local_cursor_idx,
        stdout,
    );
    local_cursor_idx = draw_row_and_return_cursor_position(
        format!("🔗 {}", article.link).as_str(),
        local_cursor_idx,
        stdout,
    );
    if let Some(date) = article.pub_date {
        local_cursor_idx = draw_row_and_return_cursor_position(
            format!("📅 {}", date.format("%Y-%m-%d %H:%M:%S")).as_str(),
            local_cursor_idx,
            stdout,
        );
    }
    local_cursor_idx = draw_row_and_return_cursor_position(
        "═══════════════════════════════════════════════════════════════════════════════",
        local_cursor_idx,
        stdout,
    );

    // Display image if available
    if let Some(img_url) = &article.image_url {
        local_cursor_idx =
            draw_row_and_return_cursor_position("🖼️  Loading image...", local_cursor_idx, stdout);

        if let Ok(response) = app.client.get(img_url).send()
            && let Ok(bytes) = response.bytes()
        {
            let temp_path = std::env::temp_dir().join("rss_image.tmp");
            if fs::write(&temp_path, &bytes).is_ok() {
                let conf = ViuerConfig {
                    absolute_offset: false,
                    width: Some(app.config.settings.image_width),
                    height: Some(app.config.settings.image_height),
                    ..Default::default()
                };
                let _ = print_from_file(&temp_path, &conf);
                local_cursor_idx =
                    draw_row_and_return_cursor_position("", local_cursor_idx, stdout);
            }
        }
    }

    local_cursor_idx = draw_row_and_return_cursor_position(
        format!("{}\n", article.content).as_str(),
        local_cursor_idx,
        stdout,
    );
    local_cursor_idx = draw_row_and_return_cursor_position(
        "───────────────────────────────────────────────────────────────────────────────",
        local_cursor_idx,
        stdout,
    );
    local_cursor_idx = draw_row_and_return_cursor_position(
        "Press 'h' or ESC to go back",
        local_cursor_idx,
        stdout,
    );

    Ok(local_cursor_idx)
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}

fn handle_input(app: &mut App, key: KeyEvent) -> Result<bool> {
    // Handle search mode
    if app.search_mode {
        match key.code {
            KeyCode::Enter => {
                let query = app.search_buffer.clone();
                app.search_mode = false;
                if !query.is_empty() {
                    app.search_forward(&query);
                }
                app.search_buffer.clear();
                return Ok(true);
            }
            KeyCode::Esc => {
                app.search_mode = false;
                app.search_buffer.clear();
                app.status_message = Some("Search cancelled".to_string());
                return Ok(true);
            }
            KeyCode::Backspace => {
                app.search_buffer.pop();
                return Ok(true);
            }
            KeyCode::Char(c) => {
                app.search_buffer.push(c);
                return Ok(true);
            }
            _ => return Ok(true),
        }
    }

    // Global commands
    match key.code {
        KeyCode::Char('q') => return Ok(false),
        KeyCode::Char('R') => {
            if let Err(e) = app.reload_config() {
                app.status_message = Some(format!("Error reloading config: {}", e));
            }
            return Ok(true);
        }
        _ => {}
    }

    match app.current_view {
        View::Feeds => handle_feeds_input(app, key),
        View::Articles => handle_articles_input(app, key),
        View::ArticleContent => handle_content_input(app, key),
    }
}

fn handle_feeds_input(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('r') => {
            if let Err(e) = app.fetch_feed() {
                app.status_message = Some(format!("Error: {}", e));
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if app.current_feed_idx < app.config.feeds.len().saturating_sub(1) {
                app.current_feed_idx += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.current_feed_idx > 0 {
                app.current_feed_idx -= 1;
            }
        }
        KeyCode::Char('g') => {
            app.goto_top();
        }
        KeyCode::Char('G') => {
            app.goto_bottom();
        }
        KeyCode::Char('l') | KeyCode::Enter => {
            if !app.config.feeds.is_empty() {
                if let Err(e) = app.fetch_feed() {
                    app.status_message = Some(format!("Error: {}", e));
                } else {
                    app.current_view = View::Articles;
                }
            }
        }
        _ => {}
    }
    Ok(true)
}

fn handle_articles_input(app: &mut App, key: KeyEvent) -> Result<bool> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('h'), _) | (KeyCode::Esc, _) | (KeyCode::Backspace, _) => {
            app.current_view = View::Feeds;
            app.scroll_offset = 0;
            app.status_message = None;
        }
        (KeyCode::Char('r'), _) => {
            if let Err(e) = app.fetch_feed() {
                app.status_message = Some(format!("Error: {}", e));
            }
        }
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
            if app.current_article_idx < app.articles.len().saturating_sub(1) {
                app.current_article_idx += 1;
                app.adjust_scroll();
            }
        }
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
            if app.current_article_idx > 0 {
                app.current_article_idx -= 1;
                app.adjust_scroll();
            }
        }
        (KeyCode::Char('g'), _) => {
            app.goto_top();
        }
        (KeyCode::Char('G'), _) => {
            app.goto_bottom();
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            app.page_down();
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            app.page_up();
        }
        (KeyCode::Char('l'), _) | (KeyCode::Enter, _) => {
            if !app.articles.is_empty() {
                app.current_view = View::ArticleContent;
            }
        }
        (KeyCode::Char('/'), _) => {
            app.search_mode = true;
            app.search_buffer.clear();
            app.status_message = Some("Enter search query...".to_string());
        }
        (KeyCode::Char('n'), _) => {
            if !app.search_buffer.is_empty() {
                app.search_forward(&app.search_buffer.clone());
            }
        }
        (KeyCode::Char('N'), _) => {
            if !app.search_buffer.is_empty() {
                app.search_backward(&app.search_buffer.clone());
            }
        }
        _ => {}
    }
    Ok(true)
}

fn handle_content_input(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('h') | KeyCode::Esc | KeyCode::Backspace => {
            app.current_view = View::Articles;
        }
        _ => {}
    }
    Ok(true)
}

fn main() -> Result<()> {
    let mut app = App::new()?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let result = run_app(&mut app);

    disable_raw_mode()?;
    execute!(stdout, LeaveAlternateScreen)?;

    result
}

fn run_app(app: &mut App) -> Result<()> {
    loop {
        draw_ui(app)?;

        let Event::Key(key) = event::read()? else {
            continue;
        };

        if !handle_input(app, key)? {
            break;
        }
    }

    Ok(())
}
