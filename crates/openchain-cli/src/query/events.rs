use super::{
    ch, merge_by_ts, render_with_csv, resolve_range, target_chains, OutputArgs, RangeArgs,
};
use clap::Args;
use eyre::{bail, Result};
use openchain_core::Config;
use serde_json::{json, Value};

/// Decoded contract events with human flags — needs ABIs registered and
/// `openchain decode` run first.
///
/// "show me Uniswap V3 Swap events in the last month" →
/// `openchain events --chain 1 --event Swap --since 30d`
#[derive(Args, Debug)]
pub struct EventsArgs {
    /// Chain id; omit to query every configured chain
    #[arg(long)]
    chain: Option<u64>,
    /// Event name to filter by (case-insensitive): Transfer, Swap, Approval
    #[arg(long)]
    event: Option<String>,
    /// Emitting contract: name (as registered via `abi add`) or address
    #[arg(long, value_name = "NAME|ADDR")]
    contract: Option<String>,
    /// Substring match against decoded params JSON: '"recipient":"0x1f98"'
    #[arg(long, value_name = "SUBSTRING")]
    params: Option<String>,
    #[command(flatten)]
    range: RangeArgs,
    #[command(flatten)]
    out: OutputArgs,
}

pub async fn run(config: &Config, args: &EventsArgs) -> Result<()> {
    let chains = target_chains(config, args.chain)?;
    let mut all_rows: Vec<Value> = Vec::new();
    let multi = chains.len() > 1;

    for c in &chains {
        if multi && !super::has_blocks(config, *c).await {
            continue;
        }
        let sql = build_sql(
            *c,
            args.event.as_deref(),
            args.contract.as_deref(),
            args.params.as_deref(),
            &resolve_range(config, *c, &args.range).await?,
            args.out.count,
            args.out.limit,
        )?;

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

fn build_sql(
    chain: u64,
    event: Option<&str>,
    contract: Option<&str>,
    params: Option<&str>,
    range: &(Option<u64>, Option<u64>),
    count: bool,
    limit: u64,
) -> eyre::Result<String> {
    let (lo, hi) = *range;
    let block_filter = match (lo, hi) {
        (Some(a), Some(b)) => format!("block_number BETWEEN {a} AND {b}"),
        (Some(a), None) => format!("block_number >= {a}"),
        (None, Some(b)) => format!("block_number <= {b}"),
        (None, None) => "1".to_string(),
    };

    let mut filters = String::new();
    if let Some(event) = event {
        let e = event.replace('\'', "''");
        filters.push_str(&format!(" AND event_name ILIKE '{e}'"));
    }
    if let Some(contract) = contract {
        if contract.starts_with("0x") {
            let a = super::parse_address(contract, "--contract")?;
            filters.push_str(&format!(" AND address = unhex('{a}')"));
        } else {
            let c = contract.replace('\'', "''");
            filters.push_str(&format!(" AND contract_name ILIKE '{c}'"));
        }
    }
    if let Some(params) = params {
        let p = params.replace('\'', "''").replace('\\', "\\\\");
        filters.push_str(&format!(" AND params LIKE '%{p}%'"));
    }

    let order = if count { "" } else { " ORDER BY block_number DESC, log_index DESC" };
    let limit = if count { String::new() } else { format!(" LIMIT {limit}") };

    Ok(format!(
        "SELECT block_number, concat('0x', lower(hex(tx_hash))) AS tx_hash, log_index, \
         concat('0x', lower(hex(address))) AS contract, contract_name, event_name, full_signature, params, \
         toUnixTimestamp(b.timestamp) AS ts \
         FROM decoded_events e \
         INNER JOIN (SELECT block_number, timestamp FROM blocks WHERE chain_id = {chain}) b \
           USING (block_number) \
         WHERE e.chain_id = {chain} AND {block_filter}{filters}{order}{limit}"
    ))
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
                obj.insert("chain_id".into(), json!(chain));
                obj.insert("time".into(), json!(super::iso_time(ts)));
                obj.insert("age".into(), json!(super::human_age(ts, now)));
            }
            v
        })
        .collect()
}

fn render_rows(rows: &[Value], args: &EventsArgs, multi_chain: bool) -> Result<()> {
    let mut table_cols = vec![
        ("age", "age"),
        ("event_name", "event"),
        ("contract_name", "contract"),
        ("contract", "address"),
        ("params", "params"),
        ("tx_hash", "tx"),
        ("block_number", "block"),
    ];
    let mut csv_cols: Vec<(&str, &str)> = Vec::new();
    if multi_chain {
        table_cols.insert(0, ("chain_id", "chain"));
        csv_cols.push(("chain_id", "chain_id"));
    }
    csv_cols.extend([
        ("block_number", "block"),
        ("time", "time"),
        ("event_name", "event"),
        ("contract_name", "contract_name"),
        ("contract", "contract"),
        ("full_signature", "signature"),
        ("params", "params"),
        ("tx_hash", "tx_hash"),
        ("log_index", "log_index"),
    ]);
    render_with_csv(rows, &table_cols, &csv_cols, &args.out, "events")?;
    Ok(())
}
