//! Google OAuth 2.0 (Desktop PKCE loopback) and Google Calendar API v3 client.
use super::gcal::{CalendarEvent, CalendarListEntry, EventKind, MAX_ICS_BYTES};
use super::model::Date;
use crate::model::wide;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::{HWND, SYSTEMTIME},
    Networking::WinHttp::*,
    Storage::FileSystem::*,
    System::Time::SystemTimeToTzSpecificLocalTime,
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::{PostMessageW, SW_SHOWNORMAL}},
};

const OAUTH_TOKENS_FILE: &str = "config/native-calendar-oauth.json";
const OAUTH_CACHE_FILE: &str = "config/native-calendar-gcal-api.json";
const OAUTH_TASKS_CACHE_FILE: &str = "config/native-calendar-gcal-tasks.json";
const OAUTH_CALS_CACHE_FILE: &str = "config/native-calendar-gcal-cals.json";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub expires_at_unix: u64,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn has_tokens(root: &Path) -> bool {
    root.join(OAUTH_TOKENS_FILE).is_file()
}

pub fn load_tokens(root: &Path) -> Option<OAuthTokens> {
    let raw = fs::read_to_string(root.join(OAUTH_TOKENS_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_tokens(root: &Path, tokens: &OAuthTokens) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(tokens).map_err(|e| e.to_string())?;
    write_atomic(&root.join(OAUTH_TOKENS_FILE), &json)
}

pub fn clear_oauth_data(root: &Path) {
    let _ = fs::remove_file(root.join(OAUTH_TOKENS_FILE));
    let _ = fs::remove_file(root.join(OAUTH_CACHE_FILE));
    let _ = fs::remove_file(root.join(OAUTH_TASKS_CACHE_FILE));
    let _ = fs::remove_file(root.join(OAUTH_CALS_CACHE_FILE));
}

pub fn load_cached_events(
    root: &Path,
    disabled_calendars: &[String],
) -> Option<(BTreeMap<Date, Vec<CalendarEvent>>, Vec<CalendarListEntry>)> {
    let raw = fs::read_to_string(root.join(OAUTH_CACHE_FILE)).ok()?;
    let today = Date::today();
    let min_d = today.add_days(-180);
    let max_d = today.add_days(365);
    let cals = fs::read_to_string(root.join(OAUTH_CALS_CACHE_FILE))
        .ok()
        .map(|s| parse_calendar_list_json(&s))
        .unwrap_or_default();
    let mut map = parse_google_events_json_filtered(&raw, min_d, max_d, disabled_calendars);
    if let Ok(tasks_raw) = fs::read_to_string(root.join(OAUTH_TASKS_CACHE_FILE)) {
        merge_events(&mut map, parse_google_tasks_json(&tasks_raw, min_d, max_d));
    }
    Some((map, cals))
}

fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temp = dest.with_extension("json.tmp");
    fs::write(&temp, bytes).map_err(|e| e.to_string())?;
    unsafe {
        if MoveFileExW(
            wide(&temp).as_ptr(),
            wide(dest).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(())
}

/// Parses either Google Cloud Console JSON (`installed`/`web`), `id|secret`, or a bare `client_id`.
pub fn parse_oauth_credentials(raw: &str) -> Result<(String, Option<String>), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Client ID cannot be empty.".into());
    }
    if trimmed.starts_with('{') {
        let v: serde_json::Value =
            serde_json::from_str(trimmed).map_err(|e| format!("Invalid JSON: {e}"))?;
        let obj = v
            .get("installed")
            .or_else(|| v.get("web"))
            .unwrap_or(&v);
        let id = obj
            .get("client_id")
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "JSON does not contain client_id.".to_string())?
            .to_string();
        let secret = obj
            .get("client_secret")
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        return Ok((id, secret));
    }
    if let Some((id, secret)) = trimmed
        .split_once('|')
        .or_else(|| trimmed.split_once(','))
        .or_else(|| trimmed.split_once(char::is_whitespace))
    {
        let id = id.trim();
        let secret = secret.trim();
        if id.is_empty() {
            return Err("Client ID cannot be empty.".into());
        }
        return Ok((
            id.to_string(),
            (!secret.is_empty()).then(|| secret.to_string()),
        ));
    }
    Ok((trimmed.to_string(), None))
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4 + 2) / 3);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        }
    }
    out
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn random_token(seed_tag: &str, bytes_len: usize) -> String {
    let mut entropy = Vec::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    entropy.extend_from_slice(&nanos.to_le_bytes());
    entropy.extend_from_slice(&std::process::id().to_le_bytes());
    entropy.extend_from_slice(seed_tag.as_bytes());
    let mut raw = Vec::with_capacity(bytes_len);
    let mut counter = 0u32;
    while raw.len() < bytes_len {
        let mut hasher = Sha256::new();
        hasher.update(&entropy);
        hasher.update(counter.to_le_bytes());
        raw.extend_from_slice(&hasher.finalize());
        counter += 1;
    }
    raw.truncate(bytes_len);
    base64url(&raw)
}

