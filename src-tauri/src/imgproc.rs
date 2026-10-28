//! 画像処理（Python 版で OpenCV / Pillow を使っていた部分の置き換え）．
//!
//! 参照画像・数字の見本は Python 版で作ったものをそのまま使うので，
//! 切り抜き位置の丸め・グレースケールの係数・INTER_AREA の縮小などは元の計算に合わせてある．

use image::{imageops, imageops::FilterType, GrayImage, Luma, Rgb, RgbImage};

/// Python の round()（偶数丸め）．
pub fn py_round(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// クライアント比率 [x, y, w, h] で切り抜く（Python 版の crop_rel と同じ丸め）．
pub fn crop_rel(img: &RgbImage, region: [f64; 4]) -> RgbImage {
    let (w, h) = (img.width() as i64, img.height() as i64);
    let [x, y, rw, rh] = region;
    let b0 = py_round(x * w as f64);
    let b1 = py_round(y * h as f64);
    let b2 = py_round((x + rw) * w as f64);
    let b3 = py_round((y + rh) * h as f64);
    let l = b0.clamp(0, w - 1);
    let t = b1.clamp(0, h - 1);
    let r = w.min((b0 + 1).max(b2)).max(l + 1);
    let b = h.min((b1 + 1).max(b3)).max(t + 1);
    imageops::crop_imm(img, l as u32, t as u32, (r - l) as u32, (b - t) as u32).to_image()
}

/// Pillow の convert("L") と同じ係数．
#[inline]
pub fn pil_luma(p: &Rgb<u8>) -> u8 {
    ((p[0] as u32 * 19595 + p[1] as u32 * 38470 + p[2] as u32 * 7471 + 0x8000) >> 16) as u8
}

/// OpenCV の COLOR_RGB2GRAY と同じ係数．
#[inline]
fn cv_luma(p: &Rgb<u8>) -> u8 {
    ((p[0] as u32 * 4899 + p[1] as u32 * 9617 + p[2] as u32 * 1868 + 8192) >> 14) as u8
}

pub fn to_gray(img: &RgbImage) -> GrayImage {
    GrayImage::from_fn(img.width(), img.height(), |x, y| Luma([pil_luma(img.get_pixel(x, y))]))
}

pub fn brightness(img: &RgbImage) -> f64 {
    let n = (img.width() * img.height()).max(1) as f64;
    img.pixels().map(|p| pil_luma(p) as f64).sum::<f64>() / n
}

pub fn resize_rgb(img: &RgbImage, w: u32, h: u32, filter: FilterType) -> RgbImage {
    if img.width() == w && img.height() == h {
        return img.clone();
    }
    imageops::resize(img, w.max(1), h.max(1), filter)
}

/// 白っぽい画素（RGB の最小値が threshold を超える）の割合．
pub fn white_ratio(img: &RgbImage, threshold: u8) -> f64 {
    let n = (img.width() * img.height()).max(1) as f64;
    img.pixels().filter(|p| p[0].min(p[1]).min(p[2]) > threshold).count() as f64 / n
}

// --------------------------------------------------------------------------
// 浮動小数の 1 チャンネル画像
// --------------------------------------------------------------------------
pub struct FloatImg {
    pub w: usize,
    pub h: usize,
    pub d: Vec<f32>,
}

impl FloatImg {
    pub fn cv_gray(img: &RgbImage) -> Self {
        FloatImg {
            w: img.width() as usize,
            h: img.height() as usize,
            d: img.pixels().map(|p| cv_luma(p) as f32).collect(),
        }
    }

    pub fn std(&self) -> f64 {
        let n = self.d.len().max(1) as f64;
        let m = self.d.iter().map(|&v| v as f64).sum::<f64>() / n;
        (self.d.iter().map(|&v| (v as f64 - m).powi(2)).sum::<f64>() / n).sqrt()
    }

    pub fn mean(&self) -> f64 {
        self.d.iter().map(|&v| v as f64).sum::<f64>() / self.d.len().max(1) as f64
    }
}

/// cv2.matchTemplate(..., TM_CCOEFF_NORMED)．
pub fn match_template(img: &FloatImg, tpl: &FloatImg) -> FloatImg {
    let (iw, ih, tw, th) = (img.w, img.h, tpl.w, tpl.h);
    if tw > iw || th > ih || tw == 0 || th == 0 {
        return FloatImg { w: 0, h: 0, d: vec![] };
    }
    let (rw, rh) = (iw - tw + 1, ih - th + 1);
    let n = (tw * th) as f64;
    let tmean = tpl.d.iter().map(|&v| v as f64).sum::<f64>() / n;
    let tc: Vec<f64> = tpl.d.iter().map(|&v| v as f64 - tmean).collect();
    let tnorm = tc.iter().map(|v| v * v).sum::<f64>().sqrt();
    if tnorm < f64::EPSILON {
        return FloatImg { w: rw, h: rh, d: vec![1.0; rw * rh] };
    }
    // 窓内の和・二乗和は積分画像で求める
    let sw = iw + 1;
    let mut s = vec![0f64; sw * (ih + 1)];
    let mut s2 = vec![0f64; sw * (ih + 1)];
    for y in 0..ih {
        let (mut rs, mut rs2) = (0.0, 0.0);
        for x in 0..iw {
            let v = img.d[y * iw + x] as f64;
            rs += v;
            rs2 += v * v;
            s[(y + 1) * sw + x + 1] = s[y * sw + x + 1] + rs;
            s2[(y + 1) * sw + x + 1] = s2[y * sw + x + 1] + rs2;
        }
    }
    let rect = |a: &Vec<f64>, x: usize, y: usize| a[(y + th) * sw + x + tw] - a[y * sw + x + tw] - a[(y + th) * sw + x] + a[y * sw + x];
    let mut out = vec![0f32; rw * rh];
    for y in 0..rh {
        for x in 0..rw {
            let mut num = 0.0;
            for j in 0..th {
                let row = &img.d[(y + j) * iw + x..(y + j) * iw + x + tw];
                let trow = &tc[j * tw..(j + 1) * tw];
                num += row.iter().zip(trow).map(|(&a, &b)| a as f64 * b).sum::<f64>();
            }
            let wsum = rect(&s, x, y);
            let wsum2 = rect(&s2, x, y);
            let t = (wsum2 - wsum * wsum / n).max(0.0).sqrt() * tnorm;
            // OpenCV と同じ扱い（分母がほぼ 0 の窓）
            let r = if num.abs() < t {
                num / t
            } else if num.abs() < t * 1.125 {
                num.signum()
            } else {
                0.0
            };
            out[y * rw + x] = r as f32;
        }
    }
    FloatImg { w: rw, h: rh, d: out }
}

/// 最大値の位置（同じ値なら先に見つかった方）．
pub fn max_loc(img: &FloatImg) -> (usize, usize) {
    let mut best = (0, 0);
    let mut bv = f32::NEG_INFINITY;
    for y in 0..img.h {
        for x in 0..img.w {
            let v = img.d[y * img.w + x];
            if v > bv {
                bv = v;
                best = (x, y);
            }
        }
    }
    best
}

// --------------------------------------------------------------------------
// HSV ヒストグラム（cv2.calcHist([hsv], [0, 1], None, [30, 32], [0, 180, 0, 256]) → normalize → CORREL）
// --------------------------------------------------------------------------
fn hsv_tables() -> &'static ([i32; 256], [i32; 256]) {
    static T: std::sync::OnceLock<([i32; 256], [i32; 256])> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let (mut sdiv, mut hdiv) = ([0i32; 256], [0i32; 256]);
        for i in 1..256 {
            sdiv[i] = ((255 << 12) as f64 / i as f64).round_ties_even() as i32;
            hdiv[i] = ((180 << 12) as f64 / (6.0 * i as f64)).round_ties_even() as i32;
        }
        (sdiv, hdiv)
    })
}

