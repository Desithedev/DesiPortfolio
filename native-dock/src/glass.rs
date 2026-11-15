//! Frosted wallpaper, rendered from the wallpaper FILE, never from screen pixels.
use crate::model::wide;
use image::{imageops, DynamicImage, ImageDecoder, ImageReader};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    mem::{size_of, zeroed},
    path::{Path, PathBuf},
    ptr::null_mut,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    time::SystemTime,
};
use windows_sys::Win32::{
    Foundation::*, Graphics::Gdi::*, System::Registry::*, System::Threading::*,
    UI::WindowsAndMessaging::*,
};

fn vertical_offset(viewport: f32, drawn: f32, style: u32, tile: bool) -> f32 {
    if tile {
        0.
    } else if (style == 10 || style == 22) && drawn > viewport {
        // Windows Fill/Span vertically crops 1/3 from the top and 2/3 from the bottom.
        (viewport - drawn) / 3.
    } else {
        (viewport - drawn) / 2.
    }
}

fn decode_wallpaper(path: &Path) -> Option<DynamicImage> {
    let mut reader = ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut source = DynamicImage::from_decoder(decoder).ok()?;
    source.apply_orientation(orientation);
    Some(source)
}

struct OwnedBitmap(HBITMAP);
impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                DeleteObject(self.0);
            }
        }
    }
}

struct WallpaperSource {
    path: PathBuf,
    small: RefCell<Option<image::RgbaImage>>,
    preview: image::RgbaImage,
    sharp: Cell<HBITMAP>,
    width: i32,
    height: i32,
    natural: [f32; 2],
    accent: u32,
}
impl Drop for WallpaperSource {
    fn drop(&mut self) {
        unsafe {
            let sharp = self.sharp.get();
            if !sharp.is_null() {
                DeleteObject(sharp);
            }
        }
    }
}
impl WallpaperSource {
    fn make_preview(small: &image::RgbaImage) -> image::RgbaImage {
        if small.width() <= 1920 && small.height() <= 1920 {
            small.clone()
        } else {
            imageops::thumbnail(small, 1920, 1920)
        }
    }
    fn release_large(&self) {
        if self.width > 1280 {
            self.small.borrow_mut().take();
        }
    }
    unsafe fn sharp_bitmap(&self) -> HBITMAP {
        let existing = self.sharp.get();
        if !existing.is_null() {
            return existing;
        }
        let guard = self.small.borrow_mut();
        let temp;
        let rgba = if let Some(small) = guard.as_ref() {
            small
        } else if let Some((decoded, _)) = Glass::decode_small(&self.path, Glass::wallpaper_cap()) {
            temp = decoded;
            &temp
        } else {
            return null_mut();
        };
        let mut sharp_bytes = rgba.as_raw().clone();
        drop(guard);
        for p in sharp_bytes.chunks_exact_mut(4) {
            p.swap(0, 2);
            p[3] = 255;
        }
        if let Some(bitmap) = dib(self.width, self.height, &sharp_bytes) {
            self.sharp.set(bitmap);
            bitmap
        } else {
            null_mut()
        }
    }
}

type SourceKey = (PathBuf, Option<SystemTime>);
type BlurKey = (PathBuf, Option<SystemTime>, i32, i32);

struct PendingWallpaper {
    path: PathBuf,
    modified: Option<SystemTime>,
    style: u32,
    tile: bool,
    blur: f32,
    dpi: f32,
    small: image::RgbaImage,
    natural: [f32; 2],
    accent: u32,
    blurred_bytes: Vec<u8>,
    blur_w: i32,
    blur_h: i32,
}

static PENDING_WALLPAPER: Mutex<Option<PendingWallpaper>> = Mutex::new(None);
static LOADING_KEY: Mutex<Option<SourceKey>> = Mutex::new(None);

thread_local! {
    static WALLPAPER_SOURCES: RefCell<HashMap<SourceKey, Rc<WallpaperSource>>> = RefCell::new(HashMap::new());
    static BLURRED_CACHE: RefCell<HashMap<BlurKey, (Rc<OwnedBitmap>, i32, i32)>> = RefCell::new(HashMap::new());
    static PATH_BUF: RefCell<Vec<u16>> = RefCell::new(vec![0u16; 32768]);
}
pub fn clear_cache() {
    WALLPAPER_SOURCES.with(|cache| cache.borrow_mut().clear());
    BLURRED_CACHE.with(|cache| cache.borrow_mut().clear());
}

pub(crate) fn trim_working_set() {
    unsafe {
        windows_sys::Win32::System::Memory::SetProcessWorkingSetSizeEx(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            usize::MAX,
            usize::MAX,
            0,
        );
    }
}

static LIQUID_GLASS: AtomicBool = AtomicBool::new(false);
static FROSTED_GLASS: AtomicBool = AtomicBool::new(true);
static GLOBAL_BLUR_TENTHS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(130);
static LIQUID_GLASS_REVISION: AtomicU64 = AtomicU64::new(0);

pub fn set_liquid_glass(enabled: bool) {
    if LIQUID_GLASS.swap(enabled, Ordering::Relaxed) != enabled {
        LIQUID_GLASS_REVISION.fetch_add(1, Ordering::Relaxed);
    }
}
pub fn liquid_glass() -> bool {
    LIQUID_GLASS.load(Ordering::Relaxed)
}
pub fn set_frosted_glass(enabled: bool) {
    if FROSTED_GLASS.swap(enabled, Ordering::Relaxed) != enabled {
        LIQUID_GLASS_REVISION.fetch_add(1, Ordering::Relaxed);
    }
}
pub fn frosted_glass() -> bool {
    FROSTED_GLASS.load(Ordering::Relaxed)
}
pub fn set_global_blur(blur: f32) {
    let tenths = (blur.clamp(0.0, 80.0) * 10.0).round() as i32;
    if GLOBAL_BLUR_TENTHS.swap(tenths, Ordering::Relaxed) != tenths {
        LIQUID_GLASS_REVISION.fetch_add(1, Ordering::Relaxed);
    }
}
pub fn global_blur() -> f32 {
    GLOBAL_BLUR_TENTHS.load(Ordering::Relaxed) as f32 / 10.0
}

pub fn move_paint_due(hwnd: HWND) -> bool {
    paint_due(hwnd, true)
}

pub fn resize_paint_due(hwnd: HWND) -> bool {
    paint_due(hwnd, false)
}

thread_local! {
    static RESIZING_WINDOWS: RefCell<std::collections::HashSet<isize>> =
        RefCell::new(std::collections::HashSet::new());
}

pub fn set_resizing(hwnd: HWND, resizing: bool) {
    RESIZING_WINDOWS.with(|set| {
        let mut set = set.borrow_mut();
        if resizing {
            set.insert(hwnd as isize);
        } else {
            set.remove(&(hwnd as isize));
        }
    });
}

pub fn is_resizing(hwnd: HWND) -> bool {
    RESIZING_WINDOWS.with(|set| set.borrow().contains(&(hwnd as isize)))
}

fn paint_due(hwnd: HWND, is_move: bool) -> bool {
    use std::collections::HashMap;
    use std::time::Instant;
    thread_local! {
        static LAST_PAINT: RefCell<HashMap<(isize, bool), Instant>> = RefCell::new(HashMap::new());
    }
    let min_ms = if crate::motion::low_power() { 16 } else { 6 };
    let now = Instant::now();
    LAST_PAINT.with(|map| {
        let mut map = map.borrow_mut();
        let key = (hwnd as isize, is_move);
        if map
            .get(&key)
            .is_some_and(|prev| now.duration_since(*prev).as_millis() < min_ms)
        {
            return false;
        }
        map.insert(key, now);
        true
    })
}