pub fn pkce_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    base64url(&hash)
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

fn https_request(
    method: &str,
    host: &str,
    path_and_query: &str,
    headers: Option<&str>,
    body: Option<&[u8]>,
) -> Result<(u32, Vec<u8>), String> {
    unsafe {
        let session = handle(WinHttpOpen(
            wide("Lumen/0.1 GoogleOAuth").as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        ))?;
        WinHttpSetTimeouts(session.0, 10000, 10000, 15000, 30000);
        let connection = handle(WinHttpConnect(session.0, wide(host).as_ptr(), 443, 0))?;
        let request = handle(WinHttpOpenRequest(
            connection.0,
            wide(method).as_ptr(),
            wide(path_and_query).as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        let hdr_wide = headers.map(wide);
        let (hdr_ptr, hdr_len) = match &hdr_wide {
            Some(w) => (w.as_ptr(), (w.len() - 1) as u32),
            None => (null(), 0),
        };
        let (body_ptr, body_len) = match body {
            Some(b) => (b.as_ptr() as *const _, b.len() as u32),
            None => (null(), 0),
        };
        if WinHttpSendRequest(
            request.0,
            hdr_ptr,
            hdr_len,
            body_ptr,
            body_len,
            body_len,
            0,
        ) == 0
            || WinHttpReceiveResponse(request.0, null_mut()) == 0
        {
            return Err(format!(
                "HTTP request failed: {}",
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
        let mut out = Vec::new();
        loop {
            let mut avail = 0u32;
            if WinHttpQueryDataAvailable(request.0, &mut avail) == 0 {
                return Err("Failed reading HTTP response".into());
            }
            if avail == 0 {
                break;
            }
            if out.len() + avail as usize > MAX_ICS_BYTES {
                return Err("Response exceeded 16 MB limit.".into());
            }
            let chunk = avail.min(65536) as usize;
            let mut buf = vec![0u8; chunk];
            let mut read = 0u32;
            if WinHttpReadData(request.0, buf.as_mut_ptr() as _, chunk as u32, &mut read) == 0 {
                return Err("Failed reading HTTP data".into());
            }
            if read == 0 {
                break;
            }
            out.extend_from_slice(&buf[..read as usize]);
        }
        Ok((status, out))
    }
}

fn exchange_token(params: &[(&str, &str)]) -> Result<serde_json::Value, String> {
    let form: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let (status, bytes) = https_request(
        "POST",
        "oauth2.googleapis.com",
        "/token",
        Some("Content-Type: application/x-www-form-urlencoded\r\n"),
        Some(form.as_bytes()),
    )?;
    let json: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Invalid OAuth token response (HTTP {status}): {e}"))?;
    if status >= 400 {
        let err = json
            .get("error_description")
            .or_else(|| json.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("OAuth token request failed");
        return Err(format!("{err} (HTTP {status})"));
    }
    Ok(json)
}

pub fn start_oauth_login(
    client_id: String,
    client_secret: Option<String>,
    root: PathBuf,
    hwnd: HWND,
    notify_msg: u32,
    pending: Arc<Mutex<Option<Result<OAuthTokens, String>>>>,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("Cannot bind local OAuth callback port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Cannot configure OAuth listener: {e}"))?;

    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let verifier = random_token("pkce", 48);
    let challenge = pkce_challenge(&verifier);
    let state = random_token("state", 24);
    let scope = "https://www.googleapis.com/auth/calendar.readonly https://www.googleapis.com/auth/calendar.events https://www.googleapis.com/auth/tasks";
    let auth_url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={}&code_challenge_method=S256&access_type=offline&prompt=consent&state={}",
        url_encode(&client_id),
        url_encode(&redirect_uri),
        url_encode(scope),
        url_encode(&challenge),
        url_encode(&state),
    );

    unsafe {
        let res = ShellExecuteW(
            null_mut(),
            wide("open").as_ptr(),
            wide(&auth_url).as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        );
        if res as isize <= 32 {
            return Err("Could not open browser for Google sign-in.".into());
        }
    }

    let hwnd_addr = hwnd as usize;
    std::thread::spawn(move || {
        let outcome = run_oauth_loopback(
            listener,
            &client_id,
            client_secret.as_deref(),
            &redirect_uri,
            &verifier,
            &state,
            &root,
            &cancel,
        );
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        *pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        unsafe {
            PostMessageW(hwnd_addr as HWND, notify_msg, 0, 0);
        }
    });
    Ok(())
}

fn run_oauth_loopback(
    listener: TcpListener,
    client_id: &str,
    client_secret: Option<&str>,
    redirect_uri: &str,
    verifier: &str,
    expected_state: &str,
    root: &Path,
    cancel: &AtomicBool,
) -> Result<OAuthTokens, String> {
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut stream = loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("Google sign-in cancelled.".into());
        }
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err("Google sign-in timed out. Click Sign in with Google to try again.".into());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("OAuth callback error: {e}")),
        }
    };
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let target = first_line.split_whitespace().nth(1).unwrap_or("");
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");

    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "code" => code = Some(url_decode(v)),
                "state" => state = Some(url_decode(v)),
                "error" => error = Some(url_decode(v)),
                _ => {}
            }
        }
    }

    let ok_html = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><html><body style='font-family:system-ui;background:#111;color:#eee;display:grid;place-items:center;height:100vh;margin:0'><div style='text-align:center'><h2>Lumen Calendar Connected</h2><p>You can close this tab and return to Lumen.</p></div></body></html>";
    let err_html = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><html><body style='font-family:system-ui;background:#111;color:#eee;display:grid;place-items:center;height:100vh;margin:0'><div style='text-align:center'><h2>Sign-in Failed</h2><p>Please return to Lumen and try again.</p></div></body></html>";

    if let Some(err) = error {
        let _ = stream.write_all(err_html.as_bytes());
        return Err(format!("Google sign-in denied: {err}"));
    }
    if state.as_deref() != Some(expected_state) {
        let _ = stream.write_all(err_html.as_bytes());
        return Err("OAuth state mismatch.".into());
    }
    let Some(code) = code.filter(|c| !c.is_empty()) else {
        let _ = stream.write_all(err_html.as_bytes());
        return Err("Missing authorization code.".into());
    };
    let _ = stream.write_all(ok_html.as_bytes());

    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }
    let json = exchange_token(&params)?;
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing access_token in OAuth response.".to_string())?
        .to_string();
    let refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);
    let tokens = OAuthTokens {
        access_token,
        refresh_token,
        expires_at_unix: now_unix().saturating_add(expires_in),
    };
    save_tokens(root, &tokens)?;
    Ok(tokens)
}

