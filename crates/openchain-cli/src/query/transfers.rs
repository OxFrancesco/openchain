use super::{
    ch, merge_by_ts, parse_address_or_name, render_with_csv, resolve_range, target_chains,
    OutputArgs, RangeArgs,
};
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
    /// Chain id; omit to query every configured chain
    #[arg(long)]
    chain: Option<u64>,
    /// Token symbol (usdt) or contract address; comma-separate for several
    #[arg(long, value_name = "SYM|ADDR")]
    token: String,
    /// Sender address or ENS name
    #[arg(long, value_name = "ADDR|NAME")]
    from: Option<String>,
    /// Recipient address or ENS name
    #[arg(long, value_name = "ADDR|NAME")]
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
    let chains = target_chains(config, args.chain)?;
    let from_filter = match &args.from {
        Some(v) => Some(parse_address_or_name(config, v, "--from").await?),
        None => None,
    };
    let to_filter = match &args.to {
        Some(v) => Some(parse_address_or_name(config, v, "--to").await?),
        None => None,
    };

    let mut all_rows: Vec<Value> = Vec::new();
    let mut skipped_chains: Vec<u64> = Vec::new();
    let mut no_data_chains: Vec<u64> = Vec::new();

    let multi = chains.len() > 1;
    for c in &chains {
        if multi && !super::has_blocks(config, *c).await {
            no_data_chains.push(*c);
            continue;
        }
        // Resolve every requested token on this chain; skip chains where a
        // bare symbol does not exist instead of failing the whole query.
        // Single-chain queries surface the error instead.
        let mut resolved = Vec::new();
        let mut missing = false;
        for t in args.token.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match tokens::resolve(config, *c, t).await {
                Ok(tok) => resolved.push(tok),
                Err(e) => {
                    if multi {
                        missing = true;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        if missing {
            skipped_chains.push(*c);
            continue;
        }
        let decimals = resolved[0].decimals;
        let addresses: Vec<String> =
            resolved.iter().map(|t| format!("unhex('{}')", t.address)).collect();

        // Human amounts → raw integer strings (decimal math on the scaled value).
        let scale = |amount: &str| -> Result<String> {
            let n = super::parse_amount(amount)?;
            if n < 0.0 {
                bail!("--min-value/--max-value must be positive");
            }
            Ok(format!("{:.0}", n * 10f64.powi(decimals as i32)))
        };
        let min_raw = match &args.min_value { Some(v) => Some(scale(v)?), None => None };
        let max_raw = match &args.max_value { Some(v) => Some(scale(v)?), None => None };

        let sql = build_sql(
            *c,
            &addresses,
            from_filter.as_deref(),
            to_filter.as_deref(),
            min_raw.as_deref(),
            max_raw.as_deref(),
            &resolve_range(config, *c, &args.range).await?,
            args.out.count,
            args.out.limit,
        );

        if args.out.sql && chains.len() == 1 {
            println!("{sql}");
            return Ok(());
        }
        if args.out.count {
            if chains.len() == 1 {
                let body = ch(config, &format!("SELECT count() AS n FROM ({sql})"), "TSV").await?;
                println!("{}", body.trim());
                return Ok(());
            }
            let body = ch(config, &format!("SELECT count() AS n FROM ({sql})"), "TSV").await?;
            all_rows.push(json!({"count": body.trim().parse::<u64>().unwrap_or(0)}));
            continue;
        }

        let body = ch(config, &format!("{sql} FORMAT JSONEachRow"), "JSONEachRow").await?;
        all_rows.extend(enrich_rows(&body, *c, decimals, &symbol_label(&resolved)));
    }

    if args.out.sql && chains.len() > 1 {
        bail!("--sql needs an explicit --chain when querying multiple chains");
    }
    if args.out.count {
        let total: u64 = all_rows.iter().filter_map(|r| r.get("count").and_then(|v| v.as_u64())).sum();
        println!("{total}");
        return Ok(());
    }

    let rows = merge_by_ts(all_rows, args.out.limit);
    let value_label = format!(
        "value ({})",
        rows.first()
            .and_then(|r| r.get("symbol"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    render_rows(&rows, args, multi, skipped_chains, no_data_chains, &value_label)?;
    Ok(())
}

fn symbol_label(resolved: &[tokens::Token]) -> String {
    resolved.iter().map(|t| t.symbol.as_str()).collect::<Vec<_>>().join("/")
}

#[allow(clippy::too_many_arguments)]
fn build_sql(
    chain: u64,
    addresses: &[String],
    from: Option<&str>,
    to: Option<&str>,
    min_raw: Option<&str>,
    max_raw: Option<&str>,
    range: &(Option<u64>, Option<u64>),
    count: bool,
    limit: u64,
) -> String {
    let (lo, hi) = *range;
    let block_filter = match (lo, hi) {
        (Some(a), Some(b)) => format!("block_number BETWEEN {a} AND {b}"),
        (Some(a), None) => format!("block_number >= {a}"),
        (None, Some(b)) => format!("block_number <= {b}"),
        (None, None) => "1".to_string(),
    };

    let mut filters = String::new();
    filters.push_str(&format!(
        " AND topic0 = unhex('{TRANSFER_TOPIC0}') AND length(data) = 32"
    ));
    filters.push_str(&format!(" AND address IN ({})", addresses.join(", ")));
    if let Some(from) = from {
        filters.push_str(&format!(" AND substring(topic1, 13, 20) = unhex('{from}')"));
    }
    if let Some(to) = to {
        filters.push_str(&format!(" AND substring(topic2, 13, 20) = unhex('{to}')"));
    }
    if let Some(min) = min_raw {
        filters.push_str(&format!(" AND reinterpretAsUInt256(reverse(data)) >= toUInt256({min})"));
    }
    if let Some(max) = max_raw {
        filters.push_str(&format!(" AND reinterpretAsUInt256(reverse(data)) <= toUInt256({max})"));
    }

    let order = if count { "" } else { " ORDER BY block_number DESC, log_index DESC" };
    let limit = if count { String::new() } else { format!(" LIMIT {limit}") };

    format!(
        "SELECT block_number, concat('0x', lower(hex(tx_hash))) AS tx_hash, tx_index, log_index, \
         concat('0x', lower(hex(address))) AS token, \
         concat('0x', lower(hex(substring(topic1, 13, 20)))) AS `from`, \
         concat('0x', lower(hex(substring(topic2, 13, 20)))) AS `to`, \
         toString(reinterpretAsUInt256(reverse(data))) AS value_raw, \
         toUnixTimestamp(b.timestamp) AS ts \
         FROM logs l \
         INNER JOIN (SELECT block_number, timestamp FROM blocks WHERE chain_id = {chain}) b \
           USING (block_number) \
         WHERE l.chain_id = {chain} AND {block_filter}{filters}{order}{limit}"
    )
}

fn enrich_rows(body: &str, chain: u64, decimals: u32, symbol: &str) -> Vec<Value> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    body.lines()
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
                obj.insert("chain_id".into(), json!(chain));
                obj.insert("time".into(), json!(super::iso_time(ts)));
                obj.insert("age".into(), json!(super::human_age(ts, now)));
            }
            v
        })
        .collect()
}

fn render_rows(
    rows: &[Value],
    args: &TransfersArgs,
    multi_chain: bool,
    skipped: Vec<u64>,
    no_data: Vec<u64>,
    value_label: &str,
) -> Result<()> {
    if multi_chain && matches!(args.out.format(), super::Format::Table) {
        if !no_data.is_empty() {
            eprintln!(
                "note: no synced data on chain(s) {} — queried the rest",
                no_data.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")
            );
        }
        if !skipped.is_empty() {
            eprintln!(
                "note: token '{}' unknown on chain(s) {} — queried the rest",
                args.token,
                skipped.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")
            );
        }
    }
    let mut table_cols = vec![
        ("age", "age"),
        ("value_human", value_label),
        ("from", "from"),
        ("to", "to"),
        ("tx_hash", "tx"),
        ("block_number", "block"),
    ];
    let mut csv_cols = vec![
        ("chain_id", "chain_id"),
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
    ];
    if multi_chain {
        table_cols.insert(0, ("chain_id", "chain"));
    } else {
        csv_cols.retain(|(n, _)| *n != "chain_id");
    }
    render_with_csv(rows, &table_cols, &csv_cols, &args.out, "transfers")?;
    Ok(())
}
