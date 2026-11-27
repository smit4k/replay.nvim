use std::collections::HashMap;
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmpv::Value;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct Cell {
    pub text: Arc<str>,
    pub highlight: u64,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: Arc::from(" "),
            highlight: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Highlight {
    pub foreground: Option<u32>,
    pub background: Option<u32>,
    pub special: Option<u32>,
    pub reverse: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) enum CursorShape {
    #[default]
    Block,
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CursorStyle {
    pub shape: CursorShape,
    pub percentage: u8,
    pub highlight: u64,
}

/// Neovim's composited screen grid, including splits and floating windows.
#[derive(Default)]
pub struct Grid {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    highlights: HashMap<u64, Highlight>,
    defaults: Highlight,
    cursor: Option<(usize, usize)>,
    cursor_styles: Vec<CursorStyle>,
    mode: usize,
    redraw_pending: bool,
}

impl Grid {
    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub fn complete(&self) -> bool {
        !self.redraw_pending && !self.cells.is_empty()
    }

    pub fn apply_redraw(&mut self, events: &Value) -> io::Result<()> {
        let invalid = || invalid_event("redraw");
        for event in events.as_array().ok_or_else(invalid)? {
            let (name, batches) = event
                .as_array()
                .and_then(|event| event.split_first())
                .ok_or_else(invalid)?;
            let name = name.as_str().ok_or_else(invalid)?;
            self.redraw_pending = name != "flush";
            for batch in batches {
                self.apply(name, batch.as_array().ok_or_else(invalid)?)?;
            }
        }
        Ok(())
    }

    /// Cells in row-major order. An empty string marks a wide glyph's second cell.
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// The visible cursor as a zero-based (column, row).
    pub fn cursor(&self) -> Option<(usize, usize)> {
        self.cursor
    }

    pub(super) fn highlight(&self, id: u64) -> Highlight {
        let mut highlight = self.highlights.get(&id).copied().unwrap_or_default();
        highlight.foreground = highlight.foreground.or(self.defaults.foreground);
        highlight.background = highlight.background.or(self.defaults.background);
        highlight.special = highlight.special.or(self.defaults.special);
        if highlight.reverse {
            std::mem::swap(&mut highlight.foreground, &mut highlight.background);
        }
        highlight
    }

    pub(super) fn cursor_style(&self) -> CursorStyle {
        self.cursor_styles
            .get(self.mode)
            .copied()
            .unwrap_or_default()
    }

    fn resize(&mut self, width: usize, height: usize) -> io::Result<()> {
        let len = width
            .checked_mul(height)
            .filter(|&len| len != 0 && len <= 512 * 512)
            .ok_or_else(|| invalid_event("grid_resize"))?;
        let mut cells = Vec::new();
        cells.try_reserve_exact(len).map_err(io::Error::other)?;
        cells.resize(len, Cell::default());
        let columns = width.min(self.width);
        for row in 0..height.min(self.height) {
            cells[row * width..][..columns]
                .clone_from_slice(&self.cells[row * self.width..][..columns]);
        }
        self.width = width;
        self.height = height;
        self.cells = cells;
        self.cursor = self
            .cursor
            .filter(|&(col, row)| col < width && row < height);
        Ok(())
    }

    fn apply(&mut self, event: &str, args: &[Value]) -> io::Result<()> {
        let invalid = || invalid_event(event);
        match (event, args) {
            ("default_colors_set", [fg, bg, sp, ..]) => {
                self.defaults.foreground = rgb(fg);
                self.defaults.background = rgb(bg);
                self.defaults.special = rgb(sp);
            }
            ("hl_attr_define", [id, attributes, ..]) => {
                let id = id.as_u64().ok_or_else(invalid)?;
                let attributes = attributes.as_map().ok_or_else(invalid)?;
                let get = |name| field(attributes, name);
                let flag = |name| get(name).and_then(Value::as_bool).unwrap_or(false);
                self.highlights.insert(
                    id,
                    Highlight {
                        foreground: get("foreground").and_then(rgb),
                        background: get("background").and_then(rgb),
                        special: get("special").and_then(rgb),
                        reverse: flag("reverse"),
                        bold: flag("bold"),
                        italic: flag("italic"),
                        // shortcut: use a straight underline, add patterned strokes if needed for fidelity.
                        underline: flag("underline")
                            || flag("undercurl")
                            || flag("underdouble")
                            || flag("underdotted")
                            || flag("underdashed"),
                        strikethrough: flag("strikethrough"),
                    },
                );
            }
            ("mode_info_set", [enabled, styles, ..]) => {
                self.cursor_styles.clear();
                if enabled.as_bool().ok_or_else(invalid)? {
                    for style in styles.as_array().ok_or_else(invalid)? {
                        let style = style.as_map().ok_or_else(invalid)?;
                        let get = |name| field(style, name);
                        self.cursor_styles.push(CursorStyle {
                            shape: match get("cursor_shape").and_then(Value::as_str) {
                                Some("horizontal") => CursorShape::Horizontal,
                                Some("vertical") => CursorShape::Vertical,
                                _ => CursorShape::Block,
                            },
                            percentage: get("cell_percentage")
                                .and_then(Value::as_u64)
                                .unwrap_or(25)
                                .clamp(1, 100) as u8,
                            highlight: get("attr_id").and_then(Value::as_u64).unwrap_or(0),
                        });
                    }
                }
            }
            ("mode_change", [_, mode, ..]) => self.mode = index(mode).ok_or_else(invalid)?,
            ("grid_resize", [grid, width, height, ..]) if grid.as_u64() == Some(1) => {
                self.resize(
                    index(width).ok_or_else(invalid)?,
                    index(height).ok_or_else(invalid)?,
                )?;
            }
            ("grid_clear", [grid, ..]) if grid.as_u64() == Some(1) => {
                self.cells.fill(Cell::default());
            }
            ("grid_destroy", [grid, ..]) if grid.as_u64() == Some(1) => {
                self.cells.clear();
                self.width = 0;
                self.height = 0;
                self.cursor = None;
            }
            ("grid_cursor_goto", [grid, row, col, ..]) if grid.as_u64() == Some(1) => {
                let (row, col) = (
                    index(row).ok_or_else(invalid)?,
                    index(col).ok_or_else(invalid)?,
                );
                self.cursor = (col < self.width && row < self.height).then_some((col, row));
            }
            ("grid_line", [grid, row, col, cells, ..]) if grid.as_u64() == Some(1) => {
                let row = index(row)
                    .filter(|&row| row < self.height)
                    .ok_or_else(invalid)?;
                let col = index(col).ok_or_else(invalid)?;
                let mut remaining = self.cells[row * self.width..][..self.width]
                    .get_mut(col..)
                    .ok_or_else(invalid)?;
                let mut highlight = 0;
                for cell in cells.as_array().ok_or_else(invalid)? {
                    let cell = cell.as_array().ok_or_else(invalid)?;
                    let text = cell.first().and_then(Value::as_str).ok_or_else(invalid)?;
                    if let Some(id) = cell.get(1) {
                        highlight = id.as_u64().ok_or_else(invalid)?;
                    }
                    let repeat = cell.get(2).map_or(Some(1), index).ok_or_else(invalid)?;
                    let (updated, rest) =
                        remaining.split_at_mut_checked(repeat).ok_or_else(invalid)?;
                    updated.fill(Cell {
                        text: Arc::from(text),
                        highlight,
                    });
                    remaining = rest;
                }
            }
            ("grid_scroll", [grid, top, bottom, left, right, rows, cols, ..])
                if grid.as_u64() == Some(1) =>
            {
                let top = index(top).ok_or_else(invalid)?;
                let bottom = index(bottom).ok_or_else(invalid)?;
                let left = index(left).ok_or_else(invalid)?;
                let right = index(right).ok_or_else(invalid)?;
                if top > bottom || bottom > self.height || left > right || right > self.width {
                    return Err(invalid());
                }
                let rows = offset(rows).ok_or_else(invalid)?;
                let cols = offset(cols).ok_or_else(invalid)?;
                self.scroll(top..bottom, left..right, rows, cols);
            }
            _ => {}
        }
        Ok(())
    }

    fn scroll(
        &mut self,
        rows: std::ops::Range<usize>,
        cols: std::ops::Range<usize>,
        dy: isize,
        dx: isize,
    ) {
        let len = rows.len() * cols.len();
        for i in 0..len {
            // Copy toward the source so overlapping scroll regions retain their original cells.
            let i = if (dy, dx) < (0, 0) { len - 1 - i } else { i };
            let row = rows.start + i / cols.len();
            let col = cols.start + i % cols.len();
            if let (Some(src_row), Some(src_col)) =
                (row.checked_add_signed(dy), col.checked_add_signed(dx))
                && rows.contains(&src_row)
                && cols.contains(&src_col)
            {
                self.cells[row * self.width + col] =
                    self.cells[src_row * self.width + src_col].clone();
            }
        }
    }
}

/// A blocking Msgpack-RPC session for an offline render worker.
pub struct HeadlessNvim {
    child: Child,
    output: BufReader<ChildStdout>,
    next_id: u32,
    grid: Grid,
}

#[derive(Default, Deserialize)]
pub struct NvimStartup {
    pub executable: Option<PathBuf>,
    pub vimrc: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
}

impl HeadlessNvim {
    /// Launch Neovim and attach a single RGB line grid.
    pub fn spawn(columns: u32, rows: u32, startup: &NvimStartup) -> io::Result<Self> {
        if columns == 0 || rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty Neovim grid",
            ));
        }
        let mut command = Command::new(startup.executable.as_deref().unwrap_or(Path::new("nvim")));
        command
            .args(["--embed", "-i", "NONE", "-n"])
            .args([
                "--cmd",
                "let g:replay_rendering = v:true | set noexrc nomodeline termguicolors",
            ])
            .arg("-u")
            .arg(startup.vimrc.as_deref().unwrap_or(Path::new("NONE")));
        if let Some(cwd) = &startup.cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let Some(output) = child.stdout.take() else {
            super::reap_child(&mut child);
            return Err(io::Error::other("Neovim stdout pipe unavailable"));
        };
        let mut nvim = Self {
            child,
            output: BufReader::new(output),
            next_id: 0,
            grid: Grid::default(),
        };
        nvim.request(
            "nvim_ui_attach",
            &(
                columns,
                rows,
                HashMap::from([("ext_linegrid", true), ("rgb", true)]),
            ),
        )?;
        if startup.vimrc.is_some() {
            // Some startup scripts process RPC while waiting for a plugin.
            let deadline = Instant::now() + Duration::from_secs(15);
            while nvim.request("nvim_eval", &("v:vim_did_enter",))?.as_u64() != Some(1) {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Neovim configuration did not finish starting",
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        nvim.redraw()?;
        Ok(nvim)
    }

    /// Call a Neovim API method with a tuple or slice of RPC arguments.
    pub fn request(&mut self, method: &str, params: &impl Serialize) -> io::Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.send(&(0, id, method, params))?;
        loop {
            let Value::Array(mut message) =
                rmpv::decode::read_value(&mut self.output).map_err(io::Error::other)?
            else {
                return Err(invalid_event("RPC message"));
            };
            match message.as_slice() {
                [kind, response_id, error, _]
                    if kind.as_u64() == Some(1) && response_id.as_u64() == Some(id.into()) =>
                {
                    if !error.is_nil() {
                        return Err(io::Error::other(format!("Neovim {method}: {error}")));
                    }
                    return message.pop().ok_or_else(|| invalid_event("RPC response"));
                }
                [kind, event, events] if kind.as_u64() == Some(2) => {
                    if event.as_str() == Some("redraw") {
                        self.grid.apply_redraw(events)?;
                    }
                }
                [kind, request_id, _, _] if kind.as_u64() == Some(0) => {
                    let request_id = request_id
                        .as_u64()
                        .ok_or_else(|| invalid_event("RPC request"))?;
                    self.send(&(
                        1,
                        request_id,
                        (0, "Renderer does not implement RPC methods"),
                        (),
                    ))?;
                }
                _ => return Err(invalid_event("RPC message")),
            }
        }
    }

    /// Ask Neovim to redraw and borrow the completed frame. `:redraw` flushes
    /// the pending grid events before its RPC response.
    pub fn redraw(&mut self) -> io::Result<&Grid> {
        self.request("nvim_command", &("redraw",))?;
        if !self.grid.complete() {
            return Err(invalid_event("redraw without a complete grid"));
        }
        Ok(&self.grid)
    }

    fn send(&mut self, message: &impl Serialize) -> io::Result<()> {
        let bytes = rmp_serde::to_vec(message).map_err(io::Error::other)?;
        self.child
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "Neovim stdin closed"))?
            .write_all(&bytes)
    }
}

impl Drop for HeadlessNvim {
    fn drop(&mut self) {
        super::reap_child(&mut self.child);
    }
}

fn index(value: &Value) -> Option<usize> {
    usize::try_from(value.as_u64()?).ok()
}

fn offset(value: &Value) -> Option<isize> {
    isize::try_from(value.as_i64()?).ok()
}

fn rgb(value: &Value) -> Option<u32> {
    u32::try_from(value.as_u64()?)
        .ok()
        .filter(|&rgb| rgb <= 0xff_ff_ff)
}

fn field<'a>(attributes: &'a [(Value, Value)], name: &str) -> Option<&'a Value> {
    attributes
        .iter()
        .find_map(|(key, value)| (key.as_str() == Some(name)).then_some(value))
}

fn invalid_event(event: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Invalid Neovim {event}"),
    )
}
