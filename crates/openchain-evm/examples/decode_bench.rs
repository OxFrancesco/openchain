//! Decode-only micro-benchmark: sequential vs rayon-parallel ABI decoding.
//!
//! Generates synthetic WETH9-style Transfer logs and times both paths, so the
//! number isolates decode CPU from ClickHouse I/O.
//!
//! Run: cargo run --release -p openchain-evm --example decode_bench

use alloy::primitives::{address, keccak256, U256};
use openchain_evm::decode::EventDecoder;
use rayon::prelude::*;
use std::time::Instant;

const WETH: [u8; 20] = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").0 .0;
const N_LOGS: usize = 1_000_000;

fn main() {
    let transfer_topic = keccak256("Transfer(address,address,uint256)").0;
    let abi_json = r#"[{
        "type": "event",
        "name": "Transfer",
        "anonymous": false,
        "inputs": [
            {"name": "src", "type": "address", "indexed": true},
            {"name": "dst", "type": "address", "indexed": true},
            {"name": "wad", "type": "uint256", "indexed": false}
        ]
    }]"#;

    let mut decoder = EventDecoder::new();
    decoder.register(WETH, "WETH9", abi_json).expect("register ABI");

    let logs: Vec<openchain_core::LogRow> = (0..N_LOGS)
        .map(|i| {
            let src = U256::from(i);
            let dst = U256::from(i ^ 0xdead_beef);
            openchain_core::LogRow {
                chain_id: 1,
                block_number: 25_790_000 + (i as u64 / 200),
                block_hash: [0; 32],
                tx_hash: [0; 32],
                tx_index: (i % 200) as u32,
                log_index: (i % 200) as u32,
                address: WETH,
                topic0: transfer_topic,
                topic1: Some(src.to_be_bytes::<32>()),
                topic2: Some(dst.to_be_bytes::<32>()),
                topic3: None,
                data: src.to_be_bytes::<32>().to_vec(),
                insert_version: 0,
            }
        })
        .collect();

    // Warm up allocator + hash maps so both paths see identical conditions.
    for log in &logs {
        let _ = decoder.decode(log);
    }

    let started = Instant::now();
    let mut seq_decoded = 0usize;
    for log in &logs {
        if decoder.decode(log).is_some() {
            seq_decoded += 1;
        }
    }
    let seq_secs = started.elapsed().as_secs_f64();

    let started = Instant::now();
    let par_decoded = logs
        .par_iter()
        .filter(|log| decoder.decode(log).is_some())
        .count();
    let par_secs = started.elapsed().as_secs_f64();

    println!(
        "{N_LOGS} logs | sequential: {seq_decoded} in {seq_secs:.3}s ({:.0} logs/s) | \
         parallel: {par_decoded} in {par_secs:.3}s ({:.0} logs/s) | speedup {:.2}x",
        seq_decoded as f64 / seq_secs,
        par_decoded as f64 / par_secs,
        seq_secs / par_secs,
    );
}
