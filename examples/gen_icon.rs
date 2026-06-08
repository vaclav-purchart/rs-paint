//! Generates the rs-paint app icon: an artist's palette with paint dabs and a
//! brush, on a rounded gradient tile. Rendered with signed-distance fields at
//! 2x supersampling for clean anti-aliased edges, then downscaled to 1024².
//!
//! Run with: cargo run --release --example gen_icon
//! Output:   assets/icon_1024.png

use image::{ImageBuffer, Rgba, RgbaImage};

const OUT: u32 = 1024;
const SS: u32 = 2; // supersample factor
const N: u32 = OUT * SS;

#[derive(Clone, Copy)]
struct V2 {
    x: f32,
    y: f32,
}
fn v(x: f32, y: f32) -> V2 {
    V2 { x, y }
}
impl V2 {
    fn sub(self, o: V2) -> V2 {
        v(self.x - o.x, self.y - o.y)
    }
    fn dot(self, o: V2) -> f32 {
        self.x * o.x + self.y * o.y
    }
    fn len(self) -> f32 {
        self.dot(self).sqrt()
    }
}

/// Signed distance to a rounded rectangle centered at `c` with half-size `h`.
fn sd_round_rect(p: V2, c: V2, h: V2, r: f32) -> f32 {
    let q = v((p.x - c.x).abs() - (h.x - r), (p.y - c.y).abs() - (h.y - r));
    let qx = q.x.max(0.0);
    let qy = q.y.max(0.0);
    (v(qx, qy).len()) + q.x.max(q.y).min(0.0) - r
}

fn sd_circle(p: V2, c: V2, r: f32) -> f32 {
    p.sub(c).len() - r
}

/// Signed distance to a capsule (thick line segment) from a to b, radius r.
fn sd_capsule(p: V2, a: V2, b: V2, r: f32) -> f32 {
    let pa = p.sub(a);
    let ba = b.sub(a);
    let t = (pa.dot(ba) / ba.dot(ba)).clamp(0.0, 1.0);
    let proj = v(a.x + ba.x * t, a.y + ba.y * t);
    p.sub(proj).len() - r
}

type Col = [f32; 4]; // straight-alpha rgba, 0..1

