//! Window pictures for the live view, streamed by Windows.Graphics.Capture
//! as the window changes, rather than drawn by `PrintWindow` one at a time.

use std::sync::Mutex;

use anyhow::{anyhow, bail, Context as _, Result};
use windows::core::{IInspectable, Interface as _};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

use ocu_core::image::Order;
use ocu_core::{Capturer, Frame, Latest, Stream, WindowInfo};

use crate::capture;

/// Streams windows' pictures; `PrintWindow` stands in where it can't.
pub struct Streamer;

impl Capturer for Streamer {
    fn stream(&self, window: &WindowInfo) -> Result<Stream> {
        // Already initialised (either way) is fine.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            bail!("Windows.Graphics.Capture is not available");
        }
        let hwnd = capture::hwnd(window.id);
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForWindow(hwnd) }.context("capturing the window")?;

        // A machine with no GPU (a VM, a CI runner) still has WARP.
        let (device, context) = [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP]
            .into_iter()
            .find_map(|driver| {
                let mut device: Option<ID3D11Device> = None;
                let mut context: Option<ID3D11DeviceContext> = None;
                unsafe {
                    D3D11CreateDevice(
                        None,
                        driver,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )
                }
                .ok()?;
                Some((device, context))
            })
            .ok_or_else(|| anyhow!("Direct3D would not start"))?;
        let device = device.ok_or_else(|| anyhow!("Direct3D made no device"))?;
        let context = context.ok_or_else(|| anyhow!("Direct3D made no context"))?;
        let dxgi: IDXGIDevice = device.cast()?;
        let shared: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?.cast()?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &shared,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            item.Size()?,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // Not every Windows lets these go; the picture is the same either way.
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);

        let latest = Latest::new();
        let copier = Mutex::new(Copier {
            device,
            context,
            staging: None,
        });
        let (arrived, id) = (latest.clone(), window.id);
        pool.FrameArrived(
            &TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                let Some(pool) = pool.as_ref() else {
                    return Ok(());
                };
                let frame = pool.TryGetNextFrame()?;
                match copier.lock().unwrap().copy(&frame, id) {
                    Ok(picture) => arrived.push(picture),
                    Err(e) => log::debug!("streaming window {id}: {e:#}"),
                }
                Ok(())
            }),
        )?;
        let closed = latest.clone();
        item.Closed(
            &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                closed.stop();
                Ok(())
            }),
        )?;
        session.StartCapture()?;
        Ok(Stream::new(
            latest,
            Running {
                pool,
                session,
                _item: item,
            },
        ))
    }
}

/// A started capture, stopped when dropped.
struct Running {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Reads captured textures back from the GPU.
struct Copier {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// A CPU-readable texture the size of the last frame.
    staging: Option<(u32, u32, ID3D11Texture2D)>,
}

impl Copier {
    fn copy(
        &mut self,
        frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
        id: u64,
    ) -> Result<Frame> {
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let staging = match &self.staging {
            Some((w, h, t)) if (*w, *h) == (desc.Width, desc.Height) => t.clone(),
            _ => {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut t = None;
                unsafe {
                    self.device
                        .CreateTexture2D(&staging_desc, None, Some(&mut t))
                }?;
                let t = t.ok_or_else(|| anyhow!("Direct3D made no texture"))?;
                self.staging = Some((desc.Width, desc.Height, t.clone()));
                t
            }
        };
        unsafe { self.context.CopyResource(&staging, &texture) };

        // The texture can be larger than the window, and a window's capture
        // can include its invisible resize borders, which its listed frame
        // (and so action coordinates) leave out.
        let content = frame.ContentSize()?;
        let (cw, ch) = (
            (content.Width.max(0) as u32).min(desc.Width),
            (content.Height.max(0) as u32).min(desc.Height),
        );
        let hwnd = capture::hwnd(id);
        let visible = capture::frame(hwnd);
        let mut outer = RECT::default();
        let (ox, oy) = match unsafe { GetWindowRect(hwnd, &mut outer) } {
            Ok(()) if cw as i32 >= outer.right - outer.left => (
                (visible.x as i32 - outer.left).max(0) as u32,
                (visible.y as i32 - outer.top).max(0) as u32,
            ),
            _ => (0, 0),
        };
        let width = (visible.width.max(1.0) as u32).min(cw.saturating_sub(ox));
        let height = (visible.height.max(1.0) as u32).min(ch.saturating_sub(oy));
        if width == 0 || height == 0 {
            bail!("an empty frame");
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }?;
        let pitch = mapped.RowPitch as usize;
        let mut data = Vec::with_capacity(width as usize * height as usize * 4);
        for row in 0..height as usize {
            let start = (oy as usize + row) * pitch + ox as usize * 4;
            let line = unsafe {
                std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(start),
                    width as usize * 4,
                )
            };
            data.extend_from_slice(line);
        }
        unsafe { self.context.Unmap(&staging, 0) };
        Ok(Frame {
            window_id: id,
            width,
            height,
            stride: width as usize * 4,
            data,
            order: Order::Bgra,
        })
    }
}
