use std::fs::File;
use std::io::{self, Cursor, Write};
use std::path::PathBuf;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, RgbImage};

/// Render an STL as an isometric image using the Kitty graphics protocol.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// STL file to preview (ASCII or binary)
    file: PathBuf,

    /// Output image width in pixels
    #[arg(long, default_value_t = 900)]
    width: u32,

    /// Output image height in pixels
    #[arg(long, default_value_t = 650)]
    height: u32,

    /// Image width in terminal cells (0 lets Kitty choose)
    #[arg(long, default_value_t = 0)]
    cols: u32,

    /// Image height in terminal cells (0 lets Kitty choose)
    #[arg(long, default_value_t = 0)]
    rows: u32,

    /// Clear this viewer's image immediately after it is sent (normally undesirable)
    #[arg(long)]
    delete_after: bool,

    /// Continuously rotate the model using Kitty animation frames
    #[arg(long)]
    rotate: bool,

    /// Rotate the model manually with arrow keys; press q or Escape to finish
    #[arg(long)]
    interactive: bool,

    /// Number of rotation frames (only used with --rotate)
    #[arg(long, default_value_t = 36)]
    frames: u16,

    /// Rotation speed in frames per second (only used with --rotate)
    #[arg(long, default_value_t = 12)]
    fps: u16,
}

#[derive(Clone, Copy)]
struct Triangle {
    p: [[f32; 3]; 3],
    normal: [f32; 3],
}

type Mat3 = [[f32; 3]; 3];

#[derive(Clone, Copy)]
struct ModelView {
    center: [f32; 3],
    radius: f32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.width < 2 || args.height < 2 {
        return Err("width and height must be at least 2".into());
    }
    if args.rotate && args.interactive {
        return Err("--rotate and --interactive cannot be used together".into());
    }
    if args.rotate && (args.frames < 2 || args.fps == 0) {
        return Err("--rotate needs at least 2 frames and a non-zero --fps".into());
    }

    let mesh = stl_io::read_stl(&mut File::open(&args.file)?)?;
    if mesh.faces.is_empty() {
        return Err("STL contains no triangles".into());
    }
    let triangles: Vec<Triangle> = mesh
        .faces
        .iter()
        .map(|face| {
            let p = face.vertices.map(|i| mesh.vertices[i].0);
            Triangle {
                p,
                normal: face.normal.0,
            }
        })
        .collect();
    let view = model_view(&triangles);

    if args.interactive {
        interactive_kitty(&triangles, view, &args)?;
    } else if args.rotate {
        animate_kitty(&triangles, view, &args)?;
    } else {
        let png = png_for(&triangles, view, args.width, args.height, identity(), 1.0)?;
        let (cols, rows) = placement_size(&args, args.width, args.height);
        move_cursor_home()?;
        display_kitty(&png, cols, rows, args.delete_after)?;
    }
    Ok(())
}

fn png_for(
    tris: &[Triangle],
    view: ModelView,
    width: u32,
    height: u32,
    orientation: Mat3,
    zoom: f32,
) -> Result<Vec<u8>, image::ImageError> {
    let img = render(tris, view, width, height, orientation, zoom);
    let mut png = Vec::new();
    // Fast compression: the flat-shaded render is cheap to compress and the
    // encoder otherwise dominates frame time.
    PngEncoder::new_with_quality(
        Cursor::new(&mut png),
        CompressionType::Fast,
        FilterType::Sub,
    )
    .write_image(img.as_raw(), width, height, ExtendedColorType::Rgb8)?;
    Ok(png)
}

