use std::{
    ffi::c_void,
    mem::size_of,
    ptr, slice,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

use windows::{
    Globalization::Language,
    Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::DataWriter,
    Win32::{
        Foundation::{E_FAIL, POINT, RECT},
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, ClientToScreen, CreateCompatibleDC,
            CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP,
            HDC, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject, StretchBlt,
        },
        System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
            WindowsAndMessaging::{GetClientRect, GetForegroundWindow, GetWindowTextW},
        },
    },
    core::{Error, HSTRING, Result},
};

use crate::detection::{BreakCue, CueLatch, Cues, HudFrame};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

pub enum Event {
    Status(&'static str),
    Chest,
    MatchStarted,
    ReplayStarted(u64),
    ReplayFinished(u64, u32, Option<crate::stats::ReplayCount>),
    ReplayTotals(crate::stats::ReplayTotals),
}

pub fn watch(events: Sender<Event>, enabled: Arc<AtomicBool>) {
    if let Err(error) = run(&events, &enabled) {
        eprintln!("Could not start screen recognition: {error}");
        let _ = events.send(Event::Status(
            "OCR unavailable · Check Windows English language options",
        ));
    }
}

fn run(events: &Sender<Event>, enabled: &AtomicBool) -> Result<()> {
    // WinRT and GDI objects stay on this dedicated worker thread.
    unsafe {
        RoInitialize(RO_INIT_MULTITHREADED)?;
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _apartment = Apartment;
    let engine =
        OcrEngine::TryCreateFromLanguage(&Language::CreateLanguage(&HSTRING::from("en-US"))?)?;
    let mut bitmap = None;
    let mut chest = CueLatch::new(5);
    let mut battle_bus = CueLatch::new(80);
    let mut broken = BreakCue::default();
    let mut previous: Option<(u64, bool)> = None;
    let clock = Instant::now();
    let mut last_status = "";

    loop {
        if !enabled.load(Ordering::Relaxed) {
            broken = BreakCue::default();
            last_status = "";
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        let start = Instant::now();
        let status = match scan(&engine, &mut bitmap) {
            Ok(None) => {
                broken = BreakCue::default();
                "Waiting for Fortnite · Bring the game to the foreground"
            }
            Ok(Some((cues, marker, impact, burst))) => {
                let match_started = battle_bus.observe(cues.battle_bus);
                let now = clock.elapsed().as_millis() as u64;
                let searched = chest.observe(cues.chest && !cues.battle_bus && !cues.combat);
                let smashed = broken.observe(now, marker, impact, burst, &cues) && !cues.battle_bus;
                let duplicate = previous
                    .is_some_and(|(at, was_smashed)| was_smashed != smashed && now - at <= 1000);
                let event = if match_started {
                    broken = BreakCue::default();
                    previous = None;
                    Some(Event::MatchStarted)
                } else if (searched || smashed) && !duplicate {
                    previous = Some((now, smashed));
                    Some(Event::Chest)
                } else {
                    None
                };
                if let Some(event) = event {
                    if events.send(event).is_err() {
                        return Ok(());
                    }
                }
                "Watching Fortnite"
            }
            Err(_) => "Capture unavailable · Try Windowed Fullscreen",
        };
        if status != last_status {
            if events.send(Event::Status(status)).is_err() {
                return Ok(());
            }
            last_status = status;
        }
        thread::sleep(SAMPLE_INTERVAL.saturating_sub(start.elapsed()));
    }
}

struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

fn scan(engine: &OcrEngine, bitmap: &mut Option<Dib>) -> Result<Option<(Cues, bool, bool, bool)>> {
    let window = unsafe { GetForegroundWindow() };
    let mut title = [0; 64];
    let length = unsafe { GetWindowTextW(window, &mut title) } as usize;
    if !String::from_utf16_lossy(&title[..length])
        .trim()
        .eq_ignore_ascii_case("Fortnite")
    {
        return Ok(None);
    }

    let mut client = RECT::default();
    unsafe { GetClientRect(window, &mut client)? };
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    // The center contains the XP toast and jump prompt. Keep the upper-center bus timer too.
    let mut origin = POINT {
        x: width / 5,
        y: height / 25,
    };
    unsafe { ClientToScreen(window, &mut origin).ok()? };
    let (width, height) = (width * 3 / 5, height * 2 / 3);
    if width < 64 || height < 64 {
        return Ok(None);
    }

    let output_width = width.min(1600).min(width * 1600 / height);
    let output_height = height * output_width / width;
    let dimensions = (output_width, output_height);
    if bitmap
        .as_ref()
        .is_none_or(|dib| dib.dimensions != dimensions)
    {
        *bitmap = Some(Dib::new(dimensions)?);
    }
    let dib = bitmap.as_ref().unwrap();
    dib.capture(origin, width, height)?;
    if unsafe { GetForegroundWindow() } != window {
        return Ok(None);
    }
    let pixels = dib.pixels();
    if pixels.chunks_exact(4).all(|pixel| pixel[..3] == [0, 0, 0]) {
        return Err(Error::new(E_FAIL, "Black capture frame"));
    }
    Ok(Some(analyze_hud(
        engine,
        pixels,
        output_width,
        output_height,
    )?))
}

fn analyze_hud(
    engine: &OcrEngine,
    pixels: &[u8],
    width: i32,
    height: i32,
) -> Result<(Cues, bool, bool, bool)> {
    let frame = HudFrame {
        pixels,
        width: width as usize,
        height: height as usize,
    };
    let mut cues = Cues::read(&recognize(engine, pixels, width, height)?);
    if cues.xp_notice && frame.chest_reward_label() {
        cues.chest = true;
    }
    if !cues.chest {
        let (xp, width, height) = frame.enlarged_xp();
        let notice = Cues::read(&recognize(engine, &xp, width, height)?);
        cues.combat |= notice.combat;
        cues.chest = notice.chest
            || (notice.chest_label && (notice.xp_notice || cues.xp_notice))
            || (notice.xp_notice && frame.chest_reward_label());
    }
    cues.chest &= !cues.combat;
    Ok((
        cues,
        frame.chest_marker(),
        frame.pickaxe_impact(),
        frame.container_burst(),
    ))
}

fn recognize(engine: &OcrEngine, pixels: &[u8], width: i32, height: i32) -> Result<String> {
    let writer = DataWriter::new()?;
    writer.WriteBytes(pixels)?;
    let image = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &writer.DetachBuffer()?,
        BitmapPixelFormat::Bgra8,
        width,
        height,
        BitmapAlphaMode::Ignore,
    )?;
    let mut text = String::new();
    for line in engine.RecognizeAsync(&image)?.get()?.Lines()? {
        text.push_str(&line.Text()?.to_string_lossy());
        text.push('\n');
    }
    Ok(text)
}

struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut c_void,
    dimensions: (i32, i32),
}

impl Dib {
    fn new(dimensions: (i32, i32)) -> Result<Self> {
        let (width, height) = dimensions;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut dib = Self {
            dc: unsafe { CreateCompatibleDC(None) },
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            bits: ptr::null_mut(),
            dimensions,
        };
        if dib.dc.is_invalid() {
            return Err(Error::from_win32());
        }
        dib.bitmap = unsafe {
            CreateDIBSection(Some(dib.dc), &info, DIB_RGB_COLORS, &mut dib.bits, None, 0)?
        };
        dib.previous = unsafe { SelectObject(dib.dc, dib.bitmap.into()) };
        if dib.previous.is_invalid() || dib.bits.is_null() {
            return Err(Error::from_win32());
        }
        Ok(dib)
    }

    fn capture(&self, origin: POINT, width: i32, height: i32) -> Result<()> {
        let screen = unsafe { GetDC(None) };
        if screen.is_invalid() {
            return Err(Error::from_win32());
        }
        let (output_width, output_height) = self.dimensions;
        unsafe {
            let copied = StretchBlt(
                self.dc,
                0,
                0,
                output_width,
                output_height,
                Some(screen),
                origin.x,
                origin.y,
                width,
                height,
                SRCCOPY,
            )
            .ok();
            ReleaseDC(None, screen);
            copied?;
            GdiFlush().ok()
        }
    }

    fn pixels(&self) -> &[u8] {
        let (width, height) = self.dimensions;
        // This is the tightly packed top-down 32-bit bitmap allocated by CreateDIBSection.
        unsafe { slice::from_raw_parts(self.bits.cast(), width as usize * height as usize * 4) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_invalid() {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap.into());
            }
            if !self.dc.is_invalid() {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}
