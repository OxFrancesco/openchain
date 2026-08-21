use super::{
    ch, merge_rows_by_volume, render_with_csv, resolve_range, target_chains, tokens, OutputArgs,
    RangeArgs,
};
use clap::Args;
use eyre::{bail, Result};
use openchain_core::Config;
use serde_json::{json, Value};

/// keccak256("Transfer(address,address,uint256)")
const TRANSFER_TOPIC0: &str = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// Rank the biggest movers.
///
/// "who received the most USDT in the last 24 hours" →
/// `openchain top --chain 1 --token usdt --since 24h`
#[derive(Args, Debug)]
pub struct TopArgs {
    /// Chain id; omit to rank across every configured chain
    #[arg(long)]
    chain: Option<u64>,
    /// Token symbol or contract address (exactly one, unless --native)
    #[arg(long, value_name = "SYM|ADDR", conflicts_with = "native")]
    token: Option<String>,
    /// Rank native ETH movers from the transactions dataset instead of a token
    #[arg(long)]
    native: bool,
    /// Which side to rank: senders or recipients (default: recipients)
    #[arg(long, default_value = "recipients")]
    side: String,
    #[command(flatten)]
    range: RangeArgs,
    /// Minimum per-transfer amount in token units (or wei when --native)
    #[arg(long, value_name = "AMOUNT")]
    min_value: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

pub async fn run(config: &Config, args: &TopArgs) -> Result<()> {
    if !args.native && args.token.is_none() {
        bail!("pass --token <SYM|ADDR> or --native");
    }
    let side = args.side.to_ascii_lowercase();
    if side != "senders" && side != "recipients" {
        bail!("--side expects 'senders' or 'recipients' (got '{}')", args.side);
    }

    // Native ETH path: single unit, no token resolution needed.
    if args.native {
        return run_native(config, args, &side).await;
    }
    let token_input = args.token.clone().unwrap();
    let parts: Vec<&str> = token_input.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if parts.len() != 1 {
        bail!("top ranks one asset at a time — pass a single --token");
    }

    let chains = target_chains(config, args.chain)?;
    let min_raw_per_chain = |decimals: u32| -> Result<Option<String>> {
        match &args.min_value {
            Some(v) => {
                let n = super::parse_amount(v)?;
                Ok(Some(format!("{:.0}", n * 10f64.powi(decimals as i32))))
            }
            None => Ok(None),
        }
    };

    let mut all_rows: Vec<Value> = Vec::new();
    let mut used_symbol = String::new();
    for c in &chains {
        // Skip chains where this symbol does not exist instead of failing the whole query.
        let token = match tokens::resolve(config, *c, &token_input).await {
            Ok(t) => t,
            Err(_) => continue,
        };
        used_symbol = token.symbol.clone();
        let (lo, hi) = resolve_range(config, *c, &args.range).await?;
        let block_filter = range_clause(lo, hi);
        let min_raw = min_raw_per_chain(token.decimals)?;
        let sql = top_sql(
            *c,
            &side,
            &block_filter,
            Some(&token.address),
            TRANSFER_TOPIC0,
            min_raw.as_deref(),
            false,
        );
        let body = ch(config, &format!("{sql} FORMAT JSONEachRow"), "JSONEachRow").await?;
        all_rows.extend(enrich(&body, *c, &used_symbol, token.decimals));
    }
    if all_rows.is_empty() && chains.len() > 1 {
        bail!("unknown token '{token_input}' on every configured chain");
    }

    finish(all_rows, args, &side, &used_symbol)
}

async fn run_native(config: &Config, args: &TopArgs, side: &str) -> Result<()> {
    let min_raw = match &args.min_value {
        Some(v) => Some(format!("{}", super::parse_native_amount(v)? as u128)),
        None => None,
    };
    let chains = target_chains(config, args.chain)?;
    let multi = chains.len() > 1;
    let mut all_rows: Vec<Value> = Vec::new();
    for c in &chains {
        if multi && !super::has_blocks(config, *c).await {
            continue;
        }
        let (lo, hi) = resolve_range(config, *c, &args.range).await?;
        let block_filter = range_clause(lo, hi);
        let sql = top_sql(*c, side, &block_filter, None, "", min_raw.as_deref(), true);
        let body = ch(config, &format!("{sql} FORMAT JSONEachRow"), "JSONEachRow").await?;
        all_rows.extend(enrich(&body, *c, "ETH", 18));
    }
    finish(all_rows, args, side, "ETH")
}

#[allow(clippy::too_many_arguments)]
fn top_sql(
    chain: u64,
    side: &str,
    block_filter: &str,
    token_address: Option<&str>,
    topic0: &str,
    min_raw: Option<&str>,
    native: bool,
) -> String {
    let limit = 10_000; // aggregate fully server-side; ranking happens over groups
    let (group_expr, filters_head) = if native {
        let col = if side == "senders" { "from_address" } else { "to_address" };
        (format!("hex({col})"), String::new())
    } else {
        let topic_col = if side == "senders" { "topic1" } else { "topic2" };
        (
            format!("hex(substring({topic_col}, 13, 20))"),
            format!(
                " AND topic0 = unhex('{topic0}') AND length(data) = 32 \
                 AND address = unhex('{}')",
                token_address.unwrap_or_default()
            ),
        )
    };
    let value_expr =
        if native { "toString(sum(value))".to_string() } else { "toString(sum(reinterpretAsUInt256(reverse(data))))".to_string() };
    let order_expr =
        if native { "sum(value)".to_string() } else { "sum(reinterpretAsUInt256(reverse(data)))".to_string() };

    let mut filters = String::new();
    if let (true, Some(min)) = (native, min_raw) {
        filters.push_str(&format!(" AND value >= {min}"));
    }
    if !native {
        if let Some(min) = min_raw {
            filters.push_str(&format!(" AND reinterpretAsUInt256(reverse(data)) >= toUInt256({min})"));
        }
    }

    format!(
        "SELECT {group_expr} AS addr, count() AS txs, {value_expr} AS volume_raw \
         FROM {} WHERE chain_id = {chain} AND {block_filter}{filters_head}{filters} \
         GROUP BY addr ORDER BY {order_expr} DESC LIMIT {limit}",
        if native { "transactions" } else { "logs" },
    )
}

fn enrich(body: &str, chain: u64, symbol: &str, decimals: u32) -> Vec<Value> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap_or(Value::Null);
            let raw = v.get("volume_raw").and_then(|x| x.as_str()).unwrap_or("0").to_owned();
            let human = super::human_amount(&raw, decimals);
            json!({
                "addr": format!("0x{}", v.get("addr").and_then(|x| x.as_str()).unwrap_or("")),
                "txs": v.get("txs").cloned().unwrap_or(json!(0)),
                "volume": human,
                "volume_human": super::group_decimal(&human),
                "volume_raw": raw,
                "symbol": symbol,
                "chain_id": chain,
            })
        })
        .collect()
}

