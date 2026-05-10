//! Build script — generates `clippet.ico` from a procedural rasterizer and
//! embeds it as a Win32 resource named "clippet". The companion
//! `assets/clippet.svg` is the human-readable design source; if you tweak
//! one, mirror the change in the other so they stay visually aligned.
//!
//! We render procedurally rather than parse the SVG so the build deps stay
//! lean (just `png` and `embed-resource`), at the cost of duplicating the
//! shape coordinates here.

use std::fs;
use std::path::PathBuf;

const SIZES: &[u32] = &[16, 24, 32, 48, 64, 128, 256];
const SS: u32 = 4; // supersample factor for antialiasing

fn main() {
    println!("cargo:rerun-if-changed=assets/clippet.svg");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
    let ico_path = out_dir.join("clippet.ico");
    let rc_path = out_dir.join("clippet.rc");

    let mut entries: Vec<(u32, Vec<u8>)> = Vec::with_capacity(SIZES.len());
    for &sz in SIZES {
        let pixels = render_icon(sz);
        let png = encode_png(sz, &pixels);
        entries.push((sz, png));
    }

    let ico_bytes = pack_ico(&entries);
    fs::write(&ico_path, &ico_bytes).expect("write clippet.ico");

    // RC syntax: `<NAME> ICON "<path>"`. The literal name is what
    // LoadIconW(hinst, w!("clippet")) matches at runtime.
    let ico_path_str = ico_path.to_string_lossy().replace('\\', "\\\\");
    let rc = format!("clippet ICON \"{}\"\n", ico_path_str);
    fs::write(&rc_path, rc).expect("write clippet.rc");

    embed_resource::compile(&rc_path, embed_resource::NONE);

    // Skip resource compilation entirely on non-Windows targets — the rest
    // of the program is windows_subsystem-only anyway, but this keeps the
    // build script honest if someone tries `cargo check` from WSL.
    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        println!(
            "cargo:warning=clippet build script: not on Windows, skipped icon resource compile"
        );
    }
}

// ---------------------------------------------------------------------
// Procedural rendering — coordinates mirror assets/clippet.svg (256x256).
// ---------------------------------------------------------------------

fn render_icon(target: u32) -> Vec<u8> {
    let s = target * SS;
    let mut buf = vec![0u8; (s * s * 4) as usize];

    let unit = s as f32 / 256.0;
    let u = |v: f32| v * unit;

    // Clipboard body — solid mid-blue (the SVG uses a gradient; we settle
    // for a single color since procedural gradients aren't worth the lines
    // of code at this size).
    fill_rounded_rect(&mut buf, s, u(32.0), u(48.0), u(224.0), u(232.0), u(20.0), [0x29, 0x6F, 0xEC, 0xFF]);

    // Clip mechanism — darker navy on top.
    fill_rounded_rect(&mut buf, s, u(80.0), u(24.0), u(176.0), u(68.0), u(10.0), [0x1E, 0x3A, 0x8A, 0xFF]);

    // Paper inset.
    fill_rounded_rect(&mut buf, s, u(56.0), u(80.0), u(200.0), u(216.0), u(6.0), [0xF8, 0xFA, 0xFC, 0xFF]);

    // Three text lines.
    let line = [0x94u8, 0xA3, 0xB8, 0xFF];
    fill_rounded_rect(&mut buf, s, u(76.0), u(108.0), u(180.0), u(118.0), u(3.0), line);
    fill_rounded_rect(&mut buf, s, u(76.0), u(138.0), u(156.0), u(148.0), u(3.0), line);
    fill_rounded_rect(&mut buf, s, u(76.0), u(168.0), u(168.0), u(178.0), u(3.0), line);

    downsample(&buf, s, target)
}

