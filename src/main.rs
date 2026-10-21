//! Minimal reproduction for rust-windowing/winit#3992.
//!
//! winit 0.30 on macOS panics ("tried to handle event while another event is currently being
//! handled") when a nested run loop executes a deferred event while `window_event` is still on
//! the stack. Real-world trigger: a synchronous `rfd` file dialog runs its panel with
//! `run_modal` inside the UI pass; a Finder drag that passes over the application's own window
//! queues a winit event as a `CFRunLoopPerformBlock` on the common modes, and the modal loop
//! executes it re-entrantly. The block is `panic_cannot_unwind`, so the process aborts (SIGABRT).
//!
//! One window, two code-drawn buttons; all text is a hand-typeset PNG (`Untitled-1.png`,
//! embedded below) anchored to the window's bottom-right corner:
//! - **red button (buggy)**: the dialog opens inside `window_event` — the crashing shape.
//! - **green button (fixed)**: an `rfd::AsyncFileDialog` completion-handler sheet begun on the
//!   main thread and awaited on a worker thread; the frame loop keeps running.
//!
//! Reproduction: click a button, then drag a file from Finder **over the exposed part of this
//! window** and onto the open dialog. The application window must be **larger than the file
//! dialog** — if the panel covers it entirely, drags land on the panel and no window event is
//! queued, so the race cannot happen. Affected macOS versions observed: 26.5.x, 26.6.2, 27.0.1.
//!
//! Deps: winit + rfd + softbuffer + png (decoding the embedded image only).

use std::num::NonZeroU32;
use std::sync::mpsc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::raw_window_handle::{DisplayHandle, HasDisplayHandle, HasWindowHandle, WindowHandle};
use winit::window::{Cursor, CursorIcon, Window, WindowId};

const WHITE: u32 = 0xFF_F8F8F8;
const RED: u32 = 0xFF_C04040;
const GREEN: u32 = 0xFF_2E8B57;
const GREY: u32 = 0xFF_EAEAEA;
const HOVER: u32 = 0xFF_D8E4F0;

/// Button geometry in physical pixels (top-left of the window; the image lives bottom-right).
const BTN: (i64, i64, i64, i64) = (48, 48, 440, 144); // x of the first button, y, width, height
const BTN_GAP: i64 = 48;

/// The hand-typeset UI text, anchored to the window's bottom-right corner.
const IMG: &[u8] = include_bytes!("Untitled-1.png");

type Surface = softbuffer::Surface<DisplayHandle<'static>, WindowHandle<'static>>;

struct App {
    window: Option<std::sync::Arc<Window>>,
    surface: Option<Surface>,
    cursor: (f64, f64),
    /// The async sheet runs on a worker thread; its answer arrives here.
    answer: Option<mpsc::Receiver<Option<String>>>,
    /// The embedded image, decoded once: 0xAARRGGBB pixels, row-major.
    img: Vec<u32>,
    img_w: usize,
    img_h: usize,
}

fn main() {
    let (img, img_w, img_h) = decode_image();
    let ev = EventLoop::new().expect("event loop");
    let mut app = App { window: None, surface: None, cursor: (0.0, 0.0), answer: None, img, img_w, img_h };
    ev.run_app(&mut app).expect("run app");
}