fn finish(mut rows: Vec<Value>, args: &TopArgs, side: &str, symbol: &str) -> Result<()> {
    rows = merge_rows_by_volume(rows, args.out.limit);

    if args.out.sql || args.out.count {
        // Aggregation queries are built per chain at run time; nothing generic to print.
        eprintln!("note: --sql/--count are not supported for top (per-chain aggregates)");
    }

    let label = format!("{side} by {}", symbol.to_lowercase());
    render_with_csv(
        &rows,
        &[
            ("rank", "#"),
            ("addr", &format!("{} ({})", side.trim_end_matches('s'), symbol)),
            ("volume_human", &format!("total ({symbol})")),
            ("txs", "txs"),
        ],
        &[
            ("rank", "rank"),
            ("addr", "address"),
            ("txs", "txs"),
            ("volume", "total"),
            ("volume_raw", "total_raw"),
            ("symbol", "symbol"),
            ("chain_id", "chain_id"),
        ],
        &args.out,
        &label,
    )?;
    Ok(())
}

fn range_clause(lo: Option<u64>, hi: Option<u64>) -> String {
    match (lo, hi) {
        (Some(a), Some(b)) => format!("block_number BETWEEN {a} AND {b}"),
        (Some(a), None) => format!("block_number >= {a}"),
        (None, Some(b)) => format!("block_number <= {b}"),
        (None, None) => "1".to_string(),
    }
}
