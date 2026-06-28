// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Optional AI matte refinement.
//!
//! Our color-key [`remove_matte`](ogre_core::remove_matte) produces a lossless,
//! crisply-decontaminated cut-out of the body, but cannot resolve background
//! trapped deep between fine structures (hair strands). This module runs a
//! pretrained segmentation model (IS-Net, general use) via pure-Rust ONNX
//! inference ([`tract`](tract_onnx)) and uses its matte to *refine* that result:
//! it only **erodes** — clearing solid color-key regions the model is confident
//! are background (the hair gaps) — and never touches the color-key's already
//! precise partial-alpha silhouette, so the crisp body edges are preserved.
//!
//! The ~170 MB model (Apache-2.0) is fetched on first use into the user cache
//! (`$XDG_CACHE_HOME/arte-ogre`) and reused thereafter.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use ogre_core::{IVec2, Rect, Rgba32F, TiledBuffer};
use sha2::{Digest, Sha256};
use tract_onnx::prelude::*;

use crate::color::linear_to_srgb;
use crate::error::IoError;

/// Square input resolution the IS-Net model expects.
const INPUT: usize = 1024;
/// Filename of the cached model.
const MODEL_FILE: &str = "isnet-general-use.onnx";
/// Where the model is fetched from on first use (rembg's release asset).
const MODEL_URL: &str =
    "https://github.com/danielgatis/rembg/releases/download/v0.0.0/isnet-general-use.onnx";
/// Expected model size in bytes — a sanity check against a truncated download.
const MODEL_BYTES: u64 = 178_648_008;
/// SHA-256 of the legitimate model, pinned so a tampered same-size download is
/// rejected before it ever reaches the ONNX runtime. HTTPS alone does not stop
/// a compromised/redirected release asset from serving a malicious model.
const MODEL_SHA256: &str = "60920e99c45464f2ba57bee2ad08c919a52bbf852739e96947fbb4358c0d964a";

/// Model-matte value below which a *solid* color-key pixel is treated as
/// background and eroded. Above it the color-key result is kept untouched.
const BG_THRESHOLD: f32 = 0.5;
/// Color-key alpha at/above which a pixel is "solid" and thus eligible for ML
/// erosion. Below it the pixel is part of the color-key's precise anti-aliased
/// edge and is left exactly as-is, so the crisp silhouette is never softened.
const SOLID_ALPHA: f32 = 0.9;

type Plan = Arc<TypedRunnableModel>;

fn ml_err<E: std::fmt::Display>(e: E) -> IoError {
    IoError::Ml(e.to_string())
}

/// The user cache directory for Arte Ogre (`$XDG_CACHE_HOME/arte-ogre` or
/// `$HOME/.cache/arte-ogre`).
fn cache_dir() -> Result<PathBuf, IoError> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or_else(|| IoError::Ml("no cache directory (set HOME or XDG_CACHE_HOME)".into()))?;
    Ok(base.join("arte-ogre"))
}

/// Whether the model is already downloaded (a fully-sized file in the cache),
/// without touching the network. Used by the UI to only warn about the one-time
/// download when it actually still has to happen.
pub fn model_is_cached() -> bool {
    cache_dir()
        .ok()
        .map(|d| d.join(MODEL_FILE))
        .and_then(|p| p.metadata().ok())
        .map(|m| m.len() == MODEL_BYTES)
        .unwrap_or(false)
}

/// Return the path to the cached model, downloading it on first use.
///
/// The download streams to a `.part` file and is atomically renamed on success,
/// so an interrupted fetch never leaves a half-written model in place.
pub fn ensure_model() -> Result<PathBuf, IoError> {
    let path = cache_dir()?.join(MODEL_FILE);
    if path.metadata().map(|m| m.len()).unwrap_or(0) == MODEL_BYTES {
        return Ok(path);
    }
    std::fs::create_dir_all(path.parent().expect("has parent"))?;
    let tmp = path.with_extension("part");
    let resp = ureq::get(MODEL_URL).call().map_err(ml_err)?;
    let mut reader = resp.into_parts().1.into_reader();
    {
        let mut file = std::fs::File::create(&tmp)?;
        std::io::copy(&mut reader, &mut file)?;
    }
    let got = tmp.metadata()?.len();
    if got != MODEL_BYTES {
        let _ = std::fs::remove_file(&tmp);
        return Err(IoError::Ml(format!(
            "model download incomplete: {got} of {MODEL_BYTES} bytes"
        )));
    }
    // Integrity gate: never load bytes we can't pin to the known-good model.
    if model_sha256(&tmp)? != MODEL_SHA256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(IoError::Ml(
            "model checksum mismatch — refusing to load (possible tampering)".into(),
        ));
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Stream a file through SHA-256 and return its lowercase-hex digest.
fn model_sha256(path: &Path) -> Result<String, IoError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Optimize + cache the runnable model so repeated refinements pay the ~170 MB
/// load cost only once per process.
fn model(path: &Path) -> Result<&'static Plan, IoError> {
    static MODEL: OnceLock<Plan> = OnceLock::new();
    if let Some(m) = MODEL.get() {
        return Ok(m);
    }
    let plan = tract_onnx::onnx()
        .model_for_path(path)
        .map_err(ml_err)?
        .with_input_fact(0, f32::fact([1, 3, INPUT, INPUT]).into())
        .map_err(ml_err)?
        .into_optimized()
        .map_err(ml_err)?
        .into_runnable()
        .map_err(ml_err)?;
    Ok(MODEL.get_or_init(|| plan))
}

