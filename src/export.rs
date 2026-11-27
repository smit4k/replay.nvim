use std::{io, path::Path, time::Duration};

use serde::Deserialize;

use crate::player::{EditorFrame, Player};
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

#[derive(Default, Deserialize)]
pub(crate) struct RenderOptions {
    #[serde(flatten)]
    startup: NvimStartup,
    font: Option<String>,
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
    let scale = options.scale.unwrap_or(2);
    if !matches!(scale, 1 | 2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "render scale must be 1 or 2",
        ));
    }
    let width = COLUMNS * CELL_WIDTH * scale;
    let height = ROWS * CELL_HEIGHT * scale;
    let font = options.font.as_deref().map(Path::new);
    let mut rasterizer =
        Rasterizer::new(CELL_WIDTH, CELL_HEIGHT, 19.0, font, Some((width, height)))?;
    let mut updates = player.visual_events();
    let first = updates.next().transpose()?;
    let mut encoder = VideoEncoder::spawn(output, width, height, FPS)?;
    let timestamps = frame_times(info.duration_ns);

    if let Some((first_time, events)) = first {
        let mut grid = Grid::default();
        grid.apply_redraw(&events)?;
        let mut last_time = first_time;
        let mut next = updates.next().transpose()?;
        for timestamp in timestamps {
            while next.as_ref().is_some_and(|(time, _)| *time <= timestamp) {
                let Some((time, events)) = next.take() else {
                    break;
                };
                if time < last_time {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unordered visual events",
                    ));
                }
                last_time = time;
                grid.apply_redraw(&events)?;
                next = updates.next().transpose()?;
            }
            if !grid.complete() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete captured screen",
                ));
            }
            encoder.write_frame(rasterizer.render(&grid, Duration::from_secs(1) / FPS)?)?;
        }
        if first_time > info.duration_ns || next.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "visual events exceed replay duration",
            ));
        }
        return encoder.finish();
    }

    let mut nvim = HeadlessNvim::spawn(COLUMNS, ROWS, &options.startup)?;
    let scheme = info.metadata.colorscheme.as_deref().unwrap_or_default();
    nvim.request(
        "nvim_exec_lua",
        &(SETUP_EDITOR, (scheme, options.startup.vimrc.is_some())),
    )?;

    let mut previous: Option<EditorFrame> = None;
    // shortcut: seeking each frame repeats decoding within a keyframe, stream deltas if long exports are too slow
    for timestamp in timestamps {
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
            previous = Some(frame);
        }
        encoder.write_frame(rasterizer.render(nvim.redraw()?, Duration::from_secs(1) / FPS)?)?;
    }
    encoder.finish()
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
