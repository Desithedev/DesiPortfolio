//! App-local magnetic edges and a shared liquid-glass junction/shadow surface.
//! No global hooks, screen capture, synthetic input or cross-process windows.
pub(crate) mod geometry;
pub(crate) mod material;
use crate::{compositor::Buffer, glass::Glass, model::wide};
use geometry::{components, snap_move, Pane};
use std::{
    cell::RefCell,
    collections::HashMap,
    mem::zeroed,
    ptr::{null, null_mut},
    rc::Rc,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::GetCapture,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

const REFRESH: u32 = WM_APP + 110;
const SUBCLASS: usize = 0x4c554d46;
#[derive(Clone)]
struct Member {
    pane: Pane,
    window: [i32; 4],
    visible: bool,
    moving: bool,
    opacity: u8,
    snap_drag: Option<(RECT, POINT)>,
}
struct Bridge {
    hwnd: HWND,
    panes: Vec<Pane>,
    opacities: Vec<f32>,
    wallpaper: u64,
    material: u64,
    uploads: u64,
}
impl Drop for Bridge {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(self.hwnd) != 0 {
                DestroyWindow(self.hwnd);
            }
        }
    }
}
struct Manager {
    members: HashMap<usize, Member>,
    materials: HashMap<usize, material::Material>,
    bridges: Vec<Bridge>,
    glass: Rc<RefCell<Glass>>,
    host: HWND,
    revision: u64,
    pending: bool,
    enabled: bool,
    wallpaper: u64,
    #[cfg(test)]
    fail_surfaces: bool,
}
thread_local! {
    static MANAGER: RefCell<Option<Manager>> = const { RefCell::new(None) };
    static PENDING_MOVE: std::cell::Cell<Option<(usize, RECT)>> = const { std::cell::Cell::new(None) };
}

pub unsafe fn window_rect(hwnd: HWND) -> RECT {
    if let Some((id, rect)) = PENDING_MOVE.with(|p| p.get()) {
        if id == hwnd as usize {
            return rect;
        }
    }
    let mut r: RECT = zeroed();
    GetWindowRect(hwnd, &mut r);
    r
}

pub fn pending_move_point(hwnd: HWND) -> Option<POINT> {
    PENDING_MOVE.with(|p| {
        p.get().and_then(|(id, r)| {
            (id == hwnd as usize).then_some(POINT {
                x: r.left,
                y: r.top,
            })
        })
    })
}

pub fn is_atomic_moving(hwnd: HWND) -> bool {
    PENDING_MOVE.with(|p| p.get().is_some_and(|(id, _)| id == hwnd as usize))
}

pub fn is_moving(hwnd: HWND) -> bool {
    if is_atomic_moving(hwnd) {
        return true;
    }
    MANAGER.with(|m| {
        m.try_borrow()
            .ok()
            .and_then(|s| s.as_ref()?.members.get(&(hwnd as usize)).map(|p| p.moving))
            .unwrap_or(false)
    })
}

