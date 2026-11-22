//! Floating non-activating event popover for hovered/clicked calendar days.
use super::{
    gcal::CalendarEvent,
    model::{Date, MONTHS},
    typography::{Style, Typography},
};
use crate::{
    compositor::{Buffer, WindowShadow},
    model::wide,
    render::{self, Canvas},
};
use std::{
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
};

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const POPOVER_W: f32 = 248.;
const POPOVER_RADIUS: f32 = 18.;
const MAX_VISIBLE_EVENTS: usize = 6;

pub struct EventPopover {
    hwnd: HWND,
    typo: Typography,
    shadow: Option<WindowShadow>,
    visible: bool,
    last_key: String,
}

impl Default for EventPopover {
    fn default() -> Self {
        Self {
            hwnd: null_mut(),
            typo: Typography::default(),
            shadow: None,
            visible: false,
            last_key: String::new(),
        }
    }
}

impl Drop for EventPopover {
    fn drop(&mut self) {
        unsafe {
            self.shadow.take();
            if !self.hwnd.is_null() && IsWindow(self.hwnd) != 0 {
                DestroyWindow(self.hwnd);
            }
        }
    }
}

unsafe extern "system" fn popover_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

pub struct PopoverAppearance<'a> {
    pub glass: &'a crate::glass::Glass,
    pub frosted: bool,
    pub frost_strength: f32,
    pub tint: f32,
    pub opacity: f32,
    pub palette: &'a str,
    pub accent: u32,
    pub light_surface: bool,
}

