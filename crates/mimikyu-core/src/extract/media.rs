use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ImageEncoder, ImageReader};
use serde::{Deserialize, Serialize};

use crate::driver::{BoundingBox, DriverSession, ScreenshotFormat, ScreenshotOpts};
use crate::error::{MimikyuError, Result};
use crate::snapshot::RefMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaTarget {
    Viewport,
    FullPage,
    Ref(String),
    Selector(String),
    Clip(BoundingBox),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaptureOpts {
    pub target: Option<MediaTarget>,
    pub format: ScreenshotFormat,
    pub full_page: bool,
    pub omit_background: bool,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub scale: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaCapture {
    pub bytes: Vec<u8>,
    pub format: ScreenshotFormat,
    pub width: u32,
    pub height: u32,
}

pub async fn capture_screenshot(
    session: &dyn DriverSession,
    ref_map: Option<&RefMap>,
    opts: CaptureOpts,
) -> Result<MediaCapture> {
    let mut full_page = opts.full_page;
    let mut clip = None;

    match opts.target {
        None | Some(MediaTarget::Viewport) => {}
        Some(MediaTarget::FullPage) => {
            full_page = true;
        }
        Some(MediaTarget::Clip(box_rect)) => {
            full_page = false;
            clip = Some(box_rect);
        }
        Some(MediaTarget::Selector(ref sel)) => {
            full_page = false;
            clip = Some(session.resolve_selector_box(sel).await?);
        }
        Some(MediaTarget::Ref(ref ref_id)) => {
            full_page = false;
            let map = ref_map.ok_or_else(|| {
                MimikyuError::ElementNotFound(format!(
                    "cannot resolve target '{ref_id}' without an active snapshot"
                ))
            })?;
            let entry = map.get(ref_id).ok_or_else(|| {
                MimikyuError::ElementNotFound(format!("unknown snapshot ref '{ref_id}'"))
            })?;
            let bbox = if let Some(bid) = entry.backend_node_id {
                match session
                    .resolve_node_box(bid, entry.frame_id.as_deref())
                    .await
                {
                    Ok(b) => b,
                    Err(e) => {
                        if let Some(ref css) = entry.css_fallback {
                            session.resolve_selector_box(css).await.map_err(|_| e)?
                        } else {
                            return Err(e);
                        }
                    }
                }
            } else if let Some(ref css) = entry.css_fallback {
                session.resolve_selector_box(css).await?
            } else {
                return Err(MimikyuError::ElementNotFound(format!(
                    "ref '{ref_id}' has no backend node or CSS fallback"
                )));
            };
            clip = Some(bbox);
        }
    }

    let raw_bytes = session
        .screenshot(ScreenshotOpts {
            format: opts.format,
            full_page,
            clip,
            omit_background: opts.omit_background,
            scale: opts.scale,
            max_width: opts.max_width,
            max_height: opts.max_height,
        })
        .await?;

    apply_max_dimensions(raw_bytes, opts.format, opts.max_width, opts.max_height)
}

pub fn apply_max_dimensions(
    bytes: Vec<u8>,
    format: ScreenshotFormat,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> Result<MediaCapture> {
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|e| MimikyuError::Protocol {
            code: -1,
            message: format!("failed to inspect image header: {e}"),
        })?;
    let (w, h) = reader.into_dimensions().map_err(|e| MimikyuError::Protocol {
        code: -1,
        message: format!("failed to decode image dimensions: {e}"),
    })?;

    let needs_resize = max_width.is_some_and(|mw| mw > 0 && w > mw)
        || max_height.is_some_and(|mh| mh > 0 && h > mh);

    if !needs_resize {
        return Ok(MediaCapture {
            bytes,
            format,
            width: w,
            height: h,
        });
    }

    let img = image::load_from_memory(&bytes).map_err(|e| MimikyuError::Protocol {
        code: -1,
        message: format!("failed to decode image for resizing: {e}"),
    })?;

    let mw = max_width.filter(|&v| v > 0).unwrap_or(u32::MAX);
    let mh = max_height.filter(|&v| v > 0).unwrap_or(u32::MAX);
    let resized = img.resize(mw, mh, FilterType::Triangle);
    let rw = resized.width();
    let rh = resized.height();

    let mut out = Vec::new();
    match format {
        ScreenshotFormat::Png => {
            let rgba = resized.to_rgba8();
            PngEncoder::new(&mut out)
                .write_image(rgba.as_raw(), rw, rh, image::ExtendedColorType::Rgba8)
                .map_err(|e| MimikyuError::Protocol {
                    code: -1,
                    message: format!("failed to encode resized PNG: {e}"),
                })?;
        }
        ScreenshotFormat::Jpeg { quality } => {
            let rgb = resized.to_rgb8();
            JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
                .write_image(rgb.as_raw(), rw, rh, image::ExtendedColorType::Rgb8)
                .map_err(|e| MimikyuError::Protocol {
                    code: -1,
                    message: format!("failed to encode resized JPEG: {e}"),
                })?;
        }
    }

    Ok(MediaCapture {
        bytes: out,
        format,
        width: rw,
        height: rh,
    })
}

pub async fn export_pdf(session: &dyn DriverSession) -> Result<Vec<u8>> {
    session.pdf().await
}

pub async fn set_instant_animations(session: &dyn DriverSession, enabled: bool) -> Result<()> {
    session.set_instant_animations(enabled).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    #[test]
    fn apply_max_dimensions_preserves_aspect_ratio_for_png_and_jpeg() {
        let img = ImageBuffer::from_pixel(400, 200, Rgba([10u8, 20, 30, 255]));
        let mut png_bytes = Vec::new();
        PngEncoder::new(&mut png_bytes)
            .write_image(img.as_raw(), 400, 200, image::ExtendedColorType::Rgba8)
            .unwrap();

        let unchanged =
            apply_max_dimensions(png_bytes.clone(), ScreenshotFormat::Png, Some(800), Some(600))
                .unwrap();
        assert_eq!((unchanged.width, unchanged.height), (400, 200));
        assert_eq!(unchanged.bytes.len(), png_bytes.len());

        let scaled_png =
            apply_max_dimensions(png_bytes.clone(), ScreenshotFormat::Png, Some(100), None)
                .unwrap();
        assert_eq!((scaled_png.width, scaled_png.height), (100, 50));
        assert!(scaled_png.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));

        let scaled_jpeg = apply_max_dimensions(
            png_bytes,
            ScreenshotFormat::Jpeg { quality: 80 },
            None,
            Some(80),
        )
        .unwrap();
        assert_eq!((scaled_jpeg.width, scaled_jpeg.height), (160, 80));
        assert!(scaled_jpeg.bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
    }
}