pub unsafe fn commit_atomic_move(hwnd: HWND, target: RECT) {
    let mut cur: RECT = zeroed();
    GetWindowRect(hwnd, &mut cur);
    if target.left == cur.left && target.top == cur.top {
        return;
    }
    PENDING_MOVE.with(|p| p.set(Some((hwnd as usize, target))));
    InvalidateRect(hwnd, null(), 0);
    UpdateWindow(hwnd);
    PENDING_MOVE.with(|p| p.set(None));
    let mut after: RECT = zeroed();
    GetWindowRect(hwnd, &mut after);
    if after.left != target.left || after.top != target.top {
        SetWindowPos(
            hwnd,
            null_mut(),
            target.left,
            target.top,
            target.right - target.left,
            target.bottom - target.top,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

pub unsafe fn move_captured_widget(hwnd: HWND, mut r: RECT) {
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    MANAGER.with(|m| {
        if let Ok(mut x) = m.try_borrow_mut() {
            if let Some(s) = x.as_mut() {
                if !s.enabled {
                    return;
                }
                if let Some(p) = s.members.get(&(hwnd as usize)).cloned() {
                    if !p.moving {
                        if let Some(p) = s.members.get_mut(&(hwnd as usize)) {
                            p.moving = true;
                        }
                        s.changed();
                    }
                    let inset = 4. * p.pane.dpi;
                    let candidate = Pane {
                        rect: [
                            r.left as f32 + inset,
                            r.top as f32 + inset,
                            (r.left + w) as f32 - inset,
                            (r.top + h) as f32 - inset,
                        ],
                        ..p.pane
                    };
                    let d = snap_move(&candidate, &s.panes());
                    let dx = d[0].round() as i32;
                    let dy = d[1].round() as i32;
                    r.left += dx;
                    r.right += dx;
                    r.top += dy;
                    r.bottom += dy;
                }
            }
        }
    });
    commit_atomic_move(hwnd, r);
}

pub unsafe fn init(glass: Rc<RefCell<Glass>>) {
    let class = wide("LumenNativeFusionSurface");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(surface_proc),
        hInstance: GetModuleHandleW(null()),
        lpszClassName: class.as_ptr(),
        ..zeroed()
    };
    RegisterClassW(&wc);
    let host = CreateWindowExW(
        0,
        class.as_ptr(),
        null(),
        0,
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        null_mut(),
        wc.hInstance,
        null(),
    );
    let wallpaper = glass.borrow().revision();
    MANAGER.with(|m| {
        *m.borrow_mut() = Some(Manager {
            members: HashMap::new(),
            materials: HashMap::new(),
            bridges: Vec::new(),
            glass,
            host,
            revision: 1,
            pending: false,
            enabled: true,
            wallpaper,
            #[cfg(test)]
            fail_surfaces: false,
        })
    });
}
pub fn shutdown() {
    MANAGER.with(|m| {
        if let Some(mut s) = m.borrow_mut().take() {
            s.bridges.clear();
            unsafe {
                DestroyWindow(s.host);
            }
        }
    });
}
pub fn enabled() -> bool {
    MANAGER.with(|m| {
        m.try_borrow()
            .ok()
            .and_then(|s| s.as_ref().map(|s| s.enabled))
            .unwrap_or(true)
    })
}
pub fn set_enabled(value: bool) {
    MANAGER.with(|m| {
        if let Ok(mut s) = m.try_borrow_mut() {
            if let Some(s) = s.as_mut() {
                if s.enabled != value {
                    s.enabled = value;
                    unsafe {
                        s.changed();
                    }
                }
            }
        }
    });
}

unsafe fn is_widget(hwnd: HWND) -> bool {
    if !GetPropW(hwnd, wide("LumenSourceWidgetPackage").as_ptr()).is_null() {
        return true;
    }
    let mut name = [0u16; 80];
    let n = GetClassNameW(hwnd, name.as_mut_ptr(), 80).max(0) as usize;
    matches!(
        String::from_utf16_lossy(&name[..n]).as_str(),
        "LumenNativeDockWindow"
            | "LumenNativeClockWindow"
            | "LumenNativeCalendarWindow"
            | "LumenNativeMusic"
            | "LumenNativeNotesBoard"
    )
}
/// The registry marks each package's main HWND; child/settings windows are excluded.
pub unsafe fn register_widget(hwnd: HWND) {
    if !hwnd.is_null() && IsWindow(hwnd) != 0 {
        SetPropW(
            hwnd,
            wide("LumenSourceWidgetPackage").as_ptr(),
            1usize as HANDLE,
        );
    }
}
unsafe fn read_member(hwnd: HWND, radius: f32) -> Member {
    let r = window_rect(hwnd);
    let dpi = (GetDpiForWindow(hwnd) as f32 / 96.).max(1.);
    let inset = 4. * dpi;
    Member {
        pane: Pane {
            id: hwnd as usize,
            rect: [
                r.left as f32 + inset,
                r.top as f32 + inset,
                r.right as f32 - inset,
                r.bottom as f32 - inset,
            ],
            radius: radius * dpi,
            dpi,
            topmost: GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST as isize != 0,
        },
        window: [r.left, r.top, r.right, r.bottom],
        visible: IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0,
        moving: false,
        opacity: 255,
        snap_drag: None,
    }
}
impl Manager {
    fn material_key(&self, panes: &[Pane]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for pane in panes {
            pane.id.hash(&mut hash);
            self.materials.get(&pane.id).hash(&mut hash);
        }
        hash.finish()
    }
    fn material_at(&self, panes: &[Pane], x: f32, y: f32) -> Option<[f32; 3]> {
        material::blend(
            panes.iter().filter_map(|p| {
                Some((
                    p,
                    self.members.get(&p.id)?.window,
                    self.materials.get(&p.id)?,
                ))
            }),
            x,
            y,
        )
    }
    unsafe fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        if !self.pending {
            self.pending = true;
            PostMessageW(self.host, REFRESH, 0, 0);
        }
    }
    fn panes(&self) -> Vec<Pane> {
        if !self.enabled {
            return Vec::new();
        }
        let mut ps: Vec<_> = self
            .members
            .values()
            .filter(|p| p.visible && !p.moving)
            .map(|p| p.pane)
            .collect();
        ps.sort_by_key(|p| p.id);
        ps
    }
    fn group(&self, id: usize) -> Option<Vec<Pane>> {
        let desired = components(&self.panes())
            .into_iter()
            .find(|g| g.len() > 1 && g.iter().any(|p| p.id == id))?;
        // Coverage and shadow suppression only commit AFTER a successful
        // junction upload. Allocation/size failures retain standalone chrome.
        self.bridges
            .iter()
            .any(|b| b.panes == desired)
            .then_some(desired)
    }
    unsafe fn update(&mut self, hwnd: HWND, radius: Option<f32>) {
        let wallpaper = self.glass.borrow().revision();
        if self.wallpaper != wallpaper {
            self.wallpaper = wallpaper;
            self.changed();
        }
        let id = hwnd as usize;
        if let Some(old) = self.members.get(&id).cloned() {
            let mut new = read_member(hwnd, radius.unwrap_or(old.pane.radius / old.pane.dpi));
            new.moving = old.moving;
            new.opacity = old.opacity;
            let changed = old.pane != new.pane || old.visible != new.visible;
            self.members.insert(id, new);
            if changed && !old.moving {
                self.materials.remove(&id);
                self.changed();
            }
        } else if is_widget(hwnd) {
            self.members
                .insert(id, read_member(hwnd, radius.unwrap_or(34.)));
            SetWindowSubclass(hwnd, Some(widget_proc), SUBCLASS, 0);
            self.changed();
        }
    }
}
/// Registers only known app-owned main widgets, never editor/settings/player HWNDs.
pub fn revision(hwnd: HWND) -> u64 {
    use std::hash::{Hash, Hasher};
    MANAGER.with(|m| {
        let Ok(mut x) = m.try_borrow_mut() else {
            return 0;
        };
        let Some(s) = x.as_mut() else { return 0 };
        unsafe {
            s.update(hwnd, None);
        }
        let Some(own) = s.members.get(&(hwnd as usize)) else {
            return 0;
        };
        let group = s.group(hwnd as usize);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        group.is_some().hash(&mut h);
        if let Some(ref panes) = group {
            s.material_key(panes).hash(&mut h);
        }
        for p in group.unwrap_or_else(|| vec![own.pane]) {
            p.id.hash(&mut h);
            for v in p.rect {
                v.to_bits().hash(&mut h);
            }
            p.radius.to_bits().hash(&mut h);
            p.dpi.to_bits().hash(&mut h);
            p.topmost.hash(&mut h);
        }
        h.finish()
    })
}
pub unsafe fn prepare(hwnd: HWND, radius: f32) {
    MANAGER.with(|m| {
        if let Ok(mut x) = m.try_borrow_mut() {
            if let Some(s) = x.as_mut() {
                s.update(hwnd, Some(radius));
            }
        }
    });
}
pub fn pane_radius(hwnd: HWND) -> Option<f32> {
    MANAGER.with(|m| {
        m.try_borrow().ok().and_then(|s| {
            let p = &s.as_ref()?.members.get(&(hwnd as usize))?.pane;
            Some(p.radius / p.dpi.max(1.0))
        })
    })
}

/// Publish only raw chrome, before shared blending, bevel, alpha and content.
/// Equality suppresses repaint feedback and keeps storage below12 KiB/member.
pub unsafe fn publish_material(hwnd: HWND, material: material::Material) {
    MANAGER.with(|manager| {
        let Ok(mut manager) = manager.try_borrow_mut() else {
            return;
        };
        let Some(s) = manager.as_mut() else { return };
        if !s.members.contains_key(&(hwnd as usize)) {
            return;
        }
        if s.materials.get(&(hwnd as usize)) == Some(&material) {
            return;
        }
        s.materials.insert(hwnd as usize, material);
        s.changed();
    });
}
pub fn may_fuse() -> bool {
    MANAGER.with(|m| {
        m.try_borrow()
            .ok()
            .and_then(|x| x.as_ref().map(|s| s.enabled && s.members.len() >= 2))
            .unwrap_or(false)
    })
}
pub fn group(hwnd: HWND) -> Option<Vec<Pane>> {
    MANAGER.with(|m| {
        m.try_borrow()
            .ok()
            .and_then(|s| s.as_ref().and_then(|s| s.group(hwnd as usize)))
    })
}
pub fn group_opacities(hwnd: HWND, own_alpha: u8) -> Option<(Vec<Pane>, Vec<u8>)> {
    MANAGER.with(|m| {
        let s = m.try_borrow().ok()?;
        let s = s.as_ref()?;
        let panes = s.group(hwnd as usize)?;
        let opacities: Vec<u8> = panes
            .iter()
            .map(|p| {
                if p.id == hwnd as usize {
                    own_alpha
                } else {
                    s.members.get(&p.id).map_or(255, |m| m.opacity)
                }
            })
            .collect();
        if opacities.iter().all(|&o| o == own_alpha) {
            None
        } else {
            Some((panes, opacities))
        }
    })
}
pub fn fused(hwnd: HWND) -> bool {
    group(hwnd).is_some()
}
pub fn is_surface(hwnd: HWND) -> bool {
    MANAGER.with(|m| {
        m.try_borrow().ok().is_some_and(|m| {
            m.as_ref()
                .is_some_and(|s| s.bridges.iter().any(|b| b.hwnd == hwnd))
        })
    })
}
unsafe fn restack(hwnd: HWND, panes: &[Pane], rect: Option<[i32; 4]>) {
    crate::desktop_layer::register(
        hwnd,
        panes
            .iter()
            .any(|pane| crate::desktop_layer::contains(pane.id as HWND)),
    );
    let mut above = GetTopWindow(null_mut());
    for _ in 0..4096 {
        if above.is_null() || panes.iter().any(|p| p.id == above as usize) {
            break;
        }
        above = GetWindow(above, GW_HWNDNEXT);
    }
    if above.is_null() {
        above = panes[0].id as HWND;
    }
    let topmost = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST as isize != 0;
    if topmost != panes[0].topmost {
        SetWindowPos(
            hwnd,
            if panes[0].topmost {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            },
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    let [x, y, w, h] = rect.unwrap_or([0, 0, 0, 0]);
    // The unowned decorative surface stays below the highest cluster member,
    // including its desktop band. Never float a junction over other apps or
    // over General Settings just because it was repainted last.
    SetWindowPos(
        hwnd,
        above,
        x,
        y,
        w,
        h,
        SWP_NOACTIVATE
            | SWP_NOOWNERZORDER
            | if rect.is_none() {
                SWP_NOMOVE | SWP_NOSIZE
            } else {
                0
            },
    );
}
pub fn opacity(hwnd: HWND, value: u8) {
    MANAGER.with(|m| {
        if let Ok(mut x) = m.try_borrow_mut() {
            if let Some(s) = x.as_mut() {
                if let Some(p) = s.members.get_mut(&(hwnd as usize)) {
                    if p.opacity != value {
                        p.opacity = value;
                        unsafe {
                            s.changed();
                        }
                    }
                }
            }
        }
    });
}
/// Shared by native WM_SIZING and app-local captured resize handlers. The
/// supplied limits belong to the widget, not its detached settings window.
pub unsafe fn snap_resize_rect(hwnd: HWND, r: &mut RECT, edge: u32, minimum: [i32; 2]) {
    MANAGER.with(|manager| {
        let Ok(manager) = manager.try_borrow() else {
            return;
        };
        let Some(manager) = manager.as_ref().filter(|s| s.enabled) else {
            return;
        };
        let Some(member) = manager.members.get(&(hwnd as usize)) else {
            return;
        };
        let inset = 4. * member.pane.dpi;
        let candidate = Pane {
            rect: [
                r.left as f32 + inset,
                r.top as f32 + inset,
                r.right as f32 - inset,
                r.bottom as f32 - inset,
            ],
            ..member.pane
        };
        let active = [
            matches!(edge, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT),
            matches!(edge, HTTOP | HTTOPLEFT | HTTOPRIGHT),
            matches!(edge, HTRIGHT | HTTOPRIGHT | HTBOTTOMRIGHT),
            matches!(edge, HTBOTTOM | HTBOTTOMLEFT | HTBOTTOMRIGHT),
        ];
        let snapped = geometry::snap_resize(
            &candidate,
            &manager.panes(),
            active,
            [
                minimum[0] as f32 - 2. * inset,
                minimum[1] as f32 - 2. * inset,
            ],
        );
        let values = [
            (snapped[0] - inset).round() as i32,
            (snapped[1] - inset).round() as i32,
            (snapped[2] + inset).round() as i32,
            (snapped[3] + inset).round() as i32,
        ];
        if values[2] - values[0] >= minimum[0] {
            if active[0] {
                r.left = values[0];
            }
            if active[2] {
                r.right = values[2];
            }
        }
        if values[3] - values[1] >= minimum[1] {
            if active[1] {
                r.top = values[1];
            }
            if active[3] {
                r.bottom = values[3];
            }
        }
    });
}
/// Both sides sample the same mix of their actual unblended chrome colors.
pub unsafe fn shared_material(
    hwnd: HWND,
    width: i32,
    height: i32,
    _dpi: f32,
) -> Option<material::Field> {
    MANAGER.with(|m| {
        let s = m.try_borrow().ok()?;
        let s = s.as_ref()?;
        let panes = s.group(hwnd as usize)?;
        if panes
            .iter()
            .filter(|p| s.materials.contains_key(&p.id))
            .count()
            < 2
        {
            return None;
        }
        let r = window_rect(hwnd);
        // Global pixel grid: adjacent widgets interpolate identical samples at
        // their seam. Only this small field evaluates distance/exponential;
        // rasterization uses bilinear samples inside the join band.
        let step = 16;
        let l = r.left.div_euclid(step) * step - step;
        let t = r.top.div_euclid(step) * step - step;
        let gw = ((r.left + width - l + step - 1) / step + 1) as usize;
        let gh = ((r.top + height - t + step - 1) / step + 1) as usize;
        let mut bgr = Vec::with_capacity(gw * gh);
        for y in 0..gh {
            for x in 0..gw {
                let color = s.material_at(
                    &panes,
                    (l + x as i32 * step) as f32 + 8.,
                    (t + y as i32 * step) as f32 + 8.,
                )?;
                bgr.push(color);
            }
        }
        Some(material::Field {
            window: [l, t, l + gw as i32 * step, t + gh as i32 * step],
            width: gw,
            height: gh,
            bgr,
        })
    })
}

unsafe extern "system" fn widget_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    match msg {
        WM_SIZING => {
            let edge = match wp as u32 {
                WMSZ_LEFT => HTLEFT,
                WMSZ_RIGHT => HTRIGHT,
                WMSZ_TOP => HTTOP,
                WMSZ_TOPLEFT => HTTOPLEFT,
                WMSZ_TOPRIGHT => HTTOPRIGHT,
                WMSZ_BOTTOM => HTBOTTOM,
                WMSZ_BOTTOMLEFT => HTBOTTOMLEFT,
                WMSZ_BOTTOMRIGHT => HTBOTTOMRIGHT,
                _ => HTCLIENT,
            };
            let mut limits: MINMAXINFO = zeroed();
            SendMessageW(hwnd, WM_GETMINMAXINFO, 0, &mut limits as *mut _ as isize);
            snap_resize_rect(
                hwnd,
                &mut *(lp as *mut RECT),
                edge,
                [
                    limits.ptMinTrackSize.x.max(1),
                    limits.ptMinTrackSize.y.max(1),
                ],
            );
        }
        WM_ENTERSIZEMOVE | WM_NCLBUTTONDOWN => {
            let drag = msg == WM_ENTERSIZEMOVE || (HTLEFT..=HTBOTTOMRIGHT).contains(&(wp as u32));
            if drag {
                MANAGER.with(|m| {
                    if let Ok(mut x) = m.try_borrow_mut() {
                        if let Some(s) = x.as_mut() {
                            if let Some(p) = s.members.get_mut(&(hwnd as usize)) {
                                p.snap_drag = None;
                                if !p.moving {
                                    p.moving = true;
                                    s.changed();
                                }
                            }
                        }
                    }
                });
            }
        }
        WM_WINDOWPOSCHANGING => {
            // Calendar and Dock also have capture-based drags. Snap those in
            // the same physical-coordinate path as Windows' native move loop.
            if GetCapture() == hwnd && !is_atomic_moving(hwnd) {
                let pos = &mut *(lp as *mut WINDOWPOS);
                if pos.flags & SWP_NOMOVE == 0 {
                    MANAGER.with(|m| {
                        if let Ok(mut x) = m.try_borrow_mut() {
                            if let Some(s) = x.as_mut() {
                                if !s.enabled {
                                    return;
                                }
                                if let Some(p) = s.members.get(&(hwnd as usize)).cloned() {
                                    let w = p.window[2] - p.window[0];
                                    let h = p.window[3] - p.window[1];
                                    let moving =
                                        pos.flags & SWP_NOSIZE != 0 || (pos.cx == w && pos.cy == h);
                                    if moving && (pos.x != p.window[0] || pos.y != p.window[1]) {
                                        if !p.moving {
                                            if let Some(p) = s.members.get_mut(&(hwnd as usize)) {
                                                p.moving = true;
                                            }
                                            s.changed();
                                        }
                                        let inset = 4. * p.pane.dpi;
                                        let candidate = Pane {
                                            rect: [
                                                pos.x as f32 + inset,
                                                pos.y as f32 + inset,
                                                (pos.x + w) as f32 - inset,
                                                (pos.y + h) as f32 - inset,
                                            ],
                                            ..p.pane
                                        };
                                        let d = snap_move(&candidate, &s.panes());
                                        pos.x += d[0].round() as i32;
                                        pos.y += d[1].round() as i32;
                                    }
                                }
                            }
                        }
                    });
                }
            }
        }
        WM_MOVING => {
            let r = &mut *(lp as *mut RECT);
            MANAGER.with(|m| {
                if let Ok(mut x) = m.try_borrow_mut() {
                    if let Some(s) = x.as_mut() {
                        if !s.enabled {
                            return;
                        }
                        let panes = s.panes();
                        if let Some(p) = s.members.get_mut(&(hwnd as usize)) {
                            let mut cursor: POINT = zeroed();
                            GetCursorPos(&mut cursor);
                            let raw = match &mut p.snap_drag {
                                Some((raw, last)) => {
                                    if cursor.x != last.x || cursor.y != last.y {
                                        let dx = cursor.x - last.x;
                                        let dy = cursor.y - last.y;
                                        raw.left += dx;
                                        raw.right += dx;
                                        raw.top += dy;
                                        raw.bottom += dy;
                                        *last = cursor;
                                    }
                                    *raw
                                }
                                None => {
                                    p.snap_drag = Some((*r, cursor));
                                    *r
                                }
                            };
                            let inset = 4. * p.pane.dpi;
                            let moving = Pane {
                                rect: [
                                    raw.left as f32 + inset,
                                    raw.top as f32 + inset,
                                    raw.right as f32 - inset,
                                    raw.bottom as f32 - inset,
                                ],
                                ..p.pane
                            };
                            let d = snap_move(&moving, &panes);
                            let dx = d[0].round() as i32;
                            let dy = d[1].round() as i32;
                            r.left = raw.left + dx;
                            r.right = raw.right + dx;
                            r.top = raw.top + dy;
                            r.bottom = raw.bottom + dy;
                        }
                    }
                }
            });
            let target = *r;
            commit_atomic_move(hwnd, target);
        }
        WM_EXITSIZEMOVE | WM_CAPTURECHANGED | WM_CANCELMODE => {
            MANAGER.with(|m| {
                if let Ok(mut x) = m.try_borrow_mut() {
                    if let Some(s) = x.as_mut() {
                        if let Some(p) = s.members.get_mut(&(hwnd as usize)) {
                            p.snap_drag = None;
                            if p.moving {
                                p.moving = false;
                                s.materials.remove(&(hwnd as usize));
                                s.update(hwnd, None);
                                s.changed();
                            }
                        }
                    }
                }
            });
        }
        WM_NCDESTROY => {
            RemoveWindowSubclass(hwnd, Some(widget_proc), SUBCLASS);
            MANAGER.with(|m| {
                if let Ok(mut x) = m.try_borrow_mut() {
                    if let Some(s) = x.as_mut() {
                        s.members.remove(&(hwnd as usize));
                        s.materials.remove(&(hwnd as usize));
                        s.changed();
                    }
                }
            });
        }
        _ => {}
    }
    let result = DefSubclassProc(hwnd, msg, wp, lp);
    if matches!(msg, WM_WINDOWPOSCHANGED | WM_SHOWWINDOW | WM_DPICHANGED) {
        MANAGER.with(|m| {
            if let Ok(mut x) = m.try_borrow_mut() {
                if let Some(s) = x.as_mut() {
                    s.update(hwnd, None);
                    if msg == WM_WINDOWPOSCHANGED
                        && (*(lp as *const WINDOWPOS)).flags & SWP_NOZORDER == 0
                    {
                        s.changed();
                    }
                }
            }
        });
    }
    result
}
unsafe extern "system" fn surface_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        REFRESH => {
            refresh();
            0
        }
        WM_NCHITTEST => HTTRANSPARENT as isize,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        WM_ERASEBKGND => 1,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
pub(crate) unsafe fn refresh() {
    // Take the manager out of TLS before Win32 calls: callbacks cannot reborrow it.
    let Some(mut s) = MANAGER.with(|m| m.borrow_mut().take()) else {
        return;
    };
    s.pending = false;
    let mut previous = std::mem::take(&mut s.bridges);
    let mut dirty = std::collections::HashSet::new();
    let all = s.panes();
    if std::env::var_os("LUMEN_TRACE_FUSION").is_some() {
        eprintln!("native-fusion panes={all:?}");
    }
    for ps in components(&all).into_iter().filter(|g| g.len() > 1) {
        #[cfg(test)]
        if s.fail_surfaces {
            dirty.extend(ps.iter().map(|p| p.id));
            continue;
        }
        let opacities: Vec<_> = ps
            .iter()
            .map(|p| s.members.get(&p.id).map_or(255, |m| m.opacity) as f32 / 255.)
            .collect();
        let wallpaper = s.glass.borrow().revision();
        let material = s.material_key(&ps);
        let old = previous
            .iter()
            .position(|b| b.panes.iter().map(|p| p.id).eq(ps.iter().map(|p| p.id)));
        let old = old.map(|i| previous.swap_remove(i));
        if old.as_ref().is_some_and(|b| {
            b.panes == ps
                && b.opacities == opacities
                && b.wallpaper == wallpaper
                && b.material == material
        }) {
            let bridge = old.unwrap();
            restack(bridge.hwnd, &ps, None);
            s.bridges.push(bridge);
            continue;
        }
        dirty.extend(ps.iter().map(|p| p.id));
        let dpi = ps.iter().map(|p| p.dpi).fold(1f32, f32::max);
        let pad = (48. * dpi).ceil() as i32;
        let l = ps.iter().map(|p| p.rect[0] as i32).min().unwrap() - pad;
        let t = ps.iter().map(|p| p.rect[1] as i32).min().unwrap() - pad;
        let r = ps.iter().map(|p| p.rect[2].ceil() as i32).max().unwrap() + pad;
        let b = ps.iter().map(|p| p.rect[3].ceil() as i32).max().unwrap() + pad;
        let (w, h) = (r - l, b - t);
        // Bound temporary allocations even for pathological multi-monitor layouts.
        if w <= 0 || h <= 0 || w as i64 * h as i64 > 16_000_000 {
            continue;
        }
        let hwnd = old.as_ref().map(|b| b.hwnd).unwrap_or_else(|| {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                wide("LumenNativeFusionSurface").as_ptr(),
                null(),
                WS_POPUP | WS_DISABLED,
                l,
                t,
                w,
                h,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        });
        if hwnd.is_null() {
            continue;
        }
        let mut bridge = old.unwrap_or_else(|| Bridge {
            hwnd,
            panes: Vec::new(),
            opacities: Vec::new(),
            wallpaper: 0,
            material: 0,
            uploads: 0,
        });
        let Some(mut buffer) = Buffer::plain(w, h, dpi) else {
            continue;
        };
        let has_material = ps.iter().any(|p| s.materials.contains_key(&p.id));
        if !has_material {
            SetWindowPos(
                hwnd,
                null_mut(),
                l,
                t,
                w,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
            );
            let brush = CreateSolidBrush(s.glass.borrow().wallpaper_accent());
            let rr = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            FillRect(buffer.dc, &rr, brush);
            DeleteObject(brush);
            s.glass.borrow().draw(buffer.dc, hwnd, w, h);
        }
        let windows: Vec<[i32; 4]> = ps
            .iter()
            .map(|p| s.members.get(&p.id).map_or([0; 4], |m| m.window))
            .collect();
        let uniform_opacity = opacities
            .first()
            .copied()
            .filter(|&a0| opacities.iter().all(|&a| (a - a0).abs() < 1e-5));
        let pad_f = pad as f32;
        let pixels = buffer.pixels_mut();
        let liquid = crate::glass::liquid_glass();
        let spec_w = crate::compositor::liquid_spec_width(dpi, w as f32, h as f32);
        let bridge_rim = (40.0 * dpi).clamp(12.0, (w.min(h) as f32) * 0.44);
        let mut rim_samples: Vec<(i32, i32, f32, f32, f32, f32, f32, f32)> = Vec::new();
        geometry::with_outline(&ps, |outline| {
            let r_max = pad_f + 6. * dpi;
            let mut x_zones: Vec<[f32; 2]> = outline
                .v_lines
                .iter()
                .map(|v| [v[0] - r_max, v[0] + r_max])
                .chain(
                    outline
                        .corners
                        .iter()
                        .map(|c| [c.min[0] - r_max, c.max[0] + r_max]),
                )
                .chain(
                    windows
                        .iter()
                        .flat_map(|w| [[w[0] as f32 - 2., w[0] as f32 + 2.], [w[2] as f32 - 2., w[2] as f32 + 2.]]),
                )
                .collect();
            x_zones.sort_unstable_by(|a, b| a[0].total_cmp(&b[0]));
            let mut merged_zones: Vec<[f32; 2]> = Vec::with_capacity(x_zones.len());
            for z in x_zones {
                if let Some(last) = merged_zones.last_mut() {
                    if z[0] <= last[1] {
                        last[1] = last[1].max(z[1]);
                        continue;
                    }
                }
                merged_zones.push(z);
            }
            for y in 0..h {
                let ly = t + y;
                let gy = ly as f32 + 0.5;
                let row = (y * w * 4) as usize;
                let mut x = 0i32;
                while x < w {
                    let lx = l + x;
                    let gx = lx as f32 + 0.5;
                    // Jump across horizontal core spans of any pane in O(1).
                    let mut jumped = false;
                    for p in &ps {
                        let [pl, pt, pr, pb] = p.rect;
                        let margin = 4. * p.dpi;
                        if gy < pt + margin || gy >= pb - margin {
                            continue;
                        }
                        let inset = p.radius + margin;
                        let (span_l, span_r) = if gy >= pt + inset && gy < pb - inset {
                            (pl + margin, pr - margin)
                        } else {
                            (pl + inset, pr - inset)
                        };
                        if gx >= span_l && gx < span_r {
                            let end_x = ((span_r - 0.5).floor() as i32 - l + 1).clamp(x + 1, w);
                            if !has_material {
                                pixels[row + (x as usize * 4)..row + (end_x as usize * 4)].fill(0);
                            }
                            x = end_x;
                            jumped = true;
                            break;
                        }
                    }
                    if jumped {
                        continue;
                    }
                    // Outside all vertical/corner X-zones, distance and shadow are 1D-invariant in X.
                    let in_zone = merged_zones.iter().any(|z| gx >= z[0] && gx <= z[1]);
                    let span_end_x = if !in_zone && uniform_opacity.is_some() {
                        let next_zone_x = merged_zones
                            .iter()
                            .filter(|z| z[0] > gx)
                            .map(|z| (z[0] - 0.5).floor() as i32 - l)
                            .min()
                            .unwrap_or(w);
                        next_zone_x.clamp(x + 1, w)
                    } else {
                        x + 1
                    };
                    let i = row + (x * 4) as usize;
                    if geometry::covered_core(&ps, gx, gy) {
                        if !has_material {
                            pixels[row + (x as usize * 4)..row + (span_end_x as usize * 4)].fill(0);
                        }
                        x = span_end_x;
                        continue;
                    }
                    let d = outline.distance([gx, gy]);
                    if d >= pad_f {
                        if !has_material {
                            pixels[row + (x as usize * 4)..row + (span_end_x as usize * 4)].fill(0);
                        }
                        x = span_end_x;
                        continue;
                    }
                    let in_window = windows
                        .iter()
                        .any(|w| lx >= w[0] && lx < w[2] && ly >= w[1] && ly < w[3]);
                    if in_window && d < -0.625 {
                        if !has_material {
                            pixels[row + (x as usize * 4)..row + (span_end_x as usize * 4)].fill(0);
                        }
                        x = span_end_x;
                        continue;
                    }
                    let coverage = outline.coverage_with_distance(gx, gy, d);
                    // The shadow must also occupy transparent rounded corners INSIDE
                    // member HWND bounds; a rectangular hole cuts it into hard squares.
                    if in_window && coverage > 0. {
                        if !has_material {
                            pixels[row + (x as usize * 4)..row + (span_end_x as usize * 4)].fill(0);
                        }
                        x = span_end_x;
                        continue;
                    }
                    // A transparent neighbor must not erase the entire cluster shadow.
                    // Blend the local members' opacity continuously near the junction.
                    let alpha = if let Some(a) = uniform_opacity {
                        a
                    } else {
                        let mut sum = 0.;
                        let mut alpha = 0.;
                        for (p, a) in ps.iter().zip(&opacities) {
                            let weight = (-(p.distance(gx, gy) - d).max(0.) / (20. * dpi)).exp();
                            sum += weight;
                            alpha += weight * a;
                        }
                        if sum > 0. {
                            alpha / sum
                        } else {
                            0.
                        }
                    };
                    if coverage > 0. {
                        let a = coverage * alpha;
                        let edge = outline.rim_light(gx, gy, d, dpi);
                        let (nx, ny, has_n) = if liquid && d > -bridge_rim {
                            let dx =
                                outline.distance([gx + 1., gy]) - outline.distance([gx - 1., gy]);
                            let dy =
                                outline.distance([gx, gy + 1.]) - outline.distance([gx, gy - 1.]);
                            let len = (dx * dx + dy * dy).sqrt().max(1e-5);
                            (dx / len, dy / len, true)
                        } else {
                            (0., 0., false)
                        };
                        let spec = if has_n && -d < spec_w {
                            crate::compositor::liquid_rim_spec(d, nx, ny, spec_w)
                        } else {
                            0.
                        };
                        let material = s.material_at(&ps, gx, gy);
                        if material.is_none() && has_n {
                            rim_samples.push((x, y, d, nx, ny, a, edge, spec));
                        } else {
                            for c in 0..3 {
                                let value = material.map_or(pixels[i + c] as f32, |m| m[c]);
                                let lit = (value + (255. - value) * edge + spec).min(255.);
                                pixels[i + c] = (lit * a) as u8;
                            }
                        }
                        pixels[i + 3] = (255. * a) as u8;
                        x += 1;
                    } else {
                        let sd = outline.distance([gx - 5. * dpi, gy - 8. * dpi]);
                        let fade = ((pad_f - d) / (8. * dpi)).clamp(0., 1.);
                        let tx = (sd / (17. * dpi)).clamp(-4.8, 4.8);
                        let tx2 = tx * tx;
                        let tanh = (tx * (135135. + tx2 * (17325. + tx2 * (378. + tx2)))
                            / (135135. + tx2 * (62370. + tx2 * (3150. + tx2 * 28.))))
                        .clamp(-1., 1.);
                        let a = (0.5 * (1. - tanh) * 96. * alpha * fade)
                            .round()
                            .clamp(0., 255.) as u8;
                        for sx in x..span_end_x {
                            let si = row + (sx * 4) as usize;
                            if !has_material {
                                pixels[si..si + 3].fill(0);
                            }
                            pixels[si + 3] = a;
                        }
                        x = span_end_x;
                    }
                }
            }
        });
        if !rim_samples.is_empty() {
            s.glass.borrow().apply_bridge_refraction(
                hwnd,
                RECT {
                    left: l,
                    top: t,
                    right: r,
                    bottom: b,
                },
                w,
                h,
                dpi,
                pixels,
                &rim_samples,
            );
        }
        restack(hwnd, &ps, Some([l, t, w, h]));
        if buffer.upload(hwnd) {
            bridge.uploads += 1;
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            bridge.panes = ps;
            bridge.opacities = opacities;
            bridge.wallpaper = wallpaper;
            bridge.material = material;
            s.bridges.push(bridge);
        }
    }
    for b in &previous {
        dirty.extend(b.panes.iter().map(|p| p.id));
    }
    drop(previous);
    let windows: Vec<_> = dirty.into_iter().collect();
    MANAGER.with(|m| *m.borrow_mut() = Some(s));
    for id in windows {
        InvalidateRect(id as HWND, null(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "Local fusion timing diagnostic"]
    fn profile_large_join_and_detach() {
        let _ui = crate::native_ui_test_lock();
        unsafe {
            init(Rc::new(RefCell::new(Glass::new(0., 1.))));
            let mut windows = Vec::new();
            for x in [-20000, -19108] {
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOOLWINDOW,
                    wide("STATIC").as_ptr(),
                    wide("Lumen fusion timing").as_ptr(),
                    WS_POPUP,
                    x,
                    -20000,
                    900,
                    650,
                    null_mut(),
                    null_mut(),
                    GetModuleHandleW(null()),
                    null(),
                );
                assert!(!hwnd.is_null());
                register_widget(hwnd);
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                prepare(hwnd, 34.);
                windows.push(hwnd);
            }
            let begin = std::time::Instant::now();
            refresh();
            eprintln!(
                "fusion-profile join_ms={:.2}",
                begin.elapsed().as_secs_f64() * 1000.
            );
            assert!(fused(windows[0]));
            let begin = std::time::Instant::now();
            SendMessageW(windows[0], WM_ENTERSIZEMOVE, 0, 0);
            refresh();
            eprintln!(
                "fusion-profile detach_ms={:.2}",
                begin.elapsed().as_secs_f64() * 1000.
            );
            assert!(!fused(windows[0]));
            for hwnd in windows {
                DestroyWindow(hwnd);
            }
            shutdown();
        }
    }
    #[test]
    fn actual_join_blends_raw_widget_colors_and_settles_without_feedback() {
        let _ui = crate::native_ui_test_lock();
        unsafe {
            init(Rc::new(RefCell::new(Glass::new(0., 1.))));
            let mut windows = Vec::new();
            for y in [-20000, -19808] {
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOOLWINDOW,
                    wide("STATIC").as_ptr(),
                    wide("Fusion material regression").as_ptr(),
                    WS_POPUP,
                    -20000,
                    y,
                    200,
                    200,
                    null_mut(),
                    null_mut(),
                    GetModuleHandleW(null()),
                    null(),
                );
                assert!(!hwnd.is_null());
                register_widget(hwnd);
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                prepare(hwnd, 34.);
                windows.push(hwnd);
            }
            let mut frames = [
                Buffer::new(200, 200, 1., 34.).unwrap(),
                Buffer::new(200, 200, 1., 34.).unwrap(),
            ];
            let fill = |frame: &mut Buffer, color| {
                let brush = CreateSolidBrush(color);
                FillRect(
                    frame.dc,
                    &RECT {
                        left: 0,
                        top: 0,
                        right: 200,
                        bottom: 200,
                    },
                    brush,
                );
                DeleteObject(brush);
                frame.background_alpha(255);
            };
            for i in 0..2 {
                fill(&mut frames[i], if i == 0 { 0x0000ff } else { 0xff0000 });
                frames[i].bevel_for(windows[i]);
            }
            refresh();
            assert!(fused(windows[0]));
            let mut key = 0;
            for pass in 0..3 {
                for i in 0..2 {
                    fill(&mut frames[i], if i == 0 { 0x0000ff } else { 0xff0000 });
                    frames[i].bevel_for(windows[i]);
                    assert!(frames[i].present_with_alpha(windows[i], true));
                }
                refresh();
                let pixels = frames[0].snapshot();
                let seam = (195 * 200 + 100) * 4;
                assert!(
                    pixels[seam] > 115 && pixels[seam + 2] > 115,
                    "join mixes both saturated widget colors"
                );
                assert!(
                    pixels[seam + 1] < 3,
                    "no wallpaper/neutral wash at the join"
                );
                assert_eq!(
                    pixels[seam + 3],
                    255,
                    "color blending does not reduce alpha"
                );
                let next = revision(windows[0]);
                if pass > 0 {
                    assert_eq!(
                        key, next,
                        "unblended source tiles must not feed their own blend back"
                    );
                }
                key = next;
            }
            for hwnd in windows {
                DestroyWindow(hwnd);
            }
            shutdown();
        }
    }
    #[test]
    fn decorative_surface_stays_below_topmost_controls() {
        let _ui = crate::native_ui_test_lock();
        unsafe {
            let make = || {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    wide("STATIC").as_ptr(),
                    wide("Lumen stack test").as_ptr(),
                    WS_POPUP,
                    -18000,
                    -18000,
                    20,
                    20,
                    null_mut(),
                    null_mut(),
                    GetModuleHandleW(null()),
                    null(),
                )
            };
            let a = make();
            let b = make();
            let controls = make();
            let bridge = make();
            assert!([a, b, controls, bridge].iter().all(|h| !h.is_null()));
            for h in [a, b, controls] {
                SetWindowPos(
                    h,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            let panes = [
                Pane {
                    id: a as usize,
                    rect: [0., 0., 400., 300.],
                    radius: 30.,
                    dpi: 1.,
                    topmost: true,
                },
                Pane {
                    id: b as usize,
                    rect: [400., 0., 700., 300.],
                    radius: 30.,
                    dpi: 1.,
                    topmost: true,
                },
            ];
            restack(bridge, &panes, None);
            let mut w = GetTopWindow(null_mut());
            let mut ranks = HashMap::new();
            for rank in 0..4096 {
                if w.is_null() {
                    break;
                }
                ranks.insert(w as usize, rank);
                w = GetWindow(w, GW_HWNDNEXT);
            }
            assert!(
                ranks[&(controls as usize)] < ranks[&(bridge as usize)],
                "bridge must not cover General Settings"
            );
            assert!(ranks[&(a as usize)].min(ranks[&(b as usize)]) < ranks[&(bridge as usize)]);
            assert!(GetWindow(bridge, GW_OWNER).is_null());
            for h in [bridge, controls, b, a] {
                DestroyWindow(h);
            }
        }
    }
    #[test]
    fn registered_native_windows_fuse_detach_hide_and_release_the_shared_surface() {
        let _ui = crate::native_ui_test_lock();
        unsafe {
            init(Rc::new(RefCell::new(Glass::new(12., 1.))));
            let instance = GetModuleHandleW(null());
            let mut windows = Vec::new();
            for (x, y, w, h) in [
                (100, 300, 500, 260),
                (100, 100, 240, 208),
                (332, 100, 240, 208),
            ] {
                // Never replace real widget classes with test wndprocs: the
                // other native host tests run in parallel in this process.
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOOLWINDOW,
                    wide("STATIC").as_ptr(),
                    wide("Lumen fusion test").as_ptr(),
                    WS_POPUP,
                    x - 20000,
                    y - 20000,
                    w,
                    h,
                    null_mut(),
                    null_mut(),
                    instance,
                    null(),
                );
                assert!(!hwnd.is_null());
                // Test-owned layered HWNDs stay far outside every desktop.
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                MANAGER.with(|m| {
                    m.borrow_mut()
                        .as_mut()
                        .unwrap()
                        .members
                        .insert(hwnd as usize, read_member(hwnd, 30.));
                });
                SetWindowSubclass(hwnd, Some(widget_proc), SUBCLASS, 0);
                windows.push(hwnd);
            }
            assert!(
                group(windows[0]).is_none(),
                "do not suppress chrome until upload commits"
            );
            refresh();
            assert_eq!(group(windows[0]).unwrap().len(), 3);
            MANAGER.with(|m| assert_eq!(m.borrow().as_ref().unwrap().bridges.len(), 1));
            // Exercise actual BGRA compositor outputs, not just the ownership
            // predicate: three translucent HWNDs must not darken either seam.
            let a = read_member(windows[0], 30.);
            let b = read_member(windows[1], 30.);
            let mut sizing = RECT {
                left: b.window[0],
                top: b.window[1],
                right: b.window[2] - 3,
                bottom: b.window[3],
            };
            SendMessageW(
                windows[1],
                WM_SIZING,
                WMSZ_RIGHT as usize,
                &mut sizing as *mut _ as isize,
            );
            assert_eq!(
                [sizing.left, sizing.top, sizing.right, sizing.bottom],
                b.window,
                "native resize snaps the grabbed edge without moving its opposite anchor"
            );
            sizing.right -= 3;
            set_enabled(false);
            let unsnapped = sizing.right;
            SendMessageW(
                windows[1],
                WM_SIZING,
                WMSZ_RIGHT as usize,
                &mut sizing as *mut _ as isize,
            );
            assert_eq!(
                sizing.right, unsnapped,
                "disabled snapping leaves resize free"
            );
            set_enabled(true);
            for alpha in [0u8, 64, 128, 255] {
                let mut outputs = Vec::new();
                for hwnd in &windows {
                    let m = read_member(*hwnd, 30.);
                    let [l, t, r, b] = m.window;
                    let mut frame = Buffer::new(r - l, b - t, m.pane.dpi, 30.).unwrap();
                    let rect = RECT {
                        left: 0,
                        top: 0,
                        right: r - l,
                        bottom: b - t,
                    };
                    let brush = CreateSolidBrush(0x405060);
                    FillRect(frame.dc, &rect, brush);
                    DeleteObject(brush);
                    frame.background_alpha(alpha);
                    assert!(frame.present_with_alpha(*hwnd, true));
                    outputs.push((m.window, frame.snapshot()));
                }
                for point in [
                    [a.pane.rect[0] + 80., a.pane.rect[1]],
                    [b.pane.rect[2], b.pane.rect[1] + 80.],
                ] {
                    for offset in -3..=3 {
                        let x = point[0] as i32 + offset;
                        let y = point[1] as i32;
                        let mut composed = 0u32;
                        for (r, pixels) in &outputs {
                            if x < r[0] || x >= r[2] || y < r[1] || y >= r[3] {
                                continue;
                            }
                            let i = (((y - r[1]) * (r[2] - r[0]) + x - r[0]) * 4) as usize;
                            let v = pixels[i + 3] as u32;
                            assert!(pixels[i..i + 3].iter().all(|c| *c as u32 <= v));
                            composed = v + (composed * (255 - v) + 127) / 255;
                        }
                        assert_eq!(
                            composed,
                            alpha.max(1) as u32,
                            "single alpha at straight/T seam"
                        );
                    }
                }
            }
            // Mixed member opacities blend continuously across the seam without a hard step.
            opacity(windows[0], 255);
            opacity(windows[1], 0);
            refresh();
            let mut mixed_outputs = Vec::new();
            for (idx, hwnd) in [windows[0], windows[1]].into_iter().enumerate() {
                let m = read_member(hwnd, 30.);
                let [l, t, r, b] = m.window;
                let mut frame = Buffer::new(r - l, b - t, m.pane.dpi, 30.).unwrap();
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: r - l,
                    bottom: b - t,
                };
                let brush = CreateSolidBrush(0x405060);
                FillRect(frame.dc, &rect, brush);
                DeleteObject(brush);
                frame.background_alpha_for(hwnd, if idx == 0 { 255 } else { 0 });
                assert!(frame.present_with_alpha(hwnd, true));
                mixed_outputs.push((m.window, frame.snapshot()));
            }
            let seam_x = a.pane.rect[0] as i32 + 80;
            let seam_y = a.pane.rect[1] as i32;
            let mut prev_alpha: Option<i32> = None;
            for y in (seam_y - 10)..=(seam_y + 10) {
                let mut composed = 0u32;
                for (r, pixels) in &mixed_outputs {
                    if seam_x < r[0] || seam_x >= r[2] || y < r[1] || y >= r[3] {
                        continue;
                    }
                    let i = (((y - r[1]) * (r[2] - r[0]) + seam_x - r[0]) * 4) as usize;
                    let v = pixels[i + 3] as u32;
                    composed = v + (composed * (255 - v) + 127) / 255;
                }
                if let Some(prev) = prev_alpha {
                    assert!(
                        (prev - composed as i32).abs() <= 4,
                        "opacity across seam must transition smoothly without a hard step (y={y}, prev={prev}, cur={composed})"
                    );
                }
                prev_alpha = Some(composed as i32);
            }
            let handle = MANAGER.with(|m| m.borrow().as_ref().unwrap().bridges[0].hwnd);
            let uploads = MANAGER.with(|m| m.borrow().as_ref().unwrap().bridges[0].uploads);
            let token = revision(windows[0]);
            // An unrelated, far-away member does not churn this bridge.
            MANAGER.with(|m| {
                let mut s = m.borrow_mut();
                let s = s.as_mut().unwrap();
                let mut p = s.members.values().next().unwrap().clone();
                p.pane.id = usize::MAX;
                p.pane.rect = [9000., 9000., 9500., 9500.];
                s.members.insert(usize::MAX, p);
                s.changed();
            });
            refresh();
            MANAGER.with(|m| assert_eq!(m.borrow().as_ref().unwrap().bridges[0].hwnd, handle));
            MANAGER.with(|m| assert_eq!(m.borrow().as_ref().unwrap().bridges[0].uploads, uploads));
            assert_eq!(
                revision(windows[0]),
                token,
                "unrelated widget keeps component cache revision"
            );
            MANAGER.with(|m| {
                let mut s = m.borrow_mut();
                let s = s.as_mut().unwrap();
                s.members.remove(&usize::MAX);
                s.fail_surfaces = true;
                s.changed();
            });
            refresh();
            assert!(
                !fused(windows[0]),
                "failed upload falls back to standalone chrome"
            );
            MANAGER.with(|m| m.borrow_mut().as_mut().unwrap().fail_surfaces = false);
            refresh();
            assert!(fused(windows[0]));
            set_enabled(false);
            assert!(!fused(windows[0]));
            refresh();
            MANAGER.with(|m| assert!(m.borrow().as_ref().unwrap().bridges.is_empty()));
            set_enabled(true);
            refresh();
            let id = windows[1] as usize;
            MANAGER.with(|m| {
                let mut s = m.borrow_mut();
                let s = s.as_mut().unwrap();
                s.members.get_mut(&id).unwrap().moving = true;
                s.changed();
            });
            refresh();
            assert!(!fused(windows[1]));
            assert!(fused(windows[0]));
            for hwnd in windows {
                DestroyWindow(hwnd);
            }
            shutdown();
            MANAGER.with(|m| assert!(m.borrow().is_none()));
        }
    }

    #[test]
    fn fused_l_junction_liquid_glass_visual_verification() {
        let _ui = crate::native_ui_test_lock();
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::glass::set_liquid_glass(self.0);
            }
        }
        let _reset = Reset(crate::glass::liquid_glass());
        crate::glass::set_liquid_glass(true);

        let verify_dir = std::env::var_os("LUMEN_VERIFY_DIR").map(std::path::PathBuf::from);
        if let Some(ref dir) = verify_dir {
            std::fs::create_dir_all(dir).unwrap();
        }
        let temp_dir = std::env::temp_dir().join(format!(
            "lumen-fusion-verify-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        struct CleanupDir(std::path::PathBuf);
        impl Drop for CleanupDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = CleanupDir(temp_dir.clone());

        let (cw, ch) = (620usize, 420usize);
        let wave_path = temp_dir.join("fusion-wave.png");
        let mut wave_img = image::RgbaImage::new(cw as u32, ch as u32);
        let tau = std::f32::consts::TAU;
        for y in 0..ch {
            let py = y as f32 / ch as f32 * tau;
            for x in 0..cw {
                let px = x as f32 / cw as f32 * tau;
                let ribbon1 = ((px + py.sin() * 0.55).sin() * 0.5 + 0.5).powf(2.0);
                let ribbon2 = (((px * 2.0 - 0.8) - py.cos() * 0.6).sin() * 0.5 + 0.5).powf(2.6);
                let crest = (1.0 - ((py.sin() - 0.55 * px.sin()).abs() * 2.4).min(1.0)).powf(2.0);
                let r = (12.0 + 45.0 * ribbon2 + 110.0 * crest).clamp(0.0, 255.0) as u8;
                let g = (44.0 + 130.0 * ribbon1 + 115.0 * crest).clamp(0.0, 255.0) as u8;
                let b = (118.0 + 128.0 * ribbon1.max(ribbon2) + 42.0 * crest).clamp(0.0, 255.0) as u8;
                wave_img.put_pixel(x as u32, y as u32, image::Rgba([r, g, b, 255]));
            }
        }
        wave_img.save(&wave_path).unwrap();

        unsafe {
            let _cap = crate::compositor::test_capture::start();
            let widget_glass = Glass::fixture_tiled_file(&wave_path, 14.0, 1.0);
            init(Rc::new(RefCell::new(Glass::fixture_tiled_file(&wave_path, 14.0, 1.0))));
            let instance = GetModuleHandleW(null());
            let (ox, oy) = (-(cw as i32) * 32, -(ch as i32) * 48);

            // L-junction cluster with deliberate 2px edge step between Clock (x=40) and Notes (x=42):
            // Clock [40..300, 40..230] (pane [44..296, 44..226]) — warm sunset orange chrome
            // Notes [42..222, 222..382] (pane [46..218, 226..378]) — soft cool slate chrome
            // Music [292..580, 40..382] (pane [296..576, 44..378]) — deep royal blue chrome
            // Concave Bridge fillet connects Notes right (x=218) + Clock bottom (y=226) + Music left (x=296).
            struct WidgetSpec {
                rect: [i32; 4],
                top_rgb: [f32; 3],
                bot_rgb: [f32; 3],
                tint_mix: f32,
            }
            let specs = [
                WidgetSpec {
                    rect: [ox + 40, oy + 40, 260, 190],
                    top_rgb: [248.0, 185.0, 138.0],
                    bot_rgb: [242.0, 128.0, 74.0],
                    tint_mix: 0.44,
                },
                WidgetSpec {
                    rect: [ox + 42, oy + 222, 180, 160],
                    top_rgb: [132.0, 168.0, 214.0],
                    bot_rgb: [96.0, 134.0, 186.0],
                    tint_mix: 0.40,
                },
                WidgetSpec {
                    rect: [ox + 292, oy + 40, 288, 342],
                    top_rgb: [42.0, 108.0, 224.0],
                    bot_rgb: [18.0, 62.0, 168.0],
                    tint_mix: 0.42,
                },
            ];
            let mut windows = Vec::new();
            for sp in &specs {
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOOLWINDOW,
                    wide("STATIC").as_ptr(),
                    wide("Fusion Liquid Glass verify").as_ptr(),
                    WS_POPUP,
                    sp.rect[0],
                    sp.rect[1],
                    sp.rect[2],
                    sp.rect[3],
                    null_mut(),
                    null_mut(),
                    instance,
                    null(),
                );
                assert!(!hwnd.is_null());
                register_widget(hwnd);
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                prepare(hwnd, 30.0);
                windows.push(hwnd);
            }

            let paint_all = || {
                for (&hwnd, sp) in windows.iter().zip(specs.iter()) {
                    let [_, _, w, h] = sp.rect;
                    let mut frame = Buffer::new(w, h, 1.0, 30.0).unwrap();
                    let brush = CreateSolidBrush(0x281e18);
                    FillRect(frame.dc, &RECT { left: 0, top: 0, right: w, bottom: h }, brush);
                    DeleteObject(brush);
                    widget_glass.draw_blended(frame.dc, hwnd, w, h, 215);
                    {
                        let px = frame.pixels_mut();
                        for y in 0..h as usize {
                            let ty = y as f32 / (h as f32).max(1.0);
                            let cr = sp.top_rgb[0] * (1.0 - ty) + sp.bot_rgb[0] * ty;
                            let cg = sp.top_rgb[1] * (1.0 - ty) + sp.bot_rgb[1] * ty;
                            let cb = sp.top_rgb[2] * (1.0 - ty) + sp.bot_rgb[2] * ty;
                            let row = y * w as usize * 4;
                            for x in 0..w as usize {
                                let i = row + x * 4;
                                px[i] = (px[i] as f32 * (1.0 - sp.tint_mix) + cb * sp.tint_mix).round() as u8;
                                px[i + 1] = (px[i + 1] as f32 * (1.0 - sp.tint_mix) + cg * sp.tint_mix).round() as u8;
                                px[i + 2] = (px[i + 2] as f32 * (1.0 - sp.tint_mix) + cr * sp.tint_mix).round() as u8;
                            }
                        }
                    }
                    frame.background_alpha_for(hwnd, 255);
                    frame.bevel_for(hwnd);
                    assert!(frame.present_with_alpha(hwnd, true));
                }
            };

            paint_all();
            refresh();
            assert!(windows.iter().all(|&h| fused(h)), "all L-junction panes must fuse");
            paint_all();
            refresh();

            let bridge_hwnd = MANAGER.with(|m| {
                let guard = m.borrow();
                let s = guard.as_ref().unwrap();
                assert_eq!(s.bridges.len(), 1);
                s.bridges[0].hwnd
            });
            let bridge_cap = crate::compositor::test_capture::get(bridge_hwnd).expect("bridge upload");
            let member_caps: Vec<_> = windows
                .iter()
                .map(|&h| crate::compositor::test_capture::get(h).expect("member upload"))
                .collect();

            // Composite onto 620x420 wave wallpaper canvas (Bridge below members, matching Z-order).
            let mut canvas = wave_img.clone();
            let blend_cap = |canvas: &mut image::RgbaImage, cap: &crate::compositor::test_capture::CapturedUpload| {
                let lx0 = cap.rect[0] - ox;
                let ly0 = cap.rect[1] - oy;
                for y in 0..cap.h {
                    let cy = ly0 + y;
                    if cy < 0 || cy >= ch as i32 {
                        continue;
                    }
                    for x in 0..cap.w {
                        let cx = lx0 + x;
                        if cx < 0 || cx >= cw as i32 {
                            continue;
                        }
                        let si = ((y * cap.w + x) * 4) as usize;
                        let a = cap.bgra[si + 3] as u32;
                        if a == 0 {
                            continue;
                        }
                        let inv = 255 - a;
                        let p = canvas.get_pixel_mut(cx as u32, cy as u32);
                        p.0[0] = (cap.bgra[si + 2] as u32 + (p.0[0] as u32 * inv + 127) / 255).min(255) as u8;
                        p.0[1] = (cap.bgra[si + 1] as u32 + (p.0[1] as u32 * inv + 127) / 255).min(255) as u8;
                        p.0[2] = (cap.bgra[si] as u32 + (p.0[2] as u32 * inv + 127) / 255).min(255) as u8;
                        p.0[3] = 255;
                    }
                }
            };
            blend_cap(&mut canvas, &bridge_cap);
            for mc in &member_caps {
                blend_cap(&mut canvas, mc);
            }

            if let Some(ref dir) = verify_dir {
                canvas.save(dir.join("verify_fused_l_junction.png")).unwrap();
            }

            // 1) Smooth blend across Clock | Music and across Music interior without seam/tile cuts.
            let mut max_seam_step = 0i32;
            for x in 270u32..555 {
                let a = canvas.get_pixel(x, 140).0;
                let b = canvas.get_pixel(x + 1, 140).0;
                for c in 0..3 {
                    max_seam_step = max_seam_step.max((a[c] as i32 - b[c] as i32).abs());
                }
            }
            assert!(
                max_seam_step <= 8,
                "fused Clock | Music seam & interior must blend smoothly without L-cut discontinuities (max 1px jump={max_seam_step})"
            );
            let mut max_vert_step = 0i32;
            for y in 65u32..355 {
                let a = canvas.get_pixel(350, y).0;
                let b = canvas.get_pixel(350, y + 1).0;
                for c in 0..3 {
                    max_vert_step = max_vert_step.max((a[c] as i32 - b[c] as i32).abs());
                }
            }
            assert!(
                max_vert_step <= 8,
                "fused Music vertical slice must remain smooth without cutoff steps (max 1px jump={max_vert_step})"
            );

            // 2) Zero square corner regression on sub-2dp offset: Notes outer corner bbox (x=42, y=381) must stay rounded (transparent), not filled square!
            let notes_cap = &member_caps[1];
            let corner_bbox = (notes_cap.h as usize - 1) * notes_cap.w as usize * 4;
            assert_eq!(
                notes_cap.bgra[corner_bbox + 3],
                0,
                "bottom-left outer corner must stay transparent with rounded corner arc"
            );
            // Also check inward corner around offset (8, h-9): should be inside rounded radius 30? Wait: radius=30, margin=4 -> (4 + 30*(1 - cos 45°)) = 4 + 8.8 = 12.8px. So (6, h-7) is outside rounded arc (alpha=0)!
            let outer_arc_px = ((notes_cap.h as usize - 7) * notes_cap.w as usize + 6) * 4;
            assert_eq!(
                notes_cap.bgra[outer_arc_px + 3],
                0,
                "notes rounded corner must NOT collapse square despite 2px left edge mismatch with clock"
            );

            // 3) Concave Bridge fillet pixel in the L-notch (x = 223, y = 232) must be opaque/blended (Bridge filled the notch!).
            let bx = 223 - (bridge_cap.rect[0] - ox);
            let by = 232 - (bridge_cap.rect[1] - oy);
            let bi = ((by * bridge_cap.w + bx) * 4) as usize;
            assert!(
                bridge_cap.bgra[bi + 3] > 200,
                "bridge must smoothly fill concave L-junction fillet (alpha={})",
                bridge_cap.bgra[bi + 3]
            );

            for hwnd in windows {
                DestroyWindow(hwnd);
            }
            shutdown();
            MANAGER.with(|m| assert!(m.borrow().is_none()));
        }
    }
}
