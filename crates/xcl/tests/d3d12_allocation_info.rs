//! Compare the console buffer allocation query with the native query on the same device.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::Arc;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIFactory6};
use windows_sys::Win32::System::Diagnostics::Debug::RtlLookupFunctionEntry;

use xcl::gpu::{adapter, agility, recomp::runner};
use xcl::hle::d3d12x::real;
use xcl::modules::{self, MappedModule};

#[derive(Clone, Copy, Debug)]
enum ArgumentOrder {
    DeviceFirst,
    ResultFirst,
}

type TitleQuery = fn(&ID3D12Device, &[D3D12_RESOURCE_DESC], ArgumentOrder) -> D3D12_RESOURCE_ALLOCATION_INFO;

#[inline(never)]
fn query_title(device: &ID3D12Device, descs: &[D3D12_RESOURCE_DESC], order: ArgumentOrder) -> D3D12_RESOURCE_ALLOCATION_INFO {
    type Slot = unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        u32,
        u32,
        *const D3D12_RESOURCE_DESC,
    ) -> *mut D3D12_RESOURCE_ALLOCATION_INFO;

    let mut result = D3D12_RESOURCE_ALLOCATION_INFO::default();
    let ret = &mut result as *mut D3D12_RESOURCE_ALLOCATION_INFO;
    let this = device.as_raw();
    let (first, second) = match order {
        ArgumentOrder::DeviceFirst => (this, ret.cast()),
        ArgumentOrder::ResultFirst => (ret.cast(), this),
    };
    // Call the adopted slot directly so its guard sees a return address in this function.
    let returned = unsafe {
        let slot: Slot = std::mem::transmute(device.vtable().GetResourceAllocationInfo);
        slot(first, second, 0, descs.len() as u32, descs.as_ptr())
    };
    assert_eq!(returned, ret, "{order:?} hidden result pointer");
    result
}

struct TitleCallRange(usize);

impl TitleCallRange {
    fn register(query: TitleQuery) -> Self {
        let address = query as *const () as u64;
        let mut image = 0;
        let function = unsafe { RtlLookupFunctionEntry(address, &mut image, null_mut()).as_ref() }
            .expect("allocation query caller must have an x64 unwind entry");
        let start = image + u64::from(function.BeginAddress);
        let end = image + u64::from(function.EndAddress);
        assert!((start..end).contains(&address));
        let base = start as usize;
        let name = "synthetic_allocation_info.exe";
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

impl Drop for TitleCallRange {
    fn drop(&mut self) {
        modules::unregister_mapped(self.0);
    }
}

#[test]
fn console_buffer_allocation_info_matches_native_in_both_argument_orders() {
    let Ok(factory) = (unsafe { CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) }) else {
        return runner::skip_test("no GPU");
    };
    let selector = adapter::Selector::from_env_or(&xcl::state::config().d3d12_adapter);
    let Some((device, adapter_desc)) = adapter::candidates(&factory, &selector).into_iter().find_map(|(candidate, desc)| {
        let device = agility::create_device(&candidate, D3D_FEATURE_LEVEL_11_0).ok()?;
        Some((device, desc))
    }) else {
        return runner::skip_test("no GPU");
    };
    println!("D3D12 adapter: {}", adapter::name(&adapter_desc));

    let native = [4096, 64 * 1024 + 4096].map(|width| D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: width,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    });
    // Native buffers require row-major layout and zero or 64 KiB alignment. XCL normalizes
    // the console descriptor before asking the host for the allocation of each buffer.
    let console = native.map(|desc| D3D12_RESOURCE_DESC {
        Alignment: 64,
        Layout: D3D12_TEXTURE_LAYOUT(0x22),
        ..desc
    });

    let expected = unsafe { device.GetResourceAllocationInfo(0, &native) };
    assert_ne!(expected.SizeInBytes, u64::MAX, "native descriptors must be valid");
    assert!(expected.SizeInBytes >= native.iter().map(|desc| desc.Width).sum::<u64>());
    assert!(expected.Alignment.is_power_of_two());
    let raw = unsafe { device.GetResourceAllocationInfo(0, &console) };
    assert_eq!(raw.SizeInBytes, u64::MAX, "raw D3D12 must reject the console descriptors");

    unsafe { real::adopt(device.as_raw(), real::Kind::Device) };
    let pc = unsafe { device.GetResourceAllocationInfo(0, &console) };
    assert_eq!(pc.SizeInBytes, u64::MAX, "the adopted device must still reject PC calls with console descriptors");

    let query = std::hint::black_box(query_title as TitleQuery);
    let _title = TitleCallRange::register(query);
    for order in [ArgumentOrder::DeviceFirst, ArgumentOrder::ResultFirst] {
        let actual = query(&device, &console, order);
        assert_eq!(actual, expected, "XCL console allocation info with {order:?} arguments");
    }
    println!("Normalized allocation: {} bytes aligned to {} bytes", expected.SizeInBytes, expected.Alignment);
}