fn render(
    tris: &[Triangle],
    view: ModelView,
    width: u32,
    height: u32,
    orientation: Mat3,
    zoom: f32,
) -> RgbImage {
    // Isometric basis: rotate around Z, then tilt down toward the XY plane.
    // Fold it and the user orientation into one matrix so each vertex costs a
    // single 3x3 multiply instead of repeated trig and two rotations.
    let angle = 45f32.to_radians();
    let tilt = 35.264f32.to_radians();
    let (sa, ca) = angle.sin_cos();
    let (st, ct) = tilt.sin_cos();
    let iso: Mat3 = [[ca, -sa, 0.0], [ct * sa, ct * ca, -st], [st * sa, st * ca, ct]];
    let m = multiply(orientation, iso);
    // A sphere around the model has the same projected radius at every angle.
    // Unlike fitting each frame's bounds, this keeps the apparent size stable.
    let margin = 0.07;
    let scale = ((width as f32 * (1.0 - 2.0 * margin)) / (2.0 * view.radius))
        .min((height as f32 * (1.0 - 2.0 * margin)) / (2.0 * view.radius))
        * zoom;
    let (half_w, half_h) = (width as f32 / 2.0, height as f32 / 2.0);
    let screen = |p: [f32; 3]| {
        let q = transform(
            m,
            [
                p[0] - view.center[0],
                p[1] - view.center[1],
                p[2] - view.center[2],
            ],
        );
        [q[0] * scale + half_w, half_h - q[1] * scale, q[2]]
    };

    let light = normalize([0.4, -0.6, 0.7]);
    // Project and shade every triangle once; the bands below then share the result.
    let project_one = |tri: &Triangle| {
        let shade = 0.22 + 0.78 * dot(normalize(tri.normal), light).abs();
        ScreenTri {
            p: tri.p.map(screen),
            color: [
                (45.0 * shade) as u8,
                (167.0 * shade) as u8,
                (235.0 * shade) as u8,
            ],
        }
    };
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let parallel = threads > 1 && tris.len() >= PARALLEL_MIN_TRIANGLES;
    let screen_tris: Vec<ScreenTri> = if parallel {
        let mut out = vec![ScreenTri::default(); tris.len()];
        let chunk = tris.len().div_ceil(threads);
        std::thread::scope(|scope| {
            for (src, dst) in tris.chunks(chunk).zip(out.chunks_mut(chunk)) {
                scope.spawn(|| {
                    for (tri, slot) in src.iter().zip(dst) {
                        *slot = project_one(tri);
                    }
                });
            }
        });
        out
    } else {
        tris.iter().map(project_one).collect()
    };

    let (w, h) = (width as usize, height as usize);
    let mut pixels = vec![0u8; w * h * 3];
    for px in pixels.chunks_exact_mut(3) {
        px.copy_from_slice(&BACKGROUND);
    }
    let mut depth = vec![f32::NEG_INFINITY; w * h];
    if parallel {
        // Each band owns disjoint rows, so no locking is needed.
        let band_rows = h.div_ceil(threads * 2).max(1);
        let screen_tris = &screen_tris;
        std::thread::scope(|scope| {
            for (i, (pix, dep)) in pixels
                .chunks_mut(band_rows * w * 3)
                .zip(depth.chunks_mut(band_rows * w))
                .enumerate()
            {
                scope.spawn(move || draw_band(screen_tris, w, i * band_rows, pix, dep));
            }
        });
    } else {
        draw_band(&screen_tris, w, 0, &mut pixels, &mut depth);
    }
    RgbImage::from_raw(width, height, pixels).expect("buffer matches dimensions")
}

fn draw_band(tris: &[ScreenTri], w: usize, first_row: usize, pix: &mut [u8], dep: &mut [f32]) {
    let rows = dep.len() / w;
    for tri in tris {
        rasterize(pix, dep, w, first_row, rows, &tri.p, tri.color);
    }
}

const PARALLEL_MIN_TRIANGLES: usize = 20_000;
const BACKGROUND: [u8; 3] = [18, 22, 30];

#[derive(Clone, Copy, Default)]
struct ScreenTri {
    p: [[f32; 3]; 3],
    color: [u8; 3],
}