fn rgb(r: u8, g: u8, b: u8) -> Col {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

/// Alpha-over compositing of `src` onto `dst` with extra coverage multiplier.
fn over(dst: Col, src: Col, coverage: f32) -> Col {
    let sa = src[3] * coverage;
    let oa = sa + dst[3] * (1.0 - sa);
    if oa <= 0.0 {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let mix = |s: f32, d: f32| (s * sa + d * dst[3] * (1.0 - sa)) / oa;
    [
        mix(src[0], dst[0]),
        mix(src[1], dst[1]),
        mix(src[2], dst[2]),
        oa,
    ]
}

/// Coverage from a signed distance (1 inside, 0 outside, AA across ~1.5px).
fn cov(sd: f32) -> f32 {
    (0.5 - sd / 1.5).clamp(0.0, 1.0)
}

fn main() {
    let mut img: RgbaImage = ImageBuffer::new(N, N);
    let s = N as f32 / 1024.0; // scale: design in 1024-space
    let cdef = |x: f32, y: f32| v(x * s, y * s);

    // Palette body geometry (a tilted disc with a thumb hole).
    let pal_c = cdef(470.0, 560.0);
    let pal_r = 300.0 * s;
    let hole_c = cdef(560.0, 640.0);
    let hole_r = 70.0 * s;

    // Paint dabs around the upper arc of the palette.
    let dabs = [
        (cdef(300.0, 430.0), 52.0, rgb(231, 76, 60)),   // red
        (cdef(420.0, 360.0), 52.0, rgb(241, 196, 15)),  // yellow
        (cdef(560.0, 360.0), 52.0, rgb(46, 204, 113)),  // green
        (cdef(680.0, 430.0), 52.0, rgb(52, 152, 219)),  // blue
        (cdef(330.0, 600.0), 52.0, rgb(155, 89, 182)),  // purple
    ];

    // Paintbrush: diagonal from lower-left to upper-right.
    let brush_tip = cdef(640.0, 250.0);
    let ferrule_a = cdef(700.0, 320.0);
    let ferrule_b = cdef(760.0, 392.0);
    let handle_end = cdef(900.0, 560.0);

    for py in 0..N {
        for px in 0..N {
            let p = v(px as f32 + 0.5, py as f32 + 0.5);

            // 1) Background rounded tile with a diagonal gradient.
            let bg_sd = sd_round_rect(
                p,
                v(N as f32 / 2.0, N as f32 / 2.0),
                v(N as f32 / 2.0, N as f32 / 2.0),
                220.0 * s,
            );
            let g = ((p.x + p.y) / (2.0 * N as f32)).clamp(0.0, 1.0);
            let bg = [
                0.36 + (0.49 - 0.36) * g,
                0.42 + (0.30 - 0.42) * g,
                0.85 + (0.62 - 0.85) * g,
                1.0,
            ];
            let mut c: Col = [0.0, 0.0, 0.0, 0.0];
            c = over(c, bg, cov(bg_sd));

            // soft inner highlight on the tile (top-left)
            let hl = sd_circle(p, cdef(330.0, 300.0), 380.0 * s);
            c = over(c, [1.0, 1.0, 1.0, 0.10], cov(hl) * cov(bg_sd));

            // 2) Brush handle (drawn under the palette edge).
            c = over(
                c,
                rgb(60, 42, 30),
                cov(sd_capsule(p, ferrule_b, handle_end, 30.0 * s)),
            );
            // metal ferrule
            c = over(
                c,
                rgb(200, 205, 210),
                cov(sd_capsule(p, ferrule_a, ferrule_b, 34.0 * s)),
            );
            // bristle tip (dipped in red)
            c = over(
                c,
                rgb(231, 76, 60),
                cov(sd_capsule(p, brush_tip, ferrule_a, 30.0 * s)),
            );

            // 3) Palette body (cream), with the thumb hole punched out.
            let pal = sd_circle(p, pal_c, pal_r);
            let hole = sd_circle(p, hole_c, hole_r);
            // coverage of palette minus hole
            let pal_cov = cov(pal) * (1.0 - cov(hole));
            // subtle shadow ring at palette edge
            c = over(c, rgb(40, 30, 60), cov(pal + 6.0 * s) * 0.25);
            c = over(c, rgb(250, 246, 238), pal_cov);

            // 4) Paint dabs on the palette.
            for (dc, dr, dcol) in dabs.iter() {
                let d = sd_circle(p, *dc, *dr * s);
                c = over(c, *dcol, cov(d) * pal_cov.max(cov(d)));
            }

            let to_u8 = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
            img.put_pixel(
                px,
                py,
                Rgba([to_u8(c[0]), to_u8(c[1]), to_u8(c[2]), to_u8(c[3])]),
            );
        }
    }

    std::fs::create_dir_all("assets").unwrap();
    let down = image::imageops::resize(&img, OUT, OUT, image::imageops::FilterType::Lanczos3);
    down.save("assets/icon_1024.png").unwrap();
    println!("wrote assets/icon_1024.png ({OUT}x{OUT})");

    // Multi-resolution .ico for the Windows executable's file icon.
    let mut icondir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let r = image::imageops::resize(&img, size, size, image::imageops::FilterType::Lanczos3);
        let icon_image = ico::IconImage::from_rgba_data(size, size, r.into_raw());
        icondir.add_entry(ico::IconDirEntry::encode(&icon_image).unwrap());
    }
    let f = std::fs::File::create("assets/icon.ico").unwrap();
    icondir.write(f).unwrap();
    println!("wrote assets/icon.ico");
}
