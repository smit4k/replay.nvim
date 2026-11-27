use std::{io, path::Path, time::Duration};

use serde::Deserialize;

use crate::player::{EditorFrame, Player};
use crate::protocol::{Event, TimedEvent};
use crate::renderer::{
    encoder::VideoEncoder,
    headless_nvim::{Grid, HeadlessNvim, NvimStartup},
    rasterizer::Rasterizer,
};

const FPS: u32 = 60;
const COLUMNS: u32 = 160;
const ROWS: u32 = 45;
const CELL_WIDTH: u32 = 12;
const CELL_HEIGHT: u32 = 24;
const KEY_IDLE_NS: u64 = 1_000_000_000;
const MAX_SEQUENCE_KEYS: usize = 8;

#[derive(Default)]
struct KeySequence {
    keys: Vec<String>,
    label: String,
    last_press_ns: Option<u64>,
}

impl KeySequence {
    fn push(&mut self, timestamp_ns: u64, key: String) {
        if self.label_at(timestamp_ns).is_none() {
            self.keys.clear();
        }
        if self.keys.len() == MAX_SEQUENCE_KEYS {
            self.keys.remove(0);
        }
        self.keys.push(key);
        self.label = self.keys.join(" ");
        self.last_press_ns = Some(timestamp_ns);
    }

    fn label_at(&self, timestamp_ns: u64) -> Option<&str> {
        self.last_press_ns
            .filter(|&last| timestamp_ns - last < KEY_IDLE_NS)
            .map(|_| self.label.as_str())
    }
}

#[derive(Default, Deserialize)]
pub(crate) struct RenderOptions {
    #[serde(flatten)]
    startup: NvimStartup,
    font: Option<String>,
    height: Option<u32>,
    scale: Option<u32>,
}

const SETUP_EDITOR: &str = r#"
  local scheme, configured = ...
  local api = vim.api
  vim.o.modeline = false
  if not vim.g.syntax_on and not vim.g.syntax_manual then
    vim.cmd.syntax('enable')
  end
  if scheme ~= vim.g.colors_name then
    local applied = scheme:match('^[%w_.-]+$') and pcall(vim.cmd.colorscheme, scheme)
    if not applied and not configured then
      vim.cmd.colorscheme('habamax')
    end
  end
  local buffers = {}
  local win = api.nvim_get_current_win()
  _G.replay_apply_frame = function(id, path, filetype, lines, row, col, topline)
    local buf = buffers[id]
    local opened = not buf or not api.nvim_buf_is_loaded(buf)
    if opened then
      buf = api.nvim_create_buf(id ~= vim.NIL, true)
      buffers[id] = buf
      vim.bo[buf].buftype = 'nowrite'
      vim.bo[buf].modeline = false
      if path ~= '' then
        pcall(api.nvim_buf_set_name, buf, path)
      end
    end
    if lines ~= vim.NIL then
      api.nvim_buf_set_lines(buf, 0, -1, false, lines)
    end
    api.nvim_buf_call(buf, function()
      if opened and path ~= '' then
        -- Replay contents are already in memory when file plugins run.
        api.nvim_exec_autocmds('BufReadPost', { buffer = buf, modeline = false })
      end
      if filetype:match('^[%w_.-]+$') and vim.bo[buf].filetype ~= filetype then
        vim.bo[buf].filetype = filetype
      end
    end)
    if api.nvim_win_get_buf(win) ~= buf then
      api.nvim_win_set_buf(win, buf)
    end
    row = math.max(1, math.min(row, api.nvim_buf_line_count(buf)))
    col = math.min(col, #api.nvim_buf_get_lines(buf, row - 1, row, false)[1])
    api.nvim_win_call(win, function()
      vim.fn.winrestview({ lnum = row, col = col, topline = topline ~= vim.NIL and topline or row })
    end)
  end
"#;

pub(crate) fn render(player: &Player, output: &Path, options: &RenderOptions) -> io::Result<()> {
    let info = player.info();
    let height = match (options.height, options.scale.unwrap_or(1)) {
        (Some(height), _) => height,
        (None, scale @ (1 | 2)) => ROWS * CELL_HEIGHT * scale,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "render scale must be 1 or 2",
            ));
        }
    };
    if !matches!(height, 1080 | 1440 | 2160) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "render height must be 1080, 1440, or 2160",
        ));
    }
    let width = height * 16 / 9;
    let font = options.font.as_deref().map(Path::new);
    let mut rasterizer =
        Rasterizer::new(CELL_WIDTH, CELL_HEIGHT, 19.0, font, Some((width, height)))?;
    let first_redraw = player
        .visual_events()
        .find_map(|update| match update {
            Ok(TimedEvent {
                event: Event::UiRedraw { events },
                ..
            }) => Some(Ok(events)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .transpose()?;
    let mut grid = Grid::default();
    let mut skip_first_redraw = first_redraw.is_some();
    let mut nvim = if let Some(events) = first_redraw {
        // Use the first captured screen for the frames before the UI attached.
        grid.apply_redraw(&events)?;
        None
    } else {
        let mut nvim = HeadlessNvim::spawn(COLUMNS, ROWS, &options.startup)?;
        let scheme = info.metadata.colorscheme.as_deref().unwrap_or_default();
        nvim.request(
            "nvim_exec_lua",
            &(SETUP_EDITOR, (scheme, options.startup.vimrc.is_some())),
        )?;
        Some(nvim)
    };

    let mut encoder = VideoEncoder::spawn(output, width, height, FPS)?;
    let mut updates = player.visual_events();
    let mut next = updates.next().transpose()?;
    let mut previous = None;
    let mut keys = KeySequence::default();
    let frame_duration = Duration::from_secs(1) / FPS;
    let mut timestamps = frame_times(info.duration_ns).peekable();
    while let Some(timestamp) = timestamps.next() {
        let mut grid_changed = false;
        while let Some(update) = next.take_if(|update| update.timestamp_ns <= timestamp) {
            match update.event {
                Event::UiRedraw { events } => {
                    if !std::mem::take(&mut skip_first_redraw) {
                        grid.apply_redraw(&events)?;
                        grid_changed = true;
                    }
                }
                Event::KeyPress { key } if !key.is_empty() => keys.push(update.timestamp_ns, key),
                _ => {}
            }
            next = updates.next().transpose()?;
        }
        let grid = if let Some(nvim) = &mut nvim {
            grid_changed = true;
            restore_editor(nvim, player, timestamp, &mut previous)?;
            nvim.redraw()?
        } else if grid.complete() {
            &grid
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete captured screen",
            ));
        };
        let label = keys.label_at(timestamp);
        let frame = rasterizer.render(grid, frame_duration, label, grid_changed)?;
        encoder.write_frame(frame, timestamps.peek().is_none())?;
    }
    encoder.finish()
}

