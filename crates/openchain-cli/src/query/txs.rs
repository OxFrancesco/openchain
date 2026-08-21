use super::{
    ch, merge_by_ts, parse_address_or_name, render_with_csv, resolve_range, target_chains,
    OutputArgs, RangeArgs,
};
use clap::Args;
use eyre::{bail, Result};
use openchain_core::Config;
use serde_json::{json, Value};

/// Transactions with human flags.
///
/// "show me transactions from this address worth over 10 ETH last week" →
/// `openchain txs --chain 1 --from 0x… --since 7d --min-value 10eth`
#[derive(Args, Debug)]
pub struct TxsArgs {
    /// Chain id; omit to query every configured chain
    #[arg(long)]
    chain: Option<u64>,
    /// Sender address or ENS name
    #[arg(long, value_name = "ADDR|NAME")]
    from: Option<String>,
    /// Recipient address or ENS name
    #[arg(long, value_name = "ADDR|NAME")]
    to: Option<String>,
    /// Method selector: 4-byte hex (0xa9059cbb) or bare hex (a9059cbb)
    #[arg(long, value_name = "SELECTOR")]
    method: Option<String>,
    /// success or reverted
    #[arg(long)]
    status: Option<String>,
    #[command(flatten)]
    range: RangeArgs,
    /// Minimum tx value in wei: 10eth, 200gwei, or plain wei
    #[arg(long, value_name = "AMOUNT")]
    min_value: Option<String>,
    /// Maximum tx value in wei
    #[arg(long, value_name = "AMOUNT")]
    max_value: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

pub async fn run(config: &Config, args: &TxsArgs) -> Result<()> {
    let chains = target_chains(config, args.chain)?;
    let from_filter = match &args.from {
        Some(v) => Some(parse_address_or_name(config, v, "--from").await?),
        None => None,
    };
    let to_filter = match &args.to {
        Some(v) => Some(parse_address_or_name(config, v, "--to").await?),
        None => None,
    };
    let method = match &args.method {
        Some(m) => {
            let sel = m.trim().trim_start_matches("0x").to_ascii_lowercase();
            if sel.len() != 8 || !sel.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!("--method expects a 4-byte selector like 0xa9059cbb (got '{m}')");
            }
            Some(sel)
        }
        None => None,
    };
    let status_bit = match args.status.as_deref() {
        Some(s) if s.eq_ignore_ascii_case("success") => Some(1u8),
        Some(s) if s.eq_ignore_ascii_case("reverted") || s.eq_ignore_ascii_case("failed") => Some(0),
        Some(other) => bail!("--status expects 'success' or 'reverted' (got '{other}')"),
        None => None,
    };
    let scale = |amount: &str, flag: &str| -> Result<u128> {
        let wei = super::parse_native_amount(amount)?;
        if wei < 0.0 {
            bail!("{flag} must be positive");
        }
        Ok(wei as u128)
    };
    let min_raw = match &args.min_value { Some(v) => Some(scale(v, "--min-value")?), None => None };
    let max_raw = match &args.max_value { Some(v) => Some(scale(v, "--max-value")?), None => None };

    let mut all_rows: Vec<Value> = Vec::new();
    let multi = chains.len() > 1;
    for c in &chains {
        if multi && !super::has_blocks(config, *c).await {
            continue;
        }
        let sql = build_sql(
            *c,
            from_filter.as_deref(),
            to_filter.as_deref(),
            method.as_deref(),
            status_bit,
            min_raw,
            max_raw,
            &resolve_range(config, *c, &args.range).await?,
            args.out.count,
            args.out.limit,
        );

        if args.out.sql && chains.len() == 1 {
            println!("{sql}");
            return Ok(());
        }
        if args.out.count {
            let body = ch(config, &format!("SELECT count() AS n FROM ({sql})"), "TSV").await?;
            if chains.len() == 1 {
                println!("{}", body.trim());
                return Ok(());
            }
            all_rows.push(json!({"count": body.trim().parse::<u64>().unwrap_or(0)}));
            continue;
        }

        let body = ch(config, &format!("{sql} FORMAT JSONEachRow"), "JSONEachRow").await?;
        all_rows.extend(enrich_rows(&body, *c));
    }

    if args.out.sql && chains.len() > 1 {
        bail!("--sql needs an explicit --chain when querying multiple chains");
    }
    if args.out.count {
        let total: u64 =
            all_rows.iter().filter_map(|r| r.get("count").and_then(|v| v.as_u64())).sum();
        println!("{total}");
        return Ok(());
    }

