//! Read a connection claim out of a picture of its QR.
//!
//! The cabinet shows one secret as a code, a link, and a QR. A scan is the
//! picture: a PNG or JPEG file, including a photograph of the screen. The
//! text inside the QR is the same secret `--claim` already accepts. This
//! module does not redeem it and does not dial.

/// A phone photo larger than this is not a claim QR.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Longest side passed to the detector. Nearest-neighbour keeps the modules
/// square; a larger frame only makes the search slower.
const MAX_SIDE: u32 = 1600;

/// The text encoded in the first QR of `bytes`, or why the picture is not one.
pub fn claim_text_from_image(bytes: &[u8]) -> Result<String, String> {
    if bytes.is_empty() {
        return Err("empty image".to_string());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("image is too large".to_string());
    }
    let img = image::load_from_memory(bytes).map_err(|_| "not a png or jpeg".to_string())?;
    let (width, height) = (img.width(), img.height());
    if width == 0 || height == 0 {
        return Err("empty image".to_string());
    }
    let long = width.max(height);
    let gray = if long > MAX_SIDE {
        let scale = MAX_SIDE as f32 / long as f32;
        img.resize(
            ((width as f32) * scale).max(1.0) as u32,
            ((height as f32) * scale).max(1.0) as u32,
            image::imageops::FilterType::Nearest,
        )
        .to_luma8()
    } else {
        img.to_luma8()
    };
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    let mut prep = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| gray.get_pixel(x as u32, y as u32)[0]);
    let grids = prep.detect_grids();
    let grid = grids.first().ok_or_else(|| "no qr in the image".to_string())?;
    let (_meta, content) = grid.decode().map_err(|_| "qr could not be read".to_string())?;
    let text = content.trim().to_string();
    if text.is_empty() {
        return Err("qr was empty".to_string());
    }
    Ok(text)
}

/// Encode `payload` as a QR and return PNG and JPEG bytes a client can be
/// handed instead of a camera frame.
#[cfg(test)]
fn render_qr(payload: &str) -> (Vec<u8>, Vec<u8>) {
    use std::io::Cursor;
    let code = qrcode::QrCode::new(payload.as_bytes()).expect("payload fits a qr");
    let buffer = code.render::<image::Luma<u8>>().min_dimensions(240, 240).build();
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(buffer.clone())
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("png");
    let mut jpeg = Cursor::new(Vec::new());
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95);
    encoder
        .encode(buffer.as_raw(), buffer.width(), buffer.height(), image::ExtendedColorType::L8)
        .expect("jpeg");
    (png.into_inner(), jpeg.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const LINK: &str = "https://panel.m1r4g3.bid/c/0123456789abcdef0123456789abcdef";
    const CODE: &str = "0123-4567-89ab-cdef-0123-4567-89ab-cdef";

    #[test]
    fn a_png_of_the_link_qr_reads_back_the_link() {
        let (png, _) = render_qr(LINK);
        let text = claim_text_from_image(&png).expect("png qr");
        assert_eq!(text, LINK);
    }

    #[test]
    fn a_jpeg_of_the_code_qr_reads_back_the_code() {
        let (_, jpeg) = render_qr(CODE);
        let text = claim_text_from_image(&jpeg).expect("jpeg qr");
        assert_eq!(text, CODE);
    }

    #[test]
    fn a_plain_picture_is_not_a_claim() {
        let image = image::ImageBuffer::<image::Rgb<u8>, Vec<u8>>::from_pixel(80, 80, image::Rgb([20, 40, 60]));
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).expect("png");
        let err = claim_text_from_image(&png.into_inner()).expect_err("no qr");
        assert_eq!(err, "no qr in the image");
    }

    #[test]
    fn a_text_file_is_not_an_image() {
        let err = claim_text_from_image(b"https://panel.m1r4g3.bid/c/0123456789abcdef0123456789abcdef").expect_err("text");
        assert_eq!(err, "not a png or jpeg");
    }
}
