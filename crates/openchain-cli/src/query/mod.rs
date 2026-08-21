pub mod ens;
pub mod events;
pub mod tokens;
pub mod top;
pub mod transfers;
pub mod txs;

pub use ens::parse_address_or_name;

use clap::ValueEnum;
use eyre::{bail, eyre, Context, Result};
use openchain_core::Config;
use serde_json::{json, Value};
use time::format_description::well_known::Iso8601;
use time::macros::format_description;
use time::PrimitiveDateTime;

/// Output format for query commands.
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum Format {
    /// Aligned human-readable table (default)
    #[default]
    Table,
    /// One JSON object per line, full precision
    Jsonl,
    /// JSON array, pretty printed
    Json,
    /// CSV with header row, full precision
    Csv,
}

/// Shared output flags for every query command.
#[derive(clap::Args, Clone, Debug)]
pub struct OutputArgs {
    /// Max rows to return
    #[arg(long, default_value_t = 50)]
    pub limit: u64,
    /// Only print the number of matching rows
    #[arg(long)]
    pub count: bool,
    /// Print the generated SQL instead of running it
    #[arg(long)]
    pub sql: bool,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,
    /// Shortcut for --format json
    #[arg(long, conflicts_with = "format")]
    pub json: bool,
    /// Shortcut for --format jsonl (one JSON object per line)
    #[arg(long, conflicts_with_all = ["format", "json"])]
    pub jsonl: bool,
    /// Shortcut for --format csv
    #[arg(long, conflicts_with_all = ["format", "json", "jsonl"])]
    pub csv: bool,
}

impl OutputArgs {
    fn format(&self) -> Format {
        if self.json {
            Format::Json
        } else if self.jsonl {
            Format::Jsonl
        } else if self.csv {
            Format::Csv
        } else {
            self.format
        }
    }
}

// ---------------------------------------------------------------------------
// Address parsing + output rendering
// ---------------------------------------------------------------------------

/// Shared time/block-range flags.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct RangeArgs {
    /// Only data after this time: 90min, 24h, 7d, 3w, 3m (months), 1y, or a date 2024-01-01
    #[arg(long)]
    pub since: Option<String>,
    /// Only data before this time (same formats as --since)
    #[arg(long)]
    pub until: Option<String>,
    /// Explicit block range instead of time, open-ended: 1000..2000, ..2000, 1000..
    #[arg(long)]
    pub blocks: Option<String>,
}

/// Run a query against ClickHouse over the HTTP interface and return the raw body.
pub async fn ch(config: &Config, query: &str, format: &str) -> Result<String> {
    let ch = &config.clickhouse;
    let response = reqwest::Client::new()
        .post(&ch.url)
        .query(&[("database", ch.database.as_str()), ("default_format", format)])
        .header("X-ClickHouse-User", &ch.user)
        .header("X-ClickHouse-Key", &ch.password)
        .body(query.to_string())
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        bail!("query failed ({status}):\n{body}");
    }
    Ok(body)
}

/// Run a query returning one row of JSONEachRow values.
async fn ch_row(config: &Config, query: &str) -> Result<Value> {
    let body = ch(config, &format!("{query} FORMAT JSONEachRow"), "JSONEachRow").await?;
    body.lines()
        .next()
        .map(|line| serde_json::from_str(line).wrap_err("bad ClickHouse response"))
        .unwrap_or_else(|| Ok(Value::Null))
}

// ---------------------------------------------------------------------------
// Human input parsing
// ---------------------------------------------------------------------------

