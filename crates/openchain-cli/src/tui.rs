use eyre::{Context as _, Result};
use futures::{FutureExt, StreamExt};
use openchain_core::{Config, Dataset};
use openchain_sink::Sink;
use ratatui::{
    crossterm::{
        event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame, Terminal,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::stdout,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// Interactive dashboard over the query layer: status, transfers, txs, events
/// and top movers with editable flag-style filters.
pub async fn run(config: &Config, chain: Option<u64>) -> Result<()> {
    let mut chains: Vec<u64> =
        config.chains.keys().filter_map(|k| k.parse::<u64>().ok()).collect();
    chains.sort_unstable();
    if chains.is_empty() {
        eyre::bail!("no [chains.<id>] sections in config");
    }
    let chain_idx = match chain {
        Some(c) => chains.iter().position(|x| *x == c).unwrap_or(0),
        None => 0,
    };

    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let res = drive(&mut terminal, config.clone(), chains, chain_idx).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Tab {
    Status,
    Transfers,
    Txs,
    Events,
    Top,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Status, Tab::Transfers, Tab::Txs, Tab::Events, Tab::Top];
    fn label(self) -> &'static str {
        match self {
            Tab::Status => "1 status",
            Tab::Transfers => "2 transfers",
            Tab::Txs => "3 txs",
            Tab::Events => "4 events",
            Tab::Top => "5 top",
        }
    }
}

#[derive(Clone)]
struct Field {
    label: String,
    value: String,
}

#[derive(Clone)]
struct View {
    fields: Vec<Field>,
    focused: usize,
    editing: bool,
    rows: Vec<Value>,
    columns: Vec<&'static str>,
    queried: bool,
    loading: bool,
    error: Option<String>,
    selected: usize,
    last_sql: String,
}

impl View {
    fn new(fields: &[(&str, &str)], columns: &[&'static str]) -> Self {
        View {
            fields: fields
                .iter()
                .map(|(l, v)| Field { label: l.to_string(), value: v.to_string() })
                .collect(),
            focused: 0,
            editing: false,
            rows: Vec::new(),
            columns: columns.to_vec(),
            queried: false,
            loading: false,
            error: None,
            selected: 0,
            last_sql: String::new(),
        }
    }

    fn get(&self, label: &str) -> String {
        self.fields
            .iter()
            .find(|f| f.label == label)
            .map(|f| f.value.clone())
            .unwrap_or_default()
    }
}

/// One dataset row in the status table: name, rows, min/max block, watermark.
type StatusRow = (String, u64, Option<u64>, Option<u64>, Option<u64>);

enum QueryOut {
    Status(Result<(Vec<StatusRow>, String)>),
    View(Tab, Result<(String, Vec<Value>)>),
}

struct App {
    config: Config,
    chains: Vec<u64>,
    chain_idx: usize,
    tab: Tab,
    views: HashMap<Tab, View>,
    status_rows: Vec<StatusRow>,
    head_line: String,
    status_loading: bool,
    status_refreshed: Option<Instant>,
    toast: Option<(String, Instant)>,
    popup: Option<Popup>,
    sql_scroll: usize,
    tx: mpsc::UnboundedSender<QueryOut>,
    rx: mpsc::UnboundedReceiver<QueryOut>,
    quit: bool,
}

enum Popup {
    Help,
    Sql,
}

impl App {
    fn new(
        config: Config,
        chains: Vec<u64>,
        chain_idx: usize,
        tx: mpsc::UnboundedSender<QueryOut>,
        rx: mpsc::UnboundedReceiver<QueryOut>,
    ) -> Self {
        let views = HashMap::from([
            (
                Tab::Status,
                View::new(&[], &["dataset", "rows", "min", "max", "watermark"]),
            ),
            (
                Tab::Transfers,
                View::new(
                    &[("token", "usdt"), ("since", "30d"), ("min-value", ""), ("max-value", ""), ("from", ""), ("to", "")],
                    &["age", "value_human", "from", "to", "tx_hash", "block_number"],
                ),
            ),
            (
                Tab::Txs,
                View::new(
                    &[("from", ""), ("to", ""), ("method", ""), ("status", ""), ("min-value", ""), ("max-value", ""), ("since", "7d")],
                    &[
                        "age",
                        "value_eth_human",
                        "from",
                        "to",
                        "method",
                        "status_h",
                        "tx_hash",
                        "block_number",
                    ],
                ),
            ),
            (
                Tab::Events,
                View::new(
                    &[("event", ""), ("contract", ""), ("params", ""), ("since", "7d")],
                    &[
                        "age",
                        "event_name",
                        "contract_name",
                        "contract",
                        "params",
                        "tx_hash",
                        "block_number",
                    ],
                ),
            ),
            (
                Tab::Top,
                View::new(
                    &[("token", "usdt"), ("native", ""), ("side", "recipients"), ("since", "24h"), ("min-value", "")],
                    &["rank", "addr", "volume_human", "txs"],
                ),
            ),
        ]);
        App {
            config,
            chains,
            chain_idx,
            tab: Tab::Status,
            views,
            status_rows: Vec::new(),
            head_line: String::new(),
            status_loading: false,
            status_refreshed: None,
            toast: None,
            popup: None,
            sql_scroll: 0,
            tx,
            rx,
            quit: false,
        }
    }

    fn chain(&self) -> u64 {
        self.chains[self.chain_idx]
    }

    fn view(&self, tab: Tab) -> &View {
        &self.views[&tab]
    }

    fn view_mut(&mut self, tab: Tab) -> &mut View {
        self.views.get_mut(&tab).expect("view exists")
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// Spawn the query for the active tab (or status refresh) in the background.
    fn request(&mut self, tab: Tab) {
        let config = self.config.clone();
        let chain = self.chain();
        let tx = self.tx.clone();
        let view = self.view(tab).clone();

        if tab == Tab::Status {
            self.status_loading = true;
            tokio::spawn(async move {
                let out = refresh_status(&config, chain).await;
                let _ = tx.send(QueryOut::Status(out));
            });
            return;
        }

        {
            let v = self.view_mut(tab);
            v.loading = true;
            v.error = None;
        }
        tokio::spawn(async move {
            let snap = view.clone_fields_snapshot();
            let out = run_view_query(&config, chain, tab, &view, &snap).await;
            let _ = tx.send(QueryOut::View(tab, out));
        });
    }

    fn apply_result(&mut self, msg: QueryOut) {
        match msg {
            QueryOut::Status(result) => {
                self.status_loading = false;
                self.status_refreshed = Some(Instant::now());
                match result {
                    Ok((rows, head)) => {
                        self.status_rows = rows;
                        self.head_line = head;
                    }
                    Err(e) => self.head_line = format!("status error: {e:#}"),
                }
            }
            QueryOut::View(tab, result) => {
                let v = self.view_mut(tab);
                v.loading = false;
                v.queried = true;
                match result {
                    Ok((sql, rows)) => {
                        v.last_sql = sql;
                        v.rows = rows;
                        v.selected = 0;
                    }
                    Err(e) => v.error = Some(format!("{e:#}")),
                }
            }
        }
    }
}

// Snapshot of user-editable filter values taken before spawning the query task.
#[derive(Clone)]
struct FieldsSnapshot(Vec<(String, String)>);

impl View {
    fn clone_fields_snapshot(&self) -> FieldsSnapshot {
        FieldsSnapshot(
            self.fields.iter().map(|f| (f.label.to_string(), f.value.clone())).collect(),
        )
    }
    fn get_in(&self, snap: &FieldsSnapshot, label: &str) -> String {
        snap.0
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, v)| v.clone())
            .or_else(|| Some(self.get(label)))
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Query execution (reuses the CLI query layer)
// ---------------------------------------------------------------------------

async fn refresh_status(config: &Config, chain: u64) -> Result<(Vec<StatusRow>, String)> {
    let sink = Sink::new(&config.clickhouse);
    let stats = sink.dataset_stats(chain).await?;
    let rows = stats
        .iter()
        .map(|s| {
            (
                s.dataset.to_string(),
                s.rows,
                (s.rows > 0).then_some(s.min_block),
                (s.rows > 0).then_some(s.max_block),
                s.watermark,
            )
        })
        .collect();
    let blocks_max = stats
        .iter()
        .find(|s| s.dataset == Dataset::Blocks)
        .filter(|s| s.rows > 0)
        .map(|s| s.max_block);
    let mut head_line = String::new();
    if let Ok(source) =
        openchain_evm::EvmSource::connect(&config.chain(chain)?.rpc, chain).await
    {
        if let Ok(head) = source.latest_block().await {
            head_line = match blocks_max {
                Some(local) => format!("head {head} · lag {} blocks", head.saturating_sub(local)),
                None => format!("head {head} · nothing synced"),
            };
        }
    }
    Ok((rows, head_line))
}

async fn run_view_query(
    config: &Config,
    chain: u64,
    tab: Tab,
    view: &View,
    snap: &FieldsSnapshot,
) -> Result<(String, Vec<Value>)> {
    let get = |l: &str| view.get_in(snap, l);
    match tab {
        Tab::Transfers => {
            let token_raw = get("token");
            let token_input = non_empty(&token_raw, "token")?;
            let token = crate::query::tokens::resolve(config, chain, token_input.trim())
                .await
                .map_err(|e| eyre::eyre!("{e:#}"))?;
            let decimals = token.decimals;
            let scale = |amount: &str| -> Result<String> {
                let n = crate::query::parse_amount(amount)?;
                Ok(format!("{:.0}", n * 10f64.powi(decimals as i32)))
            };
            let min_raw = opt_scale(&get("min-value"), &scale)?;
            let max_raw = opt_scale(&get("max-value"), &scale)?;
            let range = crate::query::RangeArgs {
                since: opt_str(&get("since")),
                until: None,
                blocks: None,
            };
            let lo_hi = crate::query::resolve_range(config, chain, &range).await?;
            let addr = |l: &str| -> Result<Option<String>> {
                let v = get(l);
                if v.trim().is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(crate::query::parse_address(&v, l)?))
                }
            };
            let sql = crate::query::transfers::build_sql(
                chain,
                &[format!("unhex('{}')", token.address)],
                addr("from")?.as_deref(),
                addr("to")?.as_deref(),
                min_raw.as_deref(),
                max_raw.as_deref(),
                &lo_hi,
                false,
                TUI_LIMIT,
            );
            let body = crate::query::ch(config, &format!("{sql} FORMAT JSONEachRow"), "JSONEachRow")
                .await?;
            let symbol = token.symbol.clone();
            let rows =
                crate::query::transfers::enrich_rows(&body, chain, decimals, &symbol);
            Ok((sql, rows))
        }
        Tab::Txs => {
            let method = {
                let m = get("method").trim().trim_start_matches("0x").to_ascii_lowercase();
                if m.is_empty() {
                    None
                } else {
                    if m.len() != 8 || !m.chars().all(|c| c.is_ascii_hexdigit()) {
                        eyre::bail!("--method expects 4-byte hex selector");
                    }
                    Some(m)
                }
            };
            let status_bit = match get("status").trim().to_ascii_lowercase().as_str() {
                "" => None,
                "success" | "ok" => Some(1u8),
                "reverted" | "failed" => Some(0u8),
                other => eyre::bail!("status must be success/reverted (got '{other}')"),
            };
            let wei = |v: &str| -> Result<Option<u128>> {
                if v.trim().is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(crate::query::parse_native_amount(v)? as u128))
                }
            };
            let range = crate::query::RangeArgs {
                since: opt_str(&get("since")),
                until: None,
                blocks: None,
            };
            let lo_hi = crate::query::resolve_range(config, chain, &range).await?;
            let addr = |l: &str| -> Result<Option<String>> {
                let v = get(l);
                if v.trim().is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(crate::query::parse_address(&v, l)?))
                }
            };
            let sql = crate::query::txs::build_sql(
                chain,
                addr("from")?.as_deref(),
                addr("to")?.as_deref(),
                method.as_deref(),
                status_bit,
                wei(&get("min-value"))?,
                wei(&get("max-value"))?,
                &lo_hi,
                false,
                TUI_LIMIT,
            );
            let body = crate::query::ch(config, &format!("{sql} FORMAT JSONEachRow"), "JSONEachRow")
                .await?;
            Ok((sql, crate::query::txs::enrich_rows(&body, chain)))
        }
        Tab::Events => {
            let range = crate::query::RangeArgs {
                since: opt_str(&get("since")),
                until: None,
                blocks: None,
            };
            let lo_hi = crate::query::resolve_range(config, chain, &range).await?;
            let contract = {
                let c = get("contract");
                if c.trim().is_empty() {
                    None
                } else {
                    Some(c)
                }
            };
            let params = {
                let p = get("params");
                if p.trim().is_empty() {
                    None
                } else {
                    Some(p)
                }
            };
            let sql = crate::query::events::build_sql(
                chain,
                opt_str(&get("event")).as_deref(),
                contract.as_deref(),
                params.as_deref(),
                &lo_hi,
                false,
                TUI_LIMIT,
            )?;
            let body = crate::query::ch(config, &format!("{sql} FORMAT JSONEachRow"), "JSONEachRow")
                .await?;
            Ok((sql, crate::query::events::enrich_rows(&body, chain)))
        }
        Tab::Top => {
            let side = match get("side").trim().to_ascii_lowercase().as_str() {
                "" | "recipients" => "recipients",
                "senders" => "senders",
                other => eyre::bail!("side must be senders/recipients (got '{other}')"),
            };
            let native = matches!(
                get("native").trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "native"
            );
            let range = crate::query::RangeArgs {
                since: opt_str(&get("since")),
                until: None,
                blocks: None,
            };
            let lo_hi = crate::query::resolve_range(config, chain, &range).await?;
            let block_filter = crate::query::top::range_clause(lo_hi.0, lo_hi.1);
            let (symbol, decimals, token_addr, topic0, min_raw) = if native {
                let min = if get("min-value").trim().is_empty() {
                    None
                } else {
                    Some(format!("{}", crate::query::parse_native_amount(&get("min-value"))? as u128))
                };
                ("ETH".to_string(), 18, None, "", min)
            } else {
                let token_raw = get("token");
                let token_input = non_empty(&token_raw, "token")?;
                let token = crate::query::tokens::resolve(config, chain, token_input.trim())
                    .await
                    .map_err(|e| eyre::eyre!("{e:#}"))?;
                let min = if get("min-value").trim().is_empty() {
                    None
                } else {
                    Some(format!(
                        "{:.0}",
                        crate::query::parse_amount(&get("min-value"))?
                            * 10f64.powi(token.decimals as i32)
                    ))
                };
                (
                    token.symbol.clone(),
                    token.decimals,
                    Some(token.address.clone()),
                    crate::query::top::TRANSFER_TOPIC0,
                    min,
                )
            };
            let sql = crate::query::top::top_sql(
                chain,
                side,
                &block_filter,
                token_addr.as_deref(),
                topic0,
                min_raw.as_deref(),
                native,
            );
            let body = crate::query::ch(config, &format!("{sql} FORMAT JSONEachRow"), "JSONEachRow")
                .await?;
            let mut rows = crate::query::top::enrich(&body, chain, &symbol, decimals);
            rows = crate::query::merge_rows_by_volume(rows, TUI_LIMIT);
            Ok((sql, rows))
        }
        Tab::Status => unreachable!("status handled separately"),
    }
}

