//! The pairing QR, drawn as an image: dark modules on transparent, one pixel
//! per module (the screen scales it up without smoothing), with no quiet zone
//! of its own (the white card around it is the quiet zone).

use qrcode::{Color, EcLevel, QrCode};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// The QR for `text`, or `None` if it can't be encoded (too long).
#[must_use]
pub fn image(text: &str) -> Option<Image> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M).ok()?;
    let n = u32::try_from(code.width()).ok()?;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(n, n);
    for (px, module) in buf.make_mut_slice().iter_mut().zip(code.to_colors()) {
        *px = match module {
            Color::Dark => Rgba8Pixel {
                r: 0x0c,
                g: 0x0d,
                b: 0x12,
                a: 255,
            },
            Color::Light => Rgba8Pixel {
                r: 255,
                g: 255,
                b: 255,
                a: 255,
            },
        };
    }
    Some(Image::from_rgba8(buf))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_pairing_link_fits() {
        let link = "tawny://pair?k=L_F7wC98BnTz78KPFl-bRg&n=Pet%20camera&h=192.168.1.20:8820\
                    &c=fpCEeBfXe2g&e=1795275874&np=79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let img = super::image(link).expect("encodes");
        assert!(img.size().width >= 41, "a real code: {}", img.size().width);
    }
}
