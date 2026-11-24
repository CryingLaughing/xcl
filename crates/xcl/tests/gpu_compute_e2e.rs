//! End to end without a game executable: a console compute shader in the retail container
//! layout (DXBC → DXIL part with XDAT → XBCH → FSSM, CS stage descriptor) goes through the
//! D3D12.X backend's CreateComputePipelineState hook as a title would hand it over →
//! recompiled → D3D12 pipeline on the adapter → dispatched → memory read back and checked.
//!
//! The program is wave64, as console compute programs are: on an adapter without 64-lane
//! waves (NVIDIA) the backend has to run it as two wave32 halves, or the driver refuses the
//! pipeline. The pipeline counters are checked, so the test cannot pass on a fallback path.

use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use xcl::gpu::rdna2::encode;
use xcl::gpu::recomp::runner;
use xcl::hle::d3d12x::{pso_async, real};

static SERIAL: Mutex<()> = Mutex::new(());

#[path = "support/console_cs.rs"]
mod fixture;
use fixture::console_cs;

const V: u64 = 256; // VALU source code of v0

/// 16×8 threads per group (two wave64s), s[0:1] = output base and row width, TGID_X/Y in
/// s2/s3, thread ids in v0/v1: each thread writes its global (x, y), and through LDS +
/// s_barrier the (x, y) of the thread at the mirrored index, which is in the other wave.
fn program() -> Vec<u8> {
    let list: &[(&str, &str, &[(&str, u64)], Option<u32>)] = &[
        ("V_LSHLREV_B32", "ENC_VOP2", &[("VDST", 2), ("SRC0", 132), ("VSRC1", 1)], None), // y · 16
        ("V_ADD_NC_U32", "ENC_VOP2", &[("VDST", 2), ("SRC0", V + 2), ("VSRC1", 0)], None), // index in group
        ("S_LSHL_B32", "ENC_SOP2", &[("SDST", 4), ("SSRC0", 2), ("SSRC1", 132)], None), // group x · 16
        ("V_ADD_NC_U32", "ENC_VOP2", &[("VDST", 3), ("SRC0", 4), ("VSRC1", 0)], None), // global x
        ("S_LSHL_B32", "ENC_SOP2", &[("SDST", 5), ("SSRC0", 3), ("SSRC1", 131)], None), // group y · 8
        ("V_ADD_NC_U32", "ENC_VOP2", &[("VDST", 4), ("SRC0", 5), ("VSRC1", 1)], None), // global y
        ("V_LSHLREV_B32", "ENC_VOP2", &[("VDST", 5), ("SRC0", 144), ("VSRC1", 4)], None),
        ("V_OR_B32", "ENC_VOP2", &[("VDST", 5), ("SRC0", V + 5), ("VSRC1", 3)], None), // x | y << 16
        ("V_LSHLREV_B32", "ENC_VOP2", &[("VDST", 6), ("SRC0", 130), ("VSRC1", 2)], None),
        ("DS_WRITE_B32", "ENC_DS", &[("ADDR", 6), ("DATA0", 5)], None),
        ("S_BARRIER", "ENC_SOPP", &[], None),
        ("V_SUB_NC_U32", "ENC_VOP2", &[("VDST", 7), ("SRC0", 255), ("VSRC1", 2)], Some(127)), // mirrored thread
        ("V_LSHLREV_B32", "ENC_VOP2", &[("VDST", 7), ("SRC0", 130), ("VSRC1", 7)], None),
        ("DS_READ_B32", "ENC_DS", &[("VDST", 12), ("ADDR", 7)], None),
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 11), ("SRC0", V + 5)], None),
        ("V_MUL_LO_U32", "ENC_VOP3", &[("VDST", 9), ("SRC0", V + 4), ("SRC1", 1)], None), // y · width
        ("V_ADD_NC_U32", "ENC_VOP2", &[("VDST", 9), ("SRC0", V + 9), ("VSRC1", 3)], None),
        ("V_LSHLREV_B32", "ENC_VOP2", &[("VDST", 9), ("SRC0", 131), ("VSRC1", 9)], None),
        ("V_ADD_NC_U32", "ENC_VOP2", &[("VDST", 9), ("SRC0", 0), ("VSRC1", 9)], None), // + output base
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 10), ("SRC0", 128)], None),
        ("GLOBAL_STORE_DWORDX2", "ENC_FLAT_GLBL", &[("ADDR", 9), ("DATA", 11), ("SADDR", 0x7D)], None),
        ("S_ENDPGM", "ENC_SOPP", &[], None),
    ];
    list.iter().flat_map(|(n, e, f, l)| encode(n, e, f, *l).unwrap_or_else(|x| panic!("{n}: {x}"))).collect()
}

