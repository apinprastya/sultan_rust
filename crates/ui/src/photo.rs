//! Where the register gets a nota photo from (spec 2.7 §2a, phase K6): a file picked with the
//! native file dialog (F), or one frame of a USB webcam (W).
//!
//! The window asks a [`PhotoSource`] ([`crate::app::Options::photos`]), so the headless tests and
//! the screenshots inject the bytes ([`FixedPhotos`]) and never open a dialog or a camera.
//!
//! - [`NativePhotos`] picks a PNG, JPEG or WebP file with [`rfd`] (on Linux through the XDG
//!   desktop portal over D-Bus, so no GTK libraries are needed; without a portal the dialog
//!   fails and the message says so). A file over 10 MB is refused before it is read
//!   ([`PhotoError::TooLarge`]).
//! - The webcam is behind the cargo feature `webcam` (off by default, so CI and the headless
//!   tests build without a capture library): with it, [`NativePhotos::has_camera`] asks the
//!   system for cameras once (`nokhwa`, V4L2 on Linux) and W is offered only when one is found;
//!   a capture opens the first camera, lets it settle for a few frames and takes one. A camera
//!   that sends MJPEG frames uploads the JPEG as it is; other formats are converted to RGB and
//!   encoded as JPEG here. The server scales and re-encodes every photo anyway.

use std::future::Future;
use std::pin::Pin;

/// The largest photo the register sends (the server's limit: 10 MB).
pub const MAX_PHOTO_BYTES: u64 = 10 * 1024 * 1024;

/// Why no photo came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhotoError {
    /// The file is over [`MAX_PHOTO_BYTES`].
    TooLarge,
    /// The dialog, the file or the camera failed (a message for the cashier).
    Failed(String),
}

/// The bytes of a photo, `None` when the cashier cancelled (or there is no camera).
pub type Picked = Result<Option<Vec<u8>>, PhotoError>;

/// A photo on its way.
pub type PhotoFuture = Pin<Box<dyn Future<Output = Picked> + Send>>;

/// A source of nota photos (see the module docs).
pub trait PhotoSource: Send + Sync {
    /// Whether a camera is there (W is offered).
    fn has_camera(&self) -> bool;
    /// Asks the cashier for an image file.
    fn pick_file(&self) -> PhotoFuture;
    /// Takes one frame from the camera.
    fn capture(&self) -> PhotoFuture;
}

/// The real file dialog and webcam.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativePhotos;

impl PhotoSource for NativePhotos {
    fn has_camera(&self) -> bool {
        webcam::available()
    }

    fn pick_file(&self) -> PhotoFuture {
        Box::pin(async {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Nota")
                .add_filter("PNG / JPEG / WebP", &["png", "jpg", "jpeg", "webp"])
                .pick_file()
                .await;
            let Some(file) = picked else {
                return Ok(None);
            };
            let path = file.path().to_path_buf();
            tokio::task::spawn_blocking(move || read_file(&path))
                .await
                .map_err(|e| PhotoError::Failed(e.to_string()))?
                .map(Some)
        })
    }

    fn capture(&self) -> PhotoFuture {
        Box::pin(async {
            tokio::task::spawn_blocking(webcam::capture)
                .await
                .map_err(|e| PhotoError::Failed(e.to_string()))?
        })
    }
}

/// The bytes of `path`, refused above [`MAX_PHOTO_BYTES`] before reading.
///
/// # Errors
/// [`PhotoError::TooLarge`], or [`PhotoError::Failed`] with the IO error.
pub fn read_file(path: &std::path::Path) -> Result<Vec<u8>, PhotoError> {
    let failed = |e: std::io::Error| PhotoError::Failed(e.to_string());
    if std::fs::metadata(path).map_err(failed)?.len() > MAX_PHOTO_BYTES {
        return Err(PhotoError::TooLarge);
    }
    std::fs::read(path).map_err(failed)
}

/// A source that answers fixed bytes (headless tests, screenshots).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedPhotos {
    /// What F answers.
    pub file: Picked,
    /// What W answers; `None` means there is no camera.
    pub camera: Option<Picked>,
}

impl FixedPhotos {
    /// F answers `bytes`; no camera.
    #[must_use]
    pub fn file(bytes: Vec<u8>) -> Self {
        Self {
            file: Ok(Some(bytes)),
            camera: None,
        }
    }