fn ensure_valid_token(
    client_id: &str,
    client_secret: Option<&str>,
    root: &Path,
) -> Result<String, String> {
    let mut tokens =
        load_tokens(root).ok_or_else(|| "Not signed in to Google Calendar.".to_string())?;
    if now_unix().saturating_add(60) >= tokens.expires_at_unix {
        let refresh = tokens
            .refresh_token
            .as_deref()
            .ok_or_else(|| "Session expired. Please sign in with Google again.".to_string())?;
        let mut params = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", client_id),
        ];
        if let Some(secret) = client_secret {
            params.push(("client_secret", secret));
        }
        let json = exchange_token(&params)?;
        tokens.access_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing refreshed access_token.".to_string())?
            .to_string();
        if let Some(rt) = json.get("refresh_token").and_then(|v| v.as_str()) {
            tokens.refresh_token = Some(rt.to_string());
        }
        let expires_in = json
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);
        tokens.expires_at_unix = now_unix().saturating_add(expires_in);
        save_tokens(root, &tokens)?;
    }
    Ok(tokens.access_token)
}

fn parse_hex_color(s: &str) -> Option<u32> {
    let hex = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()? as u32;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()? as u32;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()? as u32;
    Some(r | (g << 8) | (b << 16))
}

pub fn parse_calendar_list_json(raw: &str) -> Vec<CalendarListEntry> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let Some(items) = json.get("items").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|cal| {
            let id = cal.get("id").and_then(|v| v.as_str())?.to_string();
            let summary = cal
                .get("summaryOverride")
                .or_else(|| cal.get("summary"))
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            let color = cal
                .get("backgroundColor")
                .and_then(|v| v.as_str())
                .and_then(parse_hex_color);
            let primary = cal.get("primary").and_then(|v| v.as_bool()).unwrap_or(false);
            Some(CalendarListEntry {
                id,
                summary,
                color,
                primary,
            })
        })
        .collect()
}