pub struct Glass {
    blend_buffer: RefCell<Option<(HWND, crate::compositor::Buffer)>>,
    sharp_buffer: RefCell<Option<(HWND, crate::compositor::Buffer)>>,
    liquid_src: RefCell<Vec<u8>>,
    generation: u64,
    accent: Cell<u32>,
    bitmap: Option<Rc<OwnedBitmap>>,
    source: Option<Rc<WallpaperSource>>,
    blur_width: i32,
    blur_height: i32,
    width: i32,
    height: i32,
    natural: [f32; 2],
    path: PathBuf,
    modified: Option<SystemTime>,
    style: u32,
    tile: bool,
    low_power: bool,
    blur: f32,
    dpi: f32,
}
impl Glass {
    pub fn new(blur: f32, dpi: f32) -> Self {
        Self {
            blend_buffer: RefCell::new(None),
            sharp_buffer: RefCell::new(None),
            liquid_src: RefCell::new(Vec::new()),
            generation: 0,
            accent: Cell::new(crate::accent::FALLBACK),
            bitmap: None,
            source: None,
            blur_width: 0,
            blur_height: 0,
            width: 0,
            height: 0,
            natural: [0.0; 2],
            path: PathBuf::new(),
            modified: None,
            style: 10,
            tile: false,
            low_power: crate::motion::low_power(),
            blur,
            dpi,
        }
    }
    pub fn wallpaper_accent(&self) -> u32 {
        crate::accent::vivid(self.accent.get())
    }
    pub fn available(&self) -> bool {
        self.bitmap.is_some()
    }
    pub fn dpi(&self) -> f32 {
        self.dpi
    }
    pub fn blur(&self) -> f32 {
        self.blur
    }
    pub fn revision(&self) -> u64 {
        self.generation
            .wrapping_add(LIQUID_GLASS_REVISION.load(Ordering::Relaxed))
    }
    /// Test-only file source. Does not read or change the user's desktop setup.
    #[cfg(test)]
    pub(crate) unsafe fn fixture_file(path: &Path, blur: f32, dpi: f32) -> Self {
        let mut glass = Self::new(blur, dpi);
        glass.path = path.into();
        glass.style = 2;
        let (bitmap, blur_w, blur_h, source) = glass.load().expect("fixture wallpaper");
        glass.bitmap = Some(bitmap);
        glass.blur_width = blur_w;
        glass.blur_height = blur_h;
        glass.width = source.width;
        glass.height = source.height;
        glass.natural = source.natural;
        glass.accent.set(source.accent);
        glass.source = Some(source);
        glass.generation = 1;
        glass
    }
    pub unsafe fn set_blur(&mut self, blur: f32, dpi: f32) {
        if (self.blur - blur).abs() < 0.01 && (self.dpi - dpi).abs() < 0.01 {
            return;
        }
        self.blur = blur;
        self.dpi = dpi;
        if let Some(source) = &self.source {
            if let Some((bitmap, bw, bh)) =
                Self::get_or_build_blurred(&self.path, self.modified, source, blur, dpi)
            {
                self.bitmap = Some(bitmap);
                self.blur_width = bw;
                self.blur_height = bh;
                self.generation += 1;
                return;
            }
        }
        self.modified = None;
        self.refresh();
    }
    unsafe fn query_wallpaper() -> Option<(PathBuf, Option<SystemTime>, u32, bool)> {
        if let Some(live_path) = resolve_live_wallpaper_file() {
            let modified = std::fs::metadata(&live_path).and_then(|m| m.modified()).ok();
            return Some((live_path, modified, 10, false));
        }
        let spi_path = PATH_BUF.with(|buf| {
            let mut name = buf.borrow_mut();
            if SystemParametersInfoW(
                SPI_GETDESKWALLPAPER,
                name.len() as u32,
                name.as_mut_ptr() as _,
                0,
            ) == 0
            {
                return None;
            }
            let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
            Some(PathBuf::from(String::from_utf16_lossy(&name[..len])))
        })?;
        let path = resolve_wallpaper_path(spi_path);
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let style = registry_number("WallpaperStyle", 10);
        let tile = registry_number("TileWallpaper", 0) != 0;
        Some((path, modified, style, tile))
    }
    unsafe fn wallpaper_cap() -> Option<(u32, u32)> {
        if crate::motion::low_power() {
            return Some((640, 640));
        }
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(GetSystemMetrics(SM_CXSCREEN));
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(GetSystemMetrics(SM_CYSCREEN));
        if w > 0 && h > 0 {
            Some(((w as u32).clamp(1920, 3840), (h as u32).clamp(1080, 2160)))
        } else {
            None
        }
    }
    fn decode_small(path: &Path, cap: Option<(u32, u32)>) -> Option<(image::RgbaImage, [f32; 2])> {
        let decoded = decode_wallpaper(path)?;
        let natural = [decoded.width() as f32, decoded.height() as f32];
        let small = match cap {
            Some((cw, ch)) if decoded.width() > cw || decoded.height() > ch => {
                decoded.thumbnail(cw, ch).to_rgba8()
            }
            _ => decoded.to_rgba8(),
        };
        Some((small, natural))
    }
    /// Non-blocking startup refresh: decodes and blurs the wallpaper on a background
    /// thread and posts `message` to `notify` when ready.
    pub unsafe fn refresh_async(&mut self, notify: HWND, message: u32) -> bool {
        let Some((path, modified, style, tile)) = Self::query_wallpaper() else {
            return false;
        };
        if self.bitmap.is_some()
            && path == self.path
            && modified == self.modified
            && style == self.style
            && tile == self.tile
            && self.low_power == crate::motion::low_power()
        {
            return false;
        }
        let key: SourceKey = (path.clone(), modified);
        if WALLPAPER_SOURCES.with(|c| c.borrow().contains_key(&key)) {
            return self.refresh();
        }
        {
            let mut loading = LOADING_KEY.lock().unwrap_or_else(|e| e.into_inner());
            if loading.as_ref() == Some(&key) {
                return false;
            }
            *loading = Some(key);
        }
        self.path = path.clone();
        self.modified = modified;
        self.style = style;
        self.tile = tile;
        self.low_power = crate::motion::low_power();
        let blur = self.blur;
        let dpi = self.dpi;
        let cap = Self::wallpaper_cap();
        let notify_addr = notify as usize;
        std::thread::spawn(move || {
            let loaded = (|| {
                let (small, natural) = Self::decode_small(&path, cap)?;
                let palette = imageops::thumbnail(&small, 128, 128);
                let accent = crate::accent::dominant(palette.as_raw());
                let (blurred_bytes, blur_w, blur_h) =
                    Self::prepare_blurred_bytes(&small, blur, dpi, natural[0]);
                Some(PendingWallpaper {
                    path,
                    modified,
                    style,
                    tile,
                    blur,
                    dpi,
                    small,
                    natural,
                    accent,
                    blurred_bytes,
                    blur_w,
                    blur_h,
                })
            })();
            *LOADING_KEY.lock().unwrap_or_else(|e| e.into_inner()) = None;
            if let Some(pending) = loaded {
                *PENDING_WALLPAPER.lock().unwrap_or_else(|e| e.into_inner()) = Some(pending);
                PostMessageW(notify_addr as HWND, message, 0, 0);
            }
        });
        false
    }
    pub unsafe fn finish_async(&mut self) -> bool {
        let Some(pending) = PENDING_WALLPAPER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return false;
        };
        let source_key: SourceKey = (pending.path.clone(), pending.modified);
        let w = pending.small.width() as i32;
        let h = pending.small.height() as i32;
        let preview = WallpaperSource::make_preview(&pending.small);
        let source = Rc::new(WallpaperSource {
            path: pending.path.clone(),
            small: RefCell::new(Some(pending.small)),
            preview,
            sharp: Cell::new(null_mut()),
            width: w,
            height: h,
            natural: pending.natural,
            accent: pending.accent,
        });
        WALLPAPER_SOURCES.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= 2 {
                cache.clear();
            }
            cache.insert(source_key, source.clone());
        });
        let Some(bitmap) = dib(pending.blur_w, pending.blur_h, &pending.blurred_bytes) else {
            return false;
        };
        let owned = Rc::new(OwnedBitmap(bitmap));
        let blur_key: BlurKey = (
            pending.path.clone(),
            pending.modified,
            (pending.blur * 100.).round() as i32,
            (pending.dpi * 100.).round() as i32,
        );
        BLURRED_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= 4 {
                cache.clear();
            }
            cache.insert(blur_key, (owned.clone(), pending.blur_w, pending.blur_h));
        });
        let (final_bitmap, bw, bh) = if (self.blur - pending.blur).abs() < 0.01
            && (self.dpi - pending.dpi).abs() < 0.01
        {
            (owned, pending.blur_w, pending.blur_h)
        } else if let Some(built) = Self::get_or_build_blurred(
            &pending.path,
            pending.modified,
            &source,
            self.blur,
            self.dpi,
        ) {
            built
        } else {
            (owned, pending.blur_w, pending.blur_h)
        };
        self.path = pending.path;
        self.modified = pending.modified;
        self.style = pending.style;
        self.tile = pending.tile;
        self.low_power = crate::motion::low_power();
        self.bitmap = Some(final_bitmap);
        self.blur_width = bw;
        self.blur_height = bh;
        self.width = source.width;
        self.height = source.height;
        self.natural = source.natural;
        self.accent.set(source.accent);
        source.release_large();
        if source.width > 1280 {
            windows_sys::Win32::System::Memory::SetProcessWorkingSetSizeEx(
                windows_sys::Win32::System::Threading::GetCurrentProcess(),
                usize::MAX,
                usize::MAX,
                0,
            );
        }
        self.source = Some(source);
        self.generation += 1;
        true
    }
    /// Windows writes into our owned UTF-16 buffer. We only read the returned
    /// wallpaper file and two desktop appearance values; no registry writes.
    pub unsafe fn refresh(&mut self) -> bool {
        let Some((path, modified, style, tile)) = Self::query_wallpaper() else {
            return false;
        };
        if self.bitmap.is_some()
            && path == self.path
            && modified == self.modified
            && style == self.style
            && tile == self.tile
            && self.low_power == crate::motion::low_power()
        {
            return false;
        }
        let key: SourceKey = (path.clone(), modified);
        if LOADING_KEY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            == Some(&key)
        {
            return false;
        }
        self.path = path;
        self.generation += 1;
        self.modified = modified;
        self.style = style;
        self.tile = tile;
        self.low_power = crate::motion::low_power();
        match self.load() {
            Some((bitmap, bw, bh, source)) => {
                self.bitmap = Some(bitmap);
                self.blur_width = bw;
                self.blur_height = bh;
                self.width = source.width;
                self.height = source.height;
                self.natural = source.natural;
                self.accent.set(source.accent);
                self.source = Some(source);
                true
            }
            None => {
                self.source = None;
                self.bitmap = None;
                self.blur_width = 0;
                self.blur_height = 0;
                true
            }
        }
    }
    unsafe fn get_or_build_blurred(
        path: &Path,
        modified: Option<SystemTime>,
        source: &WallpaperSource,
        blur: f32,
        dpi: f32,
    ) -> Option<(Rc<OwnedBitmap>, i32, i32)> {
        let key: BlurKey = (
            path.to_path_buf(),
            modified,
            (blur * 100.).round() as i32,
            (dpi * 100.).round() as i32,
        );
        if let Some(hit) = BLURRED_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
            return Some(hit);
        }
        let (bitmap, bw, bh) = if let Some(small) = source.small.borrow().as_ref() {
            Self::build_blurred(small, blur, dpi, source.natural[0])?
        } else if blur > 0.05 {
            Self::build_blurred(&source.preview, blur, dpi, source.natural[0])?
        } else {
            let (decoded, _) = Self::decode_small(&source.path, Self::wallpaper_cap())?;
            Self::build_blurred(&decoded, blur, dpi, source.natural[0])?
        };
        let owned = Rc::new(OwnedBitmap(bitmap));
        BLURRED_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= 4 {
                cache.clear();
            }
            cache.insert(key, (owned.clone(), bw, bh));
        });
        Some((owned, bw, bh))
    }
    fn prepare_blurred_bytes(
        small: &image::RgbaImage,
        blur: f32,
        dpi: f32,
        natural_w: f32,
    ) -> (Vec<u8>, i32, i32) {
        let sigma = (blur * dpi * small.width() as f32 / natural_w.max(1.0)).clamp(0.0, 40.0);
        let mut blurred = if sigma <= 0.05 {
            small.clone()
        } else if sigma < 3.5 {
            imageops::blur(small, sigma)
        } else {
            imageops::fast_blur(small, sigma)
        };
        let w = blurred.width();
        let h = blurred.height();
        for pixel in blurred.pixels_mut() {
            let luminance =
                0.2126 * pixel[0] as f32 + 0.7152 * pixel[1] as f32 + 0.0722 * pixel[2] as f32;
            for channel in &mut pixel.0[..3] {
                let saturated = luminance + (*channel as f32 - luminance) * 1.28;
                *channel = (saturated * 0.92).clamp(0.0, 255.0) as u8;
            }
            pixel[3] = 255;
        }
        let mut bytes = blurred.into_raw();
        for p in bytes.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
        (bytes, w as i32, h as i32)
    }
    unsafe fn build_blurred(
        small: &image::RgbaImage,
        blur: f32,
        dpi: f32,
        natural_w: f32,
    ) -> Option<(HBITMAP, i32, i32)> {
        let (bytes, w, h) = Self::prepare_blurred_bytes(small, blur, dpi, natural_w);
        let bitmap = dib(w, h, &bytes)?;
        Some((bitmap, w, h))
    }
    unsafe fn load(&self) -> Option<(Rc<OwnedBitmap>, i32, i32, Rc<WallpaperSource>)> {
        let source_key: SourceKey = (self.path.clone(), self.modified);
        let source = if let Some(cached) =
            WALLPAPER_SOURCES.with(|cache| cache.borrow().get(&source_key).cloned())
        {
            cached
        } else {
            let cap = Self::wallpaper_cap();
            let (small, natural) = Self::decode_small(&self.path, cap)?;
            let palette = imageops::thumbnail(&small, 128, 128);
            let accent = crate::accent::dominant(palette.as_raw());
            if std::env::var_os("LUMEN_TRACE_FRAMES").is_some() {
                eprintln!("native-wallpaper-palette colorref={:06x}", accent);
            }
            let w = small.width() as i32;
            let h = small.height() as i32;
            let preview = WallpaperSource::make_preview(&small);
            let source = Rc::new(WallpaperSource {
                path: self.path.clone(),
                small: RefCell::new(Some(small)),
                preview,
                sharp: Cell::new(null_mut()),
                width: w,
                height: h,
                natural,
                accent,
            });
            WALLPAPER_SOURCES.with(|cache| {
                let mut cache = cache.borrow_mut();
                if cache.len() >= 2 {
                    cache.clear();
                }
                cache.insert(source_key, source.clone());
            });
            source
        };
        let (bitmap, blur_w, blur_h) =
            Self::get_or_build_blurred(&self.path, self.modified, &source, self.blur, self.dpi)?;
        source.release_large();
        Some((bitmap, blur_w, blur_h, source))
    }
    pub unsafe fn draw(&self, dc: HDC, hwnd: HWND, width: i32, height: i32) {
        self.draw_blended(dc, hwnd, width, height, 255);
    }
    fn blur_size(&self) -> (i32, i32) {
        if self.blur_width > 0 && self.blur_height > 0 {
            (self.blur_width, self.blur_height)
        } else {
            (self.width, self.height)
        }
    }
    pub unsafe fn draw_blended(&self, dc: HDC, hwnd: HWND, width: i32, height: i32, amount: u8) {
        let Some(bitmap) = self.bitmap.as_ref().map(|b| b.0) else {
            return;
        };
        if width <= 0 || height <= 0 || width as i64 * height as i64 > 32_000_000 {
            return;
        }
        let window = crate::fusion::window_rect(hwnd);
        let moving = crate::fusion::is_moving(hwnd) || is_resizing(hwnd);
        let apply_liquid = liquid_glass()
            && !(moving && crate::motion::low_power())
            && !crate::fusion::is_surface(hwnd);
        if self.style == 0 || self.style == 6 {
            PatBlt(dc, 0, 0, width, height, BLACKNESS);
        }
        // The caller already caches the finished widget background. A second
        // full-monitor bitmap is unnecessary.
        if amount == 255 {
            let mut scratch = self.blend_buffer.borrow_mut();
            if scratch.as_ref().is_some_and(|(owner, _)| *owner == hwnd) {
                scratch.take();
            }
            self.draw_layer(dc, hwnd, width, height, bitmap, self.blur_size(), window);
            if apply_liquid {
                self.apply_liquid_glass_with_source(dc, hwnd, width, height, window);
            }
            return;
        }
        if let Some(source) = &self.source {
            let sharp = source.sharp_bitmap();
            if !sharp.is_null() {
                self.draw_layer(
                    dc,
                    hwnd,
                    width,
                    height,
                    sharp,
                    (self.width, self.height),
                    window,
                );
            }
        }
        if amount == 0 {
            let mut scratch = self.blend_buffer.borrow_mut();
            if scratch.as_ref().is_some_and(|(owner, _)| *owner == hwnd) {
                scratch.take();
            }
            if apply_liquid {
                self.apply_liquid_glass_with_source(dc, hwnd, width, height, window);
            }
            return;
        }
        let mut scratch = self.blend_buffer.borrow_mut();
        if scratch
            .as_ref()
            .is_none_or(|(owner, b)| *owner != hwnd || b.width != width || b.height != height)
        {
            *scratch = crate::compositor::Buffer::plain(width, height, 1.).map(|b| (hwnd, b));
        }
        if let Some((_, buffer)) = scratch.as_ref() {
            PatBlt(buffer.dc, 0, 0, width, height, BLACKNESS);
            self.draw_layer(
                buffer.dc,
                hwnd,
                width,
                height,
                bitmap,
                self.blur_size(),
                window,
            );
            AlphaBlend(
                dc,
                0,
                0,
                width,
                height,
                buffer.dc,
                0,
                0,
                width,
                height,
                BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: amount,
                    AlphaFormat: 0,
                },
            );
            if apply_liquid {
                self.apply_liquid_glass_with_source(dc, hwnd, width, height, window);
            }
        }
        if crate::motion::low_power() && !moving {
            scratch.take();
        }
    }
    unsafe fn apply_liquid_glass_with_source(
        &self,
        dc: HDC,
        hwnd: HWND,
        width: i32,
        height: i32,
        window: RECT,
    ) {
        let dpi = self.dpi.max(1.0);
        let pad = (44.0 * dpi).ceil() as i32;
        let rim_source: Option<(Option<Rc<OwnedBitmap>>, i32, i32)> = None;
        let (rim_bitmap, rim_size) = if let Some((bmp, rw, rh)) = &rim_source {
            (bmp.as_ref().map(|b| b.0), (*rw, *rh))
        } else {
            let sharp_bitmap = self
                .source
                .as_ref()
                .map(|s| s.sharp_bitmap())
                .filter(|b| !b.is_null());
            (sharp_bitmap, (self.width, self.height))
        };
        if let Some(rim_bmp) = rim_bitmap {
            let sw = width + pad * 2;
            let sh = height + pad * 2;
            let mut scratch = self.sharp_buffer.borrow_mut();
            if scratch
                .as_ref()
                .is_none_or(|(owner, b)| *owner != hwnd || b.width != sw || b.height != sh)
            {
                *scratch = crate::compositor::Buffer::plain(sw, sh, 1.).map(|b| (hwnd, b));
            }
            if let Some((_, buffer)) = scratch.as_ref() {
                PatBlt(buffer.dc, 0, 0, sw, sh, BLACKNESS);
                let padded_window = RECT {
                    left: window.left - pad,
                    top: window.top - pad,
                    right: window.right + pad,
                    bottom: window.bottom + pad,
                };
                self.draw_layer(
                    buffer.dc,
                    hwnd,
                    sw,
                    sh,
                    rim_bmp,
                    rim_size,
                    padded_window,
                );
                self.apply_liquid_glass_impl(
                    dc,
                    Some((buffer.dc, pad)),
                    width,
                    height,
                    Some((hwnd, window)),
                );
                if crate::motion::low_power() {
                    scratch.take();
                }
                return;
            }
        }
        self.apply_liquid_glass_impl(dc, None, width, height, Some((hwnd, window)));
    }
    /// Applies static iOS-style edge refraction, chromatic aberration, and a
    /// subtle specular rim only across the narrow perimeter band of a 32-bit DIB.
    #[cfg(test)]
    unsafe fn apply_liquid_glass(&self, dc: HDC, width: i32, height: i32) {
        self.apply_liquid_glass_impl(dc, None, width, height, None);
    }
    unsafe fn apply_liquid_glass_impl(
        &self,
        dc: HDC,
        sharp_dc: Option<(HDC, i32)>,
        width: i32,
        height: i32,
        fusion_owner: Option<(HWND, RECT)>,
    ) {
        if width < 16 || height < 16 {
            return;
        }
        let hbmp = GetCurrentObject(dc, OBJ_BITMAP as u32);
        if hbmp.is_null() {
            return;
        }
        let mut ds: DIBSECTION = zeroed();
        if GetObjectW(hbmp, size_of::<DIBSECTION>() as i32, &mut ds as *mut _ as _)
            < size_of::<BITMAP>() as i32
        {
            return;
        }
        if ds.dsBm.bmBits.is_null()
            || ds.dsBm.bmBitsPixel != 32
            || ds.dsBm.bmWidth < width
            || ds.dsBm.bmHeight < height
        {
            return;
        }
        GdiFlush();
        let w = width as usize;
        let h = height as usize;
        let stride = ds.dsBm.bmWidthBytes.max(width * 4) as usize;
        let bits = ds.dsBm.bmBits as *mut u8;
        let mut src_guard = self.liquid_src.borrow_mut();
        let needed = w * h * 4;
        if src_guard.len() < needed {
            src_guard.resize(needed, 0);
        }
        let src = &mut src_guard[..needed];
        if stride == w * 4 {
            src.copy_from_slice(std::slice::from_raw_parts(bits, needed));
        } else {
            for y in 0..h {
                std::ptr::copy_nonoverlapping(
                    bits.add(y * stride),
                    src.as_mut_ptr().add(y * w * 4),
                    w * 4,
                );
            }
        }
        let sharp_slice: Option<(&[u8], i32)> = sharp_dc.and_then(|(sdc, pad)| {
            let sw = width + pad * 2;
            let sh = height + pad * 2;
            let shbmp = GetCurrentObject(sdc, OBJ_BITMAP as u32);
            if shbmp.is_null() {
                return None;
            }
            let mut sds: DIBSECTION = zeroed();
            if GetObjectW(shbmp, size_of::<DIBSECTION>() as i32, &mut sds as *mut _ as _)
                < size_of::<BITMAP>() as i32
            {
                return None;
            }
            if sds.dsBm.bmBits.is_null()
                || sds.dsBm.bmBitsPixel != 32
                || sds.dsBm.bmWidth < sw
                || sds.dsBm.bmHeight < sh
                || sds.dsBm.bmWidthBytes != sw * 4
            {
                return None;
            }
            Some((
                std::slice::from_raw_parts(sds.dsBm.bmBits as *const u8, sw as usize * sh as usize * 4),
                pad,
            ))
        });
        let (sample_buf, sample_w, sample_h, pad_f, has_sharp) =
            if let Some((s, pad)) = sharp_slice {
                ((s), (width + pad * 2) as usize, (height + pad * 2) as usize, pad as f32, true)
            } else {
                (src as &[u8], w, h, 0.0, false)
            };
        let dst_buf = std::slice::from_raw_parts_mut(bits, h * stride);
        let fusion = fusion_owner.and_then(|(hwnd, window)| {
            crate::fusion::group(hwnd)
                .filter(|panes| panes.len() > 1)
                .map(|panes| (panes, [window.left as f32, window.top as f32]))
        });
        apply_liquid_glass_pixels(
            dst_buf,
            stride,
            w,
            h,
            src,
            sample_buf,
            sample_w,
            sample_h,
            pad_f,
            has_sharp,
            self.dpi,
            true,
            fusion,
        );
        if crate::motion::low_power() {
            *src_guard = Vec::new();
        }
    }
}

