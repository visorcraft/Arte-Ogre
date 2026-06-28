// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Color-space conversion helpers, including sRGB transfer functions and ICC
//! profile detection/conversion.

use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use flate2::read::ZlibDecoder;
use lcms2::{CIExyY, CIExyYTRIPLE, Intent, PixelFormat, Profile, ToneCurve, Transform};
use ogre_core::Rgba32F;

use crate::error::IoError;

/// PNG file signature.
const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RgbF32 {
    r: f32,
    g: f32,
    b: f32,
}

/// An ICC colour profile stored as raw bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IccProfile {
    bytes: Vec<u8>,
}

impl IccProfile {
    /// Wrap raw ICC profile bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Borrow the raw profile bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Take ownership of the raw profile bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Convert a linear-light value in [0, 1] to sRGB.
pub fn linear_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Convert an sRGB value in [0, 1] to linear light.
pub fn srgb_to_linear(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Pack linear straight-alpha pixels into `u8` RGBA bytes after sRGB encoding.
pub fn rgba32_to_srgb_u8(pixels: &[Rgba32F]) -> Vec<u8> {
    pixels
        .iter()
        .flat_map(|p| {
            [
                (linear_to_srgb(p.r) * 255.0).round() as u8,
                (linear_to_srgb(p.g) * 255.0).round() as u8,
                (linear_to_srgb(p.b) * 255.0).round() as u8,
                (p.a.clamp(0.0, 1.0) * 255.0).round() as u8,
            ]
        })
        .collect()
}

/// Pack linear straight-alpha pixels into `u16` RGBA bytes after sRGB encoding.
pub fn rgba32_to_srgb_u16(pixels: &[Rgba32F]) -> Vec<u16> {
    pixels
        .iter()
        .flat_map(|p| {
            [
                (linear_to_srgb(p.r) * 65535.0).round() as u16,
                (linear_to_srgb(p.g) * 65535.0).round() as u16,
                (linear_to_srgb(p.b) * 65535.0).round() as u16,
                (p.a.clamp(0.0, 1.0) * 65535.0).round() as u16,
            ]
        })
        .collect()
}

/// Convert `u8` RGBA bytes (assumed sRGB) to linear straight-alpha.
pub fn srgb_u8_to_rgba32(bytes: &[u8]) -> Vec<Rgba32F> {
    bytes
        .chunks_exact(4)
        .map(|c| {
            Rgba32F::new(
                srgb_to_linear(c[0] as f32 / 255.0),
                srgb_to_linear(c[1] as f32 / 255.0),
                srgb_to_linear(c[2] as f32 / 255.0),
                c[3] as f32 / 255.0,
            )
        })
        .collect()
}

/// Convert `u16` RGBA bytes (assumed sRGB) to linear straight-alpha.
pub fn srgb_u16_to_rgba32(bytes: &[u16]) -> Vec<Rgba32F> {
    bytes
        .chunks_exact(4)
        .map(|c| {
            Rgba32F::new(
                srgb_to_linear(c[0] as f32 / 65535.0),
                srgb_to_linear(c[1] as f32 / 65535.0),
                srgb_to_linear(c[2] as f32 / 65535.0),
                c[3] as f32 / 65535.0,
            )
        })
        .collect()
}

/// Convert `u8` RGBA bytes to normalised straight-alpha without a transfer function.
pub fn raw_u8_to_rgba32(bytes: &[u8]) -> Vec<Rgba32F> {
    bytes
        .chunks_exact(4)
        .map(|c| {
            Rgba32F::new(
                c[0] as f32 / 255.0,
                c[1] as f32 / 255.0,
                c[2] as f32 / 255.0,
                c[3] as f32 / 255.0,
            )
        })
        .collect()
}

/// Convert `u8` RGB bytes to normalised straight-alpha without a transfer function.
pub fn raw_rgb_u8_to_rgba32(bytes: &[u8]) -> Vec<Rgba32F> {
    bytes
        .chunks_exact(3)
        .map(|c| {
            Rgba32F::new(
                c[0] as f32 / 255.0,
                c[1] as f32 / 255.0,
                c[2] as f32 / 255.0,
                1.0,
            )
        })
        .collect()
}

/// Convert `u16` RGBA bytes to normalised straight-alpha without a transfer function.
pub fn raw_u16_to_rgba32(bytes: &[u16]) -> Vec<Rgba32F> {
    bytes
        .chunks_exact(4)
        .map(|c| {
            Rgba32F::new(
                c[0] as f32 / 65535.0,
                c[1] as f32 / 65535.0,
                c[2] as f32 / 65535.0,
                c[3] as f32 / 65535.0,
            )
        })
        .collect()
}

/// Convert `u16` RGB bytes to normalised straight-alpha without a transfer function.
pub fn raw_rgb_u16_to_rgba32(bytes: &[u16]) -> Vec<Rgba32F> {
    bytes
        .chunks_exact(3)
        .map(|c| {
            Rgba32F::new(
                c[0] as f32 / 65535.0,
                c[1] as f32 / 65535.0,
                c[2] as f32 / 65535.0,
                1.0,
            )
        })
        .collect()
}

/// Pack linear or profile-encoded straight-alpha pixels into `u8` RGBA bytes.
pub fn rgba32_to_linear_u8(pixels: &[Rgba32F]) -> Vec<u8> {
    pixels
        .iter()
        .flat_map(|p| {
            [
                (p.r.clamp(0.0, 1.0) * 255.0).round() as u8,
                (p.g.clamp(0.0, 1.0) * 255.0).round() as u8,
                (p.b.clamp(0.0, 1.0) * 255.0).round() as u8,
                (p.a.clamp(0.0, 1.0) * 255.0).round() as u8,
            ]
        })
        .collect()
}

/// Pack linear or profile-encoded straight-alpha pixels into `u16` RGBA bytes.
pub fn rgba32_to_linear_u16(pixels: &[Rgba32F]) -> Vec<u16> {
    pixels
        .iter()
        .flat_map(|p| {
            [
                (p.r.clamp(0.0, 1.0) * 65535.0).round() as u16,
                (p.g.clamp(0.0, 1.0) * 65535.0).round() as u16,
                (p.b.clamp(0.0, 1.0) * 65535.0).round() as u16,
                (p.a.clamp(0.0, 1.0) * 65535.0).round() as u16,
            ]
        })
        .collect()
}

/// Detect an embedded ICC profile in a supported image file.
///
/// Currently only PNG `iCCP` chunks are inspected; other formats return `None`.
pub fn detect_embedded_profile(path: impl AsRef<Path>) -> Result<Option<IccProfile>, IoError> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 8];
    file.read_exact(&mut header)?;
    if header == PNG_SIG {
        return read_png_iccp(&mut file);
    }
    Ok(None)
}

