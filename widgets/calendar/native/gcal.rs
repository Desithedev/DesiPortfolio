//! Google Calendar iCal (.ics) sync and parser. Uses WinHttp off the UI thread.
use super::model::Date;
use crate::model::wide;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::Path,
    ptr::{null, null_mut},
    sync::{Arc, Mutex},
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::{SYSTEMTIME, *},
    Networking::WinHttp::*,
    System::Time::SystemTimeToTzSpecificLocalTime,
    UI::WindowsAndMessaging::PostMessageW,
};

pub const MAX_ICS_BYTES: usize = 16 * 1024 * 1024;
const CACHE_FILE: &str = "config/native-calendar-gcal.ics";
pub const SYNC_INTERVAL_SECS: u64 = 15 * 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    Event,
    Task {
        list_id: String,
        task_id: String,
        completed: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarListEntry {
    pub id: String,
    pub summary: String,
    pub color: Option<u32>,
    pub primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarEvent {
    pub summary: String,
    pub location: Option<String>,
    pub all_day: bool,
    pub start_minutes: Option<u16>,
    pub end_minutes: Option<u16>,
    pub color: Option<u32>,
    pub kind: EventKind,
    pub html_link: Option<String>,
}

impl CalendarEvent {
    pub fn time_label(&self) -> String {
        if self.all_day {
            return "All day".into();
        }
        match (self.start_minutes, self.end_minutes) {
            (Some(s), Some(e)) if e > s => format!("{:02}:{:02} - {:02}:{:02}", s / 60, s % 60, e / 60, e % 60),
            (Some(s), _) => format!("{:02}:{:02}", s / 60, s % 60),
            _ => "Timed".into(),
        }
    }
}

pub struct SyncResult {
    pub url: String,
    pub outcome: Result<(BTreeMap<Date, Vec<CalendarEvent>>, Vec<CalendarListEntry>), String>,
}

pub struct GcalStore {
    pub events: BTreeMap<Date, Vec<CalendarEvent>>,
    pub calendars: Vec<CalendarListEntry>,
    pub syncing: bool,
    pub oauth_signing_in: bool,
    pub last_sync: Option<Instant>,
    pub last_error: Option<String>,
    pending: Arc<Mutex<Option<SyncResult>>>,
    pub pending_oauth: Arc<Mutex<Option<Result<super::gcal_oauth::OAuthTokens, String>>>>,
    pub oauth_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub pending_mutation: Arc<Mutex<Option<Result<(), String>>>>,
}

impl Default for GcalStore {
    fn default() -> Self {
        Self {
            events: BTreeMap::new(),
            calendars: Vec::new(),
            syncing: false,
            oauth_signing_in: false,
            last_sync: None,
            last_error: None,
            pending: Arc::new(Mutex::new(None)),
            pending_oauth: Arc::new(Mutex::new(None)),
            oauth_cancel: None,
            pending_mutation: Arc::new(Mutex::new(None)),
        }
    }
}

impl GcalStore {
    pub fn events_for(&self, date: Date) -> &[CalendarEvent] {
        self.events.get(&date).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn has_events(&self, date: Date) -> bool {
        self.events.get(&date).is_some_and(|v| !v.is_empty())
    }

    pub fn total_events(&self) -> usize {
        self.events.values().map(|v| v.len()).sum()
    }

    /// Returns `(minutes_until_start, summary)` for the next timed event today within `0..=60` min.
    pub fn upcoming_today(&self, today: Date, now_minutes: u16) -> Option<(u16, &str)> {
        self.events.get(&today)?.iter().find_map(|ev| {
            if ev.all_day {
                return None;
            }
            let start = ev.start_minutes?;
            if start >= now_minutes && start - now_minutes <= 60 {
                Some((start - now_minutes, ev.summary.as_str()))
            } else {
                None
            }
        })
    }

    pub fn load_cache(&mut self, root: &Path, disabled_calendars: &[String]) {
        if super::gcal_oauth::has_tokens(root) {
            if let Some((events, cals)) =
                super::gcal_oauth::load_cached_events(root, disabled_calendars)
            {
                self.events = events;
                self.calendars = cals;
                return;
            }
        }
        let path = root.join(CACHE_FILE);
        if let Ok(text) = fs::read_to_string(path) {
            let today = Date::today();
            self.events = parse_ics(&text, today.add_days(-365), today.add_days(365));
        }
    }

    pub fn cancel_oauth_login(&mut self) {
        if let Some(flag) = self.oauth_cancel.take() {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.oauth_signing_in = false;
        if let Ok(mut guard) = self.pending_oauth.lock() {
            *guard = None;
        }
    }

    pub fn clear(&mut self, root: &Path) {
        self.events.clear();
        self.calendars.clear();
        self.syncing = false;
        self.cancel_oauth_login();
        self.last_sync = None;
        self.last_error = None;
        if let Ok(mut guard) = self.pending.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.pending_oauth.lock() {
            *guard = None;
        }
        let _ = fs::remove_file(root.join(CACHE_FILE));
        super::gcal_oauth::clear_oauth_data(root);
    }

    pub fn start_oauth_sync(
        &mut self,
        client_id: &str,
        client_secret: Option<&str>,
        disabled_calendars: &[String],
        hwnd: HWND,
        msg: u32,
        root: &Path,
    ) {
        if self.syncing {
            return;
        }
        let cid = client_id.to_string();
        let csec = client_secret.map(str::to_string);
        let disabled = disabled_calendars.to_vec();
        let root = root.to_path_buf();
        let pending = self.pending.clone();
        let hwnd_val = hwnd as isize;
        self.syncing = true;
        self.last_error = None;
        std::thread::spawn(move || {
            let outcome = super::gcal_oauth::fetch_google_calendar_events(
                &cid,
                csec.as_deref(),
                &disabled,
                &root,
            );
            if let Ok(mut guard) = pending.lock() {
                *guard = Some(SyncResult {
                    url: "oauth://google".into(),
                    outcome,
                });
            }
            unsafe {
                PostMessageW(hwnd_val as HWND, msg, 0, 0);
            }
        });
    }

    pub fn finish_oauth_login(&mut self) -> Option<Result<(), String>> {
        let outcome = self.pending_oauth.lock().ok().and_then(|mut g| g.take())?;
        self.oauth_signing_in = false;
        self.oauth_cancel = None;
        match outcome {
            Ok(_) => {
                self.last_error = None;
                Some(Ok(()))
            }
            Err(e) => {
                self.last_error = Some(e.clone());
                Some(Err(e))
            }
        }
    }

    pub fn start_sync(&mut self, url: &str, hwnd: HWND, msg: u32, root: &Path) {
        if self.syncing {
            return;
        }
        let url = url.to_string();
        let root = root.to_path_buf();
        let pending = self.pending.clone();
        let hwnd_val = hwnd as isize;
        self.syncing = true;
        self.last_error = None;
        std::thread::spawn(move || {
            let outcome = fetch_ics(&url).map(|ics| {
                let today = Date::today();
                let parsed = parse_ics(&ics, today.add_days(-365), today.add_days(365));
                let _ = save_cache(&root, &ics);
                (parsed, Vec::new())
            });
            if let Ok(mut guard) = pending.lock() {
                *guard = Some(SyncResult { url, outcome });
            }
            unsafe {
                PostMessageW(hwnd_val as HWND, msg, 0, 0);
            }
        });
    }

    pub fn finish_sync(&mut self, expected_url: Option<&str>) -> bool {
        let result = self.pending.lock().ok().and_then(|mut g| g.take());
        let Some(res) = result else {
            return false;
        };
        self.syncing = false;
        self.last_sync = Some(Instant::now());
        if expected_url != Some(res.url.as_str()) {
            return false;
        }
        match res.outcome {
            Ok((parsed, cals)) => {
                self.events = parsed;
                if !cals.is_empty() {
                    self.calendars = cals;
                }
                self.last_error = None;
            }
            Err(e) => {
                self.last_error = Some(e);
            }
        }
        true
    }
}

fn save_cache(root: &Path, ics: &str) -> Result<(), String> {
    let folder = root.join("config");
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let dest = root.join(CACHE_FILE);
    let temp = folder.join("native-calendar-gcal.ics.tmp");
    fs::write(&temp, ics.as_bytes()).map_err(|e| e.to_string())?;
    unsafe {
        use windows_sys::Win32::Storage::FileSystem::*;
        if MoveFileExW(
            wide(temp).as_ptr(),
            wide(dest).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(())
}

pub fn normalize_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("URL cannot be empty.".into());
    }
    if trimmed.bytes().any(|b| b <= 32 || b == 127) {
        return Err("URL contains invalid whitespace or control characters.".into());
    }
    let normalized = if trimmed.len() >= 9 && trimmed[..9].eq_ignore_ascii_case("webcal://") {
        format!("https://{}", &trimmed[9..])
    } else if trimmed.len() >= 8 && trimmed[..8].eq_ignore_ascii_case("https://") {
        format!("https://{}", &trimmed[8..])
    } else {
        return Err("Only https:// or webcal:// iCal URLs are supported.".into());
    };
    let (host, path) = split_https_url(&normalized)?;
    if host.is_empty() || path.is_empty() {
        return Err("Invalid iCal URL.".into());
    }
    Ok(normalized)
}

fn split_https_url(url: &str) -> Result<(&str, &str), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "Only HTTPS URLs are supported.".to_string())?;
    if rest.bytes().any(|b| b <= 32 || b == 127) {
        return Err("Invalid characters in URL.".into());
    }
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, p),
        None => (rest, ""),
    };
    if host.is_empty() || host.contains('@') {
        return Err("Invalid URL host.".into());
    }
    Ok((host, path))
}