/// Parse a human duration or date into a unix timestamp (seconds).
///
/// Durations are relative to now: `90s`, `30min`, `24h`, `7d`, `3w`, `3m`
/// (months = 30 days), `1y`. Absolute dates: `2024-01-01`,
/// `2024-01-01T12:00:00Z`, `2024-01-01 12:00:00` (UTC).
pub fn parse_timestamp(input: &str) -> Result<i64> {
    let input = input.trim();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;

    // Duration form: <number><unit>. Unknown units fall through to date parsing
    // so inputs like 2024-01-01 are handled below.
    let lower = input.to_ascii_lowercase();
    let split = lower.find(|c: char| !c.is_ascii_digit() && c != '.' && c != ',');
    if let Some(pos) = split {
        let (num, unit) = lower.split_at(pos);
        if let Ok(num) = num.trim().replace(',', "").parse::<f64>() {
            let secs = match unit.trim() {
                "s" | "sec" | "secs" | "second" | "seconds" => Some(num * 1.0),
                "min" | "mins" | "minute" | "minutes" => Some(num * 60.0),
                "h" | "hr" | "hour" | "hours" => Some(num * 3600.0),
                "d" | "day" | "days" => Some(num * 86400.0),
                "w" | "week" | "weeks" => Some(num * 604800.0),
                "mo" | "month" | "months" | "m" => Some(num * 2592000.0), // 30 days
                "y" | "year" | "years" => Some(num * 31536000.0),         // 365 days
                _ => None,
            };
            if let Some(secs) = secs {
                return Ok(now - secs as i64);
            }
        }
    }

    // Date forms
    if let Ok(d) = time::Date::parse(
        input,
        format_description!("[year]-[month]-[day]"),
    ) {
        return Ok(d.with_hms(0, 0, 0).unwrap().assume_utc().unix_timestamp());
    }
    let normalized = input.replace(' ', "T");
    let normalized = if normalized.len() == 16 {
        format!("{normalized}:00")
    } else {
        normalized
    };
    if let Ok(dt) = PrimitiveDateTime::parse(
        &normalized,
        &Iso8601::PARSING,
    ) {
        return Ok(dt.assume_utc().unix_timestamp());
    }
    bail!("cannot parse time '{input}' — try 24h, 7d, 3m, or a date like 2024-01-01 / 2024-01-01T12:00:00Z")
}

/// Parse a human amount with k/m/b suffixes into f64: `100k` → 100000.
pub fn parse_amount(input: &str) -> Result<f64> {
    let cleaned: String = input.trim().replace([',', '_'], "").to_ascii_lowercase();
    let (num, mult) = match cleaned.chars().last() {
        Some('k') => (&cleaned[..cleaned.len() - 1], 1e3),
        Some('m') => (&cleaned[..cleaned.len() - 1], 1e6),
        Some('b') => (&cleaned[..cleaned.len() - 1], 1e9),
        Some('t') => (&cleaned[..cleaned.len() - 1], 1e12),
        _ => (cleaned.as_str(), 1.0),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| eyre!("invalid amount '{input}' — try 100k, 1.5m, or a plain number"))?;
    Ok(n * mult)
}

/// Parse a native-ETH amount: bare numbers are wei; `gwei` and `eth` suffixes supported.
pub fn parse_native_amount(input: &str) -> Result<f64> {
    let lower = input.trim().to_ascii_lowercase();
    // Longest suffix first: "200gwei" must not match "wei".
    let wei = if let Some(n) = lower.strip_suffix("gwei") {
        n.parse::<f64>()? * 1e9
    } else if let Some(n) = lower.strip_suffix("eth") {
        parse_amount(n)? * 1e18
    } else if let Some(n) = lower.strip_suffix("wei") {
        n.parse::<f64>()?
    } else {
        parse_amount(&lower)?
    };
    Ok(wei)
}

/// Parse an explicit block range `A..B` with open ends.
fn parse_block_range(s: &str) -> Result<(Option<u64>, Option<u64>)> {
    let (a, b) = s
        .split_once("..")
        .ok_or_else(|| eyre!("--blocks expects A..B, ..B or A.. (got '{s}')"))?;
    let from = if a.trim().is_empty() { None } else { Some(a.trim().parse::<u64>()?) };
    let to = if b.trim().is_empty() { None } else { Some(b.trim().parse::<u64>()?) };
    Ok((from, to))
}