fn read_png_iccp(file: &mut File) -> Result<Option<IccProfile>, IoError> {
    loop {
        let mut len_buf = [0u8; 4];
        if file.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut type_buf = [0u8; 4];
        file.read_exact(&mut type_buf)?;
        let mut data = vec![0u8; len];
        file.read_exact(&mut data)?;
        let mut crc_buf = [0u8; 4];
        file.read_exact(&mut crc_buf)?;

        if &type_buf == b"iCCP" {
            let null_pos = data.iter().position(|&b| b == 0).ok_or_else(|| {
                IoError::ColorConversion("invalid iCCP chunk: missing null terminator".into())
            })?;
            if null_pos + 2 > data.len() {
                return Err(IoError::ColorConversion(
                    "invalid iCCP chunk: truncated header".into(),
                ));
            }
            if data[null_pos + 1] != 0 {
                return Err(IoError::ColorConversion(
                    "unsupported iCCP compression method".into(),
                ));
            }
            let compressed = &data[null_pos + 2..];
            let mut decoder = ZlibDecoder::new(Cursor::new(compressed));
            let mut bytes = Vec::new();
            decoder.read_to_end(&mut bytes)?;
            return Ok(Some(IccProfile::new(bytes)));
        }

        // iCCP must appear before IDAT; no need to keep scanning once we pass it.
        if &type_buf == b"IDAT" || &type_buf == b"IEND" {
            break;
        }
    }
    Ok(None)
}

/// Build a linear sRGB working-space profile used when no explicit profile is supplied.
fn linear_working_profile() -> Result<Profile, IoError> {
    let white = CIExyY {
        x: 0.3127,
        y: 0.3290,
        Y: 1.0,
    };
    let primaries = CIExyYTRIPLE {
        Red: CIExyY {
            x: 0.6400,
            y: 0.3300,
            Y: 1.0,
        },
        Green: CIExyY {
            x: 0.3000,
            y: 0.6000,
            Y: 1.0,
        },
        Blue: CIExyY {
            x: 0.1500,
            y: 0.0600,
            Y: 1.0,
        },
    };
    let curve = ToneCurve::new(1.0);
    Profile::new_rgb(&white, &primaries, &[&curve, &curve, &curve])
        .map_err(|e| IoError::ColorConversion(format!("lcms2: {e}")))
}

fn profile_or_srgb(profile: Option<&IccProfile>) -> Result<Profile, IoError> {
    match profile {
        Some(icc) => Profile::new_icc(icc.as_bytes())
            .map_err(|e| IoError::ColorConversion(format!("lcms2: {e}"))),
        None => Ok(Profile::new_srgb()),
    }
}