pub fn hs_hist(img: &RgbImage) -> Vec<f64> {
    let (sdiv, hdiv) = hsv_tables();
    let mut hist = vec![0f64; 30 * 32];
    for p in img.pixels() {
        let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
        let v = r.max(g).max(b);
        let diff = v - r.min(g).min(b);
        let vr = if v == r { -1 } else { 0 };
        let vg = if v == g { -1 } else { 0 };
        let s = (diff * sdiv[v as usize] + (1 << 11)) >> 12;
        let mut h = (vr & (g - b)) + (!vr & ((vg & (b - r + 2 * diff)) + ((!vg) & (r - g + 4 * diff))));
        h = (h * hdiv[diff as usize] + (1 << 11)) >> 12;
        if h < 0 {
            h += 180;
        }
        let hb = h / 6;
        let sb = s / 8;
        if (0..30).contains(&hb) && (0..32).contains(&sb) {
            hist[(hb * 32 + sb) as usize] += 1.0;
        }
    }
    let norm = hist.iter().map(|v| v * v).sum::<f64>().sqrt();
    if norm > 0.0 {
        hist.iter_mut().for_each(|v| *v /= norm);
    }
    hist
}

pub fn hist_correl(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut num, mut da, mut db) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        num += (x - ma) * (y - mb);
        da += (x - ma).powi(2);
        db += (y - mb).powi(2);
    }
    let den = da * db;
    if den.abs() > f64::EPSILON {
        num / den.sqrt()
    } else {
        1.0
    }
}