impl EventPopover {
    #[cfg(test)]
    pub fn is_visible(&self) -> bool {
        self.visible
    }
    pub unsafe fn hide(&mut self) {
        if self.visible {
            self.visible = false;
            self.last_key.clear();
            if let Some(shadow) = &self.shadow {
                shadow.hide();
            }
            if !self.hwnd.is_null() && IsWindow(self.hwnd) != 0 {
                ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }

    unsafe fn ensure_window(&mut self, owner: HWND) -> bool {
        if !self.hwnd.is_null() && IsWindow(self.hwnd) != 0 {
            return true;
        }
        let class = wide("LumenCalendarEventPopover");
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(popover_proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return false;
        }
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            class.as_ptr(),
            wide("Calendar Events").as_ptr(),
            WS_POPUP,
            -20000,
            -20000,
            10,
            10,
            owner,
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return false;
        }
        self.hwnd = hwnd;
        self.shadow = WindowShadow::new(hwnd);
        true
    }

    pub unsafe fn show_for_day(
        &mut self,
        owner: HWND,
        date: Date,
        today: Date,
        events: &[CalendarEvent],
        day_rect_dip: [f32; 4],
        dpi: f32,
        appearance: PopoverAppearance<'_>,
        smoke: bool,
    ) {
        if smoke {
            return;
        }
        if !self.ensure_window(owner) {
            return;
        }
        let accent = appearance.accent;
        let light_surface = appearance.light_surface;
        let frosted = appearance.frosted && crate::glass::frosted_glass();

        let shown = events.len().min(MAX_VISIBLE_EVENTS);
        let extra = events.len().saturating_sub(MAX_VISIBLE_EVENTS);
        let mut h_dip = 44.; // header + padding
        if events.is_empty() {
            h_dip += 28.;
        } else {
            for ev in &events[..shown] {
                h_dip += if ev.location.is_some() { 44. } else { 32. };
            }
            if extra > 0 {
                h_dip += 18.;
            }
        }
        h_dip += 10.;

        let w_px = (POPOVER_W * dpi).round() as i32;
        let h_px = (h_dip * dpi).round() as i32;

        // Position beside the calendar window (prefer left if near right edge, else right).
        let mut cal_rect: RECT = zeroed();
        GetWindowRect(owner, &mut cal_rect);
        let monitor = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        let work = if GetMonitorInfoW(monitor, &mut mi) != 0 {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        };

        let gap = (10. * dpi).round() as i32;
        let day_mid_y = cal_rect.top + (((day_rect_dip[1] + day_rect_dip[3]) * 0.5) * dpi).round() as i32;
        let mut x = if cal_rect.left - gap - w_px >= work.left {
            cal_rect.left - gap - w_px
        } else if cal_rect.right + gap + w_px <= work.right {
            cal_rect.right + gap
        } else {
            (work.right - w_px - 8).max(work.left + 8)
        };
        let mut y = day_mid_y - h_px / 3;
        x = x.clamp(work.left + 4, (work.right - w_px - 4).max(work.left + 4));
        y = y.clamp(work.top + 4, (work.bottom - h_px - 4).max(work.top + 4));

        let mut key = format!(
            "{}-{}-{}:{}:{}:{}:{}:{},{}:{}:{:.1}:{:.1}:{:.1}:{}:{}",
            date.year,
            date.month,
            date.day,
            events.len(),
            light_surface,
            accent,
            dpi,
            x,
            y,
            frosted,
            appearance.frost_strength,
            appearance.tint,
            appearance.opacity,
            appearance.palette,
            appearance.glass.revision(),
        );
        for ev in &events[..shown] {
            use std::fmt::Write;
            let _ = write!(
                key,
                "|{}:{}:{}:{:?}:{:?}",
                ev.all_day,
                ev.summary,
                ev.location.as_deref().unwrap_or(""),
                ev.start_minutes,
                ev.end_minutes
            );
        }

        if self.last_key == key {
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                x,
                y,
                w_px,
                h_px,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            self.visible = true;
            if let Some(shadow) = &mut self.shadow {
                shadow.sync_with_radius(
                    self.hwnd,
                    dpi,
                    (appearance.opacity * 2.4).round() as u8,
                    POPOVER_RADIUS,
                );
            }
            return;
        }
        self.last_key = key;

        // Position the hidden window first so Glass::draw_blended samples the desktop behind (x, y).
        SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            x,
            y,
            w_px,
            h_px,
            SWP_NOACTIVATE | SWP_NOZORDER,
        );

        let Some(mut buf) = Buffer::new(w_px, h_px, dpi, POPOVER_RADIUS) else {
            return;
        };

        let wallpaper_accent = appearance.glass.wallpaper_accent();
        let base = crate::surface::color(appearance.palette, wallpaper_accent);
        let r = RECT {
            left: 0,
            top: 0,
            right: w_px,
            bottom: h_px,
        };
        let brush = CreateSolidBrush(base);
        FillRect(buf.dc, &r, brush);
        DeleteObject(brush);
        if frosted {
            appearance.glass.draw_blended(
                buf.dc,
                self.hwnd,
                w_px,
                h_px,
                (appearance.frost_strength * 2.55).round() as u8,
            );
        }
        let blend = if frosted {
            appearance.tint / 100.
        } else {
            1.
        };
        let pane = [0., 0., POPOVER_W, h_dip];
        let chrome = [4., 4., POPOVER_W - 4., h_dip - 4.];
        let light = light_surface || crate::contrast::luminance(base) > 0.42;
        buf.paint_frosted_chrome_for(self.hwnd, frosted, |chrome_dc| {
            let c = Canvas::new(chrome_dc, dpi);
            if !crate::surface::managed(appearance.palette)
                && appearance.palette != "wallpaper-auto"
            {
                c.round(pane, 0., base, (blend * 255.).round() as u8, false, false);
            }
            crate::surface::paint(&c, pane, appearance.palette, blend);
            c.coating(chrome, POPOVER_RADIUS);
            c.draw_pane_overlay(
                [w_px as u32, h_px as u32],
                dpi,
                appearance.tint / 100.,
                light,
                pane,
            );
            c.widget_corner_reflection_for(self.hwnd, chrome, POPOVER_RADIUS, light);
        });
        buf.background_alpha_for(self.hwnd, (appearance.opacity * 2.55).round() as u8);

        let fg = crate::contrast::inks(
            crate::contrast::sample(buf.dc, dpi, [12., 12., POPOVER_W - 12., h_dip - 12.]),
            None,
        )
        .0;
        let border = if crate::contrast::luminance(fg) < 0.5 {
            0x000000
        } else {
            0xffffff
        };

        {
            let _batch = render::AlphaBatch::new(buf.dc, dpi);
            let c = Canvas::new(buf.dc, dpi);
            c.round(
                [0.5, 0.5, POPOVER_W - 0.5, h_dip - 0.5],
                POPOVER_RADIUS,
                border,
                if light_surface { 26 } else { 38 },
                true,
                false,
            );

            let month_short = &MONTHS[date.month as usize - 1][..3];
            let header = if date == today {
                format!("Today · {}, {} {}", WEEKDAYS[date.weekday()], month_short, date.day)
            } else {
                format!("{}, {} {}", WEEKDAYS[date.weekday()], month_short, date.day)
            };
            self.typo.draw(
                buf.dc,
                dpi,
                &header,
                [14., 12., POPOVER_W - 56., 32.],
                Style::new(12., 650, fg),
                0,
            );
            if !events.is_empty() {
                let count_label = format!("{} event{}", events.len(), if events.len() == 1 { "" } else { "s" });
                self.typo.draw(
                    buf.dc,
                    dpi,
                    &count_label,
                    [POPOVER_W - 74., 13., POPOVER_W - 14., 31.],
                    Style::new(9.5, 600, accent).alpha(0.92),
                    2,
                );
            }
            c.line(&[(14., 37.), (POPOVER_W - 14., 37.)], fg, 28, 1.);

            let mut cy = 44.;
            if events.is_empty() {
                self.typo.draw(
                    buf.dc,
                    dpi,
                    "No events scheduled",
                    [14., cy + 2., POPOVER_W - 14., cy + 24.],
                    Style::new(11., 400, fg).alpha(0.62),
                    0,
                );
            } else {
                for ev in &events[..shown] {
                    let item_h = if ev.location.is_some() { 40. } else { 28. };
                    c.round([14., cy + 3., 17., cy + item_h - 3.], 1.5, accent, 235, false, false);
                    let time_str = ev.time_label();
                    self.typo.draw(
                        buf.dc,
                        dpi,
                        &time_str,
                        [24., cy + 1., POPOVER_W - 14., cy + 14.],
                        Style::new(9.5, 600, accent).alpha(0.95),
                        0,
                    );
                    self.typo.draw(
                        buf.dc,
                        dpi,
                        &ev.summary,
                        [24., cy + 13., POPOVER_W - 14., cy + 29.],
                        Style::new(11., 550, fg),
                        0,
                    );
                    if let Some(loc) = &ev.location {
                        self.typo.draw(
                            buf.dc,
                            dpi,
                            loc,
                            [24., cy + 28., POPOVER_W - 14., cy + 41.],
                            Style::new(9., 400, fg).alpha(0.58),
                            0,
                        );
                    }
                    cy += if ev.location.is_some() { 44. } else { 32. };
                }
                if extra > 0 {
                    self.typo.draw(
                        buf.dc,
                        dpi,
                        &format!("+{extra} more"),
                        [24., cy, POPOVER_W - 14., cy + 16.],
                        Style::new(9.5, 500, fg).alpha(0.65),
                        0,
                    );
                }
            }
        }

        buf.present_with_alpha(self.hwnd, true);
        SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            x,
            y,
            w_px,
            h_px,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        self.visible = true;
        if let Some(shadow) = &mut self.shadow {
            shadow.sync_with_radius(
                self.hwnd,
                dpi,
                (appearance.opacity * 2.4).round() as u8,
                POPOVER_RADIUS,
            );
        }
    }
}
