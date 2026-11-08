//! Renders every register screen to PNG with Slint's software renderer (no display needed), driven
//! by keys against the fake service, like the headless tests.
//!
//! ```sh
//! cargo run -p sultan-ui --example screenshots -- docs/screenshots [WIDTHxHEIGHT] [en|id]
//! ```

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Key, Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, PhysicalSize, PlatformError, SharedString};
use sultan_client::fake::FakeService;
use sultan_ui::{App, Options, Runner, build_app};

struct Shots(Rc<MinimalSoftwareWindow>);

impl Platform for Shots {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn press(app: &App, text: impl Into<SharedString>) {
    let text = text.into();
    let window = app.ui.window();
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

fn digits(app: &App, text: &str) {
    for c in text.chars() {
        press(app, c.to_string());
    }
}

fn save(window: &MinimalSoftwareWindow, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    slint::platform::update_timers_and_animations();
    let size = window.size();
    let (width, height) = (size.width as usize, size.height as usize);
    let mut pixels = vec![PremultipliedRgbaColor::default(); width * height];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width);
    });
    let bytes: Vec<u8> = pixels
        .iter()
        .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
        .collect();
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, size.width, size.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    encoder.write_header()?.write_image_data(&bytes)?;
    println!("{}", path.display());
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "docs/screenshots".into()));
    let (width, height) = args
        .next()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1366_u32, 768_u32));
    let language = args.next().unwrap_or_else(|| "en".into());
    std::fs::create_dir_all(&dir)?;

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Shots(window.clone())))?;
    window.set_size(PhysicalSize::new(width, height));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let fake = Arc::new(FakeService::default());
    fake.set_cash_sales(1_250_000);
    let mut options = Options::new("R1 · Kasir 1");
    options.language = Some(language.clone());
    let app = build_app(
        fake.clone(),
        Runner::Block(runtime.handle().clone()),
        options,
    )?;
    app.ui.show()?;
    let shot = |name: &str| {
        save(
            &window,
            &dir.join(format!("{name}-{language}-{width}x{height}.png")),
        )
    };

    shot("1-lock")?;
    press(&app, Key::RightArrow);
    digits(&app, "12");
    shot("2-pin-pad")?;
    press(&app, Key::Escape);
    press(&app, Key::LeftArrow);
    digits(&app, "0000");
    press(&app, Key::Return);
    shot("3-wrong-pin")?;
    digits(&app, "1234");
    press(&app, Key::Return);
    digits(&app, "500000");
    shot("4-open-session")?;
    press(&app, Key::Return);
    shot("5-home")?;
    press(&app, Key::F2);
    press(&app, Key::DownArrow);
    digits(&app, "5555");
    press(&app, Key::Return);
    digits(&app, "55");
    shot("6-override")?;
    press(&app, Key::Escape);
    press(&app, Key::F10);
    press(&app, Key::DownArrow);
    digits(&app, "17");
    press(&app, Key::DownArrow);
    digits(&app, "3");
    press(&app, Key::DownArrow);
    press(&app, Key::DownArrow);
    digits(&app, "4");
    shot("7-blind-close")?;
    press(&app, Key::Return);
    press(&app, Key::Return);
    shot("8-report")?;
    press(&app, Key::Return);

    // Someone else's session blocks the register.
    fake.open_for("u-budi", 300_000);
    press(&app, Key::RightArrow);
    press(&app, Key::RightArrow);
    digits(&app, "5555");
    press(&app, Key::Return);
    shot("9-register-in-use")?;
    Ok(())
}