// --------------------------------------------------------------------------
// 2 値画像
// --------------------------------------------------------------------------
#[derive(Clone)]
pub struct Mask {
    pub w: usize,
    pub h: usize,
    pub d: Vec<u8>,
}

impl Mask {
    pub fn from_fn(img: &RgbImage, f: impl Fn(&Rgb<u8>) -> bool) -> Self {
        Mask {
            w: img.width() as usize,
            h: img.height() as usize,
            d: img.pixels().map(|p| f(p) as u8).collect(),
        }
    }

    pub fn zeros(w: usize, h: usize) -> Self {
        Mask { w, h, d: vec![0; w * h] }
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize) -> bool {
        self.d[y * self.w + x] != 0
    }

    /// 列 [a, b) を切り出す．
    pub fn cols(&self, a: usize, b: usize) -> Mask {
        let mut out = Mask::zeros(b - a, self.h);
        for y in 0..self.h {
            out.d[y * out.w..(y + 1) * out.w].copy_from_slice(&self.d[y * self.w + a..y * self.w + b]);
        }
        out
    }

    pub fn count(&self) -> usize {
        self.d.iter().filter(|&&v| v != 0).count()
    }

    /// インクのある範囲 (xmin, xmax, ymin, ymax)．
    pub fn bbox(&self) -> Option<(usize, usize, usize, usize)> {
        let (mut x0, mut x1, mut y0, mut y1) = (usize::MAX, 0, usize::MAX, 0);
        for y in 0..self.h {
            for x in 0..self.w {
                if self.get(x, y) {
                    x0 = x0.min(x);
                    x1 = x1.max(x);
                    y0 = y0.min(y);
                    y1 = y1.max(y);
                }
            }
        }
        (x0 != usize::MAX).then_some((x0, x1, y0, y1))
    }
}

/// k×k の正方形カーネルでの最大値／最小値フィルタ（cv2.dilate / cv2.erode．画像の外は無視）．
fn rank_filter(m: &Mask, k: usize, take_max: bool) -> Mask {
    let a = (k / 2) as isize;
    let pass = |src: &Mask, horizontal: bool| -> Mask {
        let mut out = Mask::zeros(src.w, src.h);
        for y in 0..src.h {
            for x in 0..src.w {
                let mut v = if take_max { 0u8 } else { 1u8 };
                for i in 0..k as isize {
                    let (sx, sy) = if horizontal { (x as isize + i - a, y as isize) } else { (x as isize, y as isize + i - a) };
                    if sx < 0 || sy < 0 || sx >= src.w as isize || sy >= src.h as isize {
                        continue;
                    }
                    let s = src.d[sy as usize * src.w + sx as usize];
                    v = if take_max { v.max(s) } else { v.min(s) };
                }
                out.d[y * src.w + x] = v;
            }
        }
        out
    };
    pass(&pass(m, true), false)
}

/// cv2.morphologyEx(MORPH_CLOSE, np.ones((k, k)))．
pub fn morph_close(m: &Mask, k: usize) -> Mask {
    rank_filter(&rank_filter(m, k, true), k, false)
}

#[derive(Clone, Copy, Debug)]
pub struct Comp {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub area: usize,
}

/// 8 近傍の連結成分．ラベルは 1 から（0 は背景），comps[i] がラベル i + 1．
pub fn connected_components(m: &Mask) -> (Vec<u32>, Vec<Comp>) {
    let mut lab = vec![0u32; m.w * m.h];
    let mut comps = Vec::new();
    let mut stack = Vec::new();
    for y0 in 0..m.h {
        for x0 in 0..m.w {
            if !m.get(x0, y0) || lab[y0 * m.w + x0] != 0 {
                continue;
            }
            let id = comps.len() as u32 + 1;
            let (mut x1, mut x2, mut y1, mut y2, mut area) = (x0, x0, y0, y0, 0);
            lab[y0 * m.w + x0] = id;
            stack.push((x0, y0));
            while let Some((x, y)) = stack.pop() {
                area += 1;
                x1 = x1.min(x);
                x2 = x2.max(x);
                y1 = y1.min(y);
                y2 = y2.max(y);
                for dy in -1isize..=1 {
                    for dx in -1isize..=1 {
                        let (nx, ny) = (x as isize + dx, y as isize + dy);
                        if nx < 0 || ny < 0 || nx >= m.w as isize || ny >= m.h as isize {
                            continue;
                        }
                        let idx = ny as usize * m.w + nx as usize;
                        if m.d[idx] != 0 && lab[idx] == 0 {
                            lab[idx] = id;
                            stack.push((nx as usize, ny as usize));
                        }
                    }
                }
            }
            comps.push(Comp { x: x1, y: y1, w: x2 - x1 + 1, h: y2 - y1 + 1, area });
        }
    }
    (lab, comps)
}

