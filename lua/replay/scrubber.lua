local api = vim.api
local uv = vim.uv
local M = {}
local frame_ns = 16666667
local speeds = { 0.25, 0.5, 1, 2, 4 }
local markers = api.nvim_create_namespace("replay.timeline")
local session

local function scratch()
  local buf = api.nvim_create_buf(false, true)
  vim.bo[buf].bufhidden = "wipe"
  vim.bo[buf].modifiable = false
  vim.bo[buf].readonly = true
  return buf
end

local function set_lines(buf, lines)
  vim.bo[buf].readonly = false
  vim.bo[buf].modifiable = true
  local ok, err = pcall(api.nvim_buf_set_lines, buf, 0, -1, false, lines)
  vim.bo[buf].modifiable = false
  vim.bo[buf].readonly = true
  return ok, err
end

local function timestamp(ns)
  local seconds = math.floor(ns / 1e9)
  return ("%02d:%02d"):format(math.floor(seconds / 60), seconds % 60)
end

local function timeline(state)
  if not api.nvim_win_is_valid(state.bar) then
    return
  end
  local width = api.nvim_win_get_width(state.bar)
  local label = (" %s / %s [%gx]%s"):format(
    timestamp(state.position),
    timestamp(state.duration),
    state.speed,
    state.playing and " >" or ""
  )
  local cells = math.max(1, width - #label - 2)
  local function column(time)
    return math.floor(math.min(time / math.max(state.duration, 1), 1) * (cells - 1))
  end

  local cursor = column(state.position)
  local bar = "[" .. string.rep("=", cursor) .. "|" .. string.rep("-", cells - cursor - 1) .. "]"
  set_lines(state.timeline, { (bar .. label):sub(1, width) })
  api.nvim_buf_clear_namespace(state.timeline, markers, 0, -1)
  local drawn = {}
  for _, time in ipairs(state.keyframes) do
    local col = column(time)
    if col ~= cursor and not drawn[col] then
      api.nvim_buf_set_extmark(state.timeline, markers, 0, col + 1, {
        virt_text = { { "+", "DiagnosticHint" } },
        virt_text_pos = "overlay",
      })
      drawn[col] = true
    end
  end
end

local function apply_frame(state, frame)
  if type(frame) ~= "table" or type(frame.lines) ~= "table" then
    return nil, "Invalid replay frame"
  end
  local ok, err = set_lines(state.buffer, frame.lines)
  if not ok then
    return nil, err
  end
  local row = math.max(1, math.min(tonumber(frame.row) or 1, api.nvim_buf_line_count(state.buffer)))
  local line = frame.lines[row] or ""
  local col = math.max(0, math.min(tonumber(frame.col) or 0, #line))
  api.nvim_win_call(state.win, function()
    vim.fn.winrestview({ lnum = row, col = col, topline = tonumber(frame.topline) or row })
  end)
  return true
end

local function pause(state)
  state.playing = false
  state.timer:stop()
end

local function seek(state)
  if state.pending then
    return
  end
  local target = state.target
  state.pending = true
  state.seek(target, function(err, frame)
    if session ~= state or not api.nvim_buf_is_valid(state.buffer) then
      return
    end
    state.pending = false
    if not err and not state.playing and state.target ~= target then
      seek(state)
      return
    end
    if not err then
      local _
      _, err = apply_frame(state, frame)
    end
    if err then
      pause(state)
      state.target = state.position
      timeline(state)
      vim.notify(err, vim.log.levels.ERROR)
      return
    end

    state.position = target
    timeline(state)
    if state.target ~= target then
      seek(state)
    end
  end)
end

local function step(state, delta)
  pause(state)
  state.target = math.max(0, math.min(state.duration, state.target + delta))
  timeline(state)
  seek(state)
end

local function tick(state)
  if session ~= state or not state.playing then
    return
  end
  local now = uv.hrtime()
  state.target = math.min(state.duration, state.target + math.floor((now - state.tick) * state.speed))
  state.tick = now
  if state.target == state.duration then
    pause(state)
  end
  seek(state)
end

local function toggle(state)
  if state.playing then
    pause(state)
    state.target = state.position
  elseif state.duration > 0 then
    if state.target == state.duration then
      state.target = 0
      seek(state)
    end
    state.playing = true
    state.tick = uv.hrtime()
    state.timer:start(
      0,
      17,
      vim.schedule_wrap(function()
        tick(state)
      end)
    )
  end
  timeline(state)
end

local function place_bar(state)
  return {
    relative = "win",
    win = state.win,
    anchor = "SW",
    row = api.nvim_win_get_height(state.win),
    col = 0,
    width = api.nvim_win_get_width(state.win),
    height = 1,
    style = "minimal",
    focusable = false,
  }
end

local function close(state)
  if session ~= state then
    return
  end
  session = nil
  state.timer:close()
  api.nvim_del_augroup_by_id(state.group)
  if api.nvim_win_is_valid(state.bar) then
    api.nvim_win_close(state.bar, true)
  end
end

function M.open(info, seek_frame)
  if
    type(info) ~= "table"
    or type(info.duration_ns) ~= "number"
    or info.duration_ns < 0
    or type(info.keyframes) ~= "table"
  then
    return nil, "Invalid replay timeline"
  end
  local keyframes = {}
  for _, time in ipairs(info.keyframes) do
    if type(time) == "number" and time >= 0 and time <= info.duration_ns then
      keyframes[#keyframes + 1] = time
    end
  end

  local buf
  if session then
    local old = session
    api.nvim_set_current_tabpage(old.tab)
    api.nvim_set_current_win(old.win)
    buf = old.buffer
    close(old)
  else
    buf = scratch()
    vim.cmd("noautocmd tab split")
    api.nvim_win_set_buf(0, buf)
  end
  local state = {
    tab = api.nvim_get_current_tabpage(),
    win = api.nvim_get_current_win(),
    buffer = buf,
    timeline = scratch(),
    timer = uv.new_timer(),
    duration = info.duration_ns,
    keyframes = keyframes,
    seek = seek_frame,
    speed = 1,
    position = 0,
    target = 0,
  }
  session = state
  state.bar = api.nvim_open_win(state.timeline, false, place_bar(state))
  state.group = api.nvim_create_augroup("ReplayScrubber", { clear = true })
  api.nvim_create_autocmd("WinClosed", {
    group = state.group,
    pattern = tostring(state.win),
    once = true,
    callback = function()
      close(state)
    end,
  })
  api.nvim_create_autocmd("BufWipeout", {
    group = state.group,
    buffer = state.buffer,
    once = true,
    callback = function()
      vim.schedule(function()
        close(state)
      end)
    end,
  })
  api.nvim_create_autocmd({ "VimResized", "WinResized" }, {
    group = state.group,
    callback = function()
      if api.nvim_win_is_valid(state.bar) then
        api.nvim_win_set_config(state.bar, place_bar(state))
        timeline(state)
      end
    end,
  })

  local function map(key, action)
    vim.keymap.set("n", key, action, { buffer = state.buffer })
  end
  map("<Left>", function()
    step(state, -frame_ns)
  end)
  map("<Right>", function()
    step(state, frame_ns)
  end)
  map("<Space>", function()
    toggle(state)
  end)
  for i, speed in ipairs(speeds) do
    map(tostring(i), function()
      state.speed = speed
      timeline(state)
    end)
  end

  timeline(state)
  seek(state)
  return true
end

return M
