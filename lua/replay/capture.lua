local api = vim.api
local uv = vim.uv
local M = {}
local attached = {}
local session

local function eligible(buf)
  return api.nvim_buf_is_loaded(buf) and vim.bo[buf].buftype == "" and vim.bo[buf].buflisted
end

local function emit(kind, fields)
  session.emit(kind, fields)
end

local function snapshot(buf)
  emit("buf_snapshot", {
    buf = buf,
    path = api.nvim_buf_get_name(buf),
    filetype = vim.bo[buf].filetype,
    lines = api.nvim_buf_get_lines(buf, 0, -1, false),
  })
  session.seen[buf] = true
end

local function attach(buf)
  if not eligible(buf) then
    return false
  end
  if not session.seen[buf] then
    snapshot(buf)
  end
  if attached[buf] then
    return true
  end

  local single_line
  attached[buf] = api.nvim_buf_attach(buf, false, {
    on_bytes = function(_, _, tick, row, _, _, old_rows, _, _, new_rows)
      if not session then
        attached[buf] = nil
        return true
      end
      -- Undo can emit byte callbacks after the underlying line changes.
      single_line = nil
      if old_rows == 0 and new_rows == 0 then
        single_line = { tick = tick + 1, row = row, lines = api.nvim_buf_get_lines(buf, row, row + 1, false) }
      end
    end,
    on_lines = function(_, _, tick, first, last, new_last)
      if not session then
        attached[buf] = nil
        return true
      end
      if not session.seen[buf] or not eligible(buf) then
        return
      end

      local lines
      if single_line and single_line.tick == tick and single_line.row == first and new_last == first + 1 then
        lines = single_line.lines
      else
        lines = api.nvim_buf_get_lines(buf, first, new_last, false)
      end
      single_line = nil
      emit("buf_lines", {
        buf = buf,
        firstline = first,
        lastline = last,
        new_lines = lines,
      })
    end,
    on_reload = function()
      if session and eligible(buf) then
        snapshot(buf)
      end
    end,
    on_detach = function()
      attached[buf] = nil
      if session then
        session.seen[buf] = nil
      end
    end,
  })
  return attached[buf]
end

local function cursor()
  if not eligible(api.nvim_get_current_buf()) then
    return
  end
  local row, col = unpack(api.nvim_win_get_cursor(0))
  return { win = api.nvim_get_current_win(), buf = api.nvim_get_current_buf(), row = row, col = col }
end

local function layout()
  if not eligible(api.nvim_get_current_buf()) then
    return
  end
  local windows = {}
  for _, win in ipairs(api.nvim_tabpage_list_wins(0)) do
    local buf = api.nvim_win_get_buf(win)
    if eligible(buf) then
      local info = vim.fn.getwininfo(win)[1]
      windows[#windows + 1] = {
        win = win,
        buf = buf,
        row = info.winrow,
        col = info.wincol,
        width = info.width,
        height = info.height,
        topline = info.topline,
      }
    end
  end
  return { windows = windows }
end

local function flush(state)
  for _, kind in ipairs({ "mode_changed", "cursor_moved", "win_layout" }) do
    local fields = state.pending[kind]
    if type(fields) == "function" then
      fields = fields()
    end
    if fields then
      state.emit(kind, fields)
    end
  end
  state.pending = {}
end

local function queue(kind, fields)
  local state = session
  state.pending[kind] = fields
  if not state.timer:is_active() then
    state.timer:start(
      17,
      0,
      vim.schedule_wrap(function()
        if session == state then
          flush(state)
        end
      end)
    )
  end
end

function M.start(send)
  local state = {
    emit = send,
    seen = {},
    pending = {},
    timer = uv.new_timer(),
    group = api.nvim_create_augroup("ReplayCapture", { clear = true }),
  }
  session = state
  for _, buf in ipairs(api.nvim_list_bufs()) do
    attach(buf)
  end

  local function on_enter(buf)
    if attach(buf) then
      emit("buf_enter", { buf = buf, win = api.nvim_get_current_win() })
      queue("cursor_moved", cursor)
      queue("win_layout", layout)
    end
  end
  on_enter(api.nvim_get_current_buf())
  api.nvim_create_autocmd("BufEnter", {
    group = state.group,
    callback = function(args)
      on_enter(args.buf)
    end,
  })
  api.nvim_create_autocmd("BufReadPost", {
    group = state.group,
    callback = function(args)
      attach(args.buf)
    end,
  })
  api.nvim_create_autocmd("BufLeave", {
    group = state.group,
    callback = function(args)
      if state.seen[args.buf] then
        emit("buf_leave", { buf = args.buf, win = api.nvim_get_current_win() })
      end
    end,
  })
  api.nvim_create_autocmd({ "CursorMoved", "CursorMovedI" }, {
    group = state.group,
    callback = function(args)
      if state.seen[args.buf] then
        queue("cursor_moved", cursor)
      end
    end,
  })
  api.nvim_create_autocmd("ModeChanged", {
    group = state.group,
    callback = function(args)
      if state.seen[args.buf] then
        queue("mode_changed", { old_mode = vim.v.event.old_mode, new_mode = vim.v.event.new_mode })
      end
    end,
  })
  api.nvim_create_autocmd({ "WinScrolled", "WinEnter" }, {
    group = state.group,
    callback = function()
      if state.seen[api.nvim_get_current_buf()] then
        queue("win_layout", layout)
      end
    end,
  })
end

function M.stop(discard)
  if session then
    local state = session
    session = nil
    api.nvim_del_augroup_by_id(state.group)
    state.timer:close()
    if not discard then
      flush(state)
    end
  end
end

return M
