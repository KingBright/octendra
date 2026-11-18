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
    fn mark_reticle(&self, point: [f64; 2]) -> Option<([i32; 2], Vec<u8>)> {
        let [w, h] = [self.size[0] as i32, self.size[1] as i32];
        if w < 8 || h < 8 {
            return None;
        }
        let cx = point[0].round().clamp(0.0, f64::from(w - 1)) as i32;
        let cy = point[1].round().clamp(0.0, f64::from(h - 1)) as i32;
        let mut marked = self.rgb.clone();
        let accent = [255u8, 40, 200];
        let border = [15u8, 23, 42];
        for dy in -14i32..=14 {
            for dx in -14i32..=14 {
                let px = cx + dx;
                let py = cy + dy;
                if px < 0 || px >= w || py < 0 || py >= h {
                    continue;
                }
                if dx.abs() <= 3 && dy.abs() <= 3 {
                    // Preserve 7x7 hollow center so glyph/icon pixels remain readable.
                    continue;
                }
                let r2 = dx * dx + dy * dy;
                let on_cross = (dx == 0 && dy.abs() >= 4 && dy.abs() <= 13)
                    || (dy == 0 && dx.abs() >= 4 && dy.abs() <= 13);
                let on_cross_border = (dx.abs() == 1 && dy.abs() >= 4 && dy.abs() <= 13)
                    || (dy.abs() == 1 && dx.abs() >= 4 && dx.abs() <= 13);
                let on_ring = (81..=110).contains(&r2);
                let on_ring_border = (64..81).contains(&r2) || (111..=130).contains(&r2);
                let color = if on_cross || on_ring {
                    Some(accent)
                } else if on_cross_border || on_ring_border {
                    Some(border)
                } else {
                    None
                };
                if let Some(rgb) = color {
                    let idx = ((py as usize) * (w as usize) + (px as usize)) * 3;
                    marked[idx..idx + 3].copy_from_slice(&rgb);
                }
            }
        }
        Some(([cx, cy], marked))
    }
    /// Encodes a model-visible JPEG with a 7x7 center-hollow crosshair reticle at `point`
    /// while leaving `self.rgb` (and therefore `self.tiles()`) untouched as pure screen pixels.
    pub fn value_with_reticle(&self, point: [f64; 2]) -> Value {
        let Some(([cx, cy], marked)) = self.mark_reticle(point) else {
            return self.value();
        };
        for quality in [85, 65, 45] {
            let mut jpeg = Vec::new();
            if jpeg_encoder::Encoder::new(&mut jpeg, quality)
                .encode(
                    &marked,
                    self.size[0] as u16,
                    self.size[1] as u16,
                    jpeg_encoder::ColorType::Rgb,
                )
                .is_ok()
                && jpeg.len() <= 112 * 1024
            {
                return json!({
                    "mime_type": "image/jpeg",
                    "width": self.size[0],
                    "height": self.size[1],
                    "reticle": [cx, cy],
                    "data": STANDARD.encode(&jpeg)
                });
            }
        }
        self.value()
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

/// Compute 0..1000 normalized `[x0, y0, x1, y1]` bounding boxes of changed 64x64 tiles.
pub(super) fn changed_regions(
    before_tiles: &[String],
    after_tiles: &[String],
    size: [u32; 2],
) -> Vec<[u32; 4]> {
    let [w, h] = size;
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let cols = w.div_ceil(64) as usize;
    let rows = h.div_ceil(64) as usize;
    if before_tiles.len() != cols * rows || after_tiles.len() != cols * rows {
        return vec![[0, 0, 1000, 1000]];
    }
    let mut changed = vec![false; cols * rows];
    let mut any = false;
    for i in 0..changed.len() {
        if before_tiles[i] != after_tiles[i] {
            changed[i] = true;
            any = true;
        }
    }
    if !any {
        return Vec::new();
    }
    let norm_x =
        |px: u32| ((f64::from(px) * 1000.0) / f64::from(w)).round().clamp(0.0, 1000.0) as u32;
    let norm_y =
        |py: u32| ((f64::from(py) * 1000.0) / f64::from(h)).round().clamp(0.0, 1000.0) as u32;
    let mut visited = vec![false; cols * rows];
    let mut regions = Vec::new();
    for start_y in 0..rows {
        for start_x in 0..cols {
            let start_idx = start_y * cols + start_x;
            if !changed[start_idx] || visited[start_idx] {
                continue;
            }
            visited[start_idx] = true;
            let mut queue = std::collections::VecDeque::from([(start_x, start_y)]);
            let (mut min_tx, mut min_ty, mut max_tx, mut max_ty) =
                (start_x, start_y, start_x, start_y);
            while let Some((tx, ty)) = queue.pop_front() {
                min_tx = min_tx.min(tx);
                min_ty = min_ty.min(ty);
                max_tx = max_tx.max(tx);
                max_ty = max_ty.max(ty);
                for (nx, ny) in [
                    (tx.wrapping_sub(1), ty),
                    (tx + 1, ty),
                    (tx, ty.wrapping_sub(1)),
                    (tx, ty + 1),
                ] {
                    if nx < cols && ny < rows {
                        let nidx = ny * cols + nx;
                        if changed[nidx] && !visited[nidx] {
                            visited[nidx] = true;
                            queue.push_back((nx, ny));
                        }
                    }
                }
            }
            let px0 = (min_tx as u32) * 64;
            let py0 = (min_ty as u32) * 64;
            let px1 = (((max_tx as u32) + 1) * 64).min(w);
            let py1 = (((max_ty as u32) + 1) * 64).min(h);
            let x0 = norm_x(px0);
            let y0 = norm_y(py0);
            let x1 = norm_x(px1).max(x0);
            let y1 = norm_y(py1).max(y0);
            regions.push([x0, y0, x1, y1]);
        }
    }
    regions.truncate(16);
    regions
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
    #[test]
    fn reticle_overlay_keeps_saved_tiles_clean() {
        let meta = RawPixels {
            encoding: "rgba8_srgb".into(),
            size: [64, 64],
            color: [0; 4],
            plane_lengths: vec![64 * 64 * 4],
            plane_sha256: vec![],
        };
        let image = Image::from_pixels(&meta, &[120u8, 130, 140, 255].repeat(64 * 64)).unwrap();
        let clean_tiles = image.tiles();
        let ([cx, cy], marked) = image.mark_reticle([32.0, 32.0]).unwrap();
        assert_eq!([cx, cy], [32, 32]);
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                let idx = (((cy + dy) as usize) * 64 + ((cx + dx) as usize)) * 3;
                assert_eq!(&marked[idx..idx + 3], &[120, 130, 140]);
            }
        }
        let cross_idx = ((32usize - 6) * 64 + 32) * 3;
        assert_ne!(&marked[cross_idx..cross_idx + 3], &[120, 130, 140]);
        let payload = image.value_with_reticle([32.0, 32.0]);
        assert_eq!(payload["reticle"], json!([32, 32]));
        assert_ne!(payload["data"], image.value()["data"]);
        assert_eq!(image.tiles(), clean_tiles);
    }
    #[test]
    fn changed_regions_merges_connected_tiles_and_normalizes_bounds() {
        let mut before = vec!["a".to_string(); 8];
        let mut after = before.clone();
        assert!(changed_regions(&before, &after, [256, 128]).is_empty());
        // Adjacent tiles (0,0) and (1,0) -> merged [0, 0, 500, 500], plus isolated (3,1) -> [750, 500, 1000, 1000].
        after[0] = "b".into();
        after[1] = "b".into();
        after[7] = "c".into();
        assert_eq!(
            changed_regions(&before, &after, [256, 128]),
            vec![[0, 0, 500, 500], [750, 500, 1000, 1000]]
        );
        before.pop();
        assert_eq!(
            changed_regions(&before, &after, [256, 128]),
            vec![[0, 0, 1000, 1000]]
        );
    }
}
