//! WGC HWND pixels and UIA standard controls; no focus, SendInput or UIPI workaround.
use super::{
    windows_session::{Backend, Pixels, WindowAuthority},
    windows_target::Target,
};
use octendra_desktop_protocol::{ErrorCode, MAX_WORKER_PIXELS, RawPixels, SourcePoint};
use std::time::{Duration, Instant};
use windows::{
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{HMODULE, HWND, POINT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::*,
            Dxgi::{Common::DXGI_FORMAT_B8G8R8A8_UNORM, IDXGIDevice},
        },
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
            },
        },
        UI::Accessibility::*,
    },
    core::{Interface, factory},
};

fn qpc_100ns() -> windows::core::Result<i64> {
    let (mut ticks, mut frequency) = (0, 0);
    unsafe {
        QueryPerformanceCounter(&mut ticks)?;
        QueryPerformanceFrequency(&mut frequency)?;
    }
    if frequency <= 0 {
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x80004005u32 as i32,
        )));
    }
    Ok(((ticks as i128) * 10_000_000 / (frequency as i128)) as i64)
}
struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
}
impl Capture {
    fn new(window: u64, expected: [u32; 2], maximum: usize) -> windows::core::Result<Self> {
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForWindow(HWND(window as usize as *mut _))? };
        let size = item.Size()?;
        let length = (size.Width as usize)
            .checked_mul(size.Height as usize)
            .and_then(|n| n.checked_mul(4));
        if size.Width <= 0
            || size.Height <= 0
            || [size.Width as u32, size.Height as u32] != expected
            || !length.is_some_and(|n| n <= maximum && n <= MAX_WORKER_PIXELS)
        {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                0x80070057u32 as i32,
            )));
        }
        let (mut device, mut context) = (None, None);
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.ok_or_else(windows::core::Error::from_thread)?;
        let context = context.ok_or_else(windows::core::Error::from_thread)?;
        let dxgi: IDXGIDevice = device.cast()?;
        let direct: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // Keep platform capture indicators and restrictions at their defaults.
        session.StartCapture()?;
        Ok(Self {
            device,
            context,
            item,
            pool,
            session,
        })
    }
    fn pixels(&self, maximum: usize) -> windows::core::Result<Pixels> {
        let admitted = qpc_100ns()?;
        let end = Instant::now() + Duration::from_millis(800);
        loop {
            if Instant::now() >= end {
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x800705B4u32 as i32,
                )));
            }
            let frame = match self.pool.TryGetNextFrame() {
                Ok(f) => f,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(4));
                    continue;
                }
            };
            if frame.SystemRelativeTime()?.Duration < admitted {
                frame.Close()?;
                continue;
            }
            let captured = Instant::now();
            let size = frame.ContentSize()?;
            let current = self.item.Size()?;
            if size.Width <= 0
                || size.Height <= 0
                || size.Width > 8192
                || size.Height > 8192
                || size != current
            {
                frame.Close()?;
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80070057u32 as i32,
                )));
            }
            let length = (size.Width as usize)
                .checked_mul(size.Height as usize)
                .and_then(|n| n.checked_mul(4))
                .filter(|n| *n <= maximum && *n <= MAX_WORKER_PIXELS)
                .ok_or_else(|| {
                    windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32))
                })?;
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe {
                texture.GetDesc(&mut desc);
            }
            if desc.Width < size.Width as u32
                || desc.Height < size.Height as u32
                || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
                || desc.MipLevels != 1
                || desc.ArraySize != 1
                || desc.SampleDesc.Count != 1
                || !super::windows_session::bounded_texture(desc.Width, desc.Height, maximum)
            {
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80070057u32 as i32,
                )));
            }
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut staging = None;
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut staging))?;
            }
            let staging = staging.ok_or_else(windows::core::Error::from_thread)?;
            unsafe {
                self.context.CopyResource(&staging, &texture);
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            }
            let row = size.Width as usize * 4;
            let copied = (|| {
                if mapped.pData.is_null() || (mapped.RowPitch as usize) < row {
                    return None;
                }
                let mut bytes = vec![0u8; length];
                for y in 0..size.Height as usize {
                    let source = unsafe {
                        std::slice::from_raw_parts(
                            (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                            row,
                        )
                    };
                    for (input, output) in source
                        .chunks_exact(4)
                        .zip(bytes[y * row..(y + 1) * row].chunks_exact_mut(4))
                    {
                        output.copy_from_slice(&[input[2], input[1], input[0], input[3]]);
                    }
                }
                Some(bytes)
            })();
            unsafe {
                self.context.Unmap(&staging, 0);
            }
            frame.Close()?;
            let bytes = copied.ok_or_else(windows::core::Error::from_thread)?;
            return Ok(Pixels {
                metadata: RawPixels {
                    encoding: "rgba8".into(),
                    size: [size.Width as u32, size.Height as u32],
                    color: [0; 4],
                    plane_lengths: vec![bytes.len()],
                    plane_sha256: vec![crate::hash(&bytes)],
                },
                bytes,
                captured,
            });
        }
    }
    fn close(&self) -> bool {
        let session = self.session.Close().is_ok();
        let pool = self.pool.Close().is_ok();
        session && pool
    }
}
pub(super) struct Native {
    target: Target,
    authority: WindowAuthority,
    size: [u32; 2],
    position: [i32; 2],
    maximum: usize,
    capture: Option<Capture>,
    automation: IUIAutomation,
}
impl Native {
    pub fn new(
        authority: WindowAuthority,
        size: [u32; 2],
        position: [i32; 2],
        maximum: usize,
    ) -> anyhow::Result<Self> {
        let target = Target::bind_expected(&authority)?;
        let automation = unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
        Ok(Self {
            target,
            authority,
            size,
            position,
            maximum,
            capture: None,
            automation,
        })
    }
    fn valid_input(&self, captured: Instant, deadline: Instant) -> bool {
        let live = self.live();
        super::windows_session::valid_input(live, captured, deadline, Instant::now())
    }
}
enum Pattern {
    Invoke(IUIAutomationInvokePattern),
    Toggle(IUIAutomationTogglePattern),
    Select(IUIAutomationSelectionItemPattern),
}
impl Backend for Native {
    fn live(&self) -> bool {
        self.target.live_geometry() == Some((self.size, self.position))
    }
    fn open(&mut self) -> Result<(), ErrorCode> {
        if !self.live() || self.capture.is_some() {
            return Err(ErrorCode::Unavailable);
        }
        self.capture = Some(
            Capture::new(self.authority.window_id, self.size, self.maximum)
                .map_err(|_| ErrorCode::Unavailable)?,
        );
        Ok(())
    }
    fn capture(&mut self, maximum: usize) -> Result<Pixels, ErrorCode> {
        if !self.live() {
            return Err(ErrorCode::StaleFrame);
        }
        let pixels = self
            .capture
            .as_ref()
            .ok_or(ErrorCode::SessionClosed)?
            .pixels(maximum)
            .map_err(|_| ErrorCode::Unavailable)?;
        if pixels.metadata.size != self.size {
            return Err(ErrorCode::StaleFrame);
        }
        Ok(pixels)
    }
    fn invoke(
        &mut self,
        point: SourcePoint,
        captured: Instant,
        deadline: Instant,
    ) -> Result<(), ErrorCode> {
        if !self.valid_input(captured, deadline) {
            return Err(ErrorCode::StaleFrame);
        }
        // Point lookup cannot activate a window. An occluding or foreign control
        // is rejected instead of falling back to focus changes or global input.
        let prepared = (|| -> windows::core::Result<_> {
            unsafe {
                let root = self
                    .automation
                    .ElementFromHandle(HWND(self.authority.window_id as usize as *mut _))?;
                let element = self.automation.ElementFromPoint(POINT {
                    x: self.position[0] + point.x.round() as i32,
                    y: self.position[1] + point.y.round() as i32,
                })?;
                if element.CurrentProcessId()? != self.authority.process_id as i32
                    || element.CurrentIsOffscreen()?.as_bool()
                    || !element.CurrentIsEnabled()?.as_bool()
                {
                    return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                        0x80070005u32 as i32,
                    )));
                }
                let walker = self.automation.RawViewWalker()?;
                let mut ancestor = element.clone();
                let mut belongs = false;
                for _ in 0..64 {
                    if self.automation.CompareElements(&root, &ancestor)?.as_bool() {
                        belongs = true;
                        break;
                    }
                    ancestor = walker.GetParentElement(&ancestor)?;
                }
                if !belongs {
                    return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                        0x80070005u32 as i32,
                    )));
                }
                use super::windows_session::{Activation, activation};
                let operation = activation(element.CurrentControlType()?.0).ok_or_else(|| {
                    windows::core::Error::from_hresult(windows::core::HRESULT(0x80070005u32 as i32))
                })?;
                let pattern = match operation {
                    Activation::Invoke => {
                        Pattern::Invoke(element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(
                            UIA_InvokePatternId,
                        )?)
                    }
                    Activation::Toggle => {
                        Pattern::Toggle(element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(
                            UIA_TogglePatternId,
                        )?)
                    }
                    Activation::Select => Pattern::Select(
                        element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                            UIA_SelectionItemPatternId,
                        )?,
                    ),
                };
                Ok(pattern)
            }
        })();
        let pattern = prepared.map_err(|_| ErrorCode::InvalidActions)?;
        if !self.valid_input(captured, deadline) {
            return Err(ErrorCode::StaleFrame);
        }
        let effect = unsafe {
            match pattern {
                Pattern::Invoke(p) => p.Invoke(),
                Pattern::Toggle(p) => p.Toggle(),
                Pattern::Select(p) => p.Select(),
            }
        };
        effect.map_err(|_| ErrorCode::OutcomeUnknown)
    }
    fn close(&mut self) -> bool {
        self.capture.take().is_none_or(|c| c.close())
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        self.close();
    }
}