pub fn fetch_google_calendar_events(
    client_id: &str,
    client_secret: Option<&str>,
    disabled_calendars: &[String],
    root: &Path,
) -> Result<(BTreeMap<Date, Vec<CalendarEvent>>, Vec<CalendarListEntry>), String> {
    let access_token = ensure_valid_token(client_id, client_secret, root)?;
    let today = Date::today();
    let min_d = today.add_days(-180);
    let max_d = today.add_days(365);
    let time_min = format!("{:04}-{:02}-{:02}T00:00:00Z", min_d.year, min_d.month, min_d.day);
    let time_max = format!("{:04}-{:02}-{:02}T23:59:59Z", max_d.year, max_d.month, max_d.day);
    let auth_header = format!("Authorization: Bearer {access_token}\r\n");

    let mut cals = Vec::new();
    if let Ok((200, list_bytes)) = https_request(
        "GET",
        "www.googleapis.com",
        "/calendar/v3/users/me/calendarList?minAccessRole=reader",
        Some(&auth_header),
        None,
    ) {
        if let Ok(list_raw) = String::from_utf8(list_bytes) {
            let _ = write_atomic(&root.join(OAUTH_CALS_CACHE_FILE), list_raw.as_bytes());
            cals = parse_calendar_list_json(&list_raw);
        }
    }

    let mut all_items = Vec::new();
    if cals.is_empty() {
        let path = format!(
            "/calendar/v3/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults=2500&timeMin={}&timeMax={}",
            url_encode(&time_min),
            url_encode(&time_max),
        );
        let (status, bytes) =
            https_request("GET", "www.googleapis.com", &path, Some(&auth_header), None)?;
        if status >= 400 {
            let msg = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(str::to_string))
                .unwrap_or_else(|| format!("Google Calendar API HTTP {status}"));
            return Err(msg);
        }
        let raw = String::from_utf8(bytes).map_err(|_| "Calendar response is not UTF-8.".to_string())?;
        if let Some(items) = serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.get("items").and_then(|i| i.as_array()).cloned())
        {
            all_items.extend(items);
        }
    } else {
        for cal in &cals {
            let cal_path = format!(
                "/calendar/v3/calendars/{}/events?singleEvents=true&orderBy=startTime&maxResults=1500&timeMin={}&timeMax={}",
                url_encode(&cal.id),
                url_encode(&time_min),
                url_encode(&time_max),
            );
            if let Ok((200, cal_bytes)) =
                https_request("GET", "www.googleapis.com", &cal_path, Some(&auth_header), None)
            {
                if let Ok(mut cal_json) = serde_json::from_slice::<serde_json::Value>(&cal_bytes) {
                    if let Some(items) = cal_json.get_mut("items").and_then(|v| v.as_array_mut()) {
                        for item in items.iter_mut() {
                            if let Some(obj) = item.as_object_mut() {
                                obj.insert("_lumen_cal_id".into(), serde_json::json!(cal.id));
                                if let Some(c) = cal.color {
                                    obj.insert("_lumen_cal_color".into(), serde_json::json!(c));
                                }
                            }
                        }
                        all_items.extend(items.drain(..));
                    }
                }
            }
        }
    }

    let merged_raw = serde_json::json!({ "items": all_items }).to_string();
    let _ = write_atomic(&root.join(OAUTH_CACHE_FILE), merged_raw.as_bytes());
    let mut events = parse_google_events_json_filtered(&merged_raw, min_d, max_d, disabled_calendars);

    if let Some((tasks_raw, task_events)) = fetch_google_tasks(&auth_header, min_d, max_d) {
        let _ = write_atomic(&root.join(OAUTH_TASKS_CACHE_FILE), tasks_raw.as_bytes());
        merge_events(&mut events, task_events);
    }
    Ok((events, cals))
}

