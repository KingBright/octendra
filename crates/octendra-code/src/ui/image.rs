//! Bounded observation images. Real-time media stays on Weavelo's native path.
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use octendra_desktop_protocol::RawPixels;
use serde_json::{Value, json};

pub(super) struct Image {
    pub size: [u32; 2],
    pub rgb: Vec<u8>,
    pub jpeg: Vec<u8>,
}
impl Image {
    pub fn from_pixels(pixels: &RawPixels, bytes: &[u8]) -> Result<Self> {
        let [w, h] = pixels.size;
        ensure!(w > 0 && h > 0 && w <= 8192 && h <= 8192, "invalid_ui_image");
        let packed = matches!(pixels.encoding.as_str(), "rgba8" | "rgba8_srgb");
        let count = w as usize * h as usize;
        ensure!(
            if packed {
                bytes.len() == count * 4
            } else {
                matches!(pixels.encoding.as_str(), "nv12" | "i420")
                    && w % 2 == 0
                    && h % 2 == 0
                    && bytes.len() == count * 3 / 2
                    && pixels.color[0] == 2
                    && pixels.color[1] == 3
                    && matches!(pixels.color[2], 5 | 7)
                    && pixels.color[3] == 1
            },
            "unsupported_ui_pixel_format"
        );
        let mut limit = 1280u32;
        loop {
            let scale = (f64::from(limit) / f64::from(w.max(h))).min(1.0);
            let size = [
                (f64::from(w) * scale).round().max(1.0) as u32,
                (f64::from(h) * scale).round().max(1.0) as u32,
            ];
            let mut rgb = Vec::with_capacity(size[0] as usize * size[1] as usize * 3);
            for y in 0..size[1] {
                for x in 0..size[0] {
                    let sx = sample(x, size[0], w) as usize;
                    let sy = sample(y, size[1], h) as usize;
                    let i = sy * w as usize + sx;
                    if packed {
                        rgb.extend_from_slice(&bytes[i * 4..i * 4 + 3]);
                    } else {
                        let uv = sy / 2 * (w as usize / 2) + sx / 2;
                        let (u, v) = if pixels.encoding == "nv12" {
                            (bytes[count + uv * 2], bytes[count + uv * 2 + 1])
                        } else {
                            (bytes[count + uv], bytes[count + count / 4 + uv])
                        };
                        let luma = (f64::from(bytes[i]) - 16.0) * (255.0 / 219.0);
                        let u = (f64::from(u) - 128.0) * (255.0 / 224.0);
                        let v = (f64::from(v) - 128.0) * (255.0 / 224.0);
                        rgb.extend(
                            [
                                luma + 1.5748 * v,
                                luma - 0.187324 * u - 0.468124 * v,
                                luma + 1.8556 * u,
                            ]
                            .map(|c| c.round().clamp(0.0, 255.0) as u8),
                        );
                    }
                }
            }
            for quality in [85, 65, 45] {
                let mut jpeg = Vec::new();
                jpeg_encoder::Encoder::new(&mut jpeg, quality).encode(
                    &rgb,
                    size[0] as u16,
                    size[1] as u16,
                    jpeg_encoder::ColorType::Rgb,
                )?;
                if jpeg.len() <= 112 * 1024 {
                    return Ok(Self { size, rgb, jpeg });
                }
            }
            ensure!(limit > 160, "ui_image_budget_exceeded");
            limit /= 2;
        }
    }
    pub fn value(&self) -> Value {
        json!({"mime_type":"image/jpeg","width":self.size[0],"height":self.size[1],
            "data":STANDARD.encode(&self.jpeg)})
    }
    pub fn tiles(&self) -> Vec<String> {
        let [w, h] = self.size;
        (0..h.div_ceil(64))
            .flat_map(|ty| {
                (0..w.div_ceil(64)).map(move |tx| {
                    let mut tile = Vec::new();
                    for y in ty * 64..((ty + 1) * 64).min(h) {
                        let start = (y * w + tx * 64) as usize * 3;
                        let end = (y * w + ((tx + 1) * 64).min(w)) as usize * 3;
                        tile.extend_from_slice(&self.rgb[start..end]);
                    }
                    crate::hash(tile)
                })
            })
            .collect()
    }
}
fn sample(point: u32, size: u32, original: u32) -> u32 {
    if size <= 1 {
        0
    } else {
        (u64::from(point) * u64::from(original - 1) / u64::from(size - 1)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn yuv_video_range_and_rgba_have_real_pixels() {
        let mut meta = RawPixels {
            encoding: "nv12".into(),
            size: [2, 2],
            color: [2, 3, 5, 1],
            plane_lengths: vec![4, 2],
            plane_sha256: vec![],
        };
        let image = Image::from_pixels(&meta, &[16, 235, 16, 235, 128, 128]).unwrap();
        assert_eq!(&image.rgb[..6], &[0, 0, 0, 255, 255, 255]);
        assert_eq!(&image.jpeg[..2], &[0xff, 0xd8]);
        meta.encoding = "rgba8_srgb".into();
        meta.color = [0; 4];
        assert_eq!(
            Image::from_pixels(&meta, &[255, 0, 0, 255].repeat(4))
                .unwrap()
                .rgb,
            [255, 0, 0].repeat(4)
        );
        meta.encoding = "nv12".into();
        assert!(Image::from_pixels(&meta, &[0; 6]).is_err());
    }
}