// --------------------------------------------------------------------------
// cv2.resize(..., interpolation=INTER_AREA)（数字の見本と同じ特徴量を作るため OpenCV の計算をなぞる）
// --------------------------------------------------------------------------
fn area_tab(ssize: usize, dsize: usize, scale: f64) -> Vec<(usize, usize, f64)> {
    let mut tab = Vec::new();
    for dx in 0..dsize {
        let fsx1 = dx as f64 * scale;
        let fsx2 = fsx1 + scale;
        let cell = scale.min(ssize as f64 - fsx1);
        let mut sx2 = fsx2.floor() as i64;
        let mut sx1 = fsx1.ceil() as i64;
        sx2 = sx2.min(ssize as i64 - 1);
        sx1 = sx1.min(sx2);
        if sx1 as f64 - fsx1 > 1e-3 {
            tab.push((dx, (sx1 - 1) as usize, (sx1 as f64 - fsx1) / cell));
        }
        for sx in sx1..sx2 {
            tab.push((dx, sx as usize, 1.0 / cell));
        }
        if fsx2 - sx2 as f64 > 1e-3 {
            tab.push((dx, sx2 as usize, (fsx2 - sx2 as f64).min(1.0).min(cell) / cell));
        }
    }
    tab
}

fn linear_tab(ssize: usize, dsize: usize, scale: f64, inv_scale: f64) -> Vec<(usize, f64)> {
    (0..dsize)
        .map(|dx| {
            let mut sx = (dx as f64 * scale).floor() as i64;
            let mut fx = (dx + 1) as f64 - (sx + 1) as f64 * inv_scale;
            fx = if fx <= 0.0 { 0.0 } else { fx - fx.floor() };
            if sx < 0 {
                fx = 0.0;
                sx = 0;
            }
            if sx >= ssize as i64 - 1 {
                fx = 0.0;
                sx = ssize as i64 - 1;
            }
            (sx as usize, fx)
        })
        .collect()
}

pub fn resize_area_f32(src: &[f32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<f32> {
    let (inv_sx, inv_sy) = (dw as f64 / sw as f64, dh as f64 / sh as f64);
    let (sx, sy) = (1.0 / inv_sx, 1.0 / inv_sy);
    let mut out = vec![0f64; dw * dh];
    if sx >= 1.0 && sy >= 1.0 {
        let tx = area_tab(sw, dw, sx);
        let ty = area_tab(sh, dh, sy);
        for &(dy, syi, ay) in &ty {
            for &(dx, sxi, ax) in &tx {
                out[dy * dw + dx] += ay * ax * src[syi * sw + sxi] as f64;
            }
        }
    } else {
        let tx = linear_tab(sw, dw, sx, inv_sx);
        let ty = linear_tab(sh, dh, sy, inv_sy);
        let at = |x: usize, y: usize| src[y.min(sh - 1) * sw + x.min(sw - 1)] as f64;
        for (dy, &(y0, fy)) in ty.iter().enumerate() {
            for (dx, &(x0, fx)) in tx.iter().enumerate() {
                let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
                let bot = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;
                out[dy * dw + dx] = top * (1.0 - fy) + bot * fy;
            }
        }
    }
    out.into_iter().map(|v| v as f32).collect()
}

/// PIL の ImageEnhance.Contrast(img).enhance(factor)．
pub fn contrast(img: &RgbImage, factor: f64) -> RgbImage {
    let n = (img.width() * img.height()).max(1) as f64;
    let mean = (img.pixels().map(|p| pil_luma(p) as f64).sum::<f64>() / n + 0.5).floor();
    let mut out = img.clone();
    for p in out.pixels_mut() {
        for c in 0..3 {
            p[c] = (mean + factor * (p[c] as f64 - mean)).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}
