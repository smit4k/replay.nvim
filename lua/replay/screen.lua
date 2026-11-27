local uv = vim.uv
local M = {}
local stream
local limit = 512
local screen_events = {
  default_colors_set = true,
  hl_attr_define = true,
  grid_resize = true,
  grid_clear = true,
  grid_destroy = true,
  grid_line = true,
  grid_scroll = true,
  grid_cursor_goto = true,
  mode_info_set = true,
  mode_change = true,
}

function M.stop()
  if stream then
    local handle = stream
    stream = nil
    handle:close()
  end
end

function M.start(emit)
  local uis = vim.api.nvim_list_uis()
  if #uis == 0 then
    return
  end
  local function warn(reason)
    vim.schedule(function()
      vim.notify("Replay is recording editor events only. " .. reason, vim.log.levels.WARN)
    end)
  end
  for _, ui in ipairs(uis) do
    if
      ui.ext_multigrid
      or ui.ext_messages
      or ui.ext_popupmenu
      or ui.ext_cmdline
      or ui.ext_tabline
      or ui.ext_wildmenu
    then
      warn("The active UI draws some widgets itself.")
      return
    end
    if ui.width > limit or ui.height > limit then
      warn("The active UI exceeds the capture grid size.")
      return
    end
  end
  local address = vim.v.servername
  if address == "" then
    warn("Neovim has no local RPC socket.")
    return
  end

  local handle = uv.new_pipe(false)
  stream = handle
  local function failed(reason)
    if stream == handle then
      M.stop()
      warn(reason)
    end
  end

  handle:connect(address, function(err)
    if stream ~= handle then
      return
    end
    if err then
      failed("Cannot connect to Neovim's local RPC socket: " .. err)
      return
    end
    local unpack = vim.mpack.Unpacker()
    local frame = {}
    handle:read_start(function(read_err, data)
      if stream ~= handle then
        return
      end
      if read_err or not data then
        failed(read_err or "The Neovim UI connection closed.")
        return
      end
      local offset = 1
      while offset <= #data do
        local ok, message, next_offset = pcall(unpack, data, offset)
        if not ok then
          failed("Cannot decode the Neovim UI stream.")
          return
        end
        offset = next_offset
        if message and message[1] == 1 and message[3] ~= vim.NIL then
          failed("Neovim rejected the UI attachment.")
          return
        end
        if message and message[1] == 2 and message[2] == "redraw" then
          for _, event in ipairs(message[3]) do
            if event[1] == "flush" and #frame > 0 then
              frame[#frame + 1] = event
              emit("ui_redraw", { events = frame })
              frame = {}
            elseif screen_events[event[1]] then
              frame[#frame + 1] = event
            end
          end
        end
      end
    end)
    -- Neovim uses the smallest attached viewport, so the recorder doesn't constrain the terminal.
    handle:write(vim.mpack.encode({ 0, 0, "nvim_ui_attach", { limit, limit, { rgb = true, ext_linegrid = true } } }))
  end)
end

return M