const TUI_LIMIT: u64 = 200;

fn opt_str(v: &str) -> Option<String> {
    let t = v.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn non_empty<'a>(v: &'a str, what: &str) -> Result<&'a str> {
    let t = v.trim();
    if t.is_empty() {
        eyre::bail!("{what} is required");
    }
    Ok(t)
}

fn opt_scale(amount: &str, scale: &dyn Fn(&str) -> Result<String>) -> Result<Option<String>> {
    if amount.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(scale(amount)?))
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

async fn drive(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    config: Config,
    chains: Vec<u64>,
    chain_idx: usize,
) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(config, chains, chain_idx, tx, rx);

    // Kick off initial data so the screen is alive immediately.
    app.request(Tab::Status);
    app.request(Tab::Transfers);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    while !app.quit {
        terminal.draw(|f| draw(f, &app)).context("draw")?;

        tokio::select! {
            maybe_event = events.next().fuse() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        if key.kind == KeyEventKind::Press {
                            on_key(&mut app, key.code, key.modifiers);
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {}
                    Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            Some(msg) = app.rx.recv() => app.apply_result(msg),
            _ = tick.tick() => {
                if let Some((_, at)) = &app.toast {
                    if at.elapsed() > Duration::from_secs(5) {
                        app.toast = None;
                    }
                }
                if app.tab == Tab::Status
                    && !app.status_loading
                    && app
                        .status_refreshed
                        .map(|t| t.elapsed() > Duration::from_secs(10))
                        .unwrap_or(true)
                {
                    app.request(Tab::Status);
                }
            }
        }
    }
    Ok(())
}

