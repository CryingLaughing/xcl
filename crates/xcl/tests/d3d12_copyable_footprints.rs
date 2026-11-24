//! Compare the console texture footprint fallback with the host query on the same device.

use std::ptr::null_mut;
use std::sync::Arc;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIFactory6};
use windows_sys::Win32::System::Diagnostics::Debug::RtlLookupFunctionEntry;

use xcl::gpu::{adapter, agility, recomp::runner};
use xcl::hle::d3d12x::real;
use xcl::modules::{self, MappedModule};

const FIRST_MIP: u32 = 1;
const MIP_COUNT: u32 = 2;
const BASE_OFFSET: u64 = D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT as u64;

#[derive(Debug, Default, PartialEq)]
struct Footprints {
    layouts: [D3D12_PLACED_SUBRESOURCE_FOOTPRINT; MIP_COUNT as usize],
    rows: [u32; MIP_COUNT as usize],
    row_bytes: [u64; MIP_COUNT as usize],
}

type FootprintQuery = fn(&ID3D12Device, &D3D12_RESOURCE_DESC, Option<&mut u64>) -> Footprints;

#[inline(never)]
fn footprints(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC, total: Option<&mut u64>) -> Footprints {
    let mut result = Footprints::default();
    unsafe {
        // Call the vtable directly so the split guard sees a return address in this function.
        (device.vtable().GetCopyableFootprints)(
            device.as_raw(),
            desc,
            FIRST_MIP,
            MIP_COUNT,
            BASE_OFFSET,
            result.layouts.as_mut_ptr(),
            result.rows.as_mut_ptr(),
            result.row_bytes.as_mut_ptr(),
            total.map_or(null_mut(), |total| total as *mut u64),
        );
    }
    result
}

struct RegisteredTitleCall(usize);

impl RegisteredTitleCall {
    fn new(query: FootprintQuery) -> Self {
        let address = query as *const () as u64;
        let mut image_base = 0;
        let entry = unsafe { RtlLookupFunctionEntry(address, &mut image_base, null_mut()).as_ref() }
            .expect("footprint query must have an x64 unwind entry");
        let start = image_base + u64::from(entry.BeginAddress);
        let end = image_base + u64::from(entry.EndAddress);
        assert!((start..end).contains(&address), "unwind entry must cover the footprint query");

        // The guard only needs this function's code range, not the rest of the test executable.
        let base = start as usize;
        let name = "synthetic_footprints.exe";
        modules::register_mapped(Arc::new(MappedModule {
            name: name.into(),
            path: name.into(),
            base,
            size: (end - start) as usize,
            entry_rva: 0,
            is_dll: false,
            exports: Default::default(),
            tls_callbacks: Default::default(),
            no_thread_calls: Default::default(),
            initialized: Default::default(),
            stack_reserve: 0,
        }));
        Self(base)
    }
}

impl Drop for RegisteredTitleCall {
    fn drop(&mut self) {
        modules::unregister_mapped(self.0);
    }
}

#[test]
fn console_texture_footprints_match_normalized_host_desc() {
    let Ok(factory) = (unsafe { CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) }) else {
        runner::skip_test("no GPU");
        return;
    };
    let selector = adapter::Selector::from_env_or(&xcl::state::config().d3d12_adapter);
    let Some((device, adapter_desc)) = adapter::candidates(&factory, &selector)
        .into_iter()
        .find_map(|(candidate, desc)| agility::create_device(&candidate, D3D_FEATURE_LEVEL_11_0).ok().map(|device| (device, desc)))
    else {
        runner::skip_test("no GPU");
        return;
    };
    println!("D3D12 adapter: {}", adapter::name(&adapter_desc));

    let pc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 31,
        Height: 17,
        DepthOrArraySize: 1,
        MipLevels: 3,
        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        ..Default::default()
    };
    let console = D3D12_RESOURCE_DESC {
        Format: DXGI_FORMAT(116), // Xbox R10G10B10_7E3_A2_FLOAT
        Layout: D3D12_TEXTURE_LAYOUT(0x105),
        ..pc
    };

    // Keep the calls indirect so they all execute the function whose range we register.
    let query = std::hint::black_box(footprints as FootprintQuery);
    let mut rejected = 0;
    query(&device, &console, Some(&mut rejected));
    assert_eq!(rejected, u64::MAX, "host must reject the console descriptor");

    let mut expected_total = 0;
    let expected = query(&device, &pc, Some(&mut expected_total));
    assert!(expected_total > 0 && expected_total != u64::MAX, "host must accept the normalized descriptor");
    assert!(expected.layouts.iter().all(|layout| layout.Footprint.RowPitch > 0));

    // Install the XCL vtable on the device that provided the native reference.
    unsafe { real::adopt(device.as_raw(), real::Kind::Device) };
    rejected = 0;
    query(&device, &console, Some(&mut rejected));
    assert_eq!(rejected, u64::MAX, "the adopted device must still reject PC calls with the console descriptor");

    let _title = RegisteredTitleCall::new(query);
    let mut actual_total = 0;
    let actual = query(&device, &console, Some(&mut actual_total));
    assert_eq!(actual, expected, "XCL footprints with total bytes requested");
    assert_eq!(actual_total, expected_total);
    assert_eq!(query(&device, &console, None), expected, "XCL footprints without total bytes requested");
}
