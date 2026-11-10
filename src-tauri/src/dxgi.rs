//! Windows screen capture via DXGI Desktop Duplication.
//!
//! Much faster than GDI BitBlt (≈2–4 ms vs ≈40 ms for 1080p), and it only
//! returns when the screen actually changed, which keeps latency and CPU low.

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

use crate::capture::{FrameSource, RawFrame};

pub struct DxgiSource {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    cursor: Option<(i32, i32)>,
}

impl DxgiSource {
    /// Opens the DXGI output whose desktop origin is (`x`, `y`).
    pub fn open(x: i32, y: i32) -> anyhow::Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut ai = 0;
            while let Ok(adapter) = factory.EnumAdapters1(ai) {
                ai += 1;
                let mut oi = 0;
                while let Ok(output) = adapter.EnumOutputs(oi) {
                    oi += 1;
                    let desc = output.GetDesc()?;
                    let r = desc.DesktopCoordinates;
                    if r.left != x || r.top != y {
                        continue;
                    }
                    let mut device = None;
                    let mut context = None;
                    D3D11CreateDevice(
                        &adapter.cast::<IDXGIAdapter>()?,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )?;
                    let device = device.ok_or_else(|| anyhow::anyhow!("no D3D device"))?;
                    let context = context.ok_or_else(|| anyhow::anyhow!("no D3D context"))?;
                    let dup = output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device)?;
                    return Ok(Self { device, context, dup, staging: None, cursor: None });
                }
            }
        }
        anyhow::bail!("No DXGI output at ({x}, {y})")
    }
}

impl FrameSource for DxgiSource {
    fn grab(&mut self, timeout_ms: u32, on_frame: &mut dyn FnMut(RawFrame)) -> anyhow::Result<bool> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            if let Err(e) = self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(false);
                }
                if e.code() == DXGI_ERROR_ACCESS_LOST {
                    anyhow::bail!("DXGI access lost (display mode change / secure desktop)");
                }
                return Err(e.into());
            }

            if info.LastMouseUpdateTime != 0 {
                self.cursor = info
                    .PointerPosition
                    .Visible
                    .as_bool()
                    .then_some((info.PointerPosition.Position.x, info.PointerPosition.Position.y));
            }

            let mut got = false;
            if info.LastPresentTime != 0 {
                if let Some(resource) = resource {
                    let result = self.copy_out(&resource, on_frame);
                    let _ = self.dup.ReleaseFrame();
                    result?;
                    got = true;
                } else {
                    let _ = self.dup.ReleaseFrame();
                }
            } else {
                let _ = self.dup.ReleaseFrame();
            }
            Ok(got)
        }
    }

    fn cursor(&mut self) -> Option<(i32, i32)> {
        self.cursor
    }
}

impl DxgiSource {
    unsafe fn copy_out(&mut self, resource: &IDXGIResource, on_frame: &mut dyn FnMut(RawFrame)) -> anyhow::Result<()> {
        let texture: ID3D11Texture2D = resource.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);

        let needs_new = !matches!(&self.staging, Some((_, w, h)) if *w == desc.Width && *h == desc.Height);
        if needs_new {
            let mut sd = desc;
            sd.BindFlags = 0;
            sd.MiscFlags = 0;
            sd.MipLevels = 1;
            sd.ArraySize = 1;
            sd.SampleDesc.Count = 1;
            sd.SampleDesc.Quality = 0;
            sd.Usage = D3D11_USAGE_STAGING;
            sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            self.device.CreateTexture2D(&sd, None, Some(&mut staging))?;
            let staging = staging.ok_or_else(|| anyhow::anyhow!("staging texture failed"))?;
            self.staging = Some((staging, desc.Width, desc.Height));
        }
        let (staging, w, h) = self.staging.as_ref().unwrap();
        let dst: ID3D11Resource = staging.cast()?;
        let src: ID3D11Resource = texture.cast()?;
        self.context.CopyResource(&dst, &src);

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.context.Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let stride = mapped.RowPitch as usize;
        let data = std::slice::from_raw_parts(mapped.pData as *const u8, stride * *h as usize);
        on_frame(RawFrame { data, width: *w, height: *h, stride, bgra: true });
        self.context.Unmap(&dst, 0);
        Ok(())
    }
}