/// Rasterize into one horizontal band of the image. `first_row` is the band's
/// first image row and `rows` its height; pixels outside the band are skipped.
fn rasterize(
    pixels: &mut [u8],
    depth: &mut [f32],
    w: usize,
    first_row: usize,
    rows: usize,
    p: &[[f32; 3]; 3],
    color: [u8; 3],
) {
    let min_y = p[0][1].min(p[1][1]).min(p[2][1]).floor().max(first_row as f32);
    let max_y = p[0][1]
        .max(p[1][1])
        .max(p[2][1])
        .ceil()
        .min((first_row + rows) as f32 - 1.0);
    let min_x = p[0][0].min(p[1][0]).min(p[2][0]).floor().max(0.0);
    let max_x = p[0][0].max(p[1][0]).max(p[2][0]).ceil().min(w as f32 - 1.0);
    if min_x > max_x || min_y > max_y {
        return; // outside this band or the image (also covers NaN)
    }
    let area = edge(p[0], p[1], p[2][0], p[2][1]);
    if area.abs() < 1e-6 {
        return;
    }
    let inv_area = 1.0 / area;
    let (lo_x, hi_x) = (min_x as usize, max_x as usize);
    for y in min_y as usize..=max_y as usize {
        let py = y as f32 + 0.5;
        let row = (y - first_row) * w;
        for x in lo_x..=hi_x {
            let px = x as f32 + 0.5;
            let a = edge(p[1], p[2], px, py) * inv_area;
            let b = edge(p[2], p[0], px, py) * inv_area;
            let c = 1.0 - a - b;
            if a >= 0.0 && b >= 0.0 && c >= 0.0 {
                let z = a * p[0][2] + b * p[1][2] + c * p[2][2];
                let i = row + x;
                if z > depth[i] {
                    depth[i] = z;
                    pixels[i * 3..i * 3 + 3].copy_from_slice(&color);
                }
            }
        }
    }
}