/// Convert RGB values in a pixel buffer between ICC profiles.
///
/// Alpha is left untouched.  A `None` profile means the linear sRGB working
/// space (the document's default compositing space).
pub fn convert_to_profile(
    pixels: &mut [Rgba32F],
    from: Option<&IccProfile>,
    to: Option<&IccProfile>,
) -> Result<(), IoError> {
    if pixels.is_empty() || (from.is_none() && to.is_none()) {
        return Ok(());
    }

    let src = if from.is_some() {
        profile_or_srgb(from)?
    } else {
        linear_working_profile()?
    };
    let dst = if to.is_some() {
        profile_or_srgb(to)?
    } else {
        linear_working_profile()?
    };

    let transform = Transform::new(
        &src,
        PixelFormat::RGB_FLT,
        &dst,
        PixelFormat::RGB_FLT,
        Intent::Perceptual,
    )
    .map_err(|e| IoError::ColorConversion(format!("lcms2: {e}")))?;

    let input: Vec<RgbF32> = pixels
        .iter()
        .map(|p| RgbF32 {
            r: p.r.clamp(0.0, 1.0),
            g: p.g.clamp(0.0, 1.0),
            b: p.b.clamp(0.0, 1.0),
        })
        .collect();
    let mut output = vec![
        RgbF32 {
            r: 0.0,
            g: 0.0,
            b: 0.0,
        };
        input.len()
    ];
    transform.transform_pixels(&input, &mut output);

    for (p, rgb) in pixels.iter_mut().zip(output) {
        p.r = rgb.r;
        p.g = rgb.g;
        p.b = rgb.b;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;

    #[test]
    fn srgb_round_trip_is_near_identity() {
        for i in 0..=255u8 {
            let f = i as f32 / 255.0;
            let back = (linear_to_srgb(srgb_to_linear(f)) * 255.0).round() as u8;
            assert_eq!(back, i);
        }
    }

    #[test]
    fn convert_between_profiles_matches_lcms_reference() {
        // Build a simple gamma 1.8 profile with sRGB primaries so that the
        // transform to sRGB is non-trivial but deterministic.
        let source = build_gamma18_profile();
        let target_profile = Profile::new_srgb();
        let target = IccProfile::new(target_profile.icc().unwrap());

        let mut pixels = vec![Rgba32F::new(1.0, 0.0, 0.0, 1.0)];
        convert_to_profile(&mut pixels, Some(&source.0), Some(&target)).unwrap();

        // Reference: transform the same source RGB through LCMS directly.
        let reference_transform = Transform::new(
            &source.profile(),
            PixelFormat::RGB_FLT,
            &target_profile,
            PixelFormat::RGB_FLT,
            Intent::Perceptual,
        )
        .unwrap();
        let mut reference = [RgbF32 {
            r: 0.0,
            g: 0.0,
            b: 0.0,
        }];
        reference_transform.transform_pixels(
            &[RgbF32 {
                r: 1.0,
                g: 0.0,
                b: 0.0,
            }],
            &mut reference,
        );

        let rgb = reference[0];
        assert!((pixels[0].r - rgb.r).abs() < 1.0 / 255.0, "r mismatch");
        assert!((pixels[0].g - rgb.g).abs() < 1.0 / 255.0, "g mismatch");
        assert!((pixels[0].b - rgb.b).abs() < 1.0 / 255.0, "b mismatch");
        assert_eq!(pixels[0].a, 1.0);
    }

    struct Gamma18Profile(IccProfile);

    impl Gamma18Profile {
        fn profile(&self) -> Profile {
            Profile::new_icc(self.0.as_bytes()).unwrap()
        }
    }

    fn build_gamma18_profile() -> Gamma18Profile {
        let white = CIExyY {
            x: 0.3127,
            y: 0.3290,
            Y: 1.0,
        };
        let primaries = CIExyYTRIPLE {
            Red: CIExyY {
                x: 0.6400,
                y: 0.3300,
                Y: 1.0,
            },
            Green: CIExyY {
                x: 0.3000,
                y: 0.6000,
                Y: 1.0,
            },
            Blue: CIExyY {
                x: 0.1500,
                y: 0.0600,
                Y: 1.0,
            },
        };
        let curve = ToneCurve::new(1.8);
        let profile = Profile::new_rgb(&white, &primaries, &[&curve, &curve, &curve]).unwrap();
        Gamma18Profile(IccProfile::new(profile.icc().unwrap()))
    }

    #[test]
    fn detect_embedded_profile_reads_png_iccp() {
        let profile = IccProfile::new(Profile::new_srgb().icc().unwrap());
        let path = temp_dir().join("ogre_io_icc_detect.png");
        write_png_with_iccp(&path, &profile).unwrap();
        let detected = detect_embedded_profile(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(detected, Some(profile));
    }

    fn write_png_with_iccp(path: &Path, profile: &IccProfile) -> std::io::Result<()> {
        use std::io::Write;

        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use png::chunk::ChunkType;
        use png::Encoder;

        let file = File::create(path)?;
        let mut encoder = Encoder::new(file, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(std::io::Error::other)?;

        // Manually write an iCCP chunk before the image data.
        let mut compressed = Vec::new();
        {
            let mut zlib = ZlibEncoder::new(&mut compressed, Compression::default());
            zlib.write_all(profile.as_bytes())?;
            zlib.finish()?;
        }
        let mut iccp = Vec::new();
        iccp.extend_from_slice(b"ICC\0"); // profile name + null terminator
        iccp.push(0); // compression method: deflate
        iccp.extend_from_slice(&compressed);
        let chunk_type = ChunkType(*b"iCCP");
        writer
            .write_chunk(chunk_type, &iccp)
            .map_err(std::io::Error::other)?;

        writer
            .write_image_data(&[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ])
            .map_err(std::io::Error::other)?;
        writer.finish().map_err(std::io::Error::other)?;
        Ok(())
    }
}