fn fetch_google_tasks(
    auth_header: &str,
    range_start: Date,
    range_end: Date,
) -> Option<(String, BTreeMap<Date, Vec<CalendarEvent>>)> {
    let (200, lists_bytes) = https_request(
        "GET",
        "tasks.googleapis.com",
        "/tasks/v1/users/@me/lists?maxResults=50",
        Some(auth_header),
        None,
    )
    .ok()?
    else {
        return None;
    };
    let lists_json: serde_json::Value = serde_json::from_slice(&lists_bytes).ok()?;
    let lists = lists_json.get("items").and_then(|v| v.as_array())?;
    let mut all_tasks = Vec::new();
    for list in lists {
        let Some(list_id) = list.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let path = format!(
            "/tasks/v1/lists/{}/tasks?showCompleted=true&showHidden=true&maxResults=100",
            url_encode(list_id)
        );
        if let Ok((200, task_bytes)) =
            https_request("GET", "tasks.googleapis.com", &path, Some(auth_header), None)
        {
            if let Ok(mut task_json) = serde_json::from_slice::<serde_json::Value>(&task_bytes) {
                if let Some(items) = task_json.get_mut("items").and_then(|v| v.as_array_mut()) {
                    for item in items.iter_mut() {
                        if let Some(obj) = item.as_object_mut() {
                            obj.insert("_lumen_list_id".into(), serde_json::json!(list_id));
                        }
                    }
                    all_tasks.extend(items.drain(..));
                }
            }
        }
    }
    let raw = serde_json::json!({ "items": all_tasks }).to_string();
    let map = parse_google_tasks_json(&raw, range_start, range_end);
    Some((raw, map))
}

pub fn toggle_google_task(
    client_id: &str,
    client_secret: Option<&str>,
    root: &Path,
    list_id: &str,
    task_id: &str,
    completed: bool,
) -> Result<(), String> {
    let access_token = ensure_valid_token(client_id, client_secret, root)?;
    let auth_header = format!("Authorization: Bearer {access_token}\r\nContent-Type: application/json\r\n");
    let path = format!(
        "/tasks/v1/lists/{}/tasks/{}",
        url_encode(list_id),
        url_encode(task_id)
    );
    let body = serde_json::json!({
        "status": if completed { "completed" } else { "needsAction" }
    })
    .to_string();
    let (status, bytes) = https_request(
        "PATCH",
        "tasks.googleapis.com",
        &path,
        Some(&auth_header),
        Some(body.as_bytes()),
    )?;
    if status >= 400 {
        let msg = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(str::to_string))
            .unwrap_or_else(|| format!("Google Tasks API HTTP {status}"));
        return Err(msg);
    }
    Ok(())
}

/// Parses `"14:30 Meeting"` as `(Some(870), "Meeting")`, or plain text as `(None, title)`.
pub fn parse_quick_add(raw: &str) -> (Option<u16>, String) {
    let trimmed = raw.trim();
    if let Some((first, rest)) = trimmed.split_once(char::is_whitespace) {
        if let Some((hh_s, mm_s)) = first.split_once(':') {
            if let (Ok(hh), Ok(mm)) = (hh_s.parse::<u16>(), mm_s.parse::<u16>()) {
                if hh < 24 && mm < 60 && !rest.trim().is_empty() {
                    return (Some(hh * 60 + mm), rest.trim().to_string());
                }
            }
        }
    }
    (None, trimmed.to_string())
}

pub fn create_google_task(
    client_id: &str,
    client_secret: Option<&str>,
    root: &Path,
    date: Date,
    title: &str,
) -> Result<(), String> {
    let access_token = ensure_valid_token(client_id, client_secret, root)?;
    let auth_header = format!("Authorization: Bearer {access_token}\r\nContent-Type: application/json\r\n");
    let due = format!("{:04}-{:02}-{:02}T00:00:00.000Z", date.year, date.month, date.day);
    let body = serde_json::json!({
        "title": title.trim(),
        "due": due,
        "status": "needsAction",
    })
    .to_string();
    let (status, bytes) = https_request(
        "POST",
        "tasks.googleapis.com",
        "/tasks/v1/lists/@default/tasks",
        Some(&auth_header),
        Some(body.as_bytes()),
    )?;
    if status >= 400 {
        let msg = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(str::to_string))
            .unwrap_or_else(|| format!("Google Tasks API HTTP {status}"));
        return Err(msg);
    }
    Ok(())
}

pub fn create_google_event(
    client_id: &str,
    client_secret: Option<&str>,
    root: &Path,
    date: Date,
    start_minutes: u16,
    title: &str,
) -> Result<(), String> {
    let access_token = ensure_valid_token(client_id, client_secret, root)?;
    let auth_header = format!("Authorization: Bearer {access_token}\r\nContent-Type: application/json\r\n");
    let end_minutes = (start_minutes + 60).min(23 * 60 + 59);
    // Use local timezone offset from Windows TIME_ZONE_INFORMATION.
    let offset_str = local_tz_offset_string();
    let start_dt = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:00{offset_str}",
        date.year,
        date.month,
        date.day,
        start_minutes / 60,
        start_minutes % 60,
    );
    let end_dt = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:00{offset_str}",
        date.year,
        date.month,
        date.day,
        end_minutes / 60,
        end_minutes % 60,
    );
    let body = serde_json::json!({
        "summary": title.trim(),
        "start": { "dateTime": start_dt },
        "end": { "dateTime": end_dt },
    })
    .to_string();
    let (status, bytes) = https_request(
        "POST",
        "www.googleapis.com",
        "/calendar/v3/calendars/primary/events",
        Some(&auth_header),
        Some(body.as_bytes()),
    )?;
    if status >= 400 {
        let msg = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(str::to_string))
            .unwrap_or_else(|| format!("Google Calendar API HTTP {status}"));
        return Err(msg);
    }
    Ok(())
}