    /// No file is picked (cancelled) and no camera.
    #[must_use]
    pub fn none() -> Self {
        Self {
            file: Ok(None),
            camera: None,
        }
    }
}

impl PhotoSource for FixedPhotos {
    fn has_camera(&self) -> bool {
        self.camera.is_some()
    }

    fn pick_file(&self) -> PhotoFuture {
        let picked = self.file.clone();
        Box::pin(async move { picked })
    }

    fn capture(&self) -> PhotoFuture {
        let picked = self.camera.clone().unwrap_or(Ok(None));
        Box::pin(async move { picked })
    }
}

#[cfg(not(feature = "webcam"))]
mod webcam {
    //! Built without the `webcam` feature: no camera.

    use super::Picked;

    pub fn available() -> bool {
        false
    }

    #[allow(clippy::unnecessary_wraps)] // the same signature as with the feature
    pub fn capture() -> Picked {
        Ok(None)
    }
}

#[cfg(feature = "webcam")]
mod webcam {
    //! One frame of the first camera (`nokhwa`).

    use std::io::Cursor;

    use nokhwa::Camera;
    use nokhwa::pixel_format::RgbFormat;
    use nokhwa::utils::{
        ApiBackend, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    };

    use super::{PhotoError, Picked};

    /// Frames read before the one kept, so the exposure settles.
    const SETTLE_FRAMES: usize = 5;

    pub fn available() -> bool {
        nokhwa::query(ApiBackend::Auto).is_ok_and(|cameras| !cameras.is_empty())
    }

    pub fn capture() -> Picked {
        let failed = |e: &dyn std::fmt::Display| PhotoError::Failed(e.to_string());
        if !available() {
            return Ok(None);
        }
        let format =
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution);
        let mut camera = Camera::new(CameraIndex::Index(0), format).map_err(|e| failed(&e))?;
        camera.open_stream().map_err(|e| failed(&e))?;
        let mut frame = camera.frame().map_err(|e| failed(&e))?;
        for _ in 0..SETTLE_FRAMES {
            frame = camera.frame().map_err(|e| failed(&e))?;
        }
        let _ = camera.stop_stream();
        if frame.source_frame_format() == FrameFormat::MJPEG {
            return Ok(Some(frame.buffer().to_vec()));
        }
        let decoded = frame.decode_image::<RgbFormat>().map_err(|e| failed(&e))?;
        let (width, height) = (decoded.width(), decoded.height());
        let rgb = image::RgbImage::from_raw(width, height, decoded.into_raw())
            .ok_or_else(|| PhotoError::Failed("the camera frame has the wrong size".into()))?;
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(rgb)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg)
            .map_err(|e| failed(&e))?;
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now<T>(future: Pin<Box<dyn Future<Output = T> + Send>>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn fixed_photos_answer_what_they_hold() {
        let source = FixedPhotos::file(vec![1, 2, 3]);
        assert!(!source.has_camera());
        assert_eq!(now(source.pick_file()), Ok(Some(vec![1, 2, 3])));
        assert_eq!(now(source.capture()), Ok(None));
        let source = FixedPhotos {
            file: Ok(None),
            camera: Some(Err(PhotoError::Failed("busy".into()))),
        };
        assert!(source.has_camera());
        assert_eq!(now(source.pick_file()), Ok(None));
        assert_eq!(
            now(source.capture()),
            Err(PhotoError::Failed("busy".into()))
        );
        assert_eq!(FixedPhotos::none().file, Ok(None));
    }

    #[test]
    fn files_are_read_up_to_the_limit() {
        let dir = std::env::temp_dir().join(format!("sultan-photo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small.png");
        std::fs::write(&small, b"png").unwrap();
        assert_eq!(read_file(&small), Ok(b"png".to_vec()));
        let big = dir.join("big.jpg");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(MAX_PHOTO_BYTES + 1).unwrap();
        assert_eq!(read_file(&big), Err(PhotoError::TooLarge));
        assert!(matches!(
            read_file(&dir.join("missing.png")),
            Err(PhotoError::Failed(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_the_feature_there_is_no_camera() {
        if cfg!(not(feature = "webcam")) {
            assert!(!NativePhotos.has_camera());
            assert_eq!(now(NativePhotos.capture()), Ok(None));
        }
    }
}