fn on_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    // Popups first.
    if app.popup.is_some() {
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') if matches!(app.popup, Some(Popup::Sql)) => {
                app.popup = None;
            }
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') if matches!(app.popup, Some(Popup::Help)) => {
                app.popup = None;
            }
            KeyCode::Up => app.sql_scroll = app.sql_scroll.saturating_sub(3),
            KeyCode::Down | KeyCode::Char('j') => app.sql_scroll += 3,
            _ => {}
        }
        return;
    }

    if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }

    let editing = app.view(app.tab).editing;
    if editing {
        let view = app.view_mut(app.tab);
        match code {
            KeyCode::Esc => view.editing = false,
            KeyCode::Tab => view.focused = (view.focused + 1) % view.fields.len(),
            KeyCode::BackTab => {
                view.focused = (view.focused + view.fields.len() - 1) % view.fields.len();
            }
            KeyCode::Backspace => {
                if let Some(f) = view.fields.get_mut(view.focused) {
                    f.value.pop();
                }
            }
            KeyCode::Enter => {
                view.editing = false;
                let tab = app.tab;
                app.request(tab);
            }
            KeyCode::Char(c) => {
                if let Some(f) = view.fields.get_mut(view.focused) {
                    f.value.push(c);
                }
            }
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('c') => {
            app.chain_idx = (app.chain_idx + 1) % app.chains.len();
            for tab in Tab::ALL {
                let v = app.view_mut(tab);
                v.rows.clear();
                v.queried = false;
                v.selected = 0;
            }
            app.notify(format!("switched to chain {}", app.chain()));
            app.request(Tab::Status);
            let tab = app.tab;
            if tab != Tab::Status {
                app.request(tab);
            }
        }
        KeyCode::Char('s') => {
            app.sql_scroll = 0;
            app.popup = Some(Popup::Sql);
        }
        KeyCode::Char('?') => app.popup = Some(Popup::Help),
        KeyCode::Char('r') => {
            let tab = app.tab;
            app.request(tab);
        }
        KeyCode::Char('f') | KeyCode::Char('/') => {
            if app.tab != Tab::Status {
                let v = app.view_mut(app.tab);
                v.editing = true;
            }
        }
        KeyCode::Tab => {
            let idx = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
            app.tab = Tab::ALL[(idx + 1) % Tab::ALL.len()];
        }
        KeyCode::BackTab => {
            let idx = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
            app.tab = Tab::ALL[(idx + Tab::ALL.len() - 1) % Tab::ALL.len()];
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let v = app.view_mut(app.tab);
            v.selected = v.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let v = app.view_mut(app.tab);
            if v.selected + 1 < v.rows.len() {
                v.selected += 1;
            }
        }
        KeyCode::PageUp => {
            let v = app.view_mut(app.tab);
            v.selected = v.selected.saturating_sub(20);
        }
        KeyCode::PageDown => {
            let v = app.view_mut(app.tab);
            if !v.rows.is_empty() {
                v.selected = (v.selected + 20).min(v.rows.len() - 1);
            }
        }
        KeyCode::Enter => {
            let tab = app.tab;
            app.request(tab);
        }
        KeyCode::Char(n @ '1'..='5') => {
            app.tab = Tab::ALL[(n as usize - '1' as usize) % Tab::ALL.len()];
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, app: &App) {
    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .split(f.area());

    draw_title(f, app, outer[0]);
    draw_tabs(f, app, outer[1]);

    if app.tab == Tab::Status {
        draw_status(f, app, outer[2], outer[3]);
    } else {
        draw_filters(f, app, outer[2]);
        draw_results(f, app, outer[3]);
    }

    draw_footer(f, app, outer[4]);

    if let Some(popup) = &app.popup {
        match popup {
            Popup::Help => draw_help(f, f.area()),
            Popup::Sql => draw_sql(f, app, f.area()),
        }
    }
}

fn draw_title(f: &mut Frame, app: &App, area: Rect) {
    let chain_name = app
        .config
        .chains
        .get(&app.chain().to_string())
        .map(|c| c.name.as_str())
        .unwrap_or("");
    let line = Line::from(vec![
        Span::styled(" ◆ OpenChain", Style::new().bold().fg(Color::Cyan)),
        Span::styled(
            format!("  chain {} {}", app.chain(), chain_name),
            Style::new().fg(Color::Yellow),
        ),
        Span::styled(
            format!("  ·  c to cycle chains ({}/{})", app.chain_idx + 1, app.chains.len()),
            Style::new().dim(),
        ),
    ]);
    f.render_widget(line, area);
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let spans: Vec<Span> = Tab::ALL
        .iter()
        .flat_map(|t| {
            let active = *t == app.tab;
            vec![
                Span::styled("[", Style::new().dim()),
                Span::styled(
                    t.label(),
                    if active {
                        Style::new().bold().fg(Color::Cyan)
                    } else {
                        Style::new().dim()
                    },
                ),
                Span::styled("]  ", Style::new().dim()),
            ]
        })
        .collect();
    f.render_widget(Line::from(spans), area);
}

fn draw_filters(f: &mut Frame, app: &App, area: Rect) {
    let view = app.view(app.tab);
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (i, field) in view.fields.iter().enumerate() {
        let focused = i == view.focused && view.editing;
        let label = Span::styled(
            format!("{}:", field.label),
            if focused {
                Style::new().bold().fg(Color::Cyan)
            } else {
                Style::new().dim()
            },
        );
        let val = if field.value.is_empty() { "–".to_string() } else { field.value.clone() };
        let value = Span::styled(
            format!("{}{} ", val, if focused { "▏" } else { " " }),
            if focused {
                Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
            } else if field.value.is_empty() {
                Style::new().dim()
            } else {
                Style::new()
            },
        );
        spans.push(label);
        spans.push(value);
    }
    let block = Block::bordered()
        .title(Span::styled(
            if view.editing { " filters — Tab next field · Enter run · Esc done " } else { " filters — f to edit · Enter run " },
            Style::new().fg(if view.editing { Color::Yellow } else { Color::DarkGray }),
        ))
        .border_style(Style::new().fg(if view.editing { Color::Yellow } else { Color::DarkGray }));
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn draw_status(f: &mut Frame, app: &App, head_area: Rect, table_area: Rect) {
    let head_text = if app.status_loading {
        "refreshing…".to_string()
    } else if app.head_line.is_empty() {
        "press r or wait for auto-refresh".to_string()
    } else {
        app.head_line.clone()
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" head ", Style::new().dim()),
            Span::styled(head_text, Style::new().fg(Color::Green)),
        ]))
        .block(Block::bordered()),
        head_area,
    );

    let header = Row::new(["dataset", "rows", "min_block", "max_block", "watermark"])
        .style(Style::new().bold().fg(Color::Cyan));
    let rows = app.status_rows.iter().map(|(name, total, min, max, wm)| {
        Row::new(vec![
            name.clone(),
            crate::query::group_decimal(&total.to_string()),
            cell_opt(*min),
            cell_opt(*max),
            cell_opt(*wm),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .block(status_block(app));
    f.render_widget(table, table_area);
}

fn status_block(app: &App) -> Block<'_> {
    let fresh = app
        .status_refreshed
        .map(|t| format!(" refreshed {}s ago ", t.elapsed().as_secs()))
        .unwrap_or_default();
    Block::bordered().title(Span::styled(
        format!(" datasets{fresh} "),
        Style::new().fg(Color::DarkGray),
    ))
}

fn cell_opt(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".into())
}

fn draw_results(f: &mut Frame, app: &App, area: Rect) {
    let view = app.view(app.tab);

    if let Some(err) = &view.error {
        let p = Paragraph::new(format!(" ✗ {err}"))
            .style(Style::new().fg(Color::Red))
            .wrap(Wrap { trim: false })
            .block(results_block(app));
        f.render_widget(p, area);
        return;
    }
    if view.loading {
        let p = Paragraph::new(" ⠋ querying…")
            .style(Style::new().fg(Color::Yellow))
            .block(results_block(app));
        f.render_widget(p, area);
        return;
    }
    if view.rows.is_empty() {
        let msg = if view.queried {
            "no matching rows — tweak filters (f) and press Enter"
        } else {
            "press Enter to query · f to edit filters"
        };
        f.render_widget(
            Paragraph::new(format!(" {msg}"))
                .style(Style::new().dim())
                .block(results_block(app)),
            area,
        );
        return;
    }

    let widths: Vec<Constraint> = view
        .columns
        .iter()
        .map(|c| match *c {
            "params" => Constraint::Percentage(40),
            "value_human" | "volume_human" | "value_eth_human" => Constraint::Length(20),
            "tx_hash" | "addr" | "from" | "to" | "contract" => Constraint::Length(18),
            _ => Constraint::Length(12),
        })
        .collect();

    let header =
        Row::new(view.columns.iter().map(|c| pretty_column(c))).style(Style::new().bold().fg(Color::Cyan));
    let rows = view.rows.iter().map(|r| {
        Row::new(view.columns.iter().map(|c| Cell::from(cell_str(r.get(*c)))))
    });

    let mut state = TableState::default().with_selected(Some(view.selected));
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::new().bg(Color::DarkGray))
        .block(results_block(app));
    f.render_stateful_widget(table, area, &mut state);
}

fn results_block(app: &App) -> Block<'_> {
    let v = app.view(app.tab);
    Block::bordered().title(Span::styled(
        format!(" {} — {} rows ", tab_title(app.tab), v.rows.len()),
        Style::new().fg(Color::DarkGray),
    ))
}

