//! WGC HWND pixels and UIA standard controls; no focus, SendInput or UIPI workaround.
use super::{
    windows_session::{
        Backend, HitNode, MAX_UIA_DEPTH, MAX_UIA_DURATION, MAX_UIA_ELEMENTS, MAX_UIA_VISITS,
        Pixels, ScrollStep, WindowAuthority, redact_uia_value, scroll_steps,
        scrollable_control_ancestor, standard_control_ancestor, uia_is_interactive,
        uia_is_title_or_text, uia_role_name, value_control_ancestor, window_bounds,
    },
    windows_target::Target,
};
use octendra_desktop_protocol::{
    ErrorCode, MAX_WORKER_PIXELS, RawPixels, SourcePoint, ui::SemanticElement,
};
use std::{
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};
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
        UI::{Accessibility::*, HiDpi::*},
    },
    core::{BSTR, Interface, factory},
};

/// UIA hit testing virtualizes coordinates for unaware clients. Set only this
/// private worker's thread, before COM/UIA creation, and restore it on all exits.
pub(super) struct PhysicalDpi {
    previous: DPI_AWARENESS_CONTEXT,
    same_thread: PhantomData<Rc<()>>,
}
impl PhysicalDpi {
    pub fn enter() -> anyhow::Result<Self> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        anyhow::ensure!(!previous.0.is_null(), "physical DPI context unavailable");
        let guard = Self {
            previous,
            same_thread: PhantomData,
        };
        anyhow::ensure!(Self::active(), "physical DPI context not established");
        Ok(guard)
    }
    fn active() -> bool {
        unsafe {
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
            .as_bool()
        }
    }
}
impl Drop for PhysicalDpi {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.previous);
        }
    }
}

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
        anyhow::ensure!(
            PhysicalDpi::active(),
            "UIA requires physical DPI coordinates"
        );
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
        let live = PhysicalDpi::active() && self.live();
        super::windows_session::valid_input(live, captured, deadline, Instant::now())
    }
    unsafe fn raw_ancestors(
        &self,
        point: [i32; 2],
        include_root: bool,
    ) -> windows::core::Result<(Vec<IUIAutomationElement>, Vec<HitNode>, bool)> {
        let denied =
            || windows::core::Error::from_hresult(windows::core::HRESULT(0x80070005u32 as i32));
        if !PhysicalDpi::active() {
            return Err(denied());
        }
        unsafe {
            let root = self
                .automation
                .ElementFromHandle(HWND(self.authority.window_id as usize as *mut _))?;
            let mut element = self.automation.ElementFromPoint(POINT {
                x: point[0],
                y: point[1],
            })?;
            let walker = self.automation.RawViewWalker()?;
            let (mut elements, mut nodes) = (Vec::new(), Vec::new());
            let mut root_reached = false;
            for _ in 0..64 {
                let is_root = self.automation.CompareElements(&root, &element)?.as_bool();
                if is_root && !include_root {
                    root_reached = true;
                    break;
                }
                let pid = element.CurrentProcessId()?;
                if pid != self.authority.process_id as i32 {
                    return Err(denied());
                }
                let rect = element.CurrentBoundingRectangle()?;
                nodes.push(HitNode {
                    control_type: element.CurrentControlType()?.0,
                    process_id: pid as u32,
                    enabled: element.CurrentIsEnabled()?.as_bool(),
                    offscreen: element.CurrentIsOffscreen()?.as_bool(),
                    bounds: [rect.left, rect.top, rect.right, rect.bottom],
                });
                elements.push(element.clone());
                if is_root {
                    root_reached = true;
                    break;
                }
                element = walker.GetParentElement(&element)?;
            }
            Ok((elements, nodes, root_reached))
        }
    }
    /// Inspect only ancestors below the pinned window. An occluding/foreign
    /// process, disabled nearest control or unproven root has no fallback.
    unsafe fn control_at(
        &self,
        point: [i32; 2],
    ) -> windows::core::Result<(IUIAutomationElement, super::windows_session::Activation)> {
        let denied =
            || windows::core::Error::from_hresult(windows::core::HRESULT(0x80070005u32 as i32));
        let (elements, nodes, root_reached) = unsafe { self.raw_ancestors(point, false)? };
        let (index, operation) =
            standard_control_ancestor(&nodes, root_reached, self.authority.process_id, point)
                .ok_or_else(denied)?;
        Ok((elements[index].clone(), operation))
    }
    unsafe fn value_control_at(
        &self,
        point: [i32; 2],
    ) -> windows::core::Result<(IUIAutomationElement, IUIAutomationValuePattern)> {
        let denied =
            || windows::core::Error::from_hresult(windows::core::HRESULT(0x80070005u32 as i32));
        let (elements, nodes, root_reached) = unsafe { self.raw_ancestors(point, false)? };
        let index = value_control_ancestor(&nodes, root_reached, self.authority.process_id, point)
            .ok_or_else(denied)?;
        let element = elements[index].clone();
        let pattern = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)?
        };
        if unsafe { pattern.CurrentIsReadOnly()?.as_bool() } {
            return Err(denied());
        }
        Ok((element, pattern))
    }
    unsafe fn scroll_control_at(
        &self,
        point: [i32; 2],
        horizontal: i16,
        vertical: i16,
    ) -> windows::core::Result<(IUIAutomationElement, IUIAutomationScrollPattern)> {
        let denied =
            || windows::core::Error::from_hresult(windows::core::HRESULT(0x80070005u32 as i32));
        let (elements, nodes, root_reached) = unsafe { self.raw_ancestors(point, true)? };
        let mut patterns: Vec<Option<IUIAutomationScrollPattern>> =
            Vec::with_capacity(elements.len());
        let mut scrollable = Vec::with_capacity(elements.len());
        for element in &elements {
            let pat = unsafe {
                element
                    .GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
                    .ok()
            };
            let ok = pat.as_ref().is_some_and(|p| unsafe {
                (horizontal == 0
                    || p.CurrentHorizontallyScrollable()
                        .is_ok_and(|b| b.as_bool()))
                    && (vertical == 0
                        || p.CurrentVerticallyScrollable()
                            .is_ok_and(|b| b.as_bool()))
            });
            scrollable.push(ok);
            patterns.push(pat);
        }
        let index = scrollable_control_ancestor(
            &nodes,
            root_reached,
            self.authority.process_id,
            point,
            &scrollable,
        )
        .ok_or_else(denied)?;
        let pattern = patterns[index].clone().ok_or_else(denied)?;
        Ok((elements[index].clone(), pattern))
    }
}
fn to_scroll_amount(step: ScrollStep) -> ScrollAmount {
    match step {
        ScrollStep::NoAmount => ScrollAmount_NoAmount,
        ScrollStep::SmallDecrement => ScrollAmount_SmallDecrement,
        ScrollStep::LargeDecrement => ScrollAmount_LargeDecrement,
        ScrollStep::SmallIncrement => ScrollAmount_SmallIncrement,
        ScrollStep::LargeIncrement => ScrollAmount_LargeIncrement,
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
        if !PhysicalDpi::active() || !self.live() || self.capture.is_some() {
            return Err(ErrorCode::Unavailable);
        }
        self.capture = Some(
            Capture::new(self.authority.window_id, self.size, self.maximum)
                .map_err(|_| ErrorCode::Unavailable)?,
        );
        Ok(())
    }
    fn capture(&mut self, maximum: usize) -> Result<Pixels, ErrorCode> {
        if !PhysicalDpi::active() || !self.live() {
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
    fn elements(&self) -> Vec<SemanticElement> {
        if !PhysicalDpi::active() || !self.live() {
            return Vec::new();
        }
        struct WalkCtx {
            pid: i32,
            position: [i32; 2],
            size: [u32; 2],
            started: Instant,
            visited: usize,
            elements: Vec<SemanticElement>,
        }
        unsafe fn walk(
            walker: &IUIAutomationTreeWalker,
            element: &IUIAutomationElement,
            depth: usize,
            ctx: &mut WalkCtx,
        ) {
            if depth > MAX_UIA_DEPTH
                || ctx.elements.len() >= MAX_UIA_ELEMENTS
                || ctx.visited >= MAX_UIA_VISITS
                || ctx.started.elapsed() >= MAX_UIA_DURATION
            {
                return;
            }
            ctx.visited += 1;
            let Ok(pid) = (unsafe { element.CurrentProcessId() }) else {
                return;
            };
            if pid != ctx.pid {
                return;
            }
            if unsafe { element.CurrentIsOffscreen() }.is_ok_and(|b| b.as_bool()) {
                return;
            }
            if let Ok(rect) = unsafe { element.CurrentBoundingRectangle() }
                && let Some(bounds) =
                    window_bounds(ctx.position, ctx.size, [rect.left, rect.top, rect.right, rect.bottom])
                && let Ok(ct) = unsafe { element.CurrentControlType() }
            {
                let control_type = ct.0;
                let interactive = uia_is_interactive(control_type);
                let is_title = uia_is_title_or_text(control_type);
                if interactive || is_title {
                    let name = unsafe { element.CurrentName() }
                        .ok()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    let enabled =
                        unsafe { element.CurrentIsEnabled() }.map_or(true, |b| b.as_bool());
                    let focused =
                        unsafe { element.CurrentHasKeyboardFocus() }.is_ok_and(|b| b.as_bool());
                    let is_password =
                        unsafe { element.CurrentIsPassword() }.is_ok_and(|b| b.as_bool());
                    let raw_value = if is_password {
                        None
                    } else {
                        unsafe {
                            element
                                .GetCurrentPatternAs::<IUIAutomationValuePattern>(
                                    UIA_ValuePatternId,
                                )
                                .ok()
                                .and_then(|p| p.CurrentValue().ok())
                                .map(|s| s.to_string().chars().take(240).collect())
                        }
                    };
                    let value = redact_uia_value(is_password, raw_value);
                    let checked = unsafe {
                        element
                            .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                            .ok()
                            .and_then(|p| p.CurrentToggleState().ok())
                            .and_then(|state| {
                                if state == ToggleState_On {
                                    Some(true)
                                } else if state == ToggleState_Off {
                                    Some(false)
                                } else {
                                    None
                                }
                            })
                    };
                    let selected = unsafe {
                        element
                            .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                                UIA_SelectionItemPatternId,
                            )
                            .ok()
                            .and_then(|p| p.CurrentIsSelected().ok())
                            .map(|b| b.as_bool())
                    };
                    if interactive || !name.is_empty() || value.is_some() {
                        ctx.elements.push(SemanticElement {
                            ref_id: format!("n{}", ctx.elements.len() + 1),
                            role: uia_role_name(control_type).to_string(),
                            name,
                            value,
                            bounds,
                            interactive,
                            enabled,
                            focused,
                            checked,
                            selected,
                        });
                        if ctx.elements.len() >= MAX_UIA_ELEMENTS {
                            return;
                        }
                    }
                }
            }
            if depth >= MAX_UIA_DEPTH || ctx.started.elapsed() >= MAX_UIA_DURATION {
                return;
            }
            let Ok(mut child) = (unsafe { walker.GetFirstChildElement(element) }) else {
                return;
            };
            loop {
                unsafe {
                    walk(walker, &child, depth + 1, ctx);
                }
                if ctx.elements.len() >= MAX_UIA_ELEMENTS
                    || ctx.visited >= MAX_UIA_VISITS
                    || ctx.started.elapsed() >= MAX_UIA_DURATION
                {
                    break;
                }
                let Ok(next) = (unsafe { walker.GetNextSiblingElement(&child) }) else {
                    break;
                };
                child = next;
            }
        }
        let Ok(root) = (unsafe {
            self.automation
                .ElementFromHandle(HWND(self.authority.window_id as usize as *mut _))
        }) else {
            return Vec::new();
        };
        let Ok(walker) = (unsafe { self.automation.ControlViewWalker() }) else {
            return Vec::new();
        };
        let mut ctx = WalkCtx {
            pid: self.authority.process_id as i32,
            position: self.position,
            size: self.size,
            started: Instant::now(),
            visited: 0,
            elements: Vec::new(),
        };
        unsafe {
            walk(&walker, &root, 0, &mut ctx);
        }
        ctx.elements
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
        let point = super::windows_session::physical_point(self.position, self.size, point)
            .ok_or(ErrorCode::InvalidCoordinates)?;
        // Point lookup cannot activate a window. An occluding or foreign control
        // is rejected instead of falling back to focus changes or global input.
        let prepared = (|| -> windows::core::Result<_> {
            unsafe {
                let (element, operation) = self.control_at(point)?;
                use super::windows_session::Activation;
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
                // Re-hit after pattern preparation. Do not invoke a stale or
                // substituted control even if it exposes the same pattern.
                let (current, current_operation) = self.control_at(point)?;
                if current_operation != operation
                    || !self
                        .automation
                        .CompareElements(&element, &current)?
                        .as_bool()
                {
                    return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                        0x80070005u32 as i32,
                    )));
                }
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
    fn set_value(
        &mut self,
        point: SourcePoint,
        text: &str,
        captured: Instant,
        deadline: Instant,
    ) -> Result<(), ErrorCode> {
        if !self.valid_input(captured, deadline) {
            return Err(ErrorCode::StaleFrame);
        }
        let point = super::windows_session::physical_point(self.position, self.size, point)
            .ok_or(ErrorCode::InvalidCoordinates)?;
        let pattern = (|| -> windows::core::Result<IUIAutomationValuePattern> {
            unsafe {
                let (element, pattern) = self.value_control_at(point)?;
                let (current, _) = self.value_control_at(point)?;
                if !self
                    .automation
                    .CompareElements(&element, &current)?
                    .as_bool()
                {
                    return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                        0x80070005u32 as i32,
                    )));
                }
                Ok(pattern)
            }
        })()
        .map_err(|_| ErrorCode::InvalidActions)?;
        if !self.valid_input(captured, deadline) {
            return Err(ErrorCode::StaleFrame);
        }
        let bstr = BSTR::from(text);
        unsafe { pattern.SetValue(&bstr) }.map_err(|_| ErrorCode::OutcomeUnknown)
    }
    fn scroll(
        &mut self,
        point: SourcePoint,
        horizontal: i16,
        vertical: i16,
        captured: Instant,
        deadline: Instant,
    ) -> Result<(), ErrorCode> {
        if !self.valid_input(captured, deadline) {
            return Err(ErrorCode::StaleFrame);
        }
        let point = super::windows_session::physical_point(self.position, self.size, point)
            .ok_or(ErrorCode::InvalidCoordinates)?;
        let pattern = (|| -> windows::core::Result<IUIAutomationScrollPattern> {
            unsafe {
                let (element, pattern) = self.scroll_control_at(point, horizontal, vertical)?;
                let (current, _) = self.scroll_control_at(point, horizontal, vertical)?;
                if !self
                    .automation
                    .CompareElements(&element, &current)?
                    .as_bool()
                {
                    return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                        0x80070005u32 as i32,
                    )));
                }
                Ok(pattern)
            }
        })()
        .map_err(|_| ErrorCode::InvalidActions)?;
        if !self.valid_input(captured, deadline) {
            return Err(ErrorCode::StaleFrame);
        }
        let (h_step, v_step) = scroll_steps(horizontal, vertical);
        unsafe { pattern.Scroll(to_scroll_amount(h_step), to_scroll_amount(v_step)) }
            .map_err(|_| ErrorCode::OutcomeUnknown)
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