/// Applies static Liquid Glass edge refraction, chromatic aberration, and a
/// specular rim to an RGBA pixel buffer using a padded sharp wallpaper source.
pub fn apply_liquid_glass_rgba(
    hwnd: HWND,
    dst: &mut [u8],
    w: usize,
    h: usize,
    sharp: &[u8],
    sw: usize,
    sh: usize,
    pad: usize,
    dpi: f32,
) {
    if w < 16 || h < 16 || dst.len() < w * h * 4 || sharp.len() < sw * sh * 4 {
        return;
    }
    let fusion = if !hwnd.is_null() {
        crate::fusion::group(hwnd)
            .filter(|panes| panes.len() > 1)
            .map(|panes| {
                let window = unsafe { crate::fusion::window_rect(hwnd) };
                (panes, [window.left as f32, window.top as f32])
            })
    } else {
        None
    };
    thread_local! {
        static RGBA_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }
    RGBA_SCRATCH.with(|scratch| {
        let mut frosted = scratch.borrow_mut();
        frosted.clear();
        frosted.extend_from_slice(&dst[..w * h * 4]);
        apply_liquid_glass_pixels(
            dst,
            w * 4,
            w,
            h,
            &frosted,
            sharp,
            sw,
            sh,
            pad as f32,
            true,
            dpi,
            false,
            fusion,
        );
        if crate::motion::low_power() && frosted.capacity() > 1024 * 1024 {
            *frosted = Vec::new();
        }
    });
}