fn local_tz_offset_string() -> String {
    unsafe {
        let mut tzi: windows_sys::Win32::System::Time::TIME_ZONE_INFORMATION = std::mem::zeroed();
        let mode = windows_sys::Win32::System::Time::GetTimeZoneInformation(&mut tzi);
        let bias = tzi.Bias
            + if mode == 2 {
                tzi.DaylightBias
            } else if mode == 1 {
                tzi.StandardBias
            } else {
                0
            };
        let total = -bias;
        let sign = if total >= 0 { '+' } else { '-' };
        let abs = total.abs();
        format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
    }
}

fn merge_events(
    target: &mut BTreeMap<Date, Vec<CalendarEvent>>,
    extra: BTreeMap<Date, Vec<CalendarEvent>>,
) {
    for (date, items) in extra {
        let entry = target.entry(date).or_default();
        entry.extend(items);
        entry.sort_by(|a, b| {
            (!a.all_day)
                .cmp(&(!b.all_day))
                .then(a.start_minutes.cmp(&b.start_minutes))
                .then(a.summary.cmp(&b.summary))
        });
    }
}

pub fn parse_google_tasks_json(
    raw: &str,
    range_start: Date,
    range_end: Date,
) -> BTreeMap<Date, Vec<CalendarEvent>> {
    let mut map: BTreeMap<Date, Vec<CalendarEvent>> = BTreeMap::new();
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) else {
        return map;
    };
    let Some(items) = json.get("items").and_then(|v| v.as_array()) else {
        return map;
    };
    for item in items {
        if item.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false)
            || item.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false)
        {
            continue;
        }
        let Some(due_str) = item.get("due").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(date) = due_str.get(0..10).and_then(parse_ymd) else {
            continue;
        };
        if date < range_start || date > range_end {
            continue;
        }
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("(Untitled task)");
        let completed = item.get("status").and_then(|v| v.as_str()) == Some("completed");
        let summary = if completed {
            format!("✓ {title}")
        } else {
            format!("☐ {title}")
        };
        let notes = item
            .get("notes")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let list_id = item
            .get("_lumen_list_id")
            .and_then(|v| v.as_str())
            .unwrap_or("@default")
            .to_string();
        let task_id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        map.entry(date).or_default().push(CalendarEvent {
            summary,
            location: notes,
            all_day: true,
            start_minutes: None,
            end_minutes: None,
            color: None,
            kind: EventKind::Task {
                list_id,
                task_id,
                completed,
            },
            html_link: None,
        });
    }
    map
}