/// Resolve --since/--until/--blocks into a concrete [from_block, to_block] window
/// using the synced blocks table for time→block mapping.
pub async fn resolve_range(
    config: &Config,
    chain: u64,
    range: &RangeArgs,
) -> Result<(Option<u64>, Option<u64>)> {
    let mut lo = None;
    let mut hi = None;

    if range.since.is_some() || range.until.is_some() {
        let coverage = ch_row(
            config,
            &format!(
                "SELECT min(block_number) AS min_b, max(block_number) AS max_b, \
                 max(toUnixTimestamp(timestamp)) AS max_ts FROM blocks WHERE chain_id = {chain}"
            ),
        )
        .await?;
        if coverage.get("min_b").and_then(|v| v.as_u64()).is_none() {
            bail!(
                "no blocks synced for chain {chain} — run `openchain sync --chain {chain} --datasets blocks,...` first"
            );
        }
        if let Some(since) = &range.since {
            let ts = parse_timestamp(since)?;
            let row = ch_row(
                config,
                &format!(
                    "SELECT min(block_number) AS b FROM blocks \
                     WHERE chain_id = {chain} AND toUnixTimestamp(timestamp) >= {ts}"
                ),
            )
            .await?;
            match row.get("b").and_then(|v| v.as_u64()) {
                Some(b) => lo = Some(b),
                None => {
                    let head_ts = coverage.get("max_ts").and_then(|v| v.as_i64()).unwrap_or(0);
                    if head_ts > 0 && ts > head_ts {
                        bail!("--since {since} is after the newest synced block (ts {head_ts})");
                    }
                }
            }
        }
        if let Some(until) = &range.until {
            let ts = parse_timestamp(until)?;
            let row = ch_row(
                config,
                &format!(
                    "SELECT max(block_number) AS b FROM blocks \
                     WHERE chain_id = {chain} AND toUnixTimestamp(timestamp) <= {ts}"
                ),
            )
            .await?;
            hi = row.get("b").and_then(|v| v.as_u64());
        }
    }

    if let Some(blocks) = &range.blocks {
        let (blo, bhi) = parse_block_range(blocks)?;
        lo = match (lo, blo) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        };
        hi = match (hi, bhi) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        };
    }

    Ok((lo, hi))
}

/// True when the blocks table has any rows for this chain.
pub async fn has_blocks(config: &Config, chain: u64) -> bool {
    ch_row(
        config,
        &format!("SELECT 1 FROM blocks WHERE chain_id = {chain} LIMIT 1"),
    )
    .await
    .map(|v| !v.is_null())
    .unwrap_or(false)
}

/// Resolve the set of chains to query: one explicit chain, or every chain
/// configured in openchain.toml when `--chain` is omitted.
pub fn target_chains(config: &Config, chain: Option<u64>) -> Result<Vec<u64>> {
    if let Some(c) = chain {
        // Validate against config so typos fail fast.
        config.chain(c)?;
        return Ok(vec![c]);
    }
    let mut chains: Vec<u64> = config
        .chains
        .keys()
        .filter_map(|k| k.parse::<u64>().ok())
        .collect();
    chains.sort_unstable();
    if chains.is_empty() {
        bail!("no [chains.<id>] sections in config — add one or pass --chain");
    }
    Ok(chains)
}

/// Merge per-chain result rows (each carrying a numeric `ts`) newest-first and
/// cap at `limit`. Rows missing `ts` sort last.
pub fn merge_by_ts(mut rows: Vec<Value>, limit: u64) -> Vec<Value> {
    rows.sort_by(|a, b| {
        let ta = a.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
        tb.cmp(&ta)
    });
    rows.truncate(limit as usize);
    rows
}