fn edge(a: [f32; 3], b: [f32; 3], x: f32, y: f32) -> f32 {
    (x - a[0]) * (b[1] - a[1]) - (y - a[1]) * (b[0] - a[0])
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn model_view(tris: &[Triangle]) -> ModelView {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for tri in tris {
        for point in tri.p {
            for axis in 0..3 {
                min[axis] = min[axis].min(point[axis]);
                max[axis] = max[axis].max(point[axis]);
            }
        }
    }
    let center = [
        (min[0] + max[0]) * 0.5,
        (min[1] + max[1]) * 0.5,
        (min[2] + max[2]) * 0.5,
    ];
    let mut radius = 0.0f32;
    for tri in tris {
        for point in tri.p {
            let offset = [
                point[0] - center[0],
                point[1] - center[1],
                point[2] - center[2],
            ];
            radius = radius.max(dot(offset, offset).sqrt());
        }
    }
    ModelView {
        center,
        radius: radius.max(1e-6),
    }
}

fn placement_size(args: &Args, image_width: u32, image_height: u32) -> (u32, u32) {
    // Leave a little terminal chrome visible. This also prevents Kitty from
    // placing a large natural-pixel image beyond a small terminal viewport.
    let (terminal_cols, terminal_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let max_cols = u32::from(terminal_cols.saturating_sub(2).max(1));
    let max_rows = u32::from(terminal_rows.saturating_sub(3).max(1));
    let available_cols = if args.cols == 0 {
        max_cols
    } else {
        args.cols.min(max_cols)
    };
    let available_rows = if args.rows == 0 {
        max_rows
    } else {
        args.rows.min(max_rows)
    };

    // Use the actual pixel size reported by the terminal, rather than assuming
    // a particular cell shape. That makes the c/r placement rectangle have the
    // same physical aspect ratio as the PNG.
    let cell_aspect = crossterm::terminal::window_size()
        .ok()
        .filter(|size| size.width != 0 && size.height != 0)
        .map(|size| {
            (size.width as f32 / size.columns.max(1) as f32)
                / (size.height as f32 / size.rows.max(1) as f32)
        })
        // When the platform does not provide pixel metrics, send only the
        // limiting placement dimension below; Kitty then derives the other.
        .unwrap_or(0.0);
    let image_aspect = image_width as f32 / image_height.max(1) as f32;
    if cell_aspect == 0.0 {
        let fallback_cell_aspect = 2.0;
        let viewport_aspect = available_cols as f32 * fallback_cell_aspect / available_rows as f32;
        return if image_aspect >= viewport_aspect {
            (available_cols, 0)
        } else {
            (0, available_rows)
        };
    }
    let viewport_aspect = available_cols as f32 * cell_aspect / available_rows as f32;
    if image_aspect >= viewport_aspect {
        let rows = ((available_cols as f32 * cell_aspect / image_aspect).ceil() as u32)
            .clamp(1, available_rows);
        (available_cols, rows)
    } else {
        let cols = ((available_rows as f32 * image_aspect / cell_aspect).ceil() as u32)
            .clamp(1, available_cols);
        (cols, available_rows)
    }
}
fn normalize(v: [f32; 3]) -> [f32; 3] {
    let n = dot(v, v).sqrt().max(1e-6);
    [v[0] / n, v[1] / n, v[2] / n]
}

fn identity() -> Mat3 {
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
}

fn transform(m: Mat3, v: [f32; 3]) -> [f32; 3] {
    [dot(m[0], v), dot(m[1], v), dot(m[2], v)]
}

fn multiply(a: Mat3, b: Mat3) -> Mat3 {
    let mut result = [[0.0; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            result[row][column] =
                a[row][0] * b[0][column] + a[row][1] * b[1][column] + a[row][2] * b[2][column];
        }
    }
    result
}

fn rotation_x(angle: f32) -> Mat3 {
    let (sin, cos) = angle.sin_cos();
    [[1.0, 0.0, 0.0], [0.0, cos, -sin], [0.0, sin, cos]]
}

fn rotation_y(angle: f32) -> Mat3 {
    let (sin, cos) = angle.sin_cos();
    [[cos, 0.0, sin], [0.0, 1.0, 0.0], [-sin, 0.0, cos]]
}

fn rotation_z(angle: f32) -> Mat3 {
    let (sin, cos) = angle.sin_cos();
    [[cos, -sin, 0.0], [sin, cos, 0.0], [0.0, 0.0, 1.0]]
}

fn display_kitty(png: &[u8], cols: u32, rows: u32, delete_after: bool) -> io::Result<()> {
    // Keep Kitty's normal cursor movement for a one-shot preview. The shell
    // prompt then lands below the image instead of being overlaid on it.
    transmit_png(png, "a=T,f=100,q=2,i=1", cols, rows)?;
    let mut out = io::stdout().lock();
    if delete_after {
        write!(out, "\x1b_Ga=d,d=I,i=1;\x1b\\")?;
    }
    out.flush()
}

struct RawMode;

impl RawMode {
    fn enable() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

struct SavedCursor;

impl SavedCursor {
    fn save_and_move_home() -> io::Result<Self> {
        let mut out = io::stdout().lock();
        // DECSC/DECRC works in Kitty and the other terminals implementing its
        // graphics protocol, without altering existing scrollback.
        write!(out, "\x1b7\x1b[H")?;
        out.flush()?;
        Ok(Self)
    }
}

impl Drop for SavedCursor {
    fn drop(&mut self) {
        let mut out = io::stdout().lock();
        let _ = write!(out, "\x1b8");
        let _ = out.flush();
    }
}

fn move_cursor_home() -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\x1b[H")?;
    out.flush()
}

fn interactive_kitty(
    tris: &[Triangle],
    view: ModelView,
    args: &Args,
) -> Result<(), Box<dyn std::error::Error>> {
    let _raw_mode = RawMode::enable()?;
    let _saved_cursor = SavedCursor::save_and_move_home()?;
    let step = 10f32.to_radians();
    let mut orientation = identity();
    let mut zoom = 1.0;
    let mut image_id = 1000u32;
    let mut previous_id = 0u32;

    loop {
        let png = png_for(tris, view, args.width, args.height, orientation, zoom)?;
        let (cols, rows) = placement_size(args, args.width, args.height);
        transmit_png(&png, &format!("a=T,f=100,q=2,i={image_id},C=1"), cols, rows)?;
        if previous_id != 0 {
            // Replace the old placement only after the new image is ready,
            // preventing an empty frame between key presses.
            let mut out = io::stdout().lock();
            write!(out, "\x1b_Ga=d,d=I,i={previous_id};\x1b\\")?;
            out.flush()?;
        }
        previous_id = image_id;

        loop {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        orientation = multiply(rotation_z(-step), orientation)
                    }
                    KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        orientation = multiply(rotation_z(step), orientation)
                    }
                    KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        orientation = multiply(rotation_z(step), orientation)
                    }
                    KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        orientation = multiply(rotation_z(-step), orientation)
                    }
                    // Pre-multiplying applies each turn around a fixed view
                    // axis, so the keys never change meaning as the model moves.
                    KeyCode::Left => orientation = multiply(rotation_y(-step), orientation),
                    KeyCode::Right => orientation = multiply(rotation_y(step), orientation),
                    KeyCode::Up => orientation = multiply(rotation_x(step), orientation),
                    KeyCode::Down => orientation = multiply(rotation_x(-step), orientation),
                    KeyCode::Char('+') | KeyCode::Char('=') => zoom *= 1.15,
                    KeyCode::Char('-') | KeyCode::Char('_') => zoom /= 1.15,
                    KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                        // C=1 intentionally leaves the cursor at the image's
                        // origin. Delete the placement before restoring normal
                        // input so the shell prompt cannot be drawn over it.
                        let mut out = io::stdout().lock();
                        write!(out, "\x1b_Ga=d,d=I,i={previous_id};\x1b\\")?;
                        out.flush()?;
                        return Ok(());
                    }
                    _ => continue,
                }
                image_id = image_id.checked_add(1).ok_or("too many rotations")?;
                break;
            }
        }
    }
}