fn tab_title(tab: Tab) -> &'static str {
    match tab {
        Tab::Status => "status",
        Tab::Transfers => "transfers",
        Tab::Txs => "transactions",
        Tab::Events => "decoded events",
        Tab::Top => "top movers",
    }
}

fn pretty_column(c: &str) -> String {
    match c {
        "value_human" | "value_eth_human" | "volume_human" => "value".into(),
        "value_raw" => "value (raw)".into(),
        "tx_hash" => "tx".into(),
        "block_number" => "block".into(),
        "log_index" => "log".into(),
        "addr" => "address".into(),
        "rank" => "#".into(),
        "contract_name" => "contract".into(),
        "event_name" => "event".into(),
        other => other.replace('_', " "),
    }
}

fn cell_str(v: Option<&Value>) -> String {
    let s = match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    if s.is_empty() || s == "0x" {
        "-".into()
    } else if s.starts_with("0x") && s.len() >= 20 && !s.contains(' ') {
        crate::query::short_hash(&s)
    } else {
        s
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(
        " enter run · f filters · ↑↓ scroll · s sql · ? help · q quit ",
        Style::new().dim(),
    )];
    if let Some((msg, _)) = &app.toast {
        spans.push(Span::styled(format!("  ✦ {msg}"), Style::new().fg(Color::Green)));
    }
    if let Some(v) = app.views.get(&app.tab) {
        if let Some(err) = &v.error {
            spans = vec![Span::styled(format!(" ✗ {err}"), Style::new().fg(Color::Red))];
        }
    }
    f.render_widget(Line::from(spans), area);
}

