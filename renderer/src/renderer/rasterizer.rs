use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, PhysicalGlyph, Shaping, Style, SwashCache,
    SwashContent, SwashImage, Weight, Wrap, fontdb,
};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};

use super::headless_nvim::{Cell, CursorShape, Grid, Highlight};

#[derive(Clone, Copy, Debug)]
pub struct RgbaFrame<'a> {
    pub width: u32,
    pub height: u32,
    /// Tightly packed, row-major RGBA8 pixels.
    pub pixels: &'a [u8],
    pub reused: bool,
}

/// Shapes text per grid cell so glyph advances never move later columns.
pub struct Rasterizer {
    fonts: FontSystem,
    family: String,
    icon_family: Option<String>,
    buffer: Buffer,
    keystroke: Buffer,
    swash: SwashCache,
    glyphs: HashMap<(Arc<str>, bool, bool), Vec<PhysicalGlyph>>,
    baseline: f32,
    cell_width: u32,
    cell_height: u32,
    canvas: Option<(u32, u32)>,
    origin: (f32, f32),
    pixmap: Pixmap,
    cursor: Option<(f32, f32)>,
    rendered_key: Option<String>,
}

impl Rasterizer {
    /// Use an installed family or font file. Sizes are in pixels.
    pub fn new(
        cell_width: u32,
        cell_height: u32,
        font_size: f32,
        font: Option<&Path>,
        canvas: Option<(u32, u32)>,
    ) -> io::Result<Self> {
        if cell_width == 0 || cell_height == 0 || !font_size.is_finite() || font_size <= 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cell or font size",
            ));
        }
        let mut fonts = FontSystem::new();
        let db = fonts.db_mut();
        let font_from_file = match font {
            Some(path) if path.is_file() => {
                let data = std::fs::read(path)?;
                db.load_font_source(fontdb::Source::Binary(Arc::new(data)))
                    .into_iter()
                    .find_map(|id| db.face(id)?.families.first().map(|(name, _)| name.clone()))
            }
            _ => None,
        };
        let installed = |name: &str| {
            db.faces()
                .flat_map(|face| &face.families)
                .find(|(family, _)| family.eq_ignore_ascii_case(name))
                .map(|(family, _)| family.clone())
        };
        let nerd = installed("JetBrainsMono Nerd Font Mono").or_else(|| {
            db.faces()
                .flat_map(|face| &face.families)
                .map(|(family, _)| family)
                .filter(|family| {
                    family.ends_with(" Nerd Font Mono") && !family.starts_with("Symbols ")
                })
                .min()
                .cloned()
        });
        let family = match font {
            Some(font) => font_from_file
                .or_else(|| font.to_str().and_then(installed))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("renderer font not found: {}", font.display()),
                    )
                })?,
            None => nerd
                .clone()
                .or_else(|| {
                    ["Menlo", "DejaVu Sans Mono", "Consolas", "Noto Sans Mono"]
                        .into_iter()
                        .find_map(installed)
                })
                .or_else(|| {
                    db.faces()
                        .find(|face| face.monospaced)?
                        .families
                        .first()
                        .map(|(family, _)| family.clone())
                })
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "no usable renderer font found")
                })?,
        };
        let icon_family = if family.contains("Nerd Font") {
            None
        } else {
            nerd.or_else(|| installed("Symbols Nerd Font Mono"))
        };

        let mut buffer = Buffer::new(&mut fonts, Metrics::new(font_size, cell_height as f32));
        buffer.borrow_with(&mut fonts).set_wrap(Wrap::None);
        let baseline = font_baseline(&mut buffer, &mut fonts, &family, canvas.map(|_| cell_width));

        Ok(Self {
            fonts,
            family,
            icon_family,
            buffer,
            keystroke: Buffer::new_empty(Metrics::new(30.0, 38.0)),
            swash: SwashCache::new(),
            glyphs: HashMap::new(),
            baseline,
            cell_width,
            cell_height,
            canvas,
            origin: (0.0, 0.0),
            pixmap: new_pixmap(1, 1)?,
            cursor: None,
            rendered_key: None,
        })
    }

    /// Render a complete grid. Set `grid_changed` after any grid update.
    pub fn render(
        &mut self,
        grid: &Grid,
        elapsed: Duration,
        key: Option<&str>,
        grid_changed: bool,
    ) -> io::Result<RgbaFrame<'_>> {
        let key = key.unwrap_or_default();
        let key_changed = self.rendered_key.as_deref() != Some(key);
        // The cursor can still be moving between captured redraws.
        if !grid_changed
            && !key_changed
            && self.cursor == grid.cursor().map(|(col, row)| (col as f32, row as f32))
        {
            return Ok(self.frame(true));
        }

        let (columns, rows) = grid.dimensions();
        if columns == 0 || rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty render grid",
            ));
        }
        let size = |cells, pixels| {
            u32::try_from(cells)
                .ok()
                .and_then(|cells| cells.checked_mul(pixels))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame size overflow"))
        };
        let (width, height) = if let Some((width, height)) = self.canvas {
            let cell_width = (width / columns as u32).min(height / rows as u32 / 2);
            if cell_width == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "render grid exceeds the video canvas",
                ));
            }
            let cell_height = cell_width * 2;
            if (cell_width, cell_height) != (self.cell_width, self.cell_height) {
                let font_size =
                    self.buffer.metrics().font_size * cell_height as f32 / self.cell_height as f32;
                self.cell_width = cell_width;
                self.cell_height = cell_height;
                self.buffer
                    .set_metrics(Metrics::new(font_size, cell_height as f32));
                self.baseline = font_baseline(
                    &mut self.buffer,
                    &mut self.fonts,
                    &self.family,
                    Some(cell_width),
                );
                self.glyphs.clear();
                self.cursor = None;
            }
            self.origin = (
                ((width - columns as u32 * cell_width) / 2) as f32,
                ((height - rows as u32 * cell_height) / 2) as f32,
            );
            (width, height)
        } else {
            (
                size(columns, self.cell_width)?,
                size(rows, self.cell_height)?,
            )
        };
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
        if !key.is_empty() {
            self.draw_keystroke(key);
        }
        if key_changed {
            self.rendered_key = Some(key.to_owned());
        }

        Ok(self.frame(false))
    }

    fn frame(&self, reused: bool) -> RgbaFrame<'_> {
        RgbaFrame {
            width: self.pixmap.width(),
            height: self.pixmap.height(),
            pixels: self.pixmap.data(),
            reused,
        }
    }

    fn draw_keystroke(&mut self, key: &str) {
        let width = self.pixmap.width() as f32;
        let height = self.pixmap.height() as f32;
        let scale = (width / 1920.0).min(height / 1080.0);
        let margin = 28.0 * scale;
        let padding = 18.0 * scale;
        let text_height = 38.0 * scale;
        let box_height = 62.0 * scale;
        let radius = 12.0 * scale;

        self.keystroke
            .set_metrics(Metrics::new(30.0 * scale, text_height));
        self.keystroke.set_text(
            key,
            &Attrs::new()
                .family(Family::Name(&self.family))
                .weight(Weight::BOLD),
            Shaping::Advanced,
            None,
        );
        self.keystroke.shape_until_scroll(&mut self.fonts, false);
        let Some(line) = self.keystroke.layout_runs().next() else {
            return;
        };
        let box_width = (line.line_w.ceil() + padding * 2.0)
            .max(76.0 * scale)
            .min(width - 2.0 * margin);
        let right = width - margin;
        let bottom = height - margin;
        let left = right - box_width;
        let top = bottom - box_height;
        let mut path = PathBuilder::new();
        path.move_to(left + radius, top);
        path.line_to(right - radius, top);
        path.quad_to(right, top, right, top + radius);
        path.line_to(right, bottom - radius);
        path.quad_to(right, bottom, right - radius, bottom);
        path.line_to(left + radius, bottom);
        path.quad_to(left, bottom, left, bottom - radius);
        path.line_to(left, top + radius);
        path.quad_to(left, top, left + radius, top);
        path.close();
        if let Some(path) = path.finish() {
            let mut paint = Paint::default();
            paint.set_color(color(0, 192));
            self.pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }

        let Some(bounds) = Rect::from_xywh(
            (left + padding).round(),
            (top + (box_height - text_height) / 2.0).round(),
            box_width - padding * 2.0,
            text_height,
        ) else {
            return;
        };
        let remaining = bounds.width() - line.line_w;
        // Keep the newest keys in view if the sequence is wider than the box.
        let offset = if remaining < 0.0 {
            remaining
        } else {
            remaining / 2.0
        };
        for glyph in line.glyphs {
            let glyph = glyph.physical((offset, line.line_y), 1.0);
            if let Some(image) = self.swash.get_image(&mut self.fonts, glyph.cache_key) {
                blend_glyph(&mut self.pixmap, bounds, &glyph, image, 0xff_ff_ff);
            }
        }
    }

    fn cell_bounds(&self, col: f32, row: f32, span: u32) -> Option<Rect> {
        Rect::from_xywh(
            self.origin.0 + col * self.cell_width as f32,
            self.origin.1 + row * self.cell_height as f32,
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
                    let icon = cell.text.chars().next().is_some_and(|ch| {
                        matches!(ch as u32, 0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd)
                    });
                    let family = if icon {
                        self.icon_family.as_deref().unwrap_or(&self.family)
                    } else {
                        &self.family
                    };
                    let attrs = Attrs::new()
                        .family(Family::Name(family))
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

fn font_baseline(
    buffer: &mut Buffer,
    fonts: &mut FontSystem,
    family: &str,
    cell_width: Option<u32>,
) -> f32 {
    let mut shaping = buffer.borrow_with(fonts);
    shaping.set_text(
        "M",
        &Attrs::new().family(Family::Name(family)),
        Shaping::Advanced,
        None,
    );
    if let Some(width) = cell_width
        && let Some(advance) = shaping.layout_runs().next().map(|line| line.line_w)
        && advance > 0.0
    {
        let metrics = shaping.metrics();
        let size = (metrics.font_size * width as f32 / advance).min(metrics.line_height);
        shaping.set_metrics(Metrics::new(size, metrics.line_height));
    }
    shaping
        .layout_runs()
        .next()
        .map_or(0.0, |line| line.line_y.round())
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::ext::to_value;

    #[test]
    #[ignore = "requires an installed monospace font"]
    fn reused_frames_match_full_redraws() -> io::Result<()> {
        let mut grid = Grid::default();
        grid.apply_redraw(
            &to_value((
                ("grid_resize", (1, 10, 5)),
                ("default_colors_set", (0xffffff, 0x303030, 0)),
                ("grid_cursor_goto", (1, 0, 0)),
                ("flush",),
            ))
            .map_err(io::Error::other)?,
        )?;
        let move_cursor =
            to_value((("grid_cursor_goto", (1, 0, 5)), ("flush",))).map_err(io::Error::other)?;
        let recolor = to_value((("default_colors_set", (0xffffff, 0x505050, 0)), ("flush",)))
            .map_err(io::Error::other)?;
        let mut cached = Rasterizer::new(12, 24, 19.0, None, Some((320, 180)))?;
        let mut reference = Rasterizer::new(12, 24, 19.0, None, Some((320, 180)))?;
        let elapsed = Duration::from_secs(1) / 60;

        for index in 0..28 {
            let redraw = match index {
                4 => Some(&move_cursor),
                24 => Some(&recolor),
                _ => None,
            };
            if let Some(events) = redraw {
                grid.apply_redraw(events)?;
            }
            let key = (1..3).contains(&index).then_some("Esc");
            let expected = reference.render(&grid, elapsed, key, true)?;
            let actual = cached.render(&grid, elapsed, key, redraw.is_some())?;
            assert!(actual.pixels == expected.pixels, "frame {index} differs");
        }
        Ok(())
    }
}