fn parse_ymd(s: &str) -> Option<Date> {
    let mut parts = s.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: i32 = parts.next()?.parse().ok()?;
    let day: i32 = parts.next()?.parse().ok()?;
    if !(1601..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    let max_day = Date { year, month, day: 1 }.days_in_month();
    if !(1..=max_day).contains(&day) {
        return None;
    }
    Some(Date { year, month, day })
}

fn parse_rfc3339_local(s: &str) -> Option<(Date, u16)> {
    let (date_part, time_rest) = s.split_once('T')?;
    let date = parse_ymd(date_part)?;
    if time_rest.len() < 5 {
        return None;
    }
    let hh: i32 = time_rest.get(0..2)?.parse().ok()?;
    let mm: i32 = time_rest.get(3..5)?.parse().ok()?;
    if !(0..=23).contains(&hh) || !(0..=59).contains(&mm) {
        return None;
    }
    let after_hm = &time_rest[5..];
    let ss: i32 = after_hm
        .strip_prefix(':')
        .and_then(|r| r.get(0..2))
        .and_then(|d| d.parse().ok())
        .unwrap_or(0)
        .clamp(0, 59);
    // Find timezone designator: 'Z' or '+'/'-' after time digits.
    let tz = after_hm.find(['Z', 'z', '+', '-']).map(|idx| &after_hm[idx..]);
    let mut utc_mins = hh * 60 + mm;
    let mut utc_date = date;
    match tz {
        Some("Z") | Some("z") => {}
        Some(off) if off.len() >= 6 && (off.starts_with('+') || off.starts_with('-')) => {
            let sign = if off.starts_with('+') { 1 } else { -1 };
            let oh: i32 = off.get(1..3)?.parse().ok()?;
            let om: i32 = off.get(4..6)?.parse().ok()?;
            utc_mins -= sign * (oh * 60 + om);
            while utc_mins < 0 {
                utc_mins += 1440;
                utc_date = utc_date.add_days(-1);
            }
            while utc_mins >= 1440 {
                utc_mins -= 1440;
                utc_date = utc_date.add_days(1);
            }
        }
        _ => return Some((date, (hh * 60 + mm) as u16)),
    }
    unsafe {
        let mut st = SYSTEMTIME {
            wYear: utc_date.year as u16,
            wMonth: utc_date.month as u16,
            wDayOfWeek: 0,
            wDay: utc_date.day as u16,
            wHour: (utc_mins / 60) as u16,
            wMinute: (utc_mins % 60) as u16,
            wSecond: ss as u16,
            wMilliseconds: 0,
        };
        let mut local: SYSTEMTIME = std::mem::zeroed();
        if SystemTimeToTzSpecificLocalTime(null(), &mut st, &mut local) != 0 {
            return Some((
                Date {
                    year: local.wYear as i32,
                    month: local.wMonth as i32,
                    day: local.wDay as i32,
                },
                local.wHour * 60 + local.wMinute,
            ));
        }
    }
    Some((utc_date, utc_mins as u16))
}

pub fn parse_google_events_json(
    raw: &str,
    range_start: Date,
    range_end: Date,
) -> BTreeMap<Date, Vec<CalendarEvent>> {
    parse_google_events_json_filtered(raw, range_start, range_end, &[])
}

pub fn parse_google_events_json_filtered(
    raw: &str,
    range_start: Date,
    range_end: Date,
    disabled_calendars: &[String],
) -> BTreeMap<Date, Vec<CalendarEvent>> {
    let mut map: BTreeMap<Date, Vec<CalendarEvent>> = BTreeMap::new();
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) else {
        return map;
    };
    let Some(items) = json.get("items").and_then(|v| v.as_array()) else {
        return map;
    };
    for item in items {
        if item.get("status").and_then(|v| v.as_str()) == Some("cancelled") {
            continue;
        }
        if let Some(cal_id) = item.get("_lumen_cal_id").and_then(|v| v.as_str()) {
            if disabled_calendars.iter().any(|d| d == cal_id) {
                continue;
            }
        }
        let color = item
            .get("_lumen_cal_color")
            .and_then(|v| v.as_u64())
            .map(|c| c as u32);
        let summary = item
            .get("summary")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("(No title)")
            .to_string();
        let location = item
            .get("location")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let html_link = item
            .get("hangoutLink")
            .and_then(|v| v.as_str())
            .filter(|s| s.starts_with("https://"))
            .map(str::to_string)
            .or_else(|| {
                location
                    .as_deref()
                    .and_then(|loc| loc.split_whitespace().find(|w| w.starts_with("https://")))
                    .map(str::to_string)
            });

        let Some(start) = item.get("start") else {
            continue;
        };
        let end = item.get("end");

        if let Some(start_date_str) = start.get("date").and_then(|v| v.as_str()) {
            let Some(sd) = parse_ymd(start_date_str) else {
                continue;
            };
            let ed = end
                .and_then(|e| e.get("date"))
                .and_then(|v| v.as_str())
                .and_then(parse_ymd)
                .unwrap_or_else(|| sd.add_days(1));
            let last = if ed > sd { ed.add_days(-1) } else { sd };
            let mut d = sd.max(range_start);
            let limit = last.min(range_end);
            let mut guard = 0;
            while d <= limit && guard < 400 {
                map.entry(d).or_default().push(CalendarEvent {
                    summary: summary.clone(),
                    location: location.clone(),
                    all_day: true,
                    start_minutes: None,
                    end_minutes: None,
                    color,
                    kind: EventKind::Event,
                    html_link: html_link.clone(),
                });
                d = d.add_days(1);
                guard += 1;
            }
        } else if let Some(start_dt_str) = start.get("dateTime").and_then(|v| v.as_str()) {
            let Some((sd, sm)) = parse_rfc3339_local(start_dt_str) else {
                continue;
            };
            let (ed, em) = end
                .and_then(|e| e.get("dateTime"))
                .and_then(|v| v.as_str())
                .and_then(parse_rfc3339_local)
                .unwrap_or((sd, sm));
            let last_day = if ed > sd && em == 0 {
                ed.add_days(-1)
            } else if ed >= sd {
                ed
            } else {
                sd
            };
            let mut d = sd.max(range_start);
            let limit = last_day.min(range_end);
            let mut guard = 0;
            while d <= limit && guard < 400 {
                let day_start = if d == sd { sm } else { 0 };
                let day_end = if d == last_day {
                    if em == 0 && ed > sd { 1440 } else { em }
                } else {
                    1440
                };
                map.entry(d).or_default().push(CalendarEvent {
                    summary: summary.clone(),
                    location: location.clone(),
                    all_day: false,
                    start_minutes: Some(day_start),
                    end_minutes: Some(day_end),
                    color,
                    kind: EventKind::Event,
                    html_link: html_link.clone(),
                });
                d = d.add_days(1);
                guard += 1;
            }
        }
    }
    for events in map.values_mut() {
        events.sort_by(|a, b| {
            (!a.all_day)
                .cmp(&(!b.all_day))
                .then(a.start_minutes.cmp(&b.start_minutes))
                .then(a.summary.cmp(&b.summary))
        });
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_oauth_credentials_accepts_google_json_pair_and_bare_id() {
        let json = r#"{"installed":{"client_id":"cid-123.apps.googleusercontent.com","client_secret":"sec-456"}}"#;
        assert_eq!(
            parse_oauth_credentials(json).unwrap(),
            ("cid-123.apps.googleusercontent.com".into(), Some("sec-456".into()))
        );
        assert_eq!(
            parse_oauth_credentials("cid-123|sec-456").unwrap(),
            ("cid-123".into(), Some("sec-456".into()))
        );
        assert_eq!(
            parse_oauth_credentials("cid-123").unwrap(),
            ("cid-123".into(), None)
        );
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn parse_google_events_json_parses_all_day_and_timed_events() {
        let raw = r#"{
          "items": [
            {
              "status": "confirmed",
              "summary": "Company Holiday",
              "start": { "date": "2026-11-22" },
              "end": { "date": "2026-11-23" }
            },
            {
              "status": "confirmed",
              "summary": "Standup",
              "location": "Meet",
              "start": { "dateTime": "2026-11-22T09:30:00" },
              "end": { "dateTime": "2026-11-22T10:00:00" }
            },
            {
              "status": "cancelled",
              "summary": "Cancelled Event",
              "start": { "date": "2026-11-22" },
              "end": { "date": "2026-11-23" }
            }
          ]
        }"#;
        let d = Date { year: 2026, month: 11, day: 22 };
        let map = parse_google_events_json(raw, d.add_days(-1), d.add_days(1));
        let events = map.get(&d).expect("events on Nov 22");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].summary, "Company Holiday");
        assert!(events[0].all_day);
        assert_eq!(events[1].summary, "Standup");
        assert_eq!(events[1].location.as_deref(), Some("Meet"));
        assert_eq!(events[1].time_label(), "09:30 - 10:00");
    }

    #[test]
    fn parse_google_tasks_json_parses_due_dates_and_status() {
        let raw = r#"{
          "items": [
            {
              "title": "Submit report",
              "due": "2026-11-26T00:00:00.000Z",
              "status": "needsAction",
              "notes": "Q4 deck"
            },
            {
              "title": "Pay bill",
              "due": "2026-11-26T00:00:00.000Z",
              "status": "completed"
            },
            {
              "title": "Deleted task",
              "due": "2026-11-26T00:00:00.000Z",
              "deleted": true
            }
          ]
        }"#;
        let d = Date { year: 2026, month: 11, day: 26 };
        let map = parse_google_tasks_json(raw, d.add_days(-1), d.add_days(1));
        let tasks = map.get(&d).expect("tasks on Nov 26");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].summary, "☐ Submit report");
        assert_eq!(tasks[0].location.as_deref(), Some("Q4 deck"));
        assert_eq!(tasks[1].summary, "✓ Pay bill");
    }

    #[test]
    fn parse_quick_add_and_calendar_list_work() {
        assert_eq!(parse_quick_add("14:30 Standup"), (Some(14 * 60 + 30), "Standup".into()));
        assert_eq!(parse_quick_add("Buy milk"), (None, "Buy milk".into()));
        let cal_json = r##"{"items":[{"id":"primary@gmail.com","summary":"Personal","backgroundColor":"#0b8043","primary":true}]}"##;
        let cals = parse_calendar_list_json(cal_json);
        assert_eq!(cals.len(), 1);
        assert_eq!(cals[0].summary, "Personal");
        assert!(cals[0].primary);
        assert!(cals[0].color.is_some());
    }
}