fn restore_editor(
    nvim: &mut HeadlessNvim,
    player: &Player,
    timestamp: u64,
    previous: &mut Option<EditorFrame>,
) -> io::Result<()> {
    // shortcut: seeking each frame repeats decoding within a keyframe, stream deltas if long exports are too slow
    let frame = EditorFrame::from(player.seek(timestamp)?.snapshot);
    if previous.as_ref() != Some(&frame) {
        let lines = previous
            .as_ref()
            .is_none_or(|prev| prev.buffer_id != frame.buffer_id || prev.lines != frame.lines)
            .then_some(&frame.lines);
        nvim.request(
            "nvim_exec_lua",
            &(
                "replay_apply_frame(...)",
                (
                    frame.buffer_id,
                    frame.path.as_deref().unwrap_or_default(),
                    frame.filetype.as_deref().unwrap_or_default(),
                    lines,
                    frame.row,
                    frame.col,
                    frame.topline,
                ),
            ),
        )?;
        *previous = Some(frame);
    }
    Ok(())
}

fn frame_times(duration_ns: u64) -> impl Iterator<Item = u64> {
    let second = Duration::from_secs(1).as_nanos();
    let duration = u128::from(duration_ns);
    let fps = u128::from(FPS);
    let last_frame = (duration * fps).div_ceil(second);
    (0..=last_frame).map(move |index| (index * second / fps).min(duration) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::Value;

    #[test]
    fn rapid_keys_share_a_line_until_the_next_pause() {
        let mut keys = KeySequence::default();
        assert_eq!(keys.label_at(0), None);
        keys.push(0, "d".into());
        keys.push(100_000_000, "i".into());
        keys.push(200_000_000, "w".into());
        assert_eq!(keys.label_at(1_199_999_999), Some("d i w"));
        assert_eq!(keys.label_at(1_200_000_000), None);
        keys.push(1_200_000_000, "Esc".into());
        assert_eq!(keys.label_at(1_200_000_000), Some("Esc"));
    }

    #[test]
    fn long_sequences_keep_the_most_recent_keys_in_order() {
        let mut keys = KeySequence::default();
        for key in ["a", "b", "c", "d", "e", "f", "g", "h", "Ctrl+W", "v"] {
            keys.push(0, key.into());
        }
        assert_eq!(keys.label_at(0), Some("c d e f g h Ctrl+W v"));
    }

    #[test]
    #[ignore = "requires Neovim on PATH"]
    fn configured_export_runs_file_plugins_on_the_recorded_buffer() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let init = root.join("init.lua");
        std::fs::write(
            &init,
            r#"
            vim.wait(75, function() return false end, 5)
            assert(vim.g.replay_rendering)
            vim.cmd.colorscheme('habamax')
            vim.o.laststatus = 2
            vim.o.statusline = 'Replay fixture statusline'
            vim.api.nvim_create_autocmd('BufReadPost', {
              pattern = '*.lua',
              callback = function(args)
                vim.b[args.buf].fixture_contents = vim.api.nvim_get_current_line()
              end,
            })
            "#,
        )?;
        let startup = NvimStartup {
            vimrc: Some(init),
            cwd: Some(root.clone()),
            ..Default::default()
        };
        let mut nvim = HeadlessNvim::spawn(80, 20, &startup)?;
        nvim.request("nvim_exec_lua", &(SETUP_EDITOR, ("habamax", true)))?;
        let path = root.join("sample.lua");
        nvim.request(
            "nvim_exec_lua",
            &(
                "replay_apply_frame(...)",
                (1, &path, "lua", ["local sample = true"], 1, 0, 1),
            ),
        )?;
        let state = nvim.request(
            "nvim_exec_lua",
            &(
                r#"
                local path = ...
                return {
                  vim.api.nvim_buf_get_name(0) == path,
                  vim.bo.filetype,
                  vim.b.fixture_contents,
                }
                "#,
                (&path,),
            ),
        )?;
        assert_eq!(
            state,
            Value::Array(vec![
                true.into(),
                "lua".into(),
                "local sample = true".into(),
            ]),
            "restored buffer and file plugin",
        );
        let grid = nvim.redraw()?;
        let screen: String = grid.cells().iter().map(|cell| cell.text.as_ref()).collect();
        assert!(screen.contains("Replay fixture statusline"));
        assert!(screen.starts_with("local sample = true"));
        assert_ne!(grid.cells()[0].highlight, grid.cells()[6].highlight);
        assert!(
            !path.exists(),
            "rendering should keep recorded text in memory"
        );
        Ok(())
    }
}
