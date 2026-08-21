use super::{ch, parse_address, render_with_csv, resolve_range, OutputArgs, RangeArgs};
use crate::query::tokens;
use clap::Args;
use eyre::{bail, Result};
use openchain_core::Config;
use serde_json::{json, Value};

/// keccak256("Transfer(address,address,uint256)")
const TRANSFER_TOPIC0: &str = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// Token transfers with human flags — the fast path from
/// "show me all USDT transfers over 100k in the last 3 months" to SQL.
#[derive(Args, Debug)]
pub struct TransfersArgs {
    /// Chain id (must exist in config)
    #[arg(long)]
    chain: u64,
    /// Token symbol (usdt) or contract address; comma-separate for several
    #[arg(long, value_name = "SYM|ADDR")]
    token: String,
    /// Sender address
    #[arg(long, value_name = "ADDR")]
    from: Option<String>,
    /// Recipient address
    #[arg(long, value_name = "ADDR")]
    to: Option<String>,
    #[command(flatten)]
    range: RangeArgs,
    /// Minimum transferred amount in token units: 100k, 1.5m, 0.5b
    #[arg(long, value_name = "AMOUNT")]
    min_value: Option<String>,
    /// Maximum transferred amount in token units
    #[arg(long, value_name = "AMOUNT")]
    max_value: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

pub async fn run(config: &Config, args: &TransfersArgs) -> Result<()> {
    let (lo, hi) = resolve_range(config, args.chain, &args.range).await?;

    // Resolve every requested token.
    let mut resolved = Vec::new();
    for t in args.token.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        resolved.push(tokens::resolve(config, args.chain, t).await?);
    }
    let addresses: Vec<String> =
        resolved.iter().map(|t| format!("unhex('{}')", t.address)).collect();
    let decimals = resolved[0].decimals;
    let symbol = resolved.iter().map(|t| t.symbol.as_str()).collect::<Vec<_>>().join("/");

    // Human amounts → raw integer strings (decimal math on the scaled value).
    let scale = |amount: &str| -> Result<String> {
        let n = super::parse_amount(amount)?;
        if n < 0.0 {
            bail!("--min-value/--max-value must be positive");
        }
        Ok(format!("{:.0}", n * 10f64.powi(decimals as i32)))
    };
    let min_raw = match &args.min_value {
        Some(v) => Some(scale(v)?),
        None => None,
    };
    let max_raw = match &args.max_value {
        Some(v) => Some(scale(v)?),
        None => None,
    };

    let mut filters = String::new();
    filters.push_str(&format!(
        " AND topic0 = unhex('{TRANSFER_TOPIC0}') AND length(data) = 32"
    ));
    filters.push_str(&format!(" AND address IN ({})", addresses.join(", ")));
    if let Some(from) = &args.from {
        let a = parse_address(from, "--from")?;
        filters.push_str(&format!(" AND substring(topic1, 13, 20) = unhex('{a}')"));
    }
    if let Some(to) = &args.to {
        let a = parse_address(to, "--to")?;
        filters.push_str(&format!(" AND substring(topic2, 13, 20) = unhex('{a}')"));
    }
    if let Some(min) = &min_raw {
        filters.push_str(&format!(" AND reinterpretAsUInt256(reverse(data)) >= toUInt256({min})"));
    }
    if let Some(max) = &max_raw {
        filters.push_str(&format!(" AND reinterpretAsUInt256(reverse(data)) <= toUInt256({max})"));
    }

    let order = if args.out.count { "" } else { " ORDER BY block_number DESC, log_index DESC" };
    let limit = if args.out.count { String::new() } else { format!(" LIMIT {}", args.out.limit) };
    let block_filter = match (lo, hi) {
        (Some(a), Some(b)) => format!("block_number BETWEEN {a} AND {b}"),
        (Some(a), None) => format!("block_number >= {a}"),
        (None, Some(b)) => format!("block_number <= {b}"),
        (None, None) => "1".to_string(),
    };

    let sql = format!(
        "SELECT block_number, concat('0x', lower(hex(tx_hash))) AS tx_hash, tx_index, log_index, \
         concat('0x', lower(hex(address))) AS token, \
         concat('0x', lower(hex(substring(topic1, 13, 20)))) AS `from`, \
         concat('0x', lower(hex(substring(topic2, 13, 20)))) AS `to`, \
         toString(reinterpretAsUInt256(reverse(data))) AS value_raw, \
         toUnixTimestamp(b.timestamp) AS ts \
         FROM logs l \
         INNER JOIN (SELECT block_number, timestamp FROM blocks WHERE chain_id = {c}) b \
           USING (block_number) \
         WHERE l.chain_id = {c} AND {block_filter}{filters}{order}{limit}",
        c = args.chain,
    );

    if args.out.sql {
        println!("{sql}");
        return Ok(());
    }

    if args.out.count {
        let count_sql = format!("SELECT count() AS n FROM ({sql})");
        let body = ch(config, &count_sql, "TSV").await?;
        println!("{}", body.trim());
        return Ok(());
    }

    let body = ch(config, &format!("{sql} FORMAT JSONEachRow"), "JSONEachRow").await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;

    let rows: Vec<Value> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut v: Value = serde_json::from_str(l).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                let raw = obj.get("value_raw").and_then(|x| x.as_str()).unwrap_or("0").to_owned();
                let ts = obj.get("ts").and_then(|x| x.as_i64()).unwrap_or(0);
                let human = super::human_amount(&raw, decimals);
                obj.insert("value".into(), json!(human));
                obj.insert("value_human".into(), json!(super::group_decimal(&human)));
                obj.insert("symbol".into(), json!(symbol));
                obj.insert("time".into(), json!(iso_time(ts)));
                obj.insert("age".into(), json!(super::human_age(ts, now)));
            }
            v
        })
        .collect();

    render_with_csv(
        &rows,
        &[
            ("age", "age"),
            ("value_human", &format!("value ({symbol})")),
            ("from", "from"),
            ("to", "to"),
            ("tx_hash", "tx"),
            ("block_number", "block"),
        ],
        &[
            ("block_number", "block"),
            ("time", "time"),
            ("value", "value"),
            ("value_raw", "value_raw"),
            ("symbol", "symbol"),
            ("token", "token"),
            ("from", "from"),
            ("to", "to"),
            ("tx_hash", "tx_hash"),
            ("log_index", "log_index"),
        ],
        &args.out,
        "transfers",
    )?;
    Ok(())
}

fn iso_time(ts: i64) -> String {
    // ClickHouse gives us the unix ts; format compactly for JSON output.
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, (secs % 3600) / 60, secs % 60)
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
