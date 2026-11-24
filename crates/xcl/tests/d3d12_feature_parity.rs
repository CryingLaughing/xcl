//! Compare the title's oversized OPTIONS query with the host D3D12 answer on the same GPU.

use std::mem::size_of_val;

use windows::core::Interface;
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIFactory6};

use xcl::gpu::{adapter, agility, recomp::runner};
use xcl::hle::d3d12x::{self, real, Backend};

#[repr(C)]
struct ConsoleOptions {
    host: D3D12_FEATURE_DATA_D3D12_OPTIONS,
    extra: u32,
}

#[test]
fn oversized_options_match_host_and_zero_extension() {
    let Ok(factory) = (unsafe { CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) }) else {
        runner::skip_test("no GPU");
        return;
    };
    let selector = adapter::Selector::from_env_or(&xcl::state::config().d3d12_adapter);
    let Some(native) = adapter::candidates(&factory, &selector)
        .into_iter()
        .find_map(|(adapter, _)| agility::create_device(&adapter, D3D_FEATURE_LEVEL_12_0).ok())
    else {
        runner::skip_test("no GPU");
        return;
    };

    // Query the native device first, before XCL adopts the driver's vtable.
    let mut expected = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    let mut actual = ConsoleOptions { host: Default::default(), extra: u32::MAX };
    let host_size = size_of_val(&expected) as u32;
    let console_size = size_of_val(&actual) as u32;
    assert_eq!((host_size, console_size), (0x3C, 0x40));
    let native_luid = unsafe { native.GetAdapterLuid() };
    unsafe {
        native.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut expected).cast(), host_size)
    }
    .expect("native OPTIONS query");
    let rejected = unsafe {
        native.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut actual).cast(), console_size)
    }
    .expect_err("native D3D12 must reject the console-sized OPTIONS query");
    assert_eq!(rejected.code(), E_INVALIDARG);

    if d3d12x::backend() != Backend::Real {
        runner::skip_test("no GPU");
        return;
    }
    let raw = real::device_raw();
    let adopted = unsafe { ID3D12Device::from_raw_borrowed(&raw) }.expect("real backend device");
    assert_eq!(unsafe { adopted.GetAdapterLuid() }, native_luid);

    actual = ConsoleOptions { host: Default::default(), extra: u32::MAX };
    unsafe {
        adopted.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut actual).cast(), console_size)
    }
    .expect("XCL must accept the console-sized OPTIONS query");
    assert_eq!(actual.host, expected);
    assert_eq!(actual.extra, 0);
}
