//! Compare the console DispatchX command signature with a native indirect dispatch.

use std::mem::{size_of, size_of_val, ManuallyDrop};
use std::ptr::null_mut;
use std::sync::Arc;

use windows::core::{Interface, Result as WindowsResult};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIFactory6};
use windows_sys::Win32::System::Diagnostics::Debug::RtlLookupFunctionEntry;

use xcl::gpu::recomp::{dxc, runner};
use xcl::gpu::{adapter, agility, texture};
use xcl::hle::d3d12x::real;
use xcl::modules::{self, MappedModule};

const WORDS_PER_RESULT: usize = 4;
const RESULT_BYTES: u64 = (WORDS_PER_RESULT * size_of::<u32>()) as u64;
const SENTINEL: u32 = 0xA1B2_C3D4;

type CreateSignature = fn(
    &ID3D12Device,
    &D3D12_COMMAND_SIGNATURE_DESC,
    Option<&ID3D12RootSignature>,
) -> WindowsResult<ID3D12CommandSignature>;

#[inline(never)]
fn create_signature(
    device: &ID3D12Device,
    desc: &D3D12_COMMAND_SIGNATURE_DESC,
    root: Option<&ID3D12RootSignature>,
) -> WindowsResult<ID3D12CommandSignature> {
    let mut signature = null_mut();
    // The split vtable slot must see a return address in this function.
    unsafe {
        (device.vtable().CreateCommandSignature)(
            device.as_raw(),
            desc,
            root.map_or(null_mut(), |root| root.as_raw()),
            &ID3D12CommandSignature::IID,
            &mut signature,
        ).ok()?;
        assert!(!signature.is_null(), "successful command signature creation must return an object");
        Ok(ID3D12CommandSignature::from_raw(signature))
    }
}

struct TitleCallRange(usize);

impl TitleCallRange {
    fn register(create: CreateSignature) -> Self {
        let address = create as *const () as u64;
        let mut image = 0;
        let function = unsafe { RtlLookupFunctionEntry(address, &mut image, null_mut()).as_ref() }
            .expect("command signature caller must have an x64 unwind entry");
        let start = image + u64::from(function.BeginAddress);
        let end = image + u64::from(function.EndAddress);
        assert!((start..end).contains(&address));
        let base = start as usize;
        let name = "synthetic_dispatchx.exe";
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

unsafe fn buffer(
    device: &ID3D12Device,
    bytes: u64,
    heap: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
    flags: D3D12_RESOURCE_FLAGS,
) -> ID3D12Resource {
    let mut resource = None;
    device.CreateCommittedResource(
        &D3D12_HEAP_PROPERTIES { Type: heap, CreationNodeMask: 1, VisibleNodeMask: 1, ..Default::default() },
        D3D12_HEAP_FLAG_NONE,
        &D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
            ..Default::default()
        },
        state,
        None,
        &mut resource,
    ).expect("create fixture buffer");
    resource.unwrap()
}

unsafe fn upload<T: Copy>(device: &ID3D12Device, value: &T) -> ID3D12Resource {
    let bytes = size_of_val(value);
    let resource = buffer(device, bytes as u64, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_RESOURCE_FLAG_NONE);
    let mut mapped = null_mut();
    resource.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut mapped)).expect("map upload buffer");
    mapped.cast::<T>().write(*value);
    resource.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: bytes }));
    resource
}

unsafe fn transition(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
        ..Default::default()
    };
    list.ResourceBarrier(std::slice::from_ref(&barrier));
    ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
}