/// Smooth rounded-rectangle SDF (`d`, `nx`, `ny`) with C¹-continuous interior
/// corner normals when `rim > radius`.
#[inline]
pub fn rounded_rect_sdf(
    fx: f32,
    fy: f32,
    hw: f32,
    hh: f32,
    rx: f32,
    ry: f32,
    radius: f32,
) -> (f32, f32, f32) {
    let qx = (fx - hw).abs() - rx;
    let qy = (fy - hh).abs() - ry;
    let (d, nx_abs, ny_abs) = if qx > 0.0 && qy > 0.0 {
        let len = (qx * qx + qy * qy).sqrt();
        let inv = 1.0 / len.max(1e-5);
        (len - radius, qx * inv, qy * inv)
    } else if qx > 0.0 {
        (qx - radius, 1.0, 0.0)
    } else if qy > 0.0 {
        (qy - radius, 0.0, 1.0)
    } else {
        let sum = (qx + qy).min(-1e-5);
        let t = (qy / sum).clamp(0.0, 1.0);
        let s = t * t * (3.0 - 2.0 * t);
        let inv = 1.0 / (s * s + (1.0 - s) * (1.0 - s)).sqrt().max(1e-5);
        let d_smooth = s * qx + (1.0 - s) * qy - radius;
        (d_smooth, s * inv, (1.0 - s) * inv)
    };
    let k = (radius * 0.5).max(1.0);
    let r_center = (qx * qx + qy * qy).sqrt();
    let ft = (r_center / k).clamp(0.0, 1.0);
    let fade = ft * ft * (3.0 - 2.0 * ft);
    let nx = (if fx < hw { -nx_abs } else { nx_abs }) * fade;
    let ny = (if fy < hh { -ny_abs } else { ny_abs }) * fade;
    (d, nx, ny)
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn sample_disp_2d(
    buf: &[u8],
    bw: usize,
    bh: usize,
    bx: f32,
    by: f32,
    nx: f32,
    ny: f32,
    disp: f32,
    signed_ca: f32,
    dpi: f32,
    ch: usize,
) -> f32 {
    if signed_ca.abs() <= 0.05 {
        sample_channel(buf, bw, bh, bx + nx * disp, by + ny * disp, ch)
    } else {
        let d0 = disp + signed_ca * 0.35;
        let d1 = disp + signed_ca;
        let d2 = disp + signed_ca * 1.65;
        let spread = (signed_ca.abs() * 0.45).max(0.75 * dpi);
        let tx = -ny * spread;
        let ty = nx * spread;
        0.20 * sample_channel(buf, bw, bh, bx + nx * d0, by + ny * d0, ch)
            + 0.30 * sample_channel(buf, bw, bh, bx + nx * d1, by + ny * d1, ch)
            + 0.15 * sample_channel(buf, bw, bh, bx + nx * d1 + tx, by + ny * d1 + ty, ch)
            + 0.15 * sample_channel(buf, bw, bh, bx + nx * d1 - tx, by + ny * d1 - ty, ch)
            + 0.20 * sample_channel(buf, bw, bh, bx + nx * d2, by + ny * d2, ch)
    }
}

/// Evaluates a single Liquid Glass rim pixel (`[B, G, R]`) from an opaque BGRA
/// backing buffer for sub-cards, pills, and control plates.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn refract_bgra_card_pixel(
    source: &[u8],
    sw: usize,
    sh: usize,
    x: f32,
    y: f32,
    d: f32,
    nx: f32,
    ny: f32,
    rim: f32,
    bend: f32,
    ca_strength: f32,
    dpi: f32,
) -> [u8; 3] {
    let u = (-d / rim).clamp(0.0, 1.0);
    let curve = u * u * (3.0 - 2.0 * u);
    let edge_align = 1.0 - curve;
    let disp = -bend * edge_align;
    let ca_u = (u * 2.2).clamp(0.0, 1.0);
    let ca_s = ca_u * ca_u * (3.0 - 2.0 * ca_u);
    let ca = 4.0 * ca_s * (1.0 - ca_s) * ca_strength;

    let b = sample_disp_2d(source, sw, sh, x, y, nx, ny, disp, ca, dpi, 0);
    let g = sample_disp_2d(source, sw, sh, x, y, nx, ny, disp, 0.0, dpi, 1);
    let r = sample_disp_2d(source, sw, sh, x, y, nx, ny, disp, -ca, dpi, 2);

    let spec = if u < 0.32 {
        let edge = (1.0 - u / 0.32).clamp(0.0, 1.0);
        let light_dir =
            (-nx * 0.55 - ny * 0.83).max(0.0) + (nx * 0.4 + ny * 0.6).max(0.0) * 0.35;
        edge * edge * (12.0 + 26.0 * light_dir)
    } else {
        0.0
    };
    [
        (b + spec).min(255.0) as u8,
        (g + spec).min(255.0) as u8,
        (r + spec).min(255.0) as u8,
    ]
}

