use super::{ch, parse_address, render_with_csv, resolve_range, OutputArgs, RangeArgs};
use clap::Args;
use eyre::Result;
use openchain_core::Config;
use serde_json::{json, Value};

/// Decoded contract events with human flags — needs ABIs registered and
/// `openchain decode` run first.
///
/// "show me Uniswap V3 Swap events in the last month" →
/// `openchain events --chain 1 --event Swap --since 30d`
#[derive(Args, Debug)]
pub struct EventsArgs {
    /// Chain id (must exist in config)
    #[arg(long)]
    chain: u64,
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
    let (lo, hi) = resolve_range(config, args.chain, &args.range).await?;

    let mut filters = String::new();
    if let Some(event) = &args.event {
        let e = event.replace('\'', "''");
        filters.push_str(&format!(" AND event_name ILIKE '{e}'"));
    }
    if let Some(contract) = &args.contract {
        if contract.starts_with("0x") {
            let a = parse_address(contract, "--contract")?;
            filters.push_str(&format!(" AND address = unhex('{a}')"));
        } else {
            let c = contract.replace('\'', "''");
            filters.push_str(&format!(" AND contract_name ILIKE '{c}'"));
        }
    }
    if let Some(params) = &args.params {
        let p = params.replace('\'', "''").replace('\\', "\\\\");
        filters.push_str(&format!(" AND params LIKE '%{p}%'"));
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
        "SELECT block_number, concat('0x', lower(hex(tx_hash))) AS tx_hash, log_index, \
         concat('0x', lower(hex(address))) AS contract, contract_name, event_name, full_signature, params, \
         toUnixTimestamp(b.timestamp) AS ts \
         FROM decoded_events e \
         INNER JOIN (SELECT block_number, timestamp FROM blocks WHERE chain_id = {c}) b \
           USING (block_number) \
         WHERE e.chain_id = {c} AND {block_filter}{filters}{order}{limit}",
        c = args.chain,
    );

    if args.out.sql {
        println!("{sql}");
        return Ok(());
    }
    if args.out.count {
        let body = ch(config, &format!("SELECT count() AS n FROM ({sql})"), "TSV").await?;
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
                let ts = obj.get("ts").and_then(|x| x.as_i64()).unwrap_or(0);
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
            ("event_name", "event"),
            ("contract_name", "contract"),
            ("contract", "address"),
            ("params", "params"),
            ("tx_hash", "tx"),
            ("block_number", "block"),
        ],
        &[
            ("block_number", "block"),
            ("time", "time"),
            ("event_name", "event"),
            ("contract_name", "contract_name"),
            ("contract", "contract"),
            ("full_signature", "signature"),
            ("params", "params"),
            ("tx_hash", "tx_hash"),
            ("log_index", "log_index"),
        ],
        &args.out,
        "events",
    )?;
    Ok(())
}

fn iso_time(ts: i64) -> String {
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, (secs % 3600) / 60, secs % 60)
}

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