/// Merge ranked rows across chains by descending `volume_raw` (numeric when it
/// fits u128, string length otherwise), cap at `limit`, and stamp 1-based
/// `rank` on each row.
pub fn merge_rows_by_volume(rows: Vec<Value>, limit: u64) -> Vec<Value> {
    let key = |v: &Value| -> (u128, usize) {
        let raw = v.get("volume_raw").and_then(|x| x.as_str()).unwrap_or("0");
        match raw.parse::<u128>() {
            Ok(n) => (n, raw.len()),
            Err(_) => (u128::MAX, raw.len()), // bigger-than-u128 magnitudes sort by digits
        }
    };
    let mut rows: Vec<Value> = rows;
    rows.sort_by_key(|v| std::cmp::Reverse(key(v)));
    rows.truncate(limit as usize);
    rows.into_iter()
        .enumerate()
        .map(|(i, mut r)| {
            if let Some(obj) = r.as_object_mut() {
                obj.insert("rank".into(), json!((i + 1).to_string()));
            }
            r
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Address parsing + output rendering
// ---------------------------------------------------------------------------

/// Normalize a 0x address to lowercase hex without prefix; validates length/hex.
pub fn parse_address(input: &str, flag: &str) -> Result<String> {
    let s = input.trim();
    let hexpart = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if hexpart.len() != 40 || !hexpart.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{flag} expects a 20-byte address like 0xdAC17F…ec7 (got '{input}')");
    }
    Ok(hexpart.to_ascii_lowercase())
}

/// Render rows in the requested output format, with a dedicated machine-friendly
/// column set for CSV.
pub fn render_with_csv(
    rows: &[Value],
    table_columns: &[(&str, &str)],
    csv_columns: &[(&str, &str)],
    out: &OutputArgs,
    label: &str,
) -> Result<()> {
    match out.format() {
        Format::Json => {
            println!("{}", serde_json::to_string_pretty(rows)?);
        }
        Format::Jsonl => {
            for r in rows {
                println!("{r}");
            }
        }
        Format::Csv => {
            let names: Vec<&str> = csv_columns.iter().map(|(n, _)| *n).collect();
            println!("{}", names.join(","));
            for r in rows {
                let cells: Vec<String> =
                    csv_columns.iter().map(|(n, _)| csv_cell(r.get(*n))).collect();
                println!("{}", cells.join(","));
            }
        }
        Format::Table => {
            if rows.is_empty() {
                println!("no matching {label}");
                return Ok(());
            }
            let headers: Vec<&str> = table_columns.iter().map(|(_, h)| *h).collect();
            let table_cells: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    table_columns.iter().map(|(n, _)| table_cell(r.get(*n))).collect()
                })
                .collect();
            let widths: Vec<usize> = headers
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    table_cells.iter().map(|row| row[i].len()).max().unwrap_or(0).max(h.len())
                })
                .collect();
            let header: String = headers
                .iter()
                .zip(&widths)
                .map(|(h, w)| format!("{h:>w$}"))
                .collect::<Vec<_>>()
                .join("  ");
            println!("{header}");
            println!("{}", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("  "));
            for row in &table_cells {
                let line: String = row
                    .iter()
                    .zip(&widths)
                    .map(|(c, w)| format!("{c:>w$}"))
                    .collect::<Vec<_>>()
                    .join("  ");
                println!("{line}");
            }
            let n = rows.len();
            println!("\n{n} row{}", if n == 1 { "" } else { "s" });
        }
    }
    Ok(())
}

fn json_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn csv_cell(v: Option<&Value>) -> String {
    let s = json_str(v);
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

fn table_cell(v: Option<&Value>) -> String {
    let s = json_str(v);
    // Shorten long hashes/addresses in table mode only (raw fields keep full values).
    if s == "0x" || s.is_empty() {
        "-".to_string()
    } else if s.starts_with("0x") && s.len() >= 20 && !s.contains(' ') {
        short_hash(&s)
    } else {
        s
    }
}

/// Add thousands separators to the integer part of a decimal string.
pub fn group_decimal(s: &str) -> String {
    match s.split_once('.') {
        Some((int, frac)) => format!("{}.{}", group_digits(int), frac),
        None => group_digits(s),
    }
}

/// `0xdAC17F95…3ec7` style shortening.
pub fn short_hash(s: &str) -> String {
    if s.len() <= 14 {
        return s.to_string();
    }
    format!("{}…{}", &s[..10], &s[s.len() - 4..])
}

/// Group digits with commas: 1234567.89 → "1,234,567.89".
pub fn group_digits(int_part: &str) -> String {
    let negative = int_part.starts_with('-');
    let digits = int_part.trim_start_matches('-');
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    if negative {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Format a raw token amount (decimal string) with decimals into a human string.
pub fn human_amount(raw: &str, decimals: u32) -> String {
    let raw = raw.trim();
    let digits = raw.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let dec = decimals as usize;
    let padded;
    let (int_raw, frac_raw): (&str, &str) = if digits.len() > dec {
        digits.split_at(digits.len() - dec)
    } else {
        padded = format!("{:0>width$}", digits, width = dec);
        ("0", &padded)
    };
    let int_clean = int_raw.trim_start_matches('0');
    let int_clean = if int_clean.is_empty() { "0" } else { int_clean };
    let frac_trimmed = frac_raw.trim_end_matches('0');
    if frac_trimmed.is_empty() {
        int_clean.to_string()
    } else {
        // Cap runaway fractions at 12 places; keep enough for tiny amounts.
        let shown = &frac_trimmed[..frac_trimmed.len().min(12)];
        format!("{}.{}", int_clean, shown)
    }
}

/// Relative age like `5m`, `3h`, `12d` from a unix timestamp.
pub fn human_age(ts: i64, now: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=59 => format!("{d}s"),
        60..=3599 => format!("{}m", d / 60),
        3600..=86399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86400),
    }
}

/// ISO-8601 UTC timestamp from unix seconds.
pub fn iso_time(ts: i64) -> String {
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
