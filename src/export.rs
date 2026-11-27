use std::{io, path::Path, time::Duration};

use crate::player::{EditorFrame, Player};
use crate::renderer::{encoder::VideoEncoder, headless_nvim::HeadlessNvim, rasterizer::Rasterizer};

const FPS: u32 = 60;
const COLUMNS: u32 = 160;
const ROWS: u32 = 45;
const CELL_WIDTH: u32 = 12;
const CELL_HEIGHT: u32 = 24;

const SETUP_EDITOR: &str = r#"
  local scheme = ...
  vim.o.modeline = false
  vim.cmd.syntax('enable')
  if not scheme:match('^[%w_.-]+$') or not pcall(vim.cmd.colorscheme, scheme) then
    vim.cmd.colorscheme('habamax')
  end
"#;

const APPLY_FRAME: &str = r#"
  local lines, filetype, row, col, topline = ...
  if filetype:match('^[%w_.-]*$') and vim.bo.filetype ~= filetype then
    vim.bo.filetype = filetype
  end
  vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
  row = math.max(1, math.min(row, #lines))
  col = math.min(col, #(lines[row] or ''))
  vim.fn.winrestview({ lnum = row, col = col, topline = topline ~= vim.NIL and topline or row })
"#;

pub(crate) fn render(player: &Player, output: &Path) -> io::Result<()> {
    let info = player.info();
    let step = Duration::from_secs(1) / FPS;
    let second = Duration::from_secs(1).as_nanos();
    let duration = u128::from(info.duration_ns);
    let last_frame = (duration * u128::from(FPS)).div_ceil(second);

    let mut nvim = HeadlessNvim::spawn(COLUMNS, ROWS)?;
    let scheme = info.metadata.colorscheme.as_deref().unwrap_or_default();
    nvim.request("nvim_exec_lua", &(SETUP_EDITOR, (scheme,)))?;
    let mut rasterizer = Rasterizer::new(CELL_WIDTH, CELL_HEIGHT, 19.0, None)?;
    let mut encoder = VideoEncoder::spawn(output, COLUMNS * CELL_WIDTH, ROWS * CELL_HEIGHT, FPS)?;

    let mut previous = None;
    // shortcut: seeking each frame repeats decoding within a keyframe, stream deltas if long exports are too slow
    for index in 0..=last_frame {
        let timestamp = (index * second / u128::from(FPS)).min(duration) as u64;
        let frame = EditorFrame::from(player.seek(timestamp)?.snapshot);
        if previous.as_ref() != Some(&frame) {
            let filetype = frame.filetype.as_deref().unwrap_or_default();
            nvim.request(
                "nvim_exec_lua",
                &(
                    APPLY_FRAME,
                    (&frame.lines, filetype, frame.row, frame.col, frame.topline),
                ),
            )?;
            previous = Some(frame);
        }
        encoder.write_frame(rasterizer.render(nvim.redraw()?, step)?)?;
    }
    encoder.finish()
}