#[test]
fn console_dispatchx_signature_matches_native_indirect_dispatch() {
    if dxc::location().is_err() {
        return runner::skip_test("no DXC");
    }
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
    let dxil = dxc::compile(
        "RWByteAddressBuffer output : register(u0);\n[numthreads(1, 1, 1)]\nvoid main(uint3 id : SV_DispatchThreadID) { output.Store(4u * id.x, 1u + id.x); }",
        "main",
        "cs_6_6",
        &[],
    ).expect("compile indirect-dispatch fixture");

    unsafe {
        let uav = D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
            Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR::default() },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let mut serialized = None;
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC { NumParameters: 1, pParameters: &uav, ..Default::default() },
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut serialized,
            None,
        ).expect("serialize compute root signature");
        let serialized = serialized.unwrap();
        let root: ID3D12RootSignature = device.CreateRootSignature(
            0,
            std::slice::from_raw_parts(serialized.GetBufferPointer().cast(), serialized.GetBufferSize()),
        ).expect("create compute root signature");
        let mut pso_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr().cast(), BytecodeLength: dxil.len() },
            ..Default::default()
        };
        let pso: WindowsResult<ID3D12PipelineState> = device.CreateComputePipelineState(&pso_desc);
        ManuallyDrop::drop(&mut pso_desc.pRootSignature);
        let pso = pso.expect("create compute PSO");

        // Create the list and resources before adoption to keep indirect command recording native.
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
        let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso).unwrap();
        let arguments = upload(&device, &D3D12_DISPATCH_ARGUMENTS { ThreadGroupCountX: 3, ThreadGroupCountY: 1, ThreadGroupCountZ: 1 });
        let initial = upload(&device, &[SENTINEL; 2 * WORDS_PER_RESULT]);
        let output = buffer(&device, 2 * RESULT_BYTES, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        let readback = buffer(&device, 2 * RESULT_BYTES, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_FLAG_NONE);

        let dispatch = D3D12_INDIRECT_ARGUMENT_DESC { Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH, ..Default::default() };
        let dispatch_x = D3D12_INDIRECT_ARGUMENT_DESC { Type: D3D12_INDIRECT_ARGUMENT_TYPE(105), ..Default::default() };
        let native_desc = D3D12_COMMAND_SIGNATURE_DESC {
            ByteStride: size_of::<D3D12_DISPATCH_ARGUMENTS>() as u32,
            NumArgumentDescs: 1,
            pArgumentDescs: &dispatch,
            NodeMask: 0,
        };
        let console_desc = D3D12_COMMAND_SIGNATURE_DESC { pArgumentDescs: &dispatch_x, ..native_desc };
        let create = std::hint::black_box(create_signature as CreateSignature);
        let native = create(&device, &native_desc, None).expect("native dispatch command signature");
        assert!(create(&device, &console_desc, None).is_err(), "raw D3D12 must reject DispatchX");
        real::adopt(device.as_raw(), real::Kind::Device);
        assert!(create(&device, &console_desc, None).is_err(), "an adopted PC call must still reject DispatchX");
        let console = {
            let _title = TitleCallRange::register(create);
            create(&device, &console_desc, Some(&root)).expect("XCL must accept a title's DispatchX signature")
        };

        list.CopyBufferRegion(&output, 0, &initial, 0, 2 * RESULT_BYTES);
        transition(&list, &output, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        list.SetComputeRootSignature(&root);
        let address = output.GetGPUVirtualAddress();
        list.SetComputeRootUnorderedAccessView(0, address);
        list.ExecuteIndirect(&native, 1, &arguments, 0, None, 0);
        list.SetComputeRootUnorderedAccessView(0, address + RESULT_BYTES);
        list.ExecuteIndirect(&console, 1, &arguments, 0, None, 0);
        transition(&list, &output, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        list.CopyBufferRegion(&readback, 0, &output, 0, 2 * RESULT_BYTES);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        texture::wait(&device, &queue).expect("indirect dispatch readback fence");

        let mut mapped = null_mut();
        let range = D3D12_RANGE { Begin: 0, End: (2 * RESULT_BYTES) as usize };
        readback.Map(0, Some(&range), Some(&mut mapped)).expect("map dispatch readback");
        let actual = mapped.cast::<[u32; 2 * WORDS_PER_RESULT]>().read();
        readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
        println!("Indirect dispatch readback: {actual:#010x?}");
        assert_eq!(actual[..WORDS_PER_RESULT], [1, 2, 3, SENTINEL], "native dispatch readback");
        assert_eq!(actual[WORDS_PER_RESULT..], actual[..WORDS_PER_RESULT], "console DispatchX readback");
    }
}