/// Run the model on `buffer`'s RGB and return a foreground matte in `0..=1`,
/// row-major over the buffer's exact occupied bounds.
fn infer(
    buffer: &TiledBuffer,
    model_path: &Path,
) -> Result<(Rect, usize, usize, Vec<f32>), IoError> {
    let bounds = buffer
        .exact_bounds()
        .ok_or_else(|| IoError::Ml("layer is empty".into()))?;
    let (w, h) = (bounds.w as usize, bounds.h as usize);
    let data = buffer.read_rect(bounds);

    // The model wants 8-bit sRGB; our buffer is linear straight-alpha. Flatten to
    // RGB (ignoring alpha — the input is the opaque source over its matte).
    let to_u8 = |c: f32| (linear_to_srgb(c).clamp(0.0, 1.0) * 255.0).round() as u8;
    let img = image::RgbImage::from_fn(w as u32, h as u32, |x, y| {
        let p = data[y as usize * w + x as usize];
        image::Rgb([to_u8(p.r), to_u8(p.g), to_u8(p.b)])
    });
    let small = image::imageops::resize(
        &img,
        INPUT as u32,
        INPUT as u32,
        image::imageops::FilterType::Lanczos3,
    );

    // NCHW, normalized to roughly [-0.5, 0.5] (IS-Net: mean 0.5, std 1.0).
    let input =
        tract_ndarray::Array4::<f32>::from_shape_fn((1, 3, INPUT, INPUT), |(_, c, y, x)| {
            small.get_pixel(x as u32, y as u32)[c] as f32 / 255.0 - 0.5
        });
    let plan = model(model_path)?;
    let result = plan
        .run(tvec!(input.into_tensor().into()))
        .map_err(ml_err)?;
    let view = result[0].to_plain_array_view::<f32>().map_err(ml_err)?;
    let flat: Vec<f32> = view.iter().copied().collect();
    if flat.len() != INPUT * INPUT {
        return Err(IoError::Ml(format!(
            "unexpected model output: {} values",
            flat.len()
        )));
    }

    // Min-max normalize to [0,1], as rembg does for IS-Net.
    let mi = flat.iter().copied().fold(f32::INFINITY, f32::min);
    let ma = flat.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let scale = if ma > mi { 1.0 / (ma - mi) } else { 1.0 };
    let mask_small = image::GrayImage::from_fn(INPUT as u32, INPUT as u32, |x, y| {
        let v = (flat[y as usize * INPUT + x as usize] - mi) * scale;
        image::Luma([(v.clamp(0.0, 1.0) * 255.0) as u8])
    });
    let mask = image::imageops::resize(
        &mask_small,
        w as u32,
        h as u32,
        image::imageops::FilterType::Lanczos3,
    );
    let matte = mask.pixels().map(|p| p[0] as f32 / 255.0).collect();
    Ok((bounds, w, h, matte))
}

/// Refine a color-key result by eroding solid regions the model is confident are
/// background. `original` is the un-cut source (what the model segments);
/// `colorkey` is the [`remove_matte`](ogre_core::remove_matte) output to refine.
/// Returns a new buffer; the color-key's precise edges are preserved.
pub fn refine_with_ml(
    colorkey: &TiledBuffer,
    original: &TiledBuffer,
    model_path: &Path,
) -> Result<TiledBuffer, IoError> {
    let (bounds, w, h, matte) = infer(original, model_path)?;
    let mut out = colorkey.clone();
    for j in 0..h {
        for i in 0..w {
            let m = matte[j * w + i];
            if m >= BG_THRESHOLD {
                continue; // model agrees it is foreground — keep color-key exactly
            }
            let local = IVec2::new(bounds.x + i as i32, bounds.y + j as i32);
            let px = out.get_pixel(local);
            if px.a < SOLID_ALPHA {
                continue; // a color-key edge pixel — never soften the silhouette
            }
            // Solid color-key pixel the model calls background: erode it.
            let factor = smoothstep(0.0, BG_THRESHOLD, m);
            out.set_pixel(local, Rgba32F::new(px.r, px.g, px.b, px.a * factor));
        }
    }
    out.prune_empty_tiles();
    Ok(out)
}

/// Hermite smoothstep, clamped.
fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_sha256_streams_and_hex_encodes() {
        // Known-answer test: SHA-256("abc") is a fixed vector. Verifies the
        // streaming hash + hex formatting that gates the model download.
        let p = std::env::temp_dir().join(format!("arte-ogre-sha-{}.bin", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            model_sha256(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn smoothstep_endpoints_and_midpoint() {
        assert_eq!(smoothstep(0.0, 0.5, 0.0), 0.0);
        assert_eq!(smoothstep(0.0, 0.5, 0.5), 1.0);
        assert_eq!(smoothstep(0.0, 0.5, 0.6), 1.0); // clamps above hi
        assert!((smoothstep(0.0, 0.5, 0.25) - 0.5).abs() < 1e-6);
    }
}