    let rows = merge_by_ts(all_rows, args.out.limit);
    render_rows(&rows, args, multi)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_sql(
    chain: u64,
    from: Option<&str>,
    to: Option<&str>,
    method: Option<&str>,
    status_bit: Option<u8>,
    min_raw: Option<u128>,
    max_raw: Option<u128>,
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
    if let Some(from) = from {
        filters.push_str(&format!(" AND from_address = unhex('{from}')"));
    }
    if let Some(to) = to {
        filters.push_str(&format!(" AND to_address = unhex('{to}')"));
    }
    if let Some(sel) = method {
        // input is stored as raw bytes; compare the first 4 bytes.
        filters.push_str(&format!(" AND substring(input, 1, 4) = unhex('{sel}')"));
    }
    if let Some(bit) = status_bit {
        filters.push_str(&format!(" AND status = {bit}"));
    }
    if let Some(min) = min_raw {
        filters.push_str(&format!(" AND value >= {min}"));
    }
    if let Some(max) = max_raw {
        filters.push_str(&format!(" AND value <= {max}"));
    }

    let order = if count { "" } else { " ORDER BY block_number DESC, tx_index DESC" };
    let limit = if count { String::new() } else { format!(" LIMIT {limit}") };

    format!(
        "SELECT block_number, concat('0x', lower(hex(tx_hash))) AS tx_hash, tx_index, \
         concat('0x', lower(hex(from_address))) AS `from`, \
         concat('0x', lower(hex(to_address))) AS `to`, \
         toString(value) AS value_eth_raw, gas_used, effective_gas_price AS gas_price, status, \
         concat('0x', lower(hex(substring(input, 1, 4)))) AS method, \
         toUnixTimestamp(b.timestamp) AS ts \
         FROM transactions t \
         INNER JOIN (SELECT block_number, timestamp FROM blocks WHERE chain_id = {chain}) b \
           USING (block_number) \
         WHERE t.chain_id = {chain} AND {block_filter}{filters}{order}{limit}"
    )
}

fn enrich_rows(body: &str, chain: u64) -> Vec<Value> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut v: Value = serde_json::from_str(l).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                let ts = obj.get("ts").and_then(|x| x.as_i64()).unwrap_or(0);
                let wei = obj.get("value_eth_raw").and_then(|x| x.as_str()).unwrap_or("0").to_owned();
                let human = super::human_amount(&wei, 18);
                obj.insert("value_eth".into(), json!(human));
                obj.insert("value_eth_human".into(), json!(super::group_decimal(&human)));
                obj.insert("value_raw".into(), json!(wei));
                obj.insert("chain_id".into(), json!(chain));
                obj.insert("time".into(), json!(super::iso_time(ts)));
                obj.insert("age".into(), json!(super::human_age(ts, now)));
                obj.insert(
                    "status_h".into(),
                    json!(if obj.get("status").and_then(|x| x.as_u64()).unwrap_or(0) == 1 {
                        "ok"
                    } else {
                        "reverted"
                    }),
                );
            }
            v
        })
        .collect()
}

fn render_rows(rows: &[Value], args: &TxsArgs, multi_chain: bool) -> Result<()> {
    let mut table_cols = vec![
        ("age", "age"),
        ("value_eth_human", "value (ETH)"),
        ("from", "from"),
        ("to", "to"),
        ("method", "method"),
        ("status_h", "status"),
        ("tx_hash", "tx"),
        ("block_number", "block"),
    ];
    if multi_chain {
        table_cols.insert(0, ("chain_id", "chain"));
    }
    let mut csv_cols: Vec<(&str, &str)> = Vec::new();
    if multi_chain {
        csv_cols.push(("chain_id", "chain_id"));
    }
    csv_cols.extend([
        ("block_number", "block"),
        ("time", "time"),
        ("value_eth", "value_eth"),
        ("value_raw", "value_wei"),
        ("from", "from"),
        ("to", "to"),
        ("method", "method"),
        ("status_h", "status"),
        ("gas_price", "gas_price_wei"),
        ("gas_used", "gas_used"),
        ("tx_hash", "tx_hash"),
        ("tx_index", "tx_index"),
    ]);
    render_with_csv(rows, &table_cols, &csv_cols, &args.out, "transactions")?;
    Ok(())
}