/// Applies Liquid Glass edge refraction, 2D-blurred chromatic aberration, and a
/// specular rim to a premultiplied BGRA card buffer (preserving its alpha mask).
pub fn apply_liquid_glass_premul_bgra(
    buf: &mut [u8],
    w: usize,
    h: usize,
    radius: f32,
    dpi: f32,
) {
    if w < 16 || h < 16 || buf.len() < w * h * 4 {
        return;
    }
    let dpi = dpi.max(1.0);
    let min_dim = w.min(h) as f32;
    let radius = radius.min(min_dim * 0.5);
    let rim = (20.0 * dpi).clamp(8.0, min_dim * 0.32);
    let bend = 10.0 * dpi;
    let ca_strength = 3.5 * dpi;
    let hw = w as f32 * 0.5;
    let hh = h as f32 * 0.5;
    let rx = (hw - radius).max(0.0);
    let ry = (hh - radius).max(0.0);
    let rim_x = rim.ceil() as usize;
    let corner_y = radius.max(rim);
    thread_local! {
        static CARD_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }
    CARD_SCRATCH.with(|scratch| {
        let mut src = scratch.borrow_mut();
        src.clear();
        src.extend_from_slice(&buf[..w * h * 4]);

        for y in 0..h {
            let fy = y as f32 + 0.5;
            let in_straight_band = fy >= corner_y && fy <= h as f32 - corner_y;
            let mut x = 0usize;
            while x < w {
                if in_straight_band && x == rim_x && w > rim_x * 2 {
                    x = w - rim_x;
                }
                let fx = x as f32 + 0.5;
                let (d, nx, ny) = rounded_rect_sdf(fx, fy, hw, hh, rx, ry, radius);
                if d > -rim && d <= 0.5 {
                    let i = (y * w + x) * 4;
                    let a = src[i + 3];
                    if a > 0 {
                        let u = (-d / rim).clamp(0.0, 1.0);
                        let curve = u * u * (3.0 - 2.0 * u);
                        let edge_align = 1.0 - curve;
                        let disp = -bend * edge_align;
                        let ca_u = (u * 2.2).clamp(0.0, 1.0);
                        let ca_s = ca_u * ca_u * (3.0 - 2.0 * ca_u);
                        let ca = 4.0 * ca_s * (1.0 - ca_s) * ca_strength;

                        let sa_b = sample_disp_2d(&src, w, h, x as f32, y as f32, nx, ny, disp, ca, dpi, 3).max(1.0);
                        let sa_g = sample_disp_2d(&src, w, h, x as f32, y as f32, nx, ny, disp, 0.0, dpi, 3).max(1.0);
                        let sa_r = sample_disp_2d(&src, w, h, x as f32, y as f32, nx, ny, disp, -ca, dpi, 3).max(1.0);

                        let a_norm = a as f32 / 255.0;
                        let b = sample_disp_2d(&src, w, h, x as f32, y as f32, nx, ny, disp, ca, dpi, 0) * (255.0 / sa_b);
                        let g = sample_disp_2d(&src, w, h, x as f32, y as f32, nx, ny, disp, 0.0, dpi, 1) * (255.0 / sa_g);
                        let r = sample_disp_2d(&src, w, h, x as f32, y as f32, nx, ny, disp, -ca, dpi, 2) * (255.0 / sa_r);

                        let spec = if u < 0.32 {
                            let edge = (1.0 - u / 0.32).clamp(0.0, 1.0);
                            let light_dir =
                                (-nx * 0.55 - ny * 0.83).max(0.0) + (nx * 0.4 + ny * 0.6).max(0.0) * 0.35;
                            edge * edge * (12.0 + 26.0 * light_dir)
                        } else {
                            0.0
                        };

                        buf[i] = ((b + spec).min(255.0) * a_norm).round().min(a as f32) as u8;
                        buf[i + 1] = ((g + spec).min(255.0) * a_norm).round().min(a as f32) as u8;
                        buf[i + 2] = ((r + spec).min(255.0) * a_norm).round().min(a as f32) as u8;
                    }
                }
                x += 1;
            }
        }
        if crate::motion::low_power() && src.capacity() > 1024 * 1024 {
            *src = Vec::new();
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn apply_liquid_glass_pixels(
    dst_buf: &mut [u8],
    dst_stride: usize,
    w: usize,
    h: usize,
    frosted: &[u8],
    sample_buf: &[u8],
    sample_w: usize,
    sample_h: usize,
    pad_f: f32,
    has_sharp: bool,
    dpi: f32,
    bgra: bool,
    fusion: Option<(Vec<crate::fusion::geometry::Pane>, [f32; 2])>,
) {
    let dpi = dpi.max(1.0);
    let min_dim = w.min(h) as f32;
    let radius = (26.0 * dpi).min(min_dim * 0.5);
    let rim = (44.0 * dpi).clamp(12.0, min_dim * 0.45);
    let bend = 22.0 * dpi;
    let ca_strength = 5.5 * dpi;
    let hw = w as f32 * 0.5;
    let hh = h as f32 * 0.5;
    let rx = (hw - radius).max(0.0);
    let ry = (hh - radius).max(0.0);
    let rim_x = rim.ceil() as usize;
    let corner_y = radius.max(rim);
    let (ch_r, ch_g, ch_b) = if bgra { (2, 1, 0) } else { (0, 1, 2) };

    let mut apply_pixel = |x: usize, y: usize, d: f32, nx: f32, ny: f32| {
        if d <= -rim || d > 0.5 {
            return;
        }
        // u = 0 at the outer edge, 1 at the inner boundary of the rim.
        let u = (-d / rim).clamp(0.0, 1.0);
        let curve = u * u * (3.0 - 2.0 * u);
        let edge_align = 1.0 - curve;
        // Normal-aligned convex lens bend so all 4 edges and corners curve smoothly like a glass bevel.
        let disp = -bend * edge_align;
        let ca_u = (u * 2.2).clamp(0.0, 1.0);
        let ca_s = ca_u * ca_u * (3.0 - 2.0 * ca_u);
        let ca = 4.0 * ca_s * (1.0 - ca_s) * ca_strength;

        let sx = x as f32 + pad_f;
        let sy = y as f32 + pad_f;
        let sample_disp = |buf: &[u8], bw: usize, bh: usize, bx: f32, by: f32, ch: usize, signed_ca: f32| {
            if signed_ca.abs() <= 0.05 {
                sample_channel(buf, bw, bh, bx + nx * disp, by + ny * disp, ch)
            } else {
                let d0 = disp + signed_ca * 0.35;
                let d1 = disp + signed_ca;
                let d2 = disp + signed_ca * 1.65;
                let spread = (signed_ca.abs() * 0.45).max(0.75 * dpi);
                let tx = -ny * spread;
                let ty = nx * spread;
                0.20 * sample_channel(buf, bw, bh, bx + nx * d0, by + ny * d0, ch)
                    + 0.30 * sample_channel(buf, bw, bh, bx + nx * d1, by + ny * d1, ch)
                    + 0.15 * sample_channel(buf, bw, bh, bx + nx * d1 + tx, by + ny * d1 + ty, ch)
                    + 0.15 * sample_channel(buf, bw, bh, bx + nx * d1 - tx, by + ny * d1 - ty, ch)
                    + 0.20 * sample_channel(buf, bw, bh, bx + nx * d2, by + ny * d2, ch)
            }
        };

        let fb = sample_disp(frosted, w, h, x as f32, y as f32, ch_b, ca);
        let fg = sample_disp(frosted, w, h, x as f32, y as f32, ch_g, 0.0);
        let fr = sample_disp(frosted, w, h, x as f32, y as f32, ch_r, -ca);

        let (b, g, r) = if has_sharp && u < (1.0 / 2.2) {
            let blur_t = u * 2.2;
            let blur_falloff = 1.0 - blur_t * blur_t * (3.0 - 2.0 * blur_t);
            let clarity = 0.75 * blur_falloff;
            let ca_s_sharp = ca * 0.35;
            let sb = sample_channel(
                sample_buf,
                sample_w,
                sample_h,
                sx + nx * (disp + ca_s_sharp),
                sy + ny * (disp + ca_s_sharp),
                ch_b,
            );
            let sg = sample_channel(
                sample_buf,
                sample_w,
                sample_h,
                sx + nx * disp,
                sy + ny * disp,
                ch_g,
            );
            let sr = sample_channel(
                sample_buf,
                sample_w,
                sample_h,
                sx + nx * (disp - ca_s_sharp),
                sy + ny * (disp - ca_s_sharp),
                ch_r,
            );
            (
                fb + (sb - fb) * clarity,
                fg + (sg - fg) * clarity,
                fr + (sr - fr) * clarity,
            )
        } else {
            (fb, fg, fr)
        };

        let i = y * dst_stride + x * 4;
        dst_buf[i + ch_b] = b.clamp(0.0, 255.0) as u8;
        dst_buf[i + ch_g] = g.clamp(0.0, 255.0) as u8;
        dst_buf[i + ch_r] = r.clamp(0.0, 255.0) as u8;
    };

    if let Some((panes, origin)) = fusion {
        let skip_margin = rim / dpi + 1.0;
        crate::fusion::geometry::with_outline(&panes, |outline| {
            for y in 0..h {
                let gy = origin[1] + y as f32 + 0.5;
                for x in 0..w {
                    let gx = origin[0] + x as f32 + 0.5;
                    if crate::fusion::geometry::covered_core_within(&panes, gx, gy, skip_margin) {
                        continue;
                    }
                    let d = outline.distance([gx, gy]);
                    if d > -rim && d <= 0.5 {
                        let dx = outline.distance([gx + 1.0, gy]) - outline.distance([gx - 1.0, gy]);
                        let dy = outline.distance([gx, gy + 1.0]) - outline.distance([gx, gy - 1.0]);
                        let len = (dx * dx + dy * dy).sqrt().max(1e-5);
                        apply_pixel(x, y, d, dx / len, dy / len);
                    }
                }
            }
        });
        return;
    }

    for y in 0..h {
        let fy = y as f32 + 0.5;
        let in_straight_band = fy >= corner_y && fy <= h as f32 - corner_y;

        let mut x = 0usize;
        while x < w {
            if in_straight_band && x == rim_x && w > rim_x * 2 {
                x = w - rim_x;
            }
            let fx = x as f32 + 0.5;
            let (d, nx, ny) = rounded_rect_sdf(fx, fy, hw, hh, rx, ry, radius);
            if d > -rim && d <= 0.5 {
                apply_pixel(x, y, d, nx, ny);
            }
            x += 1;
        }
    }
}
impl Glass {
    unsafe fn draw_layer(
        &self,
        dc: HDC,
        hwnd: HWND,
        width: i32,
        height: i32,
        bitmap: HBITMAP,
        src_size: (i32, i32),
        window: RECT,
    ) {
        if bitmap.is_null() {
            return;
        }
        let mut monitor: MONITORINFO = zeroed();
        monitor.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(
            MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
            &mut monitor,
        );
        let area = if self.style == 22 {
            RECT {
                left: GetSystemMetrics(SM_XVIRTUALSCREEN),
                top: GetSystemMetrics(SM_YVIRTUALSCREEN),
                right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
                bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
            }
        } else {
            monitor.rcMonitor
        };
        let aw = (area.right - area.left) as f32;
        let ah = (area.bottom - area.top) as f32;
        let (dw, dh) = if self.style == 2 {
            (aw, ah)
        } else if self.style == 0 || self.tile {
            (self.natural[0], self.natural[1])
        } else {
            let ratio = if self.style == 6 {
                (aw / self.natural[0]).min(ah / self.natural[1])
            } else {
                (aw / self.natural[0]).max(ah / self.natural[1])
            };
            (self.natural[0] * ratio, self.natural[1] * ratio)
        };
        let shift = if liquid_glass() { 6.0 * self.dpi.max(1.0) } else { 0.0 };
        let left_f = area.left as f32
            + if self.tile { 0.0 } else { (aw - dw) / 2.0 }
            - window.left as f32
            + shift;
        let top_f = area.top as f32
            + vertical_offset(ah, dh, self.style, self.tile)
            - window.top as f32
            + shift;
        let left = left_f.round() as i32;
        let top = top_f.round() as i32;
        let source = CreateCompatibleDC(dc);
        let previous = SelectObject(source, bitmap);
        SetStretchBltMode(dc, HALFTONE);
        SetBrushOrgEx(dc, 0, 0, null_mut());
        if self.tile {
            let step_x = (dw.round() as i32).max(1);
            let step_y = (dh.round() as i32).max(1);
            for y in ((top.rem_euclid(step_y) - step_y)..height).step_by(step_y as usize) {
                for x in ((left.rem_euclid(step_x) - step_x)..width).step_by(step_x as usize) {
                    StretchBlt(
                        dc,
                        x,
                        y,
                        step_x,
                        step_y,
                        source,
                        0,
                        0,
                        src_size.0,
                        src_size.1,
                        SRCCOPY,
                    );
                }
            }
        } else {
            let right = (left_f + dw).round() as i32;
            let bottom = (top_f + dh).round() as i32;
            if shift > 0.0 && self.style != 6 && self.style != 0 {
                if left > 0 {
                    StretchBlt(
                        dc,
                        0,
                        0,
                        left.min(width),
                        height,
                        source,
                        0,
                        0,
                        1,
                        src_size.1,
                        SRCCOPY,
                    );
                }
                if top > 0 {
                    StretchBlt(
                        dc,
                        0,
                        0,
                        width,
                        top.min(height),
                        source,
                        0,
                        0,
                        src_size.0,
                        1,
                        SRCCOPY,
                    );
                }
            }
            StretchBlt(
                dc,
                left,
                top,
                (right - left).max(1),
                (bottom - top).max(1),
                source,
                0,
                0,
                src_size.0,
                src_size.1,
                SRCCOPY,
            );
        }
        SelectObject(source, previous);
        DeleteDC(source);
    }
}
unsafe fn registry_number(name: &str, default: u32) -> u32 {
    let mut buffer = [0u16; 32];
    let mut bytes = 64;
    if RegGetValueW(
        HKEY_CURRENT_USER,
        wide("Control Panel\\Desktop").as_ptr(),
        wide(name).as_ptr(),
        RRF_RT_REG_SZ,
        null_mut(),
        buffer.as_mut_ptr() as _,
        &mut bytes,
    ) != 0
    {
        return default;
    }
    let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
        .parse()
        .unwrap_or(default)
}

fn parse_transcoded_cache_path(raw: &[u8]) -> Option<PathBuf> {
    if raw.len() < 26 {
        return None;
    }
    let words: Vec<u16> = raw[24..]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&w| w != 0)
        .collect();
    if words.is_empty() {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&words)))
}

unsafe fn resolve_wallpaper_path(spi_path: PathBuf) -> PathBuf {
    let mut buffer = vec![0u8; 2048];
    let mut bytes = buffer.len() as u32;
    if RegGetValueW(
        HKEY_CURRENT_USER,
        wide("Control Panel\\Desktop").as_ptr(),
        wide("TranscodedImageCache").as_ptr(),
        RRF_RT_REG_BINARY,
        null_mut(),
        buffer.as_mut_ptr() as _,
        &mut bytes,
    ) == 0
    {
        if let Some(cached_source) = parse_transcoded_cache_path(&buffer[..bytes as usize]) {
            if cached_source
                .to_string_lossy()
                .eq_ignore_ascii_case(&spi_path.to_string_lossy())
            {
                if let Some(appdata) = std::env::var_os("APPDATA") {
                    let transcoded = PathBuf::from(appdata)
                        .join("Microsoft\\Windows\\Themes\\TranscodedWallpaper");
                    if transcoded.is_file() {
                        return transcoded;
                    }
                }
            }
        }
    }
    spi_path
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "bmp"
            )
        })
        && path.is_file()
}

