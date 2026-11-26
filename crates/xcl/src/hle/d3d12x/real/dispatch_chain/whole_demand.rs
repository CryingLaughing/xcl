//! First-use identities for the unsplit fallback built with a dispatch-chain root signature.
use crate::gpu::{recomp::pso::StaticSampler, shader::fnv64};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, Once};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(crate) const SCHEMA: &str = "xcl-chain-whole-demand-v1";
pub(crate) const SOURCE_ID: &str = env!("XCL_DIRECT_DXIL_SOURCE_ID");
const CHAIN_ABI: &[u8] = b"dispatch-chain-abi-v1";
const MAX_RECORDS: usize = 65_536;

static PATH: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    std::env::var_os("XCL_CHAIN_WHOLE_DEMAND_FILE").filter(|path| !path.is_empty()).map(PathBuf::from)
});

static CAPTURE: LazyLock<Mutex<Option<Capture<File>>>> = LazyLock::new(|| {
    Mutex::new(PATH.as_ref().and_then(|path| {
        let opened = OpenOptions::new().write(true).create_new(true).open(path).and_then(|file| {
            let mut capture = Capture::new(file);
            capture.write_header()?;
            Ok(capture)
        });
        match opened {
            Ok(capture) => Some(capture),
            Err(e) => {
                crate::xlog_warn!("D3D12.X: could not start chain whole demand capture at {}: {e}", path.display());
                None
            }
        }
    }))
});

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reason {
    ExecuteIndirect,
    MissingAllocator,
    Bundle,
    ScratchUnavailable,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Identity {
    dxil_hash: u64,
    dxil_bytes: usize,
    flags: i32,
    node_mask: u32,
    root_signature: u64,
    adapter_luid: u64,
}

pub(crate) fn sampler_identity(samplers: &[StaticSampler]) -> u64 {
    let mut input = CHAIN_ABI.to_vec();
    for (words, comparison) in samplers {
        input.extend(words.iter().flat_map(|word| word.to_le_bytes()));
        input.push(*comparison as u8);
    }
    fnv64(&input)
}

pub(crate) fn adapter_identity(luid: windows::Win32::Foundation::LUID) -> u64 {
    (u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart)
}

pub(crate) fn options() -> std::collections::BTreeMap<String, String> {
    ["XCL_DIRECT_DXIL", "XCL_HEAP_WINDOWS", "XCL_PER_LANE", "XCL_DXIL_NO_OPT"]
        .into_iter().filter_map(|name| std::env::var(name).ok().map(|value| (name.to_owned(), value)))
        .collect()
}

/// Per-object gate: on the normal path this is absent, and a captured chain pays for hashing its
/// potentially large whole DXIL only when a call first demands it.
pub(super) struct ChainDemand {
    once: Once,
    root_signature: u64,
    adapter_luid: u64,
}

impl ChainDemand {
    pub(super) fn enabled() -> bool {
        PATH.is_some()
    }

    pub(super) fn new(samplers: &[StaticSampler], adapter_luid: u64) -> Self {
        Self { once: Once::new(), root_signature: sampler_identity(samplers), adapter_luid }
    }

    pub(super) fn note(&self, record: u64, dxil: &[u8], flags: i32, node_mask: u32, reason: Reason) {
        self.once.call_once(|| {
            let presents = crate::hle::d3d12x::present::presents();
            let mut capture = CAPTURE.lock().unwrap();
            if let Some(writer) = capture.as_mut() {
                let identity = Identity {
                    dxil_hash: fnv64(dxil), dxil_bytes: dxil.len(), flags, node_mask,
                    root_signature: self.root_signature, adapter_luid: self.adapter_luid,
                };
                if let Err(e) = writer.record(identity, record, reason, presents) {
                    crate::xlog_warn!("D3D12.X: chain whole demand capture stopped: {e}");
                    *capture = None;
                }
            }
        });
    }
}

struct Capture<W> {
    output: W,
    seen: HashSet<Identity>,
    start: Instant,
    limit_logged: bool,
}

impl<W: Write> Capture<W> {
    fn new(output: W) -> Self {
        Self { output, seen: HashSet::new(), start: Instant::now(), limit_logged: false }
    }

    fn row(&mut self, value: serde_json::Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(&value).map_err(io::Error::other)?;
        line.push(b'\n');
        self.output.write_all(&line)
    }

    fn write_header(&mut self) -> io::Result<()> {
        self.row(serde_json::json!({
            "schema": SCHEMA,
            "start_unix_ms": SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|t| t.as_millis() as u64),
            "xcl_version": env!("CARGO_PKG_VERSION"),
            "direct_dxil_source": SOURCE_ID,
            "options": options(),
        }))
    }

    fn record(&mut self, id: Identity, record: u64, reason: Reason, presents: u64) -> io::Result<()> {
        if self.seen.contains(&id) {
            return Ok(());
        }
        if self.seen.len() == MAX_RECORDS {
            if !self.limit_logged {
                self.row(serde_json::json!({ "truncated": true, "max_records": MAX_RECORDS }))?;
                self.limit_logged = true;
            }
            return Ok(());
        }
        self.row(serde_json::json!({
            "sequence": self.seen.len() + 1,
            "elapsed_ms": self.start.elapsed().as_millis() as u64,
            "presents": presents,
            "reason": reason,
            "record_hash": format!("{record:016x}"),
            "dxil_hash": format!("{:016x}", id.dxil_hash),
            "dxil_bytes": id.dxil_bytes,
            "flags": id.flags,
            "node_mask": id.node_mask,
            "root_signature": format!("{:016x}", id.root_signature),
            "adapter_luid": format!("{:016x}", id.adapter_luid),
        }))?;
        self.seen.insert(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_whole_demand_keeps_first_reason_and_distinct_final_identities() {
        let mut capture = Capture::new(Vec::new());
        capture.write_header().unwrap();
        let first = Identity { dxil_hash: 17, dxil_bytes: 512, flags: 0, node_mask: 0, root_signature: 2, adapter_luid: 4 };
        capture.record(first, 21, Reason::ExecuteIndirect, 3).unwrap();
        capture.record(first, 29, Reason::MissingAllocator, 7).unwrap();
        capture.record(Identity { dxil_hash: 19, ..first }, 21, Reason::Bundle, 8).unwrap();
        capture.record(Identity { node_mask: 1, ..first }, 21, Reason::ScratchUnavailable, 8).unwrap();
        capture.record(Identity { root_signature: 3, ..first }, 21, Reason::ExecuteIndirect, 9).unwrap();
        let lines: Vec<serde_json::Value> = std::str::from_utf8(&capture.output).unwrap().lines()
            .map(|row| serde_json::from_str(row).unwrap()).collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["schema"], SCHEMA);
        assert_eq!(lines[1]["sequence"], 1);
        assert_eq!(lines[1]["reason"], "execute_indirect");
        assert_eq!(lines[1]["record_hash"], "0000000000000015");
        assert_eq!(lines[1]["presents"], 3);
        assert_eq!(lines[2]["dxil_hash"], "0000000000000013");
        assert_eq!(lines[3]["node_mask"], 1);
        assert_eq!(lines[4]["root_signature"], "0000000000000003");
    }

    #[test]
    fn chain_whole_demand_root_identity_includes_sampler_order_and_comparison() {
        let first = ([1, 2, 3, 4], false);
        let second = ([4, 3, 2, 1], true);
        assert_ne!(sampler_identity(&[first, second]), sampler_identity(&[second, first]));
        assert_ne!(sampler_identity(&[first]), sampler_identity(&[(first.0, true)]));
    }
}