fn fill_rounded_rect(
    buf: &mut [u8],
    s: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    r: f32,
    color: [u8; 4],
) {
    let w = s as i32;
    let h = s as i32;
    let lo_x = (x0 as i32 - 1).max(0) as u32;
    let hi_x = ((x1 as i32 + 2).min(w)) as u32;
    let lo_y = (y0 as i32 - 1).max(0) as u32;
    let hi_y = ((y1 as i32 + 2).min(h)) as u32;

    for y in lo_y..hi_y {
        for x in lo_x..hi_x {
            let cx = x as f32 + 0.5;
            let cy = y as f32 + 0.5;
            if cx < x0 || cx > x1 || cy < y0 || cy > y1 {
                continue;
            }
            // Distance from the inner straight-edge band, by axis. If both
            // are zero the point is in the body. Otherwise it's somewhere
            // in the corner zone and we test against the corner radius.
            let dx = if cx < x0 + r {
                x0 + r - cx
            } else if cx > x1 - r {
                cx - (x1 - r)
            } else {
                0.0
            };
            let dy = if cy < y0 + r {
                y0 + r - cy
            } else if cy > y1 - r {
                cy - (y1 - r)
            } else {
                0.0
            };
            let inside = if dx == 0.0 && dy == 0.0 {
                true
            } else {
                (dx * dx + dy * dy) <= r * r
            };
            if inside {
                blend_pixel(buf, s, x, y, color);
            }
        }
    }
}

fn blend_pixel(buf: &mut [u8], s: u32, x: u32, y: u32, color: [u8; 4]) {
    let i = ((y * s + x) * 4) as usize;
    let sa = color[3] as f32 / 255.0;
    let inv = 1.0 - sa;
    buf[i] = (color[0] as f32 * sa + buf[i] as f32 * inv) as u8;
    buf[i + 1] = (color[1] as f32 * sa + buf[i + 1] as f32 * inv) as u8;
    buf[i + 2] = (color[2] as f32 * sa + buf[i + 2] as f32 * inv) as u8;
    let new_a = color[3] as f32 + buf[i + 3] as f32 * inv;
    buf[i + 3] = new_a.min(255.0) as u8;
}

// Box-filter downsample by SS — simple but enough quality for 16..256px
// when paired with 4x supersampling at the source.
fn downsample(src: &[u8], s_size: u32, t_size: u32) -> Vec<u8> {
    let factor = s_size / t_size;
    let mut out = vec![0u8; (t_size * t_size * 4) as usize];
    let n = (factor * factor) as u32;
    for y in 0..t_size {
        for x in 0..t_size {
            let mut r = 0u32;
            let mut g = 0u32;
            let mut b = 0u32;
            let mut a = 0u32;
            for dy in 0..factor {
                for dx in 0..factor {
                    let sx = x * factor + dx;
                    let sy = y * factor + dy;
                    let i = ((sy * s_size + sx) * 4) as usize;
                    r += src[i] as u32;
                    g += src[i + 1] as u32;
                    b += src[i + 2] as u32;
                    a += src[i + 3] as u32;
                }
            }
            let oi = ((y * t_size + x) * 4) as usize;
            out[oi] = (r / n) as u8;
            out[oi + 1] = (g / n) as u8;
            out[oi + 2] = (b / n) as u8;
            out[oi + 3] = (a / n) as u8;
        }
    }
    out
}

fn encode_png(size: u32, pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, size, size);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("png header");
        writer.write_image_data(pixels).expect("png data");
    }
    out
}

// ICO file format: 6-byte ICONDIR header, then N x 16-byte ICONDIRENTRY
// records pointing at PNG/BMP payloads. We use PNG everywhere; Vista+
// supports it and Win11's tray and titlebar both render PNG entries fine.
fn pack_ico(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // 1 = icon
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());

    let header_size = 6u32 + 16 * entries.len() as u32;
    let mut data_offset = header_size;
    for (sz, png) in entries {
        // 0 stands for 256 in the ICO byte field (it's a single-byte width).
        let dim = if *sz >= 256 { 0u8 } else { *sz as u8 };
        out.push(dim);
        out.push(dim);
        out.push(0); // palette colors
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&data_offset.to_le_bytes());
        data_offset += png.len() as u32;
    }
    for (_, png) in entries {
        out.extend_from_slice(png);
    }
    out
}