unsafe fn window_class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let len = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
    String::from_utf16_lossy(&buf[..len])
}

unsafe fn find_live_child_pid(parent: HWND, depth: u8) -> Option<u32> {
    if parent.is_null() || depth > 3 {
        return None;
    }
    let mut child = null_mut();
    for _ in 0..64 {
        child = FindWindowExW(parent, child, std::ptr::null(), std::ptr::null());
        if child.is_null() {
            break;
        }
        if IsWindowVisible(child) == 0 {
            continue;
        }
        let class = window_class_name(child);
        if matches!(
            class.as_str(),
            "SHELLDLL_DefView" | "FolderView" | "SysListView32"
        ) {
            continue;
        }
        if class == "WorkerW" {
            if let Some(pid) = find_live_child_pid(child, depth + 1) {
                return Some(pid);
            }
            continue;
        }
        let mut rect: RECT = zeroed();
        if GetWindowRect(child, &mut rect) != 0
            && rect.right - rect.left >= 200
            && rect.bottom - rect.top >= 200
        {
            let mut pid = 0u32;
            GetWindowThreadProcessId(child, &mut pid);
            if pid != 0 {
                return Some(pid);
            }
        }
    }
    None
}

unsafe fn find_live_wallpaper_pid() -> Option<u32> {
    let progman = FindWindowW(wide("Progman").as_ptr(), std::ptr::null());
    if let Some(pid) = find_live_child_pid(progman, 0) {
        return Some(pid);
    }
    let worker_class = wide("WorkerW");
    let mut worker = null_mut();
    for _ in 0..64 {
        worker = FindWindowExW(null_mut(), worker, worker_class.as_ptr(), std::ptr::null());
        if worker.is_null() {
            break;
        }
        if IsWindowVisible(worker) == 0 {
            continue;
        }
        if let Some(pid) = find_live_child_pid(worker, 0) {
            return Some(pid);
        }
    }
    None
}