#[test]
fn console_wave64_compute_through_the_pso_hook_dispatch_and_read_back() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    if xcl::gpu::recomp::dxc::location().is_err() {
        println!("no DXC – skipping");
        runner::skip_test("no DXC");
        return;
    }
    if xcl::hle::d3d12x::backend() != xcl::hle::d3d12x::Backend::Real {
        println!("no real D3D12 device – skipping");
        runner::skip_test("no GPU");
        return;
    }
    // 32- or 64-lane waves (not WARP / the Basic Render Driver: 4): the driver rejects the
    // translated program otherwise
    if runner::wave64_programs_supported() != Ok(true) {
        println!("no 32- or 64-lane waves – skipping");
        runner::skip_test("no wave64");
        return;
    }
    let raw = real::device_raw();
    let dev = unsafe { ID3D12Device::from_raw_borrowed(&raw) }.unwrap().clone();
    println!("backend device: {}", real::stats().adapter);

    // user SGPRs 2, TGID_X + TGID_Y, thread ids v0 + v1
    let blob = console_cs(&program(), 2 << 1 | 1 << 7 | 1 << 8 | 1 << 11, [16, 8, 1]);
    assert!(xcl::gpu::xbox::is_xbox(&blob), "expected a console container");
    let game_rs: ID3D12RootSignature = unsafe {
        let mut ser = None;
        D3D12SerializeRootSignature(&D3D12_ROOT_SIGNATURE_DESC::default(), D3D_ROOT_SIGNATURE_VERSION_1, &mut ser, None).unwrap();
        let ser = ser.unwrap();
        dev.CreateRootSignature(0, std::slice::from_raw_parts(ser.GetBufferPointer() as *const u8, ser.GetBufferSize())).unwrap()
    };
    let mut desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
        pRootSignature: std::mem::ManuallyDrop::new(Some(game_rs)),
        CS: D3D12_SHADER_BYTECODE { pShaderBytecode: blob.as_ptr() as _, BytecodeLength: blob.len() },
        ..Default::default()
    };
    let before = real::stats();
    let pso: Result<ID3D12PipelineState, _> = unsafe { dev.CreateComputePipelineState(&desc) };
    unsafe { std::mem::ManuallyDrop::drop(&mut desc.pRootSignature) };
    let pso = pso.expect("CreateComputePipelineState through the backend hook");
    // built in the background behind a stand-in when its shader was not compiled yet
    let real_raw = xcl::hle::d3d12x::pso_async::resolve_wait(pso.as_raw() as usize) as *mut std::ffi::c_void;
    let after = real::stats();
    assert_eq!(after.translated_pipelines, before.translated_pipelines + 1, "PSO was not built from the translated shader: {:?}", after.translation_errors);
    assert!(
        real::is_translated_compute(real_raw as usize),
        "resolved compute PSO must be registered in TRANSLATED_COMPUTE"
    );
    let pso = unsafe { ID3D12PipelineState::from_raw_borrowed(&real_raw) }.unwrap().clone();
    let rs_raw = real::pso_root_signature(pso.as_raw() as usize).expect("root signature of the translated PSO") as *mut std::ffi::c_void;
    let rs = unsafe { ID3D12RootSignature::from_raw_borrowed(&rs_raw) }.unwrap().clone();

    let (w, h, base) = (32usize, 24usize, 0x100usize);
    let mut io = vec![0u32; 128];
    io[0] = base as u32;
    io[1] = w as u32;
    let out = runner::dispatch_on(&dev, &pso, &rs, [2, 3, 1], &io, &vec![0u32; (base + w * h * 8) / 4]).expect("dispatch");
    for y in 0..h {
        for x in 0..w {
            let at = (base + (y * w + x) * 8) / 4;
            assert_eq!(out[at], (x | y << 16) as u32, "({x}, {y}): own value");
            let (gx, gy, i) = (x / 16, y / 8, (y % 8) * 16 + x % 16);
            let (px, py) = (gx * 16 + (127 - i) % 16, gy * 8 + (127 - i) / 16);
            assert_eq!(out[at + 1], (px | py << 16) as u32, "({x}, {y}): value of thread {} through LDS", 127 - i);
        }
    }
}