struct Internet(*mut std::ffi::c_void);
impl Drop for Internet {
    fn drop(&mut self) {
        unsafe {
            WinHttpCloseHandle(self.0);
        }
    }
}

fn handle(p: *mut std::ffi::c_void) -> Result<Internet, String> {
    if p.is_null() {
        Err(format!(
            "Network error: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(Internet(p))
    }
}

pub fn fetch_ics(url: &str) -> Result<String, String> {
    let mut location = normalize_url(url)?;
    for _ in 0..5 {
        let (host, path) = split_https_url(&location)?;
        let (host_only, port) = match host.rsplit_once(':') {
            Some((h, p)) if p.bytes().all(|c| c.is_ascii_digit()) => {
                (h, p.parse::<u16>().unwrap_or(443))
            }
            _ => (host, 443),
        };
        unsafe {
            let session = handle(WinHttpOpen(
                wide("Lumen/0.1 CalendarSync").as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                null(),
                null(),
                0,
            ))?;
            WinHttpSetTimeouts(session.0, 10000, 10000, 15000, 30000);
            let connection = handle(WinHttpConnect(session.0, wide(host_only).as_ptr(), port, 0))?;
            let request = handle(WinHttpOpenRequest(
                connection.0,
                wide("GET").as_ptr(),
                wide(format!("/{path}")).as_ptr(),
                null(),
                null(),
                null(),
                WINHTTP_FLAG_SECURE,
            ))?;
            let never: u32 = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
            WinHttpSetOption(
                request.0,
                WINHTTP_OPTION_REDIRECT_POLICY,
                &never as *const _ as _,
                4,
            );
            if WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) == 0
                || WinHttpReceiveResponse(request.0, null_mut()) == 0
            {
                return Err(format!(
                    "Failed to fetch calendar: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut status: u32 = 0;
            let mut size = 4u32;
            if WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                &mut status as *mut _ as _,
                &mut size,
                null_mut(),
            ) == 0
            {
                return Err("Cannot read HTTP status".into());
            }
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let mut len = 0u32;
                WinHttpQueryHeaders(
                    request.0,
                    WINHTTP_QUERY_LOCATION,
                    null(),
                    null_mut(),
                    &mut len,
                    null_mut(),
                );
                let mut buf = vec![0u16; (len as usize / 2).max(1)];
                if WinHttpQueryHeaders(
                    request.0,
                    WINHTTP_QUERY_LOCATION,
                    null(),
                    buf.as_mut_ptr() as _,
                    &mut len,
                    null_mut(),
                ) == 0
                {
                    return Err("Redirect missing Location header".into());
                }
                let next = String::from_utf16_lossy(&buf[..(len as usize / 2)]);
                location = normalize_url(next.trim_end_matches('\0'))?;
                continue;
            }
            if status != 200 {
                return Err(format!("HTTP {status} from calendar feed"));
            }
            let mut data = Vec::new();
            loop {
                let mut avail: u32 = 0;
                if WinHttpQueryDataAvailable(request.0, &mut avail) == 0 {
                    return Err("Error reading calendar response".into());
                }
                if avail == 0 {
                    break;
                }
                if data.len() + avail as usize > MAX_ICS_BYTES {
                    return Err("Calendar feed exceeds 16 MB limit".into());
                }
                let mut chunk = vec![0u8; avail as usize];
                let mut read: u32 = 0;
                if WinHttpReadData(request.0, chunk.as_mut_ptr() as _, avail, &mut read) == 0 {
                    return Err("Error reading calendar bytes".into());
                }
                data.extend_from_slice(&chunk[..read as usize]);
            }
            return String::from_utf8(data).map_err(|_| "Calendar feed is not valid UTF-8".into());
        }
    }
    Err("Too many redirects".into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DateTimeVal {
    date: Date,
    minutes: Option<u16>,
    all_day: bool,
}

#[derive(Default)]
struct RawEvent {
    summary: Option<String>,
    location: Option<String>,
    status: Option<String>,
    dtstart: Option<DateTimeVal>,
    dtend: Option<DateTimeVal>,
    duration_minutes: Option<i32>,
    rrule: Option<String>,
    exdates: HashSet<Date>,
}

fn unfold_lines(ics: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in ics.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        lines.push(line.to_string());
    }
    lines
}

fn unescape_ics_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push(' '),
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn utc_to_local(year: i32, month: i32, day: i32, hour: u16, minute: u16) -> (Date, u16) {
    let utc = SYSTEMTIME {
        wYear: year as u16,
        wMonth: month as u16,
        wDayOfWeek: 0,
        wDay: day as u16,
        wHour: hour,
        wMinute: minute,
        wSecond: 0,
        wMilliseconds: 0,
    };
    let mut local = SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    let ok = unsafe { SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) };
    if ok != 0 {
        (
            Date {
                year: local.wYear as i32,
                month: local.wMonth as i32,
                day: local.wDay as i32,
            },
            local.wHour * 60 + local.wMinute,
        )
    } else {
        (Date { year, month, day }, hour * 60 + minute)
    }
}

fn parse_ics_datetime(val: &str, force_date: bool, force_utc: bool) -> Option<DateTimeVal> {
    let v = val.trim();
    if v.len() < 8 {
        return None;
    }
    let year: i32 = v.get(0..4)?.parse().ok()?;
    let month: i32 = v.get(4..6)?.parse().ok()?;
    let day: i32 = v.get(6..8)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let date = Date { year, month, day };
    if force_date || v.len() == 8 {
        return Some(DateTimeVal {
            date,
            minutes: None,
            all_day: true,
        });
    }
    if v.as_bytes().get(8) != Some(&b'T') || v.len() < 15 {
        return Some(DateTimeVal {
            date,
            minutes: None,
            all_day: true,
        });
    }
    let hour: u16 = v.get(9..11)?.parse().ok()?;
    let minute: u16 = v.get(11..13)?.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    if force_utc || v.ends_with('Z') {
        let (local_date, local_mins) = utc_to_local(year, month, day, hour, minute);
        Some(DateTimeVal {
            date: local_date,
            minutes: Some(local_mins),
            all_day: false,
        })
    } else {
        Some(DateTimeVal {
            date,
            minutes: Some(hour * 60 + minute),
            all_day: false,
        })
    }
}

fn parse_duration_minutes(val: &str) -> Option<i32> {
    let s = val.trim().strip_prefix('P')?;
    let mut total = 0i32;
    let (date_part, time_part) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut num = String::new();
    for c in date_part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: i32 = num.parse().ok()?;
            num.clear();
            match c {
                'W' => total += n * 7 * 24 * 60,
                'D' => total += n * 24 * 60,
                _ => {}
            }
        }
    }
    if let Some(tp) = time_part {
        for c in tp.chars() {
            if c.is_ascii_digit() {
                num.push(c);
            } else {
                let n: i32 = num.parse().ok()?;
                num.clear();
                match c {
                    'H' => total += n * 60,
                    'M' => total += n,
                    _ => {}
                }
            }
        }
    }
    Some(total)
}

