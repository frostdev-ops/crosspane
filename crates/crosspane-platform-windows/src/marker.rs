//! Private M1 decoration. Only authored pixels and identity-admitted source metadata are read.
//! Lead c6471971: match the source band; hidden on failed adjacency, never move/activate source.
#![allow(unsafe_code)]
use super::{Desktop, DpiScope, Pinned, Port, backend, default_desktop, hwnd, integrity};
use crate::{
    model::parking::{
        MarkerDesktopGuid, MarkerDesktopMembership, MarkerDesktopPlacement, MarkerModel,
        MarkerObservation, MarkerState, NativeIdentity, NativePort, marker_desktop_placement,
        marker_desktop_ready,
    },
    session::WindowsSession,
};
use crosspane_platform::{IoGate, LockState, PlatformError, SessionEvent, SessionEvents};
use crosspane_types::id::WindowId;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    mem::size_of,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{null, null_mut},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{Accessibility::*, HiDpi::*, WindowsAndMessaging::*},
};

const STYLE: u32 = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
// Crosspane's FROST primary accent, matching crosspane-ui-kit/src/theme.rs without a UI dependency.
const ACCENT: u32 = 0x17 | (0xc8 << 8) | (0xf4 << 16);
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
#[derive(Clone)]
struct Signal {
    identity: NativeIdentity,
    changed: Rc<Cell<bool>>,
    destroyed: Rc<Cell<bool>>,
}
thread_local! { static HOOKS: RefCell<BTreeMap<usize, Signal>> = const { RefCell::new(BTreeMap::new()) }; }
unsafe extern "system" fn event(
    hook: HWINEVENTHOOK,
    kind: u32,
    window: HWND,
    object: i32,
    child: i32,
    thread: u32,
    _: u32,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        HOOKS.with(|hooks| {
            let Ok(hooks) = hooks.try_borrow() else {
                return;
            };
            let Some(signal) = hooks.get(&(hook as usize)) else {
                return;
            };
            // OS hook pid/tid filter plus exact source handle/thread. No window fields in callback.
            if hwnd(signal.identity) != window || signal.identity.tid != thread {
                return;
            }
            if kind >= EVENT_OBJECT_CREATE
                && (object != OBJID_WINDOW || child != CHILDID_SELF as i32)
            {
                return;
            }
            signal.changed.set(true);
            if kind == EVENT_OBJECT_DESTROY {
                signal.destroyed.set(true);
            }
        });
    }));
}
struct WindowLife(Rc<Cell<bool>>);
unsafe extern "system" fn procedure(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: CREATESTRUCT for our private class; WindowLife is stable until owned teardown.
        let state = unsafe { (*(l as *const CREATESTRUCTW)).lpCreateParams };
        // SAFETY: only the owned marker stores this pointer; WM_NCDESTROY clears it.
        unsafe {
            SetWindowLongPtrW(window, GWLP_USERDATA, state as isize);
        }
    }
    if message == WM_NCDESTROY {
        // SAFETY: class is exclusively our owned markers, pointer stays alive through this callback.
        let state = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const WindowLife;
        if !state.is_null() {
            // SAFETY: same GUI-thread owner, stable owned WindowLife allocation.
            unsafe {
                (*state).0.set(false);
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            }
        }
    }
    match message {
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_CLOSE => 0,
        // SAFETY: forwards only this own class's callback; background paints authored accent region.
        _ => unsafe { DefWindowProcW(window, message, w, l) },
    }
}
struct Class {
    name: Vec<u16>,
    module: HINSTANCE,
    brush: HBRUSH,
}
impl Class {
    fn new() -> Result<Self, PlatformError> {
        // SAFETY: borrowed executable module and this parking worker's own thread scalar.
        let (module, thread) = unsafe { (GetModuleHandleW(null()), GetCurrentThreadId()) };
        if module.is_null() {
            return Err(backend("marker module unavailable"));
        }
        let name = wide(&format!(
            "Crosspane.MirrorMarker.{}.{}",
            std::process::id(),
            thread
        ));
        // SAFETY: creates an owned solid brush of authored constant colour, no source pixels.
        let brush = unsafe { CreateSolidBrush(ACCENT) };
        if brush.is_null() {
            return Err(backend("marker brush unavailable"));
        }
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: module,
            lpszClassName: name.as_ptr(),
            hbrBackground: brush,
            ..Default::default()
        };
        // SAFETY: initialized private class references module/name/brush retained by this RAII owner.
        if unsafe { RegisterClassW(&class) } == 0 {
            // SAFETY: failed registration never took brush ownership.
            unsafe {
                DeleteObject(brush);
            }
            return Err(backend("marker class unavailable"));
        }
        Ok(Self {
            name,
            module,
            brush,
        })
    }
}
impl Drop for Class {
    fn drop(&mut self) {
        // SAFETY: owner destroys every marker first; unregister/delete only its own class and brush.
        let removed = unsafe { UnregisterClassW(self.name.as_ptr(), self.module) };
        if removed != 0 {
            // SAFETY: registration is gone, so no remaining class can borrow this own brush.
            unsafe {
                DeleteObject(self.brush);
            }
        } // Failed native teardown retains its brush until this process exits.
    }
}
struct Window {
    handle: HWND,
    life: Option<Box<WindowLife>>,
    desktop_keep_logged: Cell<bool>,
}
impl Window {
    fn new(class: &Class) -> Result<Self, PlatformError> {
        let life = Box::new(WindowLife(Rc::new(Cell::new(true))));
        let caption = [0_u16];
        // SAFETY: creates only our initially hidden private popup; no source owner/parent relation.
        let handle = unsafe {
            CreateWindowExW(
                STYLE,
                class.name.as_ptr(),
                caption.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                null_mut(),
                null_mut(),
                class.module,
                (&*life as *const WindowLife).cast(),
            )
        };
        if handle.is_null() {
            return Err(backend("marker window unavailable"));
        }
        let value = Self {
            handle,
            life: Some(life),
            desktop_keep_logged: Cell::new(false),
        };
        // SAFETY: only our layered marker; constant authored region opacity, no captured pixels.
        if unsafe { SetLayeredWindowAttributes(handle, 0, 255, LWA_ALPHA) } == 0 {
            return Err(backend("marker layered presentation unavailable"));
        }
        Ok(value)
    }
    fn alive(&self) -> bool {
        self.life.as_ref().is_some_and(|life| life.0.get())
    }
    fn hide(&self) -> Result<(), PlatformError> {
        if !self.alive() {
            return Ok(());
        }
        // SAFETY: hides/demotes only this exact live owned marker, never the source or its owner.
        let hidden = unsafe {
            SetWindowPos(
                self.handle,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_HIDEWINDOW,
            ) != 0
                && IsWindowVisible(self.handle) == 0
                && GetWindowLongPtrW(self.handle, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST == 0
        };
        if hidden {
            Ok(())
        } else {
            Err(backend("marker hide refused"))
        }
    }
    fn placed(&self, source: NativeIdentity, state: crate::model::parking::MarkerFrame) -> bool {
        if !self.alive() {
            return false;
        }
        let mut r = RECT::default();
        // SAFETY: own marker fields and adjacent handle of the freshly admitted source only.
        unsafe {
            GetWindowRect(self.handle, &mut r) != 0
                && [r.left, r.top, r.right, r.bottom] == state.rect
                && IsWindowVisible(self.handle) != 0
                && GetWindow(hwnd(source), GW_HWNDPREV) == self.handle
                && (GetWindowLongPtrW(self.handle, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0)
                    == state.topmost
        }
    }
    fn show(
        &self,
        observed: MarkerObservation,
        state: crate::model::parking::MarkerFrame,
        initial: bool,
        mut admit: impl FnMut() -> Result<bool, PlatformError>,
    ) -> Result<bool, PlatformError> {
        let source = observed.identity;
        if !self.alive() {
            return Err(backend("marker owned window disappeared"));
        }
        let size = crate::model::parking::rect_size(state.rect)?;
        let width = i32::try_from(size.width).map_err(|_| backend("marker width"))?;
        let height = i32::try_from(size.height).map_err(|_| backend("marker height"))?;
        let border = i32::try_from(state.border).map_err(|_| backend("marker border"))?;
        // SAFETY: creates only owned geometry regions; all dimensions are validated physical pixels.
        let (outer, inner) = unsafe {
            (
                CreateRectRgn(0, 0, width, height),
                CreateRectRgn(
                    border,
                    border,
                    (width - border).max(border),
                    (height - border).max(border),
                ),
            )
        };
        if outer.is_null() || inner.is_null() {
            // SAFETY: free only whichever owned region creation succeeded.
            unsafe {
                if !outer.is_null() {
                    DeleteObject(outer);
                }
                if !inner.is_null() {
                    DeleteObject(inner);
                }
            }
            return Err(backend("marker region unavailable"));
        }
        // SAFETY: hollow only our authored region; never read/render source window contents.
        let combined = unsafe { CombineRgn(outer, outer, inner, RGN_DIFF) };
        // SAFETY: inner region never transfers ownership.
        unsafe {
            DeleteObject(inner);
        }
        if combined == ERROR {
            // SAFETY: outer is still ours after failed combination.
            unsafe {
                DeleteObject(outer);
            }
            return Err(backend("marker region refused"));
        }
        match admit() {
            Ok(true) => {}
            result => {
                // SAFETY: admission failed/changed before transfer; this region is still owned here.
                unsafe {
                    DeleteObject(outer);
                }
                self.hide()?;
                return result.map(|_| false);
            }
        }
        // SAFETY: ownership of outer transfers on success to this live own marker only.
        if unsafe { SetWindowRgn(self.handle, outer, 1) } == 0 {
            // SAFETY: failed SetWindowRgn left the region with this owner.
            unsafe {
                DeleteObject(outer);
            }
            return Err(backend("marker region installation refused"));
        }
        // Follow only the admitted source's public virtual-desktop GUID. Our own hidden popup
        // moves; neither the source nor the active desktop is changed, and no foreground is queried.
        self.hide()?;
        if !admit()? {
            self.hide()?;
            return Ok(false);
        }
        let desktop = Desktop::new()?;
        if !admit()? {
            self.hide()?;
            return Ok(false);
        }
        let membership = desktop_membership(&desktop, hwnd(source));
        let (guid_state, guid) = if membership == MarkerDesktopMembership::Current {
            if !admit()? {
                self.hide()?;
                return Ok(false);
            }
            match desktop.0.as_ref() {
                Some(manager) => {
                    // SAFETY: source tuple/context is freshly admitted; metadata-only GUID query.
                    match unsafe {
                        manager.GetWindowDesktopId(windows::Win32::Foundation::HWND(hwnd(source)))
                    } {
                        Ok(guid) if guid != windows::core::GUID::zeroed() => {
                            (MarkerDesktopGuid::Known, Some(guid))
                        }
                        Ok(_) => (MarkerDesktopGuid::Zero, None),
                        Err(_) => (MarkerDesktopGuid::Error, None),
                    }
                }
                None => (MarkerDesktopGuid::Error, None),
            }
        } else {
            (MarkerDesktopGuid::Error, None) // No GUID query is needed on the initial unknown path.
        };
        let decide = |membership| {
            marker_desktop_placement(
                initial,
                membership,
                guid_state,
                observed.visible,
                observed.cloaked,
            )
        };
        if decide(membership) == MarkerDesktopPlacement::Hide {
            self.hide()?;
            return Ok(false);
        }
        if !admit()? {
            self.hide()?;
            return Ok(false);
        }
        let placement = decide(desktop_membership(&desktop, hwnd(source)));
        if placement == MarkerDesktopPlacement::Hide {
            self.hide()?;
            return Ok(false);
        }
        if !admit()? {
            self.hide()?;
            return Ok(false);
        }
        if placement == MarkerDesktopPlacement::Follow {
            let manager = desktop
                .0
                .as_ref()
                .ok_or_else(|| backend("marker desktop manager"))?;
            let guid = guid.ok_or_else(|| backend("marker desktop GUID plan unavailable"))?;
            // SAFETY: only this owned marker moves to the admitted source's known nonzero GUID.
            unsafe {
                manager.MoveWindowToDesktop(windows::Win32::Foundation::HWND(self.handle), &guid)
            }
            .map_err(|_| backend("marker owned desktop follow refused"))?;
        } else if !self.desktop_keep_logged.replace(true) {
            let message = wide(&format!(
                "Crosspane marker desktop id unavailable; retaining marker desktop HWND={}",
                source.hwnd
            ));
            // SAFETY: authored HWND-only debug text is NUL-terminated and valid for this call.
            unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW(message.as_ptr())
            };
        }
        if !admit()? {
            self.hide()?;
            return Ok(false);
        }
        if decide(desktop_membership(&desktop, hwnd(source))) == MarkerDesktopPlacement::Hide
            || !marker_desktop_ready(
                initial,
                placement,
                desktop_membership(&desktop, self.handle),
            )
        {
            self.hide()?;
            return Ok(false);
        }
        // Lead c6471971: source band is the only permission to make this own marker topmost.
        // SAFETY: only marker changes band; initially hidden and no activation/source owner changes.
        if unsafe {
            SetWindowPos(
                self.handle,
                if state.topmost {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                },
                state.rect[0],
                state.rect[1],
                width,
                height,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_HIDEWINDOW,
            )
        } == 0
        {
            return Err(backend("marker band refused"));
        }
        if !admit()? {
            self.hide()?;
            return Ok(false);
        }
        // SAFETY: handle-only adjacency query of the exact admitted source, no foreign fields.
        let previous = unsafe { GetWindow(hwnd(source), GW_HWNDPREV) };
        let flags = SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW;
        let (after, flags) = if previous == self.handle {
            (null_mut(), flags | SWP_NOZORDER)
        } else {
            (previous, flags)
        };
        if !admit()? {
            self.hide()?;
            return Ok(false);
        }
        // SAFETY: positions only own marker after the handle above admitted source; never source.
        if unsafe {
            SetWindowPos(
                self.handle,
                after,
                state.rect[0],
                state.rect[1],
                width,
                height,
                flags,
            )
        } == 0
        {
            self.hide()?;
            return Ok(false);
        }
        // SAFETY: own marker fields and admitted source's adjacent handle only; no foreign metadata.
        let matched = unsafe {
            GetWindow(hwnd(source), GW_HWNDPREV) == self.handle
                && (GetWindowLongPtrW(self.handle, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0)
                    == state.topmost
                && IsWindowVisible(self.handle) != 0
        };
        if !matched {
            self.hide()?;
        }
        Ok(matched)
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        if self.alive() {
            let _ = self.hide();
            // SAFETY: own live class instance on its exact owner thread; callback retires its life.
            let destroyed = unsafe { DestroyWindow(self.handle) };
            if destroyed == 0
                && let Some(life) = self.life.take()
            {
                // Keep callback memory alive on uncertain teardown; never use a freed HWND payload.
                let _ = Box::leak(life);
            }
        }
    }
}
struct Hooks {
    values: Vec<HWINEVENTHOOK>,
}
impl Hooks {
    fn new(
        identity: NativeIdentity,
        changed: Rc<Cell<bool>>,
        destroyed: Rc<Cell<bool>>,
        port: &Port,
    ) -> Result<Self, PlatformError> {
        let mut hooks = Self { values: Vec::new() };
        for (first, last) in [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
            (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
            (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
            (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
            (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
        ] {
            port.check()?;
            // SAFETY: out-of-context callback stays on parking thread, filtered to admitted pid/tid.
            let hook = unsafe {
                SetWinEventHook(
                    first,
                    last,
                    null_mut(),
                    Some(event),
                    identity.pid,
                    identity.tid,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if hook.is_null() {
                return Err(backend("marker observer unavailable"));
            }
            HOOKS.with(|registry| {
                registry.borrow_mut().insert(
                    hook as usize,
                    Signal {
                        identity,
                        changed: changed.clone(),
                        destroyed: destroyed.clone(),
                    },
                )
            });
            hooks.values.push(hook);
        }
        Ok(hooks)
    }
}
impl Drop for Hooks {
    fn drop(&mut self) {
        for hook in self.values.drain(..) {
            HOOKS.with(|registry| registry.borrow_mut().remove(&(hook as usize)));
            // SAFETY: unregister exactly hooks installed by this same owner thread.
            unsafe {
                UnhookWinEvent(hook);
            }
        }
    }
}
struct Entry {
    identity: NativeIdentity,
    pinned: Pinned,
    window: Window,
    _hooks: Hooks,
    model: MarkerModel,
    changed: Rc<Cell<bool>>,
    destroyed: Rc<Cell<bool>>,
    previous: MarkerObservation,
}
#[derive(Default)]
pub(super) struct Owner {
    class: Option<Class>,
    session: Option<WindowsSession>,
    retire: Arc<AtomicBool>,
    entries: BTreeMap<WindowId, Entry>,
}
impl Owner {
    fn allowed(&self) -> bool {
        self.session.as_ref().is_some_and(|s| {
            let state = s.state();
            state.lock == LockState::Unlocked && state.active == Some(true)
        }) && default_desktop()
    }
    fn session(&mut self) -> Result<(), PlatformError> {
        if self.session.is_none() {
            let mut session = WindowsSession::new(IoGate::new())?;
            let flag = self.retire.clone();
            session.subscribe(Arc::new(move |event: SessionEvent| {
                if let SessionEvent::State(state) = event
                    && (state.lock != LockState::Unlocked || state.active != Some(true))
                {
                    flag.store(true, Ordering::Release);
                }
            }))?;
            self.session = Some(session);
        }
        Ok(())
    }
    pub(super) fn park(&mut self, id: WindowId, port: &mut Port) -> Result<(), PlatformError> {
        port.check()?;
        self.session()?; // Only called after successful actual runtime Controller::park.
        if self.retire.swap(false, Ordering::AcqRel) {
            self.clear();
        }
        port.check()?;
        if !self.allowed() {
            return Err(PlatformError::SecureInput);
        }
        let identity = port.resolve(id)?;
        port.verify_resolver(identity, Some(id))?;
        let pinned = Pinned::open(identity)?.ok_or(PlatformError::NotFound)?;
        let observed = facts(&pinned, port, id, true)?;
        port.check()?;
        if self.class.is_none() {
            self.class = Some(Class::new()?);
        }
        if let Some(existing) = self.entries.get_mut(&id)
            && existing.identity == identity
        {
            existing.changed.set(true);
            sync_entry(existing, port, id, &self.session, &self.retire, false)?;
            return port.check();
        }
        self.entries.remove(&id);
        let changed = Rc::new(Cell::new(true));
        let destroyed = Rc::new(Cell::new(false));
        port.check()?;
        let window = Window::new(
            self.class
                .as_ref()
                .ok_or_else(|| backend("marker class missing"))?,
        )?;
        port.check()?;
        let hooks = Hooks::new(identity, changed.clone(), destroyed.clone(), port)?;
        port.check()?;
        let mut model = MarkerModel::default();
        model.park(observed)?;
        let mut entry = Entry {
            identity,
            pinned,
            window,
            _hooks: hooks,
            model,
            changed,
            destroyed,
            previous: observed,
        };
        // Initial native region/presentation errors retain their exact cause for strict rollback.
        // Honest hide/cloak/membership/adjacency changes keep this created window and its hooks.
        sync_entry(&mut entry, port, id, &self.session, &self.retire, true)?;
        port.check()?;
        self.entries.insert(id, entry);
        Ok(())
    }
    pub(super) fn remove(&mut self, id: WindowId) {
        self.entries.remove(&id);
    }
    fn clear(&mut self) {
        self.entries.clear();
    }
    pub(super) fn retire_all(&mut self) {
        self.clear();
    }
    pub(super) fn suspend(&mut self) {
        self.entries.retain(|_, entry| {
            entry.changed.set(true);
            entry.window.hide().is_ok()
        });
    }
    pub(super) fn poll(&mut self, port: &mut Port) {
        if self.entries.is_empty() {
            return;
        }
        let _dpi = match DpiScope::new() {
            Ok(v) => v,
            Err(_) => {
                self.clear();
                return;
            }
        };
        if self.retire.swap(false, Ordering::AcqRel) || !self.allowed() || port.check().is_err() {
            self.clear();
            return;
        }
        let mut removed = Vec::new();
        for (&id, entry) in &mut self.entries {
            if let Err(error) = sync_entry(entry, port, id, &self.session, &self.retire, false) {
                // Strict identity/context loss retires. A later owned presentation/query refusal
                // hides safely and preserves hooks for a future admitted change, never re-parks.
                let retire = matches!(
                    error,
                    PlatformError::NotFound
                        | PlatformError::SecureInput
                        | PlatformError::Locked
                        | PlatformError::Timeout
                );
                if retire || entry.window.hide().is_err() {
                    removed.push(id);
                } else {
                    entry.model.adjacency_failed();
                }
            }
        }
        for id in removed {
            self.remove(id)
        }
        if self.retire.load(Ordering::Acquire) || !self.allowed() {
            self.clear();
        }
    }
    #[cfg(test)]
    pub(super) fn fixture_window(&self, id: WindowId) -> u64 {
        self.entries
            .get(&id)
            .filter(|e| e.window.alive())
            .map_or(0, |e| e.window.handle as usize as u64)
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.clear();
        self.class.take();
        self.session.take();
    }
}
pub(super) fn pump() {
    let mut message = MSG::default();
    // SAFETY: only this parking worker queue. It has our marker HWNDs and out-of-context callbacks.
    unsafe {
        for _ in 0..128 {
            if PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) == 0 {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
fn facts(
    pinned: &Pinned,
    port: &mut Port,
    id: WindowId,
    session_allowed: bool,
) -> Result<MarkerObservation, PlatformError> {
    port.check()?;
    port.verify_resolver(pinned.identity, Some(id))?;
    if !pinned.matches()? {
        return Err(PlatformError::NotFound);
    }
    // SAFETY: fresh read-only own/target integrity facts; a private marker never bypasses UIPI.
    if !default_desktop()
        || integrity(pinned.process.0)?
            // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle, used only for token queries.
        > integrity(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })?
    {
        return Err(PlatformError::SecureInput);
    }
    let window = hwnd(pinned.identity);
    let mut visible = RECT::default();
    let mut cloaked = 0_u32;
    // SAFETY: exact freshly admitted source tuple, PMv2 thread; bounded geometry-only outputs.
    let okay = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut visible as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        ) >= 0
            && DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED as u32,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            ) >= 0
    };
    if !okay {
        return Err(backend("marker source geometry unavailable"));
    }
    // SAFETY: metadata/state/style only after exact tuple admission, no captions or content.
    let observed = unsafe {
        MarkerObservation {
            identity: pinned.identity,
            frame: [visible.left, visible.top, visible.right, visible.bottom],
            dpi: GetDpiForWindow(window),
            visible: IsWindowVisible(window) != 0,
            minimized: IsIconic(window) != 0,
            cloaked: cloaked != 0,
            topmost: GetWindowLongPtrW(window, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0,
            session_allowed,
        }
    };
    if !pinned.matches()? {
        return Err(PlatformError::NotFound);
    }
    port.verify_resolver(pinned.identity, Some(id))?;
    port.check()?;
    Ok(observed)
}

fn session_allowed(session: &Option<WindowsSession>, retire: &AtomicBool) -> bool {
    !retire.load(Ordering::Acquire)
        && session.as_ref().is_some_and(|s| {
            let state = s.state();
            state.lock == LockState::Unlocked && state.active == Some(true)
        })
        && default_desktop()
}
fn admit(
    pinned: &Pinned,
    port: &mut Port,
    id: WindowId,
    session: &Option<WindowsSession>,
    retire: &AtomicBool,
) -> Result<(), PlatformError> {
    port.check()?;
    if !session_allowed(session, retire) {
        return Err(PlatformError::SecureInput);
    }
    port.verify_resolver(pinned.identity, Some(id))?;
    if !pinned.matches()? {
        return Err(PlatformError::NotFound);
    }
    // SAFETY: current process pseudo-handle is read-only; target process remains pinned.
    if integrity(pinned.process.0)?
        // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle, used only for token queries.
        > integrity(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })?
    {
        return Err(PlatformError::SecureInput);
    }
    port.check()
}

fn current_desktop(desktop: &Desktop, window: HWND) -> Result<bool, PlatformError> {
    let manager = desktop
        .0
        .as_ref()
        .ok_or_else(|| backend("marker desktop manager"))?;
    // SAFETY: caller admits exact source or owns marker, no desktop switch or unrelated query.
    unsafe { manager.IsWindowOnCurrentVirtualDesktop(windows::Win32::Foundation::HWND(window)) }
        .map(|value| value.as_bool())
        .map_err(|_| backend("marker desktop membership unknown"))
}
fn desktop_membership(desktop: &Desktop, window: HWND) -> MarkerDesktopMembership {
    match current_desktop(desktop, window) {
        Ok(true) => MarkerDesktopMembership::Current,
        Ok(false) => MarkerDesktopMembership::Other,
        Err(_) => MarkerDesktopMembership::Unknown,
    }
}
fn sync_entry(
    entry: &mut Entry,
    port: &mut Port,
    id: WindowId,
    session: &Option<WindowsSession>,
    retire: &AtomicBool,
    initial: bool,
) -> Result<(), PlatformError> {
    if entry.destroyed.get() {
        return Err(PlatformError::NotFound);
    }
    admit(&entry.pinned, port, id, session, retire)?;
    let observed = facts(&entry.pinned, port, id, true)?;
    let changed = entry.changed.replace(false) || entry.previous != observed;
    entry.previous = observed;
    match entry.model.observe(observed, changed)? {
        MarkerState::Shown(frame) => {
            admit(&entry.pinned, port, id, session, retire)?;
            if changed {
                let shown = entry.window.show(observed, frame, initial, || {
                    admit(&entry.pinned, port, id, session, retire)?;
                    Ok(facts(&entry.pinned, port, id, true)? == observed)
                })?;
                if !shown {
                    entry.model.adjacency_failed();
                }
            } else if !entry.window.placed(entry.identity, frame) {
                entry.window.hide()?;
                entry.model.adjacency_failed();
            }
            admit(&entry.pinned, port, id, session, retire)?;
            let after = facts(&entry.pinned, port, id, true)?;
            if after != observed {
                entry.window.hide()?;
                entry.model.adjacency_failed();
            }
        }
        MarkerState::Hidden => entry.window.hide()?,
        MarkerState::Absent => return Err(PlatformError::NotFound),
    }
    admit(&entry.pinned, port, id, session, retire)
}
