//! Prewarm the chain-ABI whole compute forms a prior title run actually requested.
use super::{Input, PipelineOutcome, Pipelines};
use crate::gpu::{recomp::{self, pso::{self, Compiled, Stage}}, shader::fnv64};
use crate::hle::d3d12x::real::{chain_whole_demand as demand, driver_cache};
use serde::Deserialize;
use std::collections::{hash_map::Entry, BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::Path;

const MAX_CAPTURE_BYTES: u64 = 32 << 20;

#[derive(Deserialize)]
struct Header {
    schema: String,
    direct_dxil_source: String,
    options: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Request {
    sequence: usize,
    #[serde(deserialize_with = "hex_u64")]
    record_hash: u64,
    #[serde(deserialize_with = "hex_u64")]
    dxil_hash: u64,
    dxil_bytes: usize,
    flags: i32,
    node_mask: u32,
    #[serde(deserialize_with = "hex_u64")]
    root_signature: u64,
    #[serde(deserialize_with = "hex_u64")]
    adapter_luid: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Row {
    Request(Request),
    Limit { truncated: bool },
}

fn hex_u64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let text = String::deserialize(deserializer)?;
    if text.len() != 16 {
        return Err(serde::de::Error::custom("expected 16 hexadecimal digits"));
    }
    u64::from_str_radix(&text, 16).map_err(serde::de::Error::custom)
}

fn select(capture: &str, through: Option<usize>) -> Result<Vec<Request>, String> {
    let mut lines = capture.lines();
    let header: Header = serde_json::from_str(lines.next().ok_or("empty capture")?)
        .map_err(|e| format!("invalid capture header: {e}"))?;
    if header.schema != demand::SCHEMA {
        return Err(format!("unsupported capture schema {}", header.schema));
    }
    if header.direct_dxil_source != demand::SOURCE_ID || header.options != demand::options() {
        return Err("capture translator identity or options differ from this process".into());
    }
    let mut selected = Vec::new();
    let mut identities = HashSet::new();
    for (i, line) in lines.enumerate() {
        if through.is_some_and(|limit| selected.len() >= limit) {
            break;
        }
        let request = match serde_json::from_str::<Row>(line).map_err(|e| format!("capture line {}: {e}", i + 2))? {
            Row::Request(request) => request,
            Row::Limit { truncated: true } => return Err("capture was truncated; select a prefix with --chain-through".into()),
            Row::Limit { truncated: false } => return Err("invalid capture limit marker".into()),
        };
        if request.sequence != selected.len() + 1 || request.dxil_bytes == 0 {
            return Err(format!("invalid sequence or DXIL length at capture line {}", i + 2));
        }
        if !identities.insert((request.dxil_hash, request.dxil_bytes, request.flags, request.node_mask,
            request.root_signature, request.adapter_luid)) {
            return Err(format!("duplicate final pipeline identity at capture line {}", i + 2));
        }
        selected.push(request);
    }
    if let Some(limit) = through {
        if selected.len() < limit {
            return Err(format!("capture ended after {} requests before required sequence {limit}", selected.len()));
        }
    }
    if selected.is_empty() { return Err("no whole fallback requests in the selected capture".into()); }
    Ok(selected)
}

fn read_requests(path: &Path, through: Option<usize>) -> Result<Vec<Request>, String> {
    let mut capture = String::new();
    std::fs::File::open(path).and_then(|file| file.take(MAX_CAPTURE_BYTES + 1).read_to_string(&mut capture))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if capture.len() as u64 > MAX_CAPTURE_BYTES {
        return Err("whole fallback capture exceeds 32 MiB".into());
    }
    select(&capture, through)
}

/// One pass over the input. Keep indices rather than holding all selected records in memory.
fn find_records(input: &Input, requests: &[Request]) -> HashMap<u64, Option<usize>> {
    let wanted: HashSet<u64> = requests.iter().map(|request| request.record_hash).collect();
    let mut matches = HashMap::new();
    for i in 0..input.len() {
        let (_, Some(blob)) = input.get(i) else { continue };
        let key = fnv64(&blob);
        if !wanted.contains(&key) { continue; }
        match matches.entry(key) {
            Entry::Vacant(slot) => { slot.insert(Some(i)); }
            Entry::Occupied(mut slot) => {
                if let Some(prior) = *slot.get() {
                    if input.get(prior).1.as_deref() != Some(blob.as_slice()) { slot.insert(None); }
                }
            }
        }
    }
    matches
}

fn validate_translation(request: &Request, compiled: &Compiled) -> Result<(), &'static str> {
    if compiled.dispatch_chain.as_ref().is_none_or(|chain| chain.pieces.len() < 2) {
        return Err("the shader no longer has a dispatch chain");
    }
    if compiled.dxil.len() != request.dxil_bytes || fnv64(&compiled.dxil) != request.dxil_hash {
        return Err("whole DXIL identity changed");
    }
    if demand::sampler_identity(&compiled.samplers) != request.root_signature {
        return Err("dispatch-chain root signature identity changed");
    }
    Ok(())
}

fn warm(input: &Input, index: usize, request: &Request, pipelines: &Pipelines,
    adapter: u64, windows: u32, split: bool) -> Result<PipelineOutcome, String> {
    if request.node_mask != 0 { return Err("only node mask zero is supported".into()); }
    if request.adapter_luid != adapter { return Err("request belongs to another adapter".into()); }
    let Some(path) = driver_cache::chain_whole_path(request.dxil_hash, request.dxil_bytes, request.flags,
        request.root_signature, adapter) else { return Err("the driver blob cache is disabled".into()); };
    if pipelines.max_dxil != 0 && request.dxil_bytes > pipelines.max_dxil { return Ok(PipelineOutcome::Capped); }
    let (_, Some(blob)) = input.get(index) else { return Err("source record is no longer readable".into()); };
    if fnv64(&blob) != request.record_hash { return Err("source record changed during replay".into()); }
    let compiled = pso::console_to_dxil_in(&blob, Stage::Compute, 0, split, windows)?;
    validate_translation(request, &compiled)?;
    Ok(pipelines.make_one(&compiled.dxil, &compiled.samplers, true, request.flags, Some(path)))
}

pub(super) fn run(input: &Input, path: &Path, through: Option<usize>, windows: u32, split: Option<bool>) -> i32 {
    let requests = match read_requests(path, through) {
        Ok(requests) => requests,
        Err(e) => { eprintln!("xcl shaders --prewarm-chain-whole: {e}"); return 2; }
    };
    let pipelines = match Pipelines::new() {
        Ok(pipelines) => pipelines,
        Err(e) => { eprintln!("xcl shaders --prewarm-chain-whole: no GPU device: {e}"); return 2; }
    };
    let adapter = demand::adapter_identity(unsafe { pipelines.dev.GetAdapterLuid() });
    let split = split.unwrap_or_else(|| pso::split_wave64_forced() || recomp::runner::wave64_supported().is_ok_and(|w| !w));
    let matches = find_records(input, &requests);
    let (mut reused, mut stored) = (0, 0);
    for request in &requests {
        let status = match matches.get(&request.record_hash) {
            Some(Some(index)) => warm(input, *index, request, &pipelines, adapter, windows, split),
            Some(None) => Err("source record hash is ambiguous".into()),
            None => Err("source record is missing".into()),
        };
        match &status {
            Ok(PipelineOutcome::Reused) => reused += 1,
            Ok(PipelineOutcome::Stored) => stored += 1,
            _ => {}
        }
        match status {
            Ok(outcome) => println!("  chain whole #{} record {:016x}: {outcome:?}", request.sequence, request.record_hash),
            Err(reason) => eprintln!("  chain whole #{} record {:016x}: {reason}", request.sequence, request.record_hash),
        }
    }
    let incomplete = requests.len() - reused - stored;
    println!("chain whole prewarm: {} requested, {reused} reused, {stored} stored, {incomplete} incomplete", requests.len());
    if incomplete == 0 { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> serde_json::Value {
        serde_json::json!({ "schema": demand::SCHEMA, "direct_dxil_source": demand::SOURCE_ID, "options": demand::options() })
    }

    fn request(sequence: usize, dxil_hash: &str) -> serde_json::Value {
        serde_json::json!({ "sequence": sequence, "record_hash": "0000000000000001", "dxil_hash": dxil_hash,
            "dxil_bytes": 256, "flags": 0, "node_mask": 0, "root_signature": "0000000000000002", "adapter_luid": "0000000000000003" })
    }

    #[test]
    fn chain_whole_replay_keeps_selection_order_and_checks_final_identity() {
        let first = request(1, "0000000000000020");
        let second = request(2, "0000000000000010");
        let text = format!("{}\n{first}\n{second}\n{{\"truncated\":true}}\n", header());
        assert!(select(&text, None).is_err());
        let entries = select(&text, Some(2)).unwrap();
        assert_eq!(entries.iter().map(|row| row.dxil_hash).collect::<Vec<_>>(), [32, 16]);
        assert_eq!(entries[0].record_hash, entries[1].record_hash);
        let duplicate = request(2, "0000000000000020");
        assert!(select(&format!("{}\n{first}\n{duplicate}\n", header()), None).is_err());
    }

    #[test]
    fn chain_whole_replay_rejects_an_incomplete_explicit_prefix() {
        let first = request(1, "0000000000000020");
        let second = request(2, "0000000000000010");
        let capture = format!("{}\n{first}\n{second}\n", header());
        // The replay entry checks this selection before it requests a D3D12 device.
        assert_eq!(select(&capture, Some(3)).err().as_deref(),
            Some("capture ended after 2 requests before required sequence 3"));
        assert_eq!(select(&capture, Some(1)).unwrap().len(), 1);
        assert_eq!(select(&capture, Some(2)).unwrap().len(), 2);
        assert_eq!(select(&capture, None).unwrap().len(), 2);
    }

    #[test]
    fn chain_whole_replay_rejects_changed_shader_or_root_signature() {
        let mut compiled = Compiled {
            dxil: vec![35; 256], samplers: vec![],
            dispatch_chain: Some(std::sync::Arc::new(crate::gpu::dxil::kernel::DispatchChain {
                dwords_per_group: 1, pieces: vec![vec![0], vec![1]],
            })),
        };
        let mut entry: Request = serde_json::from_value(request(1, "0000000000000020")).unwrap();
        entry.dxil_hash = fnv64(&compiled.dxil);
        entry.root_signature = demand::sampler_identity(&compiled.samplers);
        assert_eq!(validate_translation(&entry, &compiled), Ok(()));
        compiled.samplers.push(([1, 2, 3, 4], false));
        assert_eq!(validate_translation(&entry, &compiled), Err("dispatch-chain root signature identity changed"));
        compiled.dxil[32] = 12;
        assert_eq!(validate_translation(&entry, &compiled), Err("whole DXIL identity changed"));
    }
}