fn parse_weekday_code(code: &str) -> Option<usize> {
    let tail = if code.len() > 2 {
        &code[code.len() - 2..]
    } else {
        code
    };
    match tail {
        "SU" => Some(0),
        "MO" => Some(1),
        "TU" => Some(2),
        "WE" => Some(3),
        "TH" => Some(4),
        "FR" => Some(5),
        "SA" => Some(6),
        _ => None,
    }
}

fn add_months(d: Date, months: i32) -> Date {
    let total = d.year * 12 + d.month - 1 + months;
    let year = total.div_euclid(12).clamp(1601, 9999);
    let month = total.rem_euclid(12) + 1;
    let probe = Date { year, month, day: 1 };
    Date {
        year,
        month,
        day: d.day.min(probe.days_in_month()),
    }
}

fn expand_rrule(
    start: Date,
    rrule: &str,
    range_start: Date,
    range_end: Date,
    exdates: &HashSet<Date>,
) -> Vec<Date> {
    let mut freq = "";
    let mut interval = 1i32;
    let mut count: Option<usize> = None;
    let mut until: Option<Date> = None;
    let mut byday: Vec<usize> = Vec::new();
    for part in rrule.split(';') {
        if let Some((k, v)) = part.split_once('=') {
            match k {
                "FREQ" => freq = v,
                "INTERVAL" => interval = v.parse::<i32>().unwrap_or(1).max(1),
                "COUNT" => count = v.parse::<usize>().ok(),
                "UNTIL" => until = parse_ics_datetime(v, false, false).map(|dt| dt.date),
                "BYDAY" => {
                    for code in v.split(',') {
                        if let Some(wd) = parse_weekday_code(code.trim()) {
                            byday.push(wd);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    let max_limit = count.unwrap_or(800).min(2000);
    let mut generated = 0usize;
    let effective_end = match until {
        Some(u) if u < range_end => u,
        _ => range_end,
    };

    match freq {
        "DAILY" => {
            let mut cur = start;
            while cur <= effective_end && generated < max_limit {
                generated += 1;
                if cur >= range_start && !exdates.contains(&cur) {
                    out.push(cur);
                }
                cur = cur.add_days(interval);
            }
        }
        "WEEKLY" => {
            if byday.is_empty() {
                let mut cur = start;
                while cur <= effective_end && generated < max_limit {
                    generated += 1;
                    if cur >= range_start && !exdates.contains(&cur) {
                        out.push(cur);
                    }
                    cur = cur.add_days(interval * 7);
                }
            } else {
                let week_start = start.add_days(-(start.weekday() as i32));
                let mut wk = week_start;
                'outer: while wk <= effective_end {
                    for wd in 0..7 {
                        let d = wk.add_days(wd);
                        if d < start || !byday.contains(&d.weekday()) {
                            continue;
                        }
                        if d > effective_end || generated >= max_limit {
                            break 'outer;
                        }
                        generated += 1;
                        if d >= range_start && !exdates.contains(&d) {
                            out.push(d);
                        }
                    }
                    wk = wk.add_days(interval * 7);
                }
            }
        }
        "MONTHLY" => {
            let mut step = 0i32;
            loop {
                let cur = add_months(start, step * interval);
                if cur > effective_end || generated >= max_limit {
                    break;
                }
                generated += 1;
                if cur >= range_start && !exdates.contains(&cur) {
                    out.push(cur);
                }
                step += 1;
            }
        }
        "YEARLY" => {
            let mut step = 0i32;
            loop {
                let cur = add_months(start, step * interval * 12);
                if cur > effective_end || generated >= max_limit {
                    break;
                }
                generated += 1;
                if cur >= range_start && !exdates.contains(&cur) {
                    out.push(cur);
                }
                step += 1;
            }
        }
        _ => {
            if start >= range_start && start <= range_end && !exdates.contains(&start) {
                out.push(start);
            }
        }
    }
    out
}

fn date_diff_days(a: Date, b: Date) -> i32 {
    // Simple exact day difference for reasonable ranges
    if a == b {
        return 0;
    }
    let mut cur = a;
    let mut diff = 0i32;
    if a < b {
        while cur < b && diff < 4000 {
            cur = cur.add_days(1);
            diff += 1;
        }
        diff
    } else {
        while cur > b && diff > -4000 {
            cur = cur.add_days(-1);
            diff -= 1;
        }
        diff
    }
}

pub fn parse_ics(ics: &str, range_start: Date, range_end: Date) -> BTreeMap<Date, Vec<CalendarEvent>> {
    let mut map: BTreeMap<Date, Vec<CalendarEvent>> = BTreeMap::new();
    let mut current: Option<RawEvent> = None;
    let mut subcomponent_depth: usize = 0;

    for line in unfold_lines(ics) {
        if line == "BEGIN:VEVENT" {
            current = Some(RawEvent::default());
            subcomponent_depth = 0;
            continue;
        }
        if line == "END:VEVENT" {
            if let Some(ev) = current.take() {
                insert_raw_event(&mut map, ev, range_start, range_end);
            }
            subcomponent_depth = 0;
            continue;
        }
        let Some(ev) = current.as_mut() else {
            continue;
        };
        if line.starts_with("BEGIN:") {
            subcomponent_depth += 1;
            continue;
        }
        if line.starts_with("END:") {
            subcomponent_depth = subcomponent_depth.saturating_sub(1);
            continue;
        }
        if subcomponent_depth > 0 {
            continue;
        }
        let Some((prop, val)) = line.split_once(':') else {
            continue;
        };
        let mut parts = prop.split(';');
        let name = parts.next().unwrap_or("");
        let mut is_date = false;
        let mut is_utc = false;
        for p in parts {
            if p.eq_ignore_ascii_case("VALUE=DATE") {
                is_date = true;
            } else if p.eq_ignore_ascii_case("TZID=UTC")
                || p.eq_ignore_ascii_case("TZID=Etc/UTC")
                || p.eq_ignore_ascii_case("TZID=GMT")
            {
                is_utc = true;
            }
        }
        match name {
            "SUMMARY" => {
                let s = unescape_ics_text(val).trim().to_string();
                if !s.is_empty() {
                    ev.summary = Some(s);
                }
            }
            "LOCATION" => {
                let s = unescape_ics_text(val).trim().to_string();
                if !s.is_empty() {
                    ev.location = Some(s);
                }
            }
            "STATUS" => ev.status = Some(val.trim().to_uppercase()),
            "DTSTART" => ev.dtstart = parse_ics_datetime(val, is_date, is_utc),
            "DTEND" => ev.dtend = parse_ics_datetime(val, is_date, is_utc),
            "DURATION" => ev.duration_minutes = parse_duration_minutes(val),
            "RRULE" => ev.rrule = Some(val.trim().to_string()),
            "EXDATE" => {
                for item in val.split(',') {
                    if let Some(dt) = parse_ics_datetime(item, is_date, is_utc) {
                        ev.exdates.insert(dt.date);
                    }
                }
            }
            _ => {}
        }
    }

    for list in map.values_mut() {
        list.sort_by(|a, b| {
            b.all_day
                .cmp(&a.all_day)
                .then_with(|| a.start_minutes.cmp(&b.start_minutes))
                .then_with(|| a.summary.cmp(&b.summary))
        });
    }
    map
}

fn insert_raw_event(
    map: &mut BTreeMap<Date, Vec<CalendarEvent>>,
    ev: RawEvent,
    range_start: Date,
    range_end: Date,
) {
    if ev.status.as_deref() == Some("CANCELLED") {
        return;
    }
    let Some(dtstart) = ev.dtstart else {
        return;
    };
    let summary = ev.summary.unwrap_or_else(|| "(No title)".into());

    // Compute span in days and end minutes
    let (span_days, end_mins) = if dtstart.all_day {
        let days = if let Some(dtend) = ev.dtend {
            date_diff_days(dtstart.date, dtend.date).max(1)
        } else if let Some(dur) = ev.duration_minutes {
            (dur / (24 * 60)).max(1)
        } else {
            1
        };
        (days, None)
    } else if let Some(dtend) = ev.dtend {
        let days = date_diff_days(dtstart.date, dtend.date).max(0);
        if days == 0 {
            (1, dtend.minutes)
        } else if dtend.minutes == Some(0) {
            (days, Some(24 * 60))
        } else {
            (days + 1, dtend.minutes)
        }
    } else if let Some(dur) = ev.duration_minutes {
        let start_m = dtstart.minutes.unwrap_or(0) as i32;
        let total = start_m + dur;
        let extra_days = total / (24 * 60);
        let rem = (total % (24 * 60)) as u16;
        if total > 0 && rem == 0 {
            (extra_days.max(1), Some(24 * 60))
        } else {
            ((extra_days + 1).max(1), Some(rem))
        }
    } else {
        (1, None)
    };

    let occurrences = if let Some(rrule) = &ev.rrule {
        expand_rrule(
            dtstart.date,
            rrule,
            range_start.add_days(-(span_days - 1).min(30)),
            range_end,
            &ev.exdates,
        )
    } else if dtstart.date <= range_end
        && dtstart.date.add_days(span_days - 1) >= range_start
        && !ev.exdates.contains(&dtstart.date)
    {
        vec![dtstart.date]
    } else {
        Vec::new()
    };

    for occ in occurrences {
        for offset in 0..span_days.min(31) {
            let d = occ.add_days(offset);
            if d < range_start || d > range_end {
                continue;
            }
            let (s_min, e_min) = if dtstart.all_day {
                (None, None)
            } else if span_days == 1 {
                (dtstart.minutes, end_mins)
            } else if offset == 0 {
                (dtstart.minutes, Some(24 * 60))
            } else if offset == span_days - 1 {
                (Some(0), end_mins)
            } else {
                (Some(0), Some(24 * 60))
            };
            map.entry(d).or_default().push(CalendarEvent {
                summary: summary.clone(),
                location: ev.location.clone(),
                all_day: dtstart.all_day,
                start_minutes: s_min,
                end_minutes: e_min,
                color: None,
                kind: EventKind::Event,
                html_link: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_normalization_handles_webcal_https_and_rejects_bad_inputs() {
        assert_eq!(
            normalize_url("  webcal://calendar.google.com/calendar/ical/abc/basic.ics  ").unwrap(),
            "https://calendar.google.com/calendar/ical/abc/basic.ics"
        );
        assert_eq!(
            normalize_url("HTTPS://calendar.google.com/calendar/ical/abc/basic.ics").unwrap(),
            "https://calendar.google.com/calendar/ical/abc/basic.ics"
        );
        assert!(normalize_url("").is_err());
        assert!(normalize_url("http://calendar.google.com/test.ics").is_err());
        assert!(normalize_url("https://calendar.google.com/bad url.ics").is_err());
    }

    #[test]
    fn parses_all_day_timed_folded_cancelled_and_recurring_events() {
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
DTSTART;VALUE=DATE:20261122\r\n\
DTEND;VALUE=DATE:20261124\r\n\
SUMMARY:Design Sprint\\, Day 1 & 2\r\n\
LOCATION:Hanoi\\; Office\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
DTSTART:20261122T090000\r\n\
DTEND:20261122T103000\r\n\
SUMMARY:Morning Stand\r\n\x20up\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
DTSTART:20261122T140000\r\n\
DTEND:20261122T150000\r\n\
STATUS:CANCELLED\r\n\
SUMMARY:Cancelled Sync\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
DTSTART:20261120T080000\r\n\
DURATION:PT45M\r\n\
RRULE:FREQ=DAILY;COUNT=4\r\n\
EXDATE:20261121T080000\r\n\
SUMMARY:Daily Focus\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

        let start = Date { year: 2026, month: 11, day: 1 };
        let end = Date { year: 2026, month: 11, day: 30 };
        let map = parse_ics(ics, start, end);

        let d22 = Date { year: 2026, month: 11, day: 22 };
        let ev22 = &map[&d22];
        assert_eq!(ev22.len(), 3);
        assert!(ev22[0].all_day);
        assert_eq!(ev22[0].summary, "Design Sprint, Day 1 & 2");
        assert_eq!(ev22[0].location.as_deref(), Some("Hanoi; Office"));
        assert_eq!(ev22[1].summary, "Daily Focus");
        assert_eq!(ev22[1].time_label(), "08:00 - 08:45");
        assert_eq!(ev22[2].summary, "Morning Standup");
        assert_eq!(ev22[2].time_label(), "09:00 - 10:30");

        let d23 = Date { year: 2026, month: 11, day: 23 };
        assert_eq!(map[&d23].len(), 2); // All-day Design Sprint + Daily Focus
        let d21 = Date { year: 2026, month: 11, day: 21 };
        assert!(!map.contains_key(&d21)); // Excluded by EXDATE
    }

    #[test]
    fn parses_weekly_byday_rrule() {
        let ics = "\
BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
DTSTART:20261102T100000\r\n\
DTEND:20261102T110000\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=4\r\n\
SUMMARY:Team Sync\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let start = Date { year: 2026, month: 11, day: 1 };
        let end = Date { year: 2026, month: 11, day: 30 };
        let map = parse_ics(ics, start, end);
        assert_eq!(map.len(), 4);
        assert!(map.contains_key(&Date { year: 2026, month: 11, day: 2 }));
        assert!(map.contains_key(&Date { year: 2026, month: 11, day: 4 }));
        assert!(map.contains_key(&Date { year: 2026, month: 11, day: 9 }));
        assert!(map.contains_key(&Date { year: 2026, month: 11, day: 11 }));
    }

    #[test]
    fn handles_empty_malformed_and_until_and_multiday_events() {
        let start = Date { year: 2026, month: 11, day: 1 };
        let end = Date { year: 2026, month: 11, day: 30 };
        assert!(parse_ics("", start, end).is_empty());
        assert!(parse_ics("NOT AN ICS FILE", start, end).is_empty());

        let ics = "\
BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
DTSTART:20261110T220000\r\n\
DTEND:20261111T020000\r\n\
SUMMARY:Overnight Deploy\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
DTSTART:20261101T090000\r\n\
DTEND:20261101T100000\r\n\
RRULE:FREQ=WEEKLY;UNTIL=20261115T235959Z\r\n\
SUMMARY:Weekly Checkin\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let map = parse_ics(ics, start, end);
        let d10 = Date { year: 2026, month: 11, day: 10 };
        let d11 = Date { year: 2026, month: 11, day: 11 };
        assert_eq!(map[&d10][0].summary, "Overnight Deploy");
        assert_eq!(map[&d10][0].time_label(), "22:00 - 24:00");
        assert_eq!(map[&d11][0].summary, "Overnight Deploy");
        assert_eq!(map[&d11][0].time_label(), "00:00 - 02:00");

        assert!(map.contains_key(&Date { year: 2026, month: 11, day: 1 }));
        assert!(map.contains_key(&Date { year: 2026, month: 11, day: 8 }));
        assert!(map.contains_key(&Date { year: 2026, month: 11, day: 15 }));
        assert!(!map.contains_key(&Date { year: 2026, month: 11, day: 22 }));
    }

    #[test]
    fn valarm_subcomponent_does_not_overwrite_event_and_midnight_duration_does_not_spill() {
        let ics = "\
BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
DTSTART:20261112T220000\r\n\
DURATION:PT2H\r\n\
SUMMARY:Late Work\r\n\
BEGIN:VALARM\r\n\
ACTION:DISPLAY\r\n\
TRIGGER:-PT10M\r\n\
DURATION:PT15M\r\n\
SUMMARY:Alarm Notification\r\n\
END:VALARM\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let start = Date { year: 2026, month: 11, day: 1 };
        let end = Date { year: 2026, month: 11, day: 30 };
        let map = parse_ics(ics, start, end);
        let d12 = Date { year: 2026, month: 11, day: 12 };
        let d13 = Date { year: 2026, month: 11, day: 13 };
        assert_eq!(map[&d12].len(), 1);
        assert_eq!(map[&d12][0].summary, "Late Work");
        assert_eq!(map[&d12][0].time_label(), "22:00 - 24:00");
        assert!(!map.contains_key(&d13));
    }
}
