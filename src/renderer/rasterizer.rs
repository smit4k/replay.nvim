use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, PhysicalGlyph, Shaping, Style, SwashCache,
    SwashContent, SwashImage, Weight, Wrap, fontdb,
};
use tiny_skia::{Color, Paint, Pixmap, Rect, Transform};

use super::headless_nvim::{Cell, CursorShape, Grid, Highlight};

#[derive(Clone, Copy, Debug)]
pub struct RgbaFrame<'a> {
    pub width: u32,
    pub height: u32,
    /// Tightly packed, row-major RGBA8 pixels.
    pub pixels: &'a [u8],
}

/// Shapes text per grid cell so glyph advances never move later columns.
pub struct Rasterizer {
    fonts: FontSystem,
    family: String,
    buffer: Buffer,
    swash: SwashCache,
    glyphs: HashMap<(Arc<str>, bool, bool), Vec<PhysicalGlyph>>,
    baseline: f32,
    cell_width: u32,
    cell_height: u32,
    pixmap: Pixmap,
    cursor: Option<(f32, f32)>,
}

impl Rasterizer {
    /// Use a TrueType font file or a system monospace font. Sizes are in pixels.
    pub fn new(
        cell_width: u32,
        cell_height: u32,
        font_size: f32,
        font_path: Option<&Path>,
    ) -> io::Result<Self> {
        if cell_width == 0 || cell_height == 0 || !font_size.is_finite() || font_size <= 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cell or font size",
            ));
        }
        let mut fonts = FontSystem::new();
        let db = fonts.db_mut();
        let family = if let Some(path) = font_path {
            let data = std::fs::read(path)?;
            let ids = db.load_font_source(fontdb::Source::Binary(Arc::new(data)));
            ids.first().and_then(|id| db.face(*id))
        } else {
            ["Menlo", "DejaVu Sans Mono", "Consolas", "Noto Sans Mono"]
                .into_iter()
                .find_map(|name| {
                    db.query(&fontdb::Query {
                        families: &[Family::Name(name)],
                        ..fontdb::Query::default()
                    })
                })
                .and_then(|id| db.face(id))
                .or_else(|| db.faces().find(|face| face.monospaced))
        }
        .and_then(|face| face.families.first())
        .map(|(name, _)| name.clone())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no usable renderer font found"))?;

        let mut buffer = Buffer::new(&mut fonts, Metrics::new(font_size, cell_height as f32));
        let mut shaping = buffer.borrow_with(&mut fonts);
        shaping.set_wrap(Wrap::None);
        shaping.set_text(
            "M",
            &Attrs::new().family(Family::Name(&family)),
            Shaping::Advanced,
            None,
        );
        let baseline = shaping
            .layout_runs()
            .next()
            .map_or(0.0, |line| line.line_y.round());

        Ok(Self {
            fonts,
            family,
            buffer,
            swash: SwashCache::new(),
            glyphs: HashMap::new(),
            baseline,
            cell_width,
            cell_height,
            pixmap: new_pixmap(1, 1)?,
            cursor: None,
        })
    }

    /// Render a complete grid. Use elapsed video time to advance cursor motion.
    pub fn render(&mut self, grid: &Grid, elapsed: Duration) -> io::Result<RgbaFrame<'_>> {
        let (columns, rows) = grid.dimensions();
        let size = |cells, pixels| {
            u32::try_from(cells)
                .ok()
                .and_then(|cells| cells.checked_mul(pixels))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame size overflow"))
        };
        let width = size(columns, self.cell_width)?;
        let height = size(rows, self.cell_height)?;
        if self.pixmap.width() != width || self.pixmap.height() != height {
            self.pixmap = new_pixmap(width, height)?;
            self.cursor = None;
        }

        let background = grid.highlight(0).background.unwrap_or(0);
        self.pixmap.fill(color(background, 255));
        for (i, cell) in grid.cells().iter().enumerate() {
            let bg = grid.highlight(cell.highlight).background.unwrap_or(0);
            if bg != background
                && let Some(bounds) =
                    self.cell_bounds((i % columns) as f32, (i / columns) as f32, 1)
            {
                fill_rect(&mut self.pixmap, bounds, bg, 255);
            }
        }
        for (i, cell) in grid.cells().iter().enumerate() {
            let highlight = grid.highlight(cell.highlight);
            if let Some(bounds) = self.cell_bounds(
                (i % columns) as f32,
                (i / columns) as f32,
                cell_span(grid, columns, i),
            ) {
                self.draw_cell(cell, highlight, bounds);
            }
        }
        self.draw_cursor(grid, elapsed);

        Ok(RgbaFrame {
            width,
            height,
            pixels: self.pixmap.data(),
        })
    }

    fn cell_bounds(&self, col: f32, row: f32, span: u32) -> Option<Rect> {
        Rect::from_xywh(
            col * self.cell_width as f32,
            row * self.cell_height as f32,
            (self.cell_width * span) as f32,
            self.cell_height as f32,
        )
    }

    fn draw_cell(&mut self, cell: &Cell, highlight: Highlight, bounds: Rect) {
        let foreground = highlight.foreground.unwrap_or(0xff_ff_ff);
        if !cell.text.trim().is_empty() {
            let glyphs = self
                .glyphs
                .entry((cell.text.clone(), highlight.bold, highlight.italic))
                .or_insert_with(|| {
                    let attrs = Attrs::new()
                        .family(Family::Name(&self.family))
                        .weight(if highlight.bold {
                            Weight::BOLD
                        } else {
                            Weight::NORMAL
                        })
                        .style(if highlight.italic {
                            Style::Italic
                        } else {
                            Style::Normal
                        });
                    let mut shaping = self.buffer.borrow_with(&mut self.fonts);
                    shaping.set_text(&cell.text, &attrs, Shaping::Advanced, None);
                    shaping
                        .layout_runs()
                        .flat_map(|line| line.glyphs.iter())
                        .map(|glyph| glyph.physical((0.0, self.baseline), 1.0))
                        .collect()
                });
            for glyph in glyphs {
                if let Some(image) = self.swash.get_image(&mut self.fonts, glyph.cache_key) {
                    blend_glyph(&mut self.pixmap, bounds, glyph, image, foreground);
                }
            }
        }
        // Continuation cells are already covered by their leading glyph's decoration.
        if !cell.text.is_empty() {
            if highlight.underline
                && let Some(line) =
                    Rect::from_xywh(bounds.left(), bounds.bottom() - 1.0, bounds.width(), 1.0)
            {
                fill_rect(
                    &mut self.pixmap,
                    line,
                    highlight.special.unwrap_or(foreground),
                    255,
                );
            }
            if highlight.strikethrough
                && let Some(line) = Rect::from_xywh(
                    bounds.left(),
                    bounds.top() + (bounds.height() * 0.5).floor(),
                    bounds.width(),
                    1.0,
                )
            {
                fill_rect(&mut self.pixmap, line, foreground, 255);
            }
        }
    }

    fn draw_cursor(&mut self, grid: &Grid, elapsed: Duration) {
        let Some((col, row)) = grid.cursor() else {
            self.cursor = None;
            return;
        };
        let target = (col as f32, row as f32);
        let mut position = self.cursor.unwrap_or(target);
        let follow = 1.0 - (-elapsed.as_secs_f32() / 0.045).exp();
        position.0 += (target.0 - position.0) * follow;
        position.1 += (target.1 - position.1) * follow;
        if (target.0 - position.0)
            .abs()
            .max((target.1 - position.1).abs())
            < 0.01
        {
            position = target;
        }
        self.cursor = Some(position);

        let (columns, _) = grid.dimensions();
        let i = row * columns + col;
        let cell = &grid.cells()[i];
        let highlight = grid.highlight(cell.highlight);
        let style = grid.cursor_style();
        let (foreground, background) = if style.highlight == 0 {
            (
                highlight.background.unwrap_or(0),
                highlight.foreground.unwrap_or(0xff_ff_ff),
            )
        } else {
            let cursor = grid.highlight(style.highlight);
            (
                cursor.foreground.unwrap_or(0),
                cursor.background.unwrap_or(0xff_ff_ff),
            )
        };
        let Some(bounds) = self.cell_bounds(position.0, position.1, cell_span(grid, columns, i))
        else {
            return;
        };
        let fraction = f32::from(style.percentage) / 100.0;
        match style.shape {
            CursorShape::Block => {
                let settled = position == target;
                fill_rect(
                    &mut self.pixmap,
                    bounds,
                    background,
                    if settled { 255 } else { 176 },
                );
                if settled {
                    self.draw_cell(
                        cell,
                        Highlight {
                            foreground: Some(foreground),
                            ..highlight
                        },
                        bounds,
                    );
                }
            }
            CursorShape::Horizontal => {
                let height = (fraction * bounds.height()).max(1.0);
                if let Some(bar) = Rect::from_xywh(
                    bounds.left(),
                    bounds.bottom() - height,
                    bounds.width(),
                    height,
                ) {
                    fill_rect(&mut self.pixmap, bar, background, 255);
                }
            }
            CursorShape::Vertical => {
                let width = (fraction * self.cell_width as f32).max(1.0);
                if let Some(bar) =
                    Rect::from_xywh(bounds.left(), bounds.top(), width, bounds.height())
                {
                    fill_rect(&mut self.pixmap, bar, background, 255);
                }
            }
        }
    }
}