/// Decode the embedded PNG into 0xAARRGGBB pixels.
fn decode_image() -> (Vec<u32>, usize, usize) {
    let mut reader = png::Decoder::new(std::io::Cursor::new(IMG)).read_info().expect("png header");
    let mut data = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut data).expect("png frame");
    let (w, h) = (info.width as usize, info.height as usize);
    let px = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        other => panic!("unexpected PNG colour type {other:?} (re-export as RGB or RGBA)"),
    };
    let mut img = Vec::with_capacity(w * h);
    for p in data.chunks(px).take(w * h) {
        let (r, g, b) = (p[0] as u32, p[1] as u32, p[2] as u32);
        img.push(0xFF00_0000 | (r << 16) | (g << 8) | b);
    }
    (img, w, h)
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("winit#3992 MRE — the red button opens a sync dialog (buggy), the green one an async sheet (fixed). The window must be larger than the dialog to reproduce.")
            .with_inner_size(LogicalSize::new(760.0, 420.0));
        let window = std::sync::Arc::new(el.create_window(attrs).expect("create window"));
        // SAFETY: both raw handles come from live winit objects that outlive the surface (they
        // are held in `App` for the whole session).
        let dh = unsafe { DisplayHandle::borrow_raw(el.display_handle().expect("display handle").as_raw()) };
        let context = softbuffer::Context::new(dh).expect("softbuffer context");
        let wh = unsafe { WindowHandle::borrow_raw(window.window_handle().expect("window handle").as_raw()) };
        let surface = softbuffer::Surface::new(&context, wh).expect("softbuffer surface");
        self.window = Some(window.clone());
        self.surface = Some(surface);
        self.window.as_ref().unwrap().request_redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                let Some(w) = &self.window else { return };
                w.set_cursor(Cursor::from(if hovered_button(&self.cursor).is_some() {
                    CursorIcon::Pointer
                } else {
                    CursorIcon::Default
                }));
                w.request_redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => match hovered_button(&self.cursor) {
                Some(0) => {
                    // The nested run loop runs inside this handler: the shape that trips the
                    // re-entrancy guard when a drag over this window queues an event.
                    println!("[buggy] opening the sync dialog inside window_event...");
                    let picked = rfd::FileDialog::new().pick_file();
                    println!("[buggy] dialog returned: {}", picked.is_some());
                }
                Some(1) => {
                    if self.answer.is_none() {
                        // Created on the main thread (AppKit requirement), awaited off it.
                        let fut = rfd::AsyncFileDialog::new().pick_file();
                        let (tx, rx) = mpsc::channel();
                        std::thread::spawn(move || {
                            let picked = pollster::block_on(fut).map(|f| f.file_name());
                            let _ = tx.send(picked);
                        });
                        self.answer = Some(rx);
                        println!("[fixed] sheet opened; the frame loop keeps running");
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        if let Some(rx) = &self.answer
            && let Ok(picked) = rx.try_recv()
        {
            println!("[fixed] dialog returned: {}", picked.is_some());
            self.answer = None;
        }
    }
}

impl App {
    fn draw(&mut self) {
        let Some(w) = &self.window else { return };
        let Some(surface) = &mut self.surface else { return };
        let size = w.inner_size();
        let (bw, bh) = (size.width.max(1) as usize, size.height.max(1) as usize);
        let _ = surface.resize(NonZeroU32::new(bw as u32).expect("nonzero"), NonZeroU32::new(bh as u32).expect("nonzero"));
        let Ok(mut buf) = surface.buffer_mut() else { return };

        buf[..bw * bh].fill(WHITE);

        // buttons (top-left; the image is anchored bottom-right)
        let (_, by, bwid, bhei) = BTN;
        for (i, accent) in [RED, GREEN].into_iter().enumerate() {
            let bx = BTN.0 + i as i64 * (bwid + BTN_GAP);
            let fill = if hovered_button(&self.cursor) == Some(i) { HOVER } else { GREY };
            rect(&mut buf, bw, bx, by, bwid, bhei, fill);
            frame(&mut buf, bw, bx, by, bwid, bhei, 6, accent);
        }

        // the typeset text, anchored to the bottom-right corner (clipped if the window is smaller)
        let ox = bw as i64 - self.img_w as i64;
        let oy = bh as i64 - self.img_h as i64;
        for iy in 0..self.img_h as i64 {
            let dy = oy + iy;
            if dy < 0 || dy >= bh as i64 {
                continue;
            }
            for ix in 0..self.img_w as i64 {
                let dx = ox + ix;
                if dx < 0 || dx >= bw as i64 {
                    continue;
                }
                buf[dy as usize * bw + dx as usize] = self.img[iy as usize * self.img_w + ix as usize];
            }
        }

        let _ = buf.present();
    }
}

/// 0 = buggy button, 1 = fixed button, none = elsewhere.
fn hovered_button(cursor: &(f64, f64)) -> Option<usize> {
    let (bx, by, bwid, bhei) = (BTN.0 as f64, BTN.1 as f64, BTN.2 as f64, BTN.3 as f64);
    let (cx, cy) = (cursor.0, cursor.1);
    for (i, x) in [(0, bx), (1, bx + bwid + BTN_GAP as f64)] {
        if cx >= x && cx <= x + bwid && cy >= by && cy <= by + bhei {
            return Some(i);
        }
    }
    None
}

// --- tiny software renderer helpers (buttons only; all text comes from the embedded image) ---

fn rect(buf: &mut [u32], w: usize, x: i64, y: i64, rw: i64, rh: i64, color: u32) {
    for dy in 0..rh {
        for dx in 0..rw {
            let (px, py) = (x + dx, y + dy);
            if px >= 0 && py >= 0 {
                let (px, py) = (px as usize, py as usize);
                if py < buf.len() / w && px < w {
                    buf[py * w + px] = color;
                }
            }
        }
    }
}

fn frame(buf: &mut [u32], w: usize, x: i64, y: i64, rw: i64, rh: i64, t: i64, color: u32) {
    rect(buf, w, x, y, rw, t, color);
    rect(buf, w, x, y + rh - t, rw, t, color);
    rect(buf, w, x, y, t, rh, color);
    rect(buf, w, x + rw - t, y, t, rh, color);
}
