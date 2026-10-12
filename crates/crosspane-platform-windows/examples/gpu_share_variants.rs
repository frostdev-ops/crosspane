//! Lead probe: which D3D12 resource description opens on a D3D11 device here (no capture, no window).
//! `cargo run -p crosspane-platform-windows --example gpu_share_variants`
#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    use windows::{
        Win32::{
            Foundation::{CloseHandle, E_POINTER, GENERIC_ALL, HMODULE},
            Graphics::{
                Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0},
                Direct3D11::*,
                Direct3D12::*,
                Dxgi::Common::*,
                Dxgi::*,
            },
        },
        core::{Error, Interface},
    };
    let missing = || Error::from(E_POINTER);
    // SAFETY: standalone probe; every created object is owned by this function.
    unsafe {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let d11: ID3D11Device = device.ok_or_else(missing)?;
        let dxgi: IDXGIDevice = d11.cast()?;
        let adapter = dxgi.GetAdapter()?;
        println!(
            "adapter: {:?}",
            adapter.GetDesc().map(|d| (
                d.AdapterLuid.LowPart,
                String::from_utf16_lossy(&d.Description)
            ))
        );
        let mut d12 = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut d12)?;
        let d12: ID3D12Device = d12.ok_or_else(missing)?;
        let d11_1: ID3D11Device1 = d11.cast()?;
        let cases: &[(&str, D3D12_RESOURCE_FLAGS, DXGI_FORMAT)] = &[
            (
                "simultaneous BGRA (old, no render target)",
                D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            ),
            (
                "none BGRA",
                D3D12_RESOURCE_FLAG_NONE,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            ),
            (
                "render-target BGRA",
                D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            ),
            (
                "uav BGRA",
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            ),
            (
                "rt+simultaneous BGRA (current)",
                D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
                    | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            ),
            (
                "uav+simultaneous BGRA",
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
                    | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            ),
            (
                "none BGRA typeless",
                D3D12_RESOURCE_FLAG_NONE,
                DXGI_FORMAT_B8G8R8A8_TYPELESS,
            ),
            (
                "simultaneous RGBA",
                D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
                DXGI_FORMAT_R8G8B8A8_UNORM,
            ),
        ];
        for (name, flags, format) in cases {
            for heap in [
                D3D12_HEAP_FLAG_SHARED,
                D3D12_HEAP_FLAG_SHARED | D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER,
            ] {
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: 1280,
                    Height: 720,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: *format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                    Flags: *flags,
                    ..Default::default()
                };
                let mut resource: Option<ID3D12Resource> = None;
                let created = d12.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    heap,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut resource,
                );
                let label = format!("{name} heap={:#x}", heap.0);
                let Some(resource) = resource.filter(|_| created.is_ok()) else {
                    println!("{label}: create {created:?}");
                    continue;
                };
                let handle = match d12.CreateSharedHandle(&resource, None, GENERIC_ALL.0, None) {
                    Ok(handle) => handle,
                    Err(e) => {
                        println!("{label}: CreateSharedHandle {e:?}");
                        continue;
                    }
                };
                let opened: windows::core::Result<ID3D11Texture2D> =
                    d11_1.OpenSharedResource1(handle);
                match &opened {
                    Ok(texture) => {
                        let mut d = D3D11_TEXTURE2D_DESC::default();
                        texture.GetDesc(&mut d);
                        println!(
                            "{label}: OPENED bind={:#x} misc={:#x} usage={:?} fmt={:?}",
                            d.BindFlags, d.MiscFlags, d.Usage.0, d.Format.0
                        );
                    }
                    Err(e) => println!("{label}: OpenSharedResource1 {e:?}"),
                }
                let _ = CloseHandle(handle);
            }
        }
    }
    Ok(())
}
#[cfg(not(windows))]
fn main() {}
