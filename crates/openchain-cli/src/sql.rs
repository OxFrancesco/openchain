use eyre::{bail, Result};
use openchain_core::Config;

/// Run an arbitrary SQL query through the ClickHouse HTTP interface and print
/// the raw response. `default_format` only applies when the query has no
/// explicit FORMAT clause.
pub async fn run(config: &Config, query: &str, format: &str) -> Result<()> {
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
    print!("{body}");
    Ok(())
}
