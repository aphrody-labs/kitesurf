//! Conversion of [`peniko::ImageData`] to vello_cpu [`Pixmap`]s.

use std::sync::Arc;
use vello_common::pixmap::PixelMetadata;
use vello_cpu::Pixmap;

/// Convert a [`peniko::ImageData`] to a premultiplied RGBA8 [`Pixmap`].
///
/// Equivalent to `ImageSource::from_peniko_image_data`, but computes an exact
/// transparency hint for already-premultiplied images.
///
/// # Panics
///
/// Panics if `image` has a `width` or `height` greater than `u16::MAX`.
pub(crate) fn convert_image(image: &peniko::ImageData) -> Arc<Pixmap> {
    assert!(
        image.width <= u16::MAX as u32 && image.height <= u16::MAX as u32,
        "The image is too big. Its width and height can be no larger than {} pixels.",
        u16::MAX,
    );
    let width = image.width.try_into().unwrap();
    let height = image.height.try_into().unwrap();

    let data = image.data.data();
    let pixel_bytes = data.len() & !3;
    let mut bytes = Vec::with_capacity(pixel_bytes);
    bytes.extend_from_slice(&data[..pixel_bytes]);

    match image.format {
        peniko::ImageFormat::Rgba8 => {}
        peniko::ImageFormat::Bgra8 => {
            for pixel in bytes.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        format => unimplemented!("Unsupported image format: {format:?}"),
    }

    // `Pixmap::from_parts` trusts a `false` hint and skips its premultiply
    // pass. Opaque pixels premultiply to themselves, so a read-only alpha scan
    // spares opaque straight-alpha images (canvas frames) the store pass.
    let may_have_transparency = !all_opaque(&bytes);

    Arc::new(Pixmap::from_parts(
        bytes,
        width,
        height,
        PixelMetadata::new(image.alpha_type, may_have_transparency),
    ))
}

/// Whether every RGBA8 pixel in `data` has alpha 255.
fn all_opaque(data: &[u8]) -> bool {
    // AND the alphas of a 64-byte block together so the inner loop
    // vectorizes, and stop at the first block with a translucent pixel.
    let (body, tail) = data.as_chunks::<64>();
    body.iter()
        .all(|block| block.as_chunks::<4>().0.iter().fold(255, |acc, p| acc & p[3]) == 255)
        && tail.as_chunks::<4>().0.iter().all(|p| p[3] == 255)
}

#[cfg(test)]
mod tests {
    use super::*;
    use peniko::{Blob, ImageAlphaType, ImageData, ImageFormat};

    fn image(pixels: &[[u8; 4]]) -> ImageData {
        ImageData {
            data: Blob::new(Arc::new(pixels.concat())),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width: pixels.len() as u32,
            height: 1,
        }
    }

    #[test]
    fn premultiply() {
        let pixmap = convert_image(&image(&[[100, 150, 200, 128], [10, 20, 30, 255]]));
        assert!(pixmap.may_have_transparency());
        let px = pixmap.data()[0];
        assert_eq!((px.r, px.g, px.b, px.a), (50, 75, 100, 128));
        let px = pixmap.data()[1];
        assert_eq!((px.r, px.g, px.b, px.a), (10, 20, 30, 255));
    }

    // 40 pixels: two 64-byte blocks (32 pixels) and an 8-pixel tail.
    #[test]
    fn premultiply_finds_one_translucent_pixel_in_a_block_or_the_tail() {
        for translucent in [20, 36] {
            let mut pixels = [[10, 20, 30, 255]; 40];
            pixels[translucent] = [100, 150, 200, 128];
            let pixmap = convert_image(&image(&pixels));
            assert!(pixmap.may_have_transparency(), "pixel {translucent}");
            let px = pixmap.data()[translucent];
            assert_eq!((px.r, px.g, px.b, px.a), (50, 75, 100, 128), "pixel {translucent}");
            let px = pixmap.data()[0];
            assert_eq!((px.r, px.g, px.b, px.a), (10, 20, 30, 255));
        }
    }

    #[test]
    fn premultiply_bgra_opaque() {
        let mut img = image(&[[1, 2, 3, 255]; 20]);
        img.format = ImageFormat::Bgra8;
        let pixmap = convert_image(&img);
        assert!(!pixmap.may_have_transparency());
        let px = pixmap.data()[0];
        assert_eq!((px.r, px.g, px.b, px.a), (3, 2, 1, 255));
    }
}