unsafe fn process_image_path(pid: u32) -> Option<PathBuf> {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if handle.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) != 0;
    CloseHandle(handle);
    if !ok || len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

fn resolve_project_json_image(project_path: &Path) -> Option<PathBuf> {
    let dir = project_path.parent()?;
    let raw = std::fs::read_to_string(project_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    for key in ["file", "preview", "Thumbnail", "Preview", "FileName"] {
        if let Some(rel) = json.get(key).and_then(|v| v.as_str()) {
            let candidate = Path::new(rel);
            let full = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                dir.join(candidate)
            };
            if is_supported_image(&full) {
                return Some(full);
            }
        }
    }
    for fallback in ["preview.jpg", "preview.png", "preview.bmp"] {
        let full = dir.join(fallback);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

fn resolve_wallpaper_engine_image(exe_dir: &Path) -> Option<PathBuf> {
    let config_path = exe_dir.join("config.json");
    let raw = std::fs::read_to_string(config_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let root = json.as_object()?;
    for profile in root.values() {
        let Some(selected) = profile
            .get("general")
            .and_then(|g| g.get("wallpaperconfig"))
            .and_then(|w| w.get("selectedwallpapers"))
            .or_else(|| profile.get("selectedwallpapers"))
            .and_then(|s| s.as_object())
        else {
            continue;
        };
        let entry = selected
            .get("Monitor0")
            .or_else(|| selected.values().next())?;
        let Some(file_str) = entry.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let file_path = PathBuf::from(file_str);
        if is_supported_image(&file_path) {
            return Some(file_path);
        }
        if file_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("project.json"))
        {
            if let Some(img) = resolve_project_json_image(&file_path) {
                return Some(img);
            }
        }
    }
    None
}

fn resolve_lively_image() -> Option<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    let layout_path =
        PathBuf::from(local_app_data).join("Lively Wallpaper\\SaveData\\wpdata\\WallpaperLayout.json");
    let raw = std::fs::read_to_string(layout_path).ok()?;
    let layouts: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let first = layouts.as_array()?.first()?;
    if let Some(info_dir) = first.get("LivelyInfoPath").and_then(|v| v.as_str()) {
        let info_path = Path::new(info_dir).join("LivelyInfo.json");
        if let Some(img) = resolve_project_json_image(&info_path) {
            return Some(img);
        }
    }
    if let Some(file_str) = first.get("FilePath").and_then(|v| v.as_str()) {
        let path = PathBuf::from(file_str);
        if is_supported_image(&path) {
            return Some(path);
        }
    }
    None
}

unsafe fn resolve_live_wallpaper_file() -> Option<PathBuf> {
    use std::time::{Duration, Instant};
    thread_local! {
        static LIVE_WP_CACHE: RefCell<Option<(Instant, Option<PathBuf>)>> = const { RefCell::new(None) };
    }
    let ttl = if crate::motion::low_power() {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(5)
    };
    if let Some(cached) = LIVE_WP_CACHE.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|(t, _)| t.elapsed() < ttl)
            .map(|(_, p)| p.clone())
    }) {
        return cached;
    }
    let resolved = (|| {
        let pid = find_live_wallpaper_pid()?;
        let exe_path = process_image_path(pid)?;
        let exe_name = exe_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if exe_name.contains("wallpaper32") || exe_name.contains("wallpaper64") {
            if let Some(dir) = exe_path.parent() {
                if let Some(img) = resolve_wallpaper_engine_image(dir) {
                    return Some(img);
                }
            }
        }
        if exe_name.contains("lively") || exe_name.contains("cefsharp") {
            if let Some(img) = resolve_lively_image() {
                return Some(img);
            }
        }
        if let Some(dir) = exe_path.parent() {
            if let Some(img) = resolve_wallpaper_engine_image(dir) {
                return Some(img);
            }
        }
        None
    })();
    LIVE_WP_CACHE.with(|c| *c.borrow_mut() = Some((Instant::now(), resolved.clone())));
    resolved
}
#[inline]
fn sample_channel(src: &[u8], w: usize, h: usize, x: f32, y: f32, ch: usize) -> f32 {
    let cx = x.clamp(0.0, (w - 1) as f32);
    let cy = y.clamp(0.0, (h - 1) as f32);
    let x0 = cx.floor() as usize;
    let y0 = cy.floor() as usize;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = cx - x0 as f32;
    let fy = cy - y0 as f32;
    let p00 = src[(y0 * w + x0) * 4 + ch] as f32;
    let p10 = src[(y0 * w + x1) * 4 + ch] as f32;
    let p01 = src[(y1 * w + x0) * 4 + ch] as f32;
    let p11 = src[(y1 * w + x1) * 4 + ch] as f32;
    let top = p00 + (p10 - p00) * fx;
    let bot = p01 + (p11 - p01) * fx;
    top + (bot - top) * fy
}