fn cell_span(grid: &Grid, columns: usize, i: usize) -> u32 {
    if i % columns + 1 < columns && grid.cells()[i + 1].text.is_empty() {
        2
    } else {
        1
    }
}

fn new_pixmap(width: u32, height: u32) -> io::Result<Pixmap> {
    Pixmap::new(width, height).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid frame size {width}x{height}"),
        )
    })
}

fn color(rgb: u32, alpha: u8) -> Color {
    Color::from_rgba8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, alpha)
}

fn fill_rect(pixmap: &mut Pixmap, rect: Rect, rgb: u32, alpha: u8) {
    let mut paint = Paint::default();
    paint.set_color(color(rgb, alpha));
    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
}

fn blend_glyph(
    pixmap: &mut Pixmap,
    bounds: Rect,
    glyph: &PhysicalGlyph,
    image: &SwashImage,
    rgb: u32,
) {
    let x = bounds.left() as i32 + glyph.x + image.placement.left;
    let y = bounds.top() as i32 + glyph.y - image.placement.top;
    let width = image.placement.width as usize;
    let left = x.max(bounds.left() as i32);
    let right = (x + image.placement.width as i32).min(bounds.right() as i32);
    let top = y.max(bounds.top() as i32);
    let bottom = (y + image.placement.height as i32).min(bounds.bottom() as i32);
    let stride = pixmap.width() as usize * 4;
    let rgba = pixmap.data_mut();
    let mask_rgb = [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8];

    for py in top..bottom {
        for px in left..right {
            let src = (py - y) as usize * width + (px - x) as usize;
            let (color, alpha) = match image.content {
                SwashContent::Mask => (mask_rgb, image.data[src]),
                SwashContent::Color => {
                    let i = src * 4;
                    (
                        [image.data[i], image.data[i + 1], image.data[i + 2]],
                        image.data[i + 3],
                    )
                }
                SwashContent::SubpixelMask => return,
            };
            if alpha != 0 {
                let dst = py as usize * stride + px as usize * 4;
                // The frame is opaque, so source-over only changes the RGB channels.
                let a = u16::from(alpha);
                for (channel, source) in rgba[dst..dst + 3].iter_mut().zip(color) {
                    *channel = ((u16::from(source) * a + u16::from(*channel) * (255 - a) + 127)
                        / 255) as u8;
                }
            }
        }
    }
}