fn animate_kitty(
    tris: &[Triangle],
    view: ModelView,
    args: &Args,
) -> Result<(), Box<dyn std::error::Error>> {
    let gap_ms = (1000 / u32::from(args.fps)).max(1);
    // Frame one is both the visible placement and the animation's root frame.
    let first = png_for(tris, view, args.width, args.height, identity(), 1.0)?;
    let (cols, rows) = placement_size(args, args.width, args.height);
    move_cursor_home()?;
    transmit_png(&first, "a=T,f=100,q=2,i=1", cols, rows)?;
    for frame in 1..args.frames {
        let spin = std::f32::consts::TAU * f32::from(frame) / f32::from(args.frames);
        let png = png_for(tris, view, args.width, args.height, rotation_y(spin), 1.0)?;
        transmit_png(&png, &format!("a=f,f=100,q=2,i=1,z={gap_ms}"), 0, 0)?;
    }
    let mut out = io::stdout().lock();
    // Give the root frame a duration, then let Kitty cycle frames indefinitely.
    write!(out, "\x1b_Ga=a,i=1,r=1,z={gap_ms};\x1b\\")?;
    write!(out, "\x1b_Ga=a,i=1,s=3,v=1;\x1b\\")?;
    out.flush()?;
    Ok(())
}

fn transmit_png(png: &[u8], action: &str, cols: u32, rows: u32) -> io::Result<()> {
    let encoded = STANDARD.encode(png);
    // Assemble the whole escape sequence and write it once instead of issuing
    // many small writes through stdout's line buffer.
    let mut buf = Vec::with_capacity(encoded.len() + encoded.len() / 4096 * 16 + 64);
    // f=100 declares PNG. Kitty requires a=f to be repeated on every frame chunk.
    let chunks = encoded.as_bytes().chunks(4096);
    let last = chunks.len().saturating_sub(1);
    for (part, chunk) in chunks.enumerate() {
        let more = u8::from(part != last);
        if part == 0 {
            write!(buf, "\x1b_G{action},m={more}")?;
            if cols != 0 {
                write!(buf, ",c={cols}")?;
            }
            if rows != 0 {
                write!(buf, ",r={rows}")?;
            }
            buf.push(b';');
        } else if action.starts_with("a=f,") {
            write!(buf, "\x1b_Ga=f,m={more};")?;
        } else {
            write!(buf, "\x1b_Gm={more};")?;
        }
        buf.extend_from_slice(chunk);
        buf.extend_from_slice(b"\x1b\\");
    }
    let mut out = io::stdout().lock();
    out.write_all(&buf)?;
    out.flush()
}