pub unsafe fn dib(width: i32, height: i32, bytes: &[u8]) -> Option<HBITMAP> {
    if width <= 0 || height <= 0 || bytes.len() != width as usize * height as usize * 4 {
        return None;
    }
    let mut info: BITMAPINFO = zeroed();
    info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width;
    info.bmiHeader.biHeight = -height;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    let mut bits = null_mut();
    let bitmap = CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bitmap.is_null() || bits.is_null() {
        if !bitmap.is_null() {
            DeleteObject(bitmap);
        }
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), bits as *mut u8, bytes.len());
    Some(bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn viewport_render_matches_the_old_monitor_crop_and_only_blends_in_viewport_memory() {
        let _ui = crate::native_ui_test_lock();
        unsafe {
            let make_window = |x, y, w, h| {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    wide("STATIC").as_ptr(),
                    wide("Lumen crop test").as_ptr(),
                    WS_POPUP,
                    x,
                    y,
                    w,
                    h,
                    null_mut(),
                    null_mut(),
                    windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()),
                    std::ptr::null(),
                )
            };
            let hwnd = make_window(83, 47, 96, 64);
            let other = make_window(10, 10, 16, 16);
            assert!(!hwnd.is_null() && !other.is_null());
            let mut glass = Glass::new(0., 1.);
            struct Reset(bool, bool);
            impl Drop for Reset {
                fn drop(&mut self) {
                    set_liquid_glass(self.0);
                    crate::motion::set_low_power(self.1);
                }
            }
            let _reset = Reset(liquid_glass(), crate::motion::low_power());
            set_liquid_glass(false);
            crate::motion::set_low_power(false);
            glass.width = 512;
            glass.height = 320;
            glass.natural = [512., 320.];
            let pixels: Vec<u8> = (0..320)
                .flat_map(|y| {
                    (0..512).flat_map(move |x| {
                        let c = (20 + x * 150 / 512 + y * 50 / 320) as u8;
                        [c, c, c, 255]
                    })
                })
                .collect();
            glass.bitmap = Some(Rc::new(OwnedBitmap(dib(512, 320, &pixels).unwrap())));
            let mut window: RECT = zeroed();
            GetWindowRect(hwnd, &mut window);
            let mut monitor: MONITORINFO = zeroed();
            monitor.cbSize = size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(
                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            );
            let area = monitor.rcMonitor;
            let full = crate::compositor::Buffer::plain(
                area.right - area.left,
                area.bottom - area.top,
                1.,
            )
            .unwrap();
            let crop = crate::compositor::Buffer::plain(96, 64, 1.).unwrap();
            for style in [10, 6] {
                glass.style = style;
                PatBlt(full.dc, 0, 0, full.width, full.height, BLACKNESS);
                PatBlt(crop.dc, 0, 0, 96, 64, WHITENESS);
                glass.draw_layer(
                    full.dc,
                    hwnd,
                    full.width,
                    full.height,
                    glass.bitmap.as_ref().unwrap().0,
                    glass.blur_size(),
                    area,
                );
                glass.draw_blended(crop.dc, hwnd, 96, 64, 255);
                for y in (0..64).step_by(8) {
                    for x in (0..96).step_by(8) {
                        let expected = GetPixel(
                            full.dc,
                            window.left - area.left + x,
                            window.top - area.top + y,
                        );
                        let actual = GetPixel(crop.dc, x, y);
                        for shift in [0, 8, 16] {
                            assert!(
                                (((expected >> shift) & 255) as i32
                                    - ((actual >> shift) & 255) as i32)
                                    .abs()
                                    <= 2
                            );
                        }
                    }
                }
                assert!(glass.blend_buffer.borrow().is_none());
            }
            glass.style = 10;
            glass.draw_blended(crop.dc, hwnd, 96, 64, 128);
            {
                let scratch = glass.blend_buffer.borrow();
                let (owner, buffer) = scratch.as_ref().unwrap();
                assert_eq!(*owner, hwnd);
                assert_eq!((buffer.width, buffer.height), (96, 64));
            }
            let auxiliary = crate::compositor::Buffer::plain(16, 16, 1.).unwrap();
            glass.draw(auxiliary.dc, other, 16, 16);
            assert!(glass.blend_buffer.borrow().is_some());
            glass.draw(crop.dc, hwnd, 96, 64);
            assert!(glass.blend_buffer.borrow().is_none());
            DestroyWindow(other);
            DestroyWindow(hwnd);
        }
    }
    #[test]
    fn wallpaper_decoder_applies_exif_rotation_before_layout_and_sampling() {
        let image = image::RgbImage::from_fn(16, 8, |x, _| {
            image::Rgb(if x < 8 { [255, 0, 0] } else { [0, 0, 255] })
        });
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100)
            .encode_image(&image)
            .unwrap();
        let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x08\0\0\0\0\0\0\0";
        let mut file = vec![255, 216, 255, 225];
        file.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        file.extend_from_slice(exif);
        file.extend_from_slice(&jpeg[2..]);
        let path = std::env::temp_dir().join(format!(
            "lumen-wallpaper-orientation-{}.jpg",
            std::process::id()
        ));
        std::fs::write(&path, file).unwrap();
        let result = super::decode_wallpaper(&path);
        std::fs::remove_file(path).unwrap();
        let decoded = result.unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (8, 16));
        assert!(decoded.get_pixel(4, 2)[2] > 230);
        assert!(decoded.get_pixel(4, 13)[0] > 230);
    }
    #[test]
    fn fill_crop_matches_windows_wallpaper_and_other_layouts_stay_centered() {
        // Windows Fill/Span crops 1/3 from the top and 2/3 from the bottom when taller than viewport.
        assert_eq!(super::vertical_offset(2160., 3840., 10, false), -560.);
        assert_eq!(super::vertical_offset(2160., 3840., 22, false), -560.);
        assert!((super::vertical_offset(2160., 2560., 10, false) + 133.33333).abs() < 0.01);
        assert_eq!(super::vertical_offset(2160., 1600., 6, false), 280.);
        assert_eq!(super::vertical_offset(2160., 6000., 0, false), -1920.);
        assert_eq!(super::vertical_offset(2160., 3840., 10, true), 0.);
    }
    #[test]
    fn invalid_bitmap_dimensions_are_rejected() {
        unsafe {
            assert!(super::dib(-1, 10, &[]).is_none());
            assert!(super::dib(2, 2, &[0; 4]).is_none());
        }
    }
    #[test]
    fn transcoded_cache_path_decodes_utf16_source_after_header() {
        let mut raw = vec![0u8; 24];
        for ch in "E:\\wallpaper\\test.jpg".encode_utf16() {
            raw.extend_from_slice(&ch.to_le_bytes());
        }
        raw.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(
            super::parse_transcoded_cache_path(&raw),
            Some(PathBuf::from("E:\\wallpaper\\test.jpg"))
        );
        assert_eq!(super::parse_transcoded_cache_path(&[0u8; 10]), None);
    }
    #[test]
    fn liquid_glass_refracts_rim_with_channel_shift_and_preserves_interior() {
        let _ui = crate::native_ui_test_lock();
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                set_liquid_glass(self.0);
            }
        }
        let _reset = Reset(liquid_glass());
        set_liquid_glass(false);
        let glass = Glass::new(0., 1.);
        let rev_before = glass.revision();
        set_liquid_glass(true);
        assert_ne!(glass.revision(), rev_before);

        unsafe {
            let mut buf = crate::compositor::Buffer::plain(120, 80, 1.0).unwrap();
            let px = buf.pixels_mut();
            for y in 0..80usize {
                for x in 0..120usize {
                    let v = ((x * 2).min(240)) as u8;
                    let i = (y * 120 + x) * 4;
                    px[i] = v;
                    px[i + 1] = v;
                    px[i + 2] = v;
                    px[i + 3] = 255;
                }
            }
            let center_before = buf.pixels_mut()[(40 * 120 + 60) * 4..][..4].to_vec();
            glass.apply_liquid_glass(buf.dc, 120, 80);
            let after = buf.pixels_mut();
            // Center interior is untouched.
            assert_eq!(&after[(40 * 120 + 60) * 4..][..4], &center_before[..]);
            // Left rim pixel has chromatic channel separation (B, G, R sampled at different offsets).
            let rim_px = &after[(40 * 120 + 4) * 4..][..4];
            assert_ne!(rim_px[0], rim_px[2]);
        }
    }

    #[test]
    fn refresh_async_contended_loading_key_does_not_block_subsequent_refresh() {
        let _ui = crate::native_ui_test_lock();
        unsafe {
            let Some((path, modified, _, _)) = Glass::query_wallpaper() else {
                return;
            };
            clear_cache();
            let mut glass = Glass::new(13., 1.);
            {
                *LOADING_KEY.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((path.clone(), modified));
            }
            assert!(!glass.refresh_async(null_mut(), 0));
            assert!(glass.bitmap.is_none());
            assert!(glass.path.as_os_str().is_empty());
            *LOADING_KEY.lock().unwrap_or_else(|e| e.into_inner()) = None;
            assert!(glass.refresh());
            assert!(glass.bitmap.is_some());
        }
    }
}