fn centered(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let v = Layout::vertical([Constraint::Percentage(percent_y)]).split(area);
    let h = Layout::horizontal([Constraint::Percentage(percent_x)]).split(v[0]);
    h[0]
}

fn draw_help(f: &mut Frame, area: Rect) {
    let area = centered(62, 70, area);
    let lines = vec![
        Line::from(Span::styled("OpenChain TUI — keys", Style::new().bold().fg(Color::Cyan))),
        Line::from(""),
        Line::from(vec![key("1-5 / Tab"), text("switch view: status · transfers · txs · events · top")]),
        Line::from(vec![key("enter"), text("run the query for this view")]),
        Line::from(vec![key("f"), text("edit filters (Tab between fields, Enter to run)")]),
        Line::from(vec![key("↑ ↓ / j k"), text("scroll results")]),
        Line::from(vec![key("pgup pgdn"), text("fast scroll")]),
        Line::from(vec![key("s"), text("show the generated SQL")]),
        Line::from(vec![key("r"), text("refresh current view")]),
        Line::from(vec![key("c"), text("cycle configured chains")]),
        Line::from(vec![key("?"), text("this help")]),
        Line::from(vec![key("q / ctrl-c"), text("quit")]),
        Line::from(""),
        Line::from(Span::styled(
            "Filters speak the same grammar as the CLI: 3m months · 24h hours ·\nk/m/b amounts (100k) · eth/gwei units · ENS names in from/to.",
            Style::new().dim(),
        )),
    ];
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" help ").border_style(Style::new().fg(Color::Cyan))),
        area,
    );
}

fn key(name: &str) -> Span<'static> {
    Span::styled(format!(" {name:<12}"), Style::new().bold().fg(Color::Yellow))
}

fn text(s: &str) -> Span<'static> {
    Span::raw(format!(" {s}"))
}

fn draw_sql(f: &mut Frame, app: &App, area: Rect) {
    let area = centered(80, 60, area);
    let sql = app.view(app.tab).last_sql.clone();
    let sql = if sql.is_empty() { "(run a query first)".to_string() } else { sql };
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(sql)
            .wrap(Wrap { trim: false })
            .scroll((app.sql_scroll as u16, 0))
            .block(
                Block::bordered()
                    .title(" generated SQL — esc close · ↑↓ scroll ")
                    .border_style(Style::new().fg(Color::Magenta)),
            ),
        area,
    );
}
