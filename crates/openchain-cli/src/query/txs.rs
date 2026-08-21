use super::{ch, parse_address, render_with_csv, resolve_range, OutputArgs, RangeArgs};
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
    /// Chain id (must exist in config)
    #[arg(long)]
    chain: u64,
    /// Sender address
    #[arg(long, value_name = "ADDR")]
    from: Option<String>,
    /// Recipient address (EOA or contract)
    #[arg(long, value_name = "ADDR")]
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
    let (lo, hi) = resolve_range(config, args.chain, &args.range).await?;

    let mut filters = String::new();
    if let Some(from) = &args.from {
        let a = parse_address(from, "--from")?;
        filters.push_str(&format!(" AND from_address = unhex('{a}')"));
    }
    if let Some(to) = &args.to {
        let a = parse_address(to, "--to")?;
        filters.push_str(&format!(" AND to_address = unhex('{a}')"));
    }
    if let Some(method) = &args.method {
        let sel = method.trim().trim_start_matches("0x").to_ascii_lowercase();
        if sel.len() != 8 || !sel.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("--method expects a 4-byte selector like 0xa9059cbb (got '{method}')");
        }
        // input is stored as raw bytes; compare the first 4 bytes.
        filters.push_str(&format!(" AND substring(input, 1, 4) = unhex('{sel}')"));
    }
    match args.status.as_deref() {
        Some(s) if s.eq_ignore_ascii_case("success") => filters.push_str(" AND status = 1"),
        Some(s) if s.eq_ignore_ascii_case("reverted") || s.eq_ignore_ascii_case("failed") => {
            filters.push_str(" AND status = 0")
        }
        Some(other) => bail!("--status expects 'success' or 'reverted' (got '{other}')"),
        None => {}
    }
    let scale = |amount: &str, flag: &str| -> Result<u128> {
        let wei = super::parse_native_amount(amount)?;
        if wei < 0.0 {
            bail!("{flag} must be positive");
        }
        Ok(wei as u128)
    };
    if let Some(v) = &args.min_value {
        filters.push_str(&format!(" AND value >= {}", scale(v, "--min-value")?));
    }
    if let Some(v) = &args.max_value {
        filters.push_str(&format!(" AND value <= {}", scale(v, "--max-value")?));
    }

    let order = if args.out.count { "" } else { " ORDER BY block_number DESC, tx_index DESC" };
    let limit = if args.out.count { String::new() } else { format!(" LIMIT {}", args.out.limit) };
    let block_filter = match (lo, hi) {
        (Some(a), Some(b)) => format!("block_number BETWEEN {a} AND {b}"),
        (Some(a), None) => format!("block_number >= {a}"),
        (None, Some(b)) => format!("block_number <= {b}"),
        (None, None) => "1".to_string(),
    };

    let sql = format!(
        "SELECT block_number, concat('0x', lower(hex(tx_hash))) AS tx_hash, tx_index, \
         concat('0x', lower(hex(from_address))) AS `from`, concat('0x', lower(hex(to_address))) AS `to`, \
         toString(value) AS value_eth_raw, gas_used, effective_gas_price AS gas_price, status, \
         concat('0x', lower(hex(substring(input, 1, 4)))) AS method, \
         toUnixTimestamp(b.timestamp) AS ts \
         FROM transactions t \
         INNER JOIN (SELECT block_number, timestamp FROM blocks WHERE chain_id = {c}) b \
           USING (block_number) \
         WHERE t.chain_id = {c} AND {block_filter}{filters}{order}{limit}",
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
                let wei = obj.get("value_eth_raw").and_then(|x| x.as_str()).unwrap_or("0").to_owned();
                let human = super::human_amount(&wei, 18);
                obj.insert("value_eth".into(), json!(human));
                obj.insert("value_eth_human".into(), json!(super::group_decimal(&human)));
                obj.insert("value_raw".into(), json!(wei));
                obj.insert("time".into(), json!(iso_time(ts)));
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
        .collect();

    render_with_csv(
        &rows,
        &[
            ("age", "age"),
            ("value_eth_human", "value (ETH)"),
            ("from", "from"),
            ("to", "to"),
            ("method", "method"),
            ("status_h", "status"),
            ("tx_hash", "tx"),
            ("block_number", "block"),
        ],
        &[
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
        ],
        &args.out,
        "transactions",
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
