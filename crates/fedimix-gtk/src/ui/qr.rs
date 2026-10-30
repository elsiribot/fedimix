//! QR code rendering: black modules on a white quiet zone, in every theme.

use gtk::gdk;
use gtk::prelude::*;

/// Quiet zone around the code, in modules.
const QUIET: usize = 4;
/// Preferred texture edge in pixels; each module is a whole number of pixels.
const TARGET_PX: usize = 240;

/// Dark-module grid of `data` (row-major) and its side length.
pub fn matrix(data: &str) -> Result<(usize, Vec<bool>), String> {
    let code = qrcode::QrCode::new(data.as_bytes()).map_err(|e| format!("QR code: {e}"))?;
    let side = code.width();
    let modules = code
        .to_colors()
        .into_iter()
        .map(|c| c == qrcode::Color::Dark)
        .collect();
    Ok((side, modules))
}

/// Texture of the code, `scale` pixels per module, quiet zone included.
pub fn texture(data: &str) -> Result<gdk::Texture, String> {
    let (side, modules) = matrix(data)?;
    let total = side + 2 * QUIET;
    let scale = (TARGET_PX / total).max(1);
    let px = total * scale;
    // Opaque white, then black modules.
    let mut rgb = vec![0xff_u8; px * px * 3];
    for y in 0..side {
        for x in 0..side {
            if !modules[y * side + x] {
                continue;
            }
            for dy in 0..scale {
                let row = ((y + QUIET) * scale + dy) * px;
                for dx in 0..scale {
                    let i = (row + (x + QUIET) * scale + dx) * 3;
                    rgb[i..i + 3].fill(0);
                }
            }
        }
    }
    let bytes = gtk::glib::Bytes::from_owned(rgb);
    Ok(gdk::MemoryTexture::new(
        px as i32,
        px as i32,
        gdk::MemoryFormat::R8g8b8,
        &bytes,
        px * 3,
    )
    .upcast())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_size_is_nonzero_data() {
        let (side, modules) = matrix("bitcoin:bc1qdemo").unwrap();
        assert!(side >= 21);
        assert_eq!(modules.len(), side * side);
        assert!(modules.iter().any(|m| *m));
        assert!(modules.iter().any(|m| !*m));
    }

    #[test]
    fn matrix_handles_long_invoice() {
        let data = "lnbc".repeat(500);
        assert_eq!(data.len(), 2000);
        let (side, modules) = matrix(&data).unwrap();
        assert!(side > 100);
        assert_eq!(modules.len(), side * side);
    }

    #[test]
    fn matrix_rejects_oversized_data() {
        assert!(matrix(&"x".repeat(4000)).is_err());
    }
}