/// Cold DXIL cache: `CreateComputePipelineState` returns a stand-in immediately and compiles
/// the real compute PSO on the background worker. The real PSO (and NOT the stand-in) must be
/// registered in `TRANSLATED_COMPUTE` once the background build completes.
#[test]
fn async_compute_pso_registers_real_pso_and_not_standin() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    if xcl::gpu::recomp::dxc::location().is_err() {
        println!("no DXC – skipping");
        runner::skip_test("no DXC");
        return;
    }
    if xcl::hle::d3d12x::backend() != xcl::hle::d3d12x::Backend::Real {
        println!("no real D3D12 device – skipping");
        runner::skip_test("no GPU");
        return;
    }
    // 32- or 64-lane waves (not WARP / the Basic Render Driver: 4): the driver rejects the
    // translated program otherwise
    if runner::wave64_programs_supported() != Ok(true) {
        println!("no 32- or 64-lane waves – skipping");
        runner::skip_test("no wave64");
        return;
    }
    if !pso_async::enabled() {
        println!("XCL_ASYNC_PSO=0 – skipping");
        return;
    }

    struct ColdCache(std::path::PathBuf, Option<std::ffi::OsString>);
    impl Drop for ColdCache {
        fn drop(&mut self) {
            match &self.1 {
                Some(prev) => std::env::set_var("XCL_DXIL_CACHE", prev),
                None => std::env::remove_var("XCL_DXIL_CACHE"),
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let cold = ColdCache(
        std::env::temp_dir().join(format!("xcl-async-compute-pso-{}", std::process::id())),
        std::env::var_os("XCL_DXIL_CACHE"),
    );
    let _ = std::fs::remove_dir_all(&cold.0);
    std::env::set_var("XCL_DXIL_CACHE", &cold.0);

    let raw = real::device_raw();
    let dev = unsafe { ID3D12Device::from_raw_borrowed(&raw) }.unwrap().clone();

    // Distinct minimal console CS so neither disk nor in-memory DXIL cache already holds it.
    let code: Vec<u8> = [
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 2), ("SRC0", 0)][..], None),
        ("S_ENDPGM", "ENC_SOPP", &[][..], None),
    ]
    .iter()
    .flat_map(|(n, e, f, l)| encode(n, e, f, *l).unwrap_or_else(|x| panic!("{n}: {x}")))
    .collect();
    let blob = console_cs(&code, 2 << 1 | 1 << 7, [8, 8, 1]);

    let game_rs: ID3D12RootSignature = unsafe {
        let mut ser = None;
        D3D12SerializeRootSignature(&D3D12_ROOT_SIGNATURE_DESC::default(), D3D_ROOT_SIGNATURE_VERSION_1, &mut ser, None).unwrap();
        let ser = ser.unwrap();
        dev.CreateRootSignature(0, std::slice::from_raw_parts(ser.GetBufferPointer() as *const u8, ser.GetBufferSize())).unwrap()
    };
    let mut desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
        pRootSignature: std::mem::ManuallyDrop::new(Some(game_rs)),
        CS: D3D12_SHADER_BYTECODE { pShaderBytecode: blob.as_ptr() as _, BytecodeLength: blob.len() },
        ..Default::default()
    };
    let pso: Result<ID3D12PipelineState, _> = unsafe { dev.CreateComputePipelineState(&desc) };
    unsafe { std::mem::ManuallyDrop::drop(&mut desc.pRootSignature) };
    let pso = pso.expect("CreateComputePipelineState stand-in");
    let standin_raw = pso.as_raw() as usize;

    assert!(!real::is_translated_compute(standin_raw), "stand-in compute PSO must not be registered in TRANSLATED_COMPUTE");
    let real_raw = pso_async::resolve_wait(standin_raw);
    assert_ne!(real_raw, standin_raw, "cold DXIL cache must build compute PSO asynchronously behind a stand-in");
    assert!(real::is_translated_compute(real_raw), "background-built real compute PSO must be registered in TRANSLATED_COMPUTE");
    assert!(!real::is_translated_compute(standin_raw), "stand-in must remain unregistered after background build");
    assert!(real::pso_root_signature(real_raw).is_some(), "real compute PSO must have an ABI root signature");

    // Releasing the stand-in must release and forget the background-built real PSO too.
    drop(pso);
    assert!(!real::is_translated_compute(real_raw), "forgetting released async compute PSO must clean up TRANSLATED_COMPUTE");
}
