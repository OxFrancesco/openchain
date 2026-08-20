use alloy::dyn_abi::{DynSolEvent, DynSolValue, Specifier};
use alloy::json_abi::JsonAbi;
use alloy::primitives::{hex, B256};
use eyre::{Context, Result};
use openchain_core::{now_millis, DecodedEventRow, LogRow};
use std::collections::HashMap;

struct PreparedEvent {
    name: String,
    full_signature: String,
    dyn_event: DynSolEvent,
    /// (param name, indexed) in declaration order.
    inputs: Vec<(String, bool)>,
}

struct Contract {
    name: String,
    events: HashMap<[u8; 32], PreparedEvent>,
}

/// Decodes raw logs into named events using registered contract ABIs.
/// Lookup is two hash-map hits (address, then topic0), so decoding is
/// CPU-bound on ABI decoding itself.
#[derive(Default)]
pub struct EventDecoder {
    contracts: HashMap<[u8; 20], Contract>,
}

impl EventDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contract_count(&self) -> usize {
        self.contracts.len()
    }

    pub fn addresses(&self) -> impl Iterator<Item = &[u8; 20]> {
        self.contracts.keys()
    }

    /// Register a contract ABI. Returns the number of decodable (non-anonymous) events.
    pub fn register(&mut self, address: [u8; 20], contract_name: &str, abi_json: &str) -> Result<usize> {
        let abi: JsonAbi = serde_json::from_str(abi_json).wrap_err("invalid ABI JSON")?;
        let mut events = HashMap::new();
        for event in abi.events() {
            if event.anonymous {
                continue;
            }
            let dyn_event = event.resolve().wrap_err_with(|| format!("cannot resolve event {}", event.name))?;
            events.insert(
                event.selector().0,
                PreparedEvent {
                    name: event.name.clone(),
                    full_signature: event.full_signature(),
                    dyn_event,
                    inputs: event.inputs.iter().map(|p| (p.name.clone(), p.indexed)).collect(),
                },
            );
        }
        let count = events.len();
        self.contracts.insert(address, Contract { name: contract_name.to_string(), events });
        Ok(count)
    }

    /// Decode one log. Returns None when the contract or event is not registered,
    /// Some(Err) when the log does not match the registered ABI.
    pub fn decode(&self, log: &LogRow) -> Option<Result<DecodedEventRow>> {
        let contract = self.contracts.get(&log.address)?;
        let event = contract.events.get(&log.topic0)?;
        Some(decode_row(log, contract, event))
    }
}

fn decode_row(log: &LogRow, contract: &Contract, event: &PreparedEvent) -> Result<DecodedEventRow> {
    let topics: Vec<B256> = [Some(log.topic0), log.topic1, log.topic2, log.topic3]
        .into_iter()
        .flatten()
        .map(B256::from)
        .collect();
    let decoded = event.dyn_event.decode_log_parts(topics, &log.data)?;

    let mut indexed = decoded.indexed.into_iter();
    let mut body = decoded.body.into_iter();
    let mut params = serde_json::Map::new();
    for (i, (name, is_indexed)) in event.inputs.iter().enumerate() {
        let value = if *is_indexed { indexed.next() } else { body.next() }
            .ok_or_else(|| eyre::eyre!("decoded value count mismatch for {}", event.name))?;
        let key = if name.is_empty() { format!("param{i}") } else { name.clone() };
        params.insert(key, to_json(&value));
    }

    Ok(DecodedEventRow {
        chain_id: log.chain_id,
        block_number: log.block_number,
        tx_hash: log.tx_hash,
        tx_index: log.tx_index,
        log_index: log.log_index,
        address: log.address,
        contract_name: contract.name.clone(),
        event_name: event.name.clone(),
        full_signature: event.full_signature.clone(),
        params: serde_json::Value::Object(params).to_string(),
        insert_version: now_millis(),
    })
}

/// Convert a decoded Solidity value to JSON. Numbers become decimal strings
/// (uint256 does not fit JSON numbers), byte values become 0x-hex.
fn to_json(value: &DynSolValue) -> serde_json::Value {
    use serde_json::Value;
    match value {
        DynSolValue::Address(a) => Value::String(a.to_string()),
        DynSolValue::Bool(b) => Value::Bool(*b),
        DynSolValue::Uint(u, _) => Value::String(u.to_string()),
        DynSolValue::Int(i, _) => Value::String(i.to_string()),
        DynSolValue::FixedBytes(b, size) => Value::String(format!("0x{}", hex::encode(&b[..*size]))),
        DynSolValue::Bytes(b) => Value::String(format!("0x{}", hex::encode(b))),
        DynSolValue::String(s) => Value::String(s.clone()),
        DynSolValue::Function(f) => Value::String(f.to_string()),
        DynSolValue::Array(items) | DynSolValue::FixedArray(items) | DynSolValue::Tuple(items) => {
            Value::Array(items.iter().map(to_json).collect())
        }
    }
}
