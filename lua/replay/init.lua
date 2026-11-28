local ipc = require("replay.ipc")
local capture = require("replay.capture")
local screen = require("replay.screen")
local M = {}
local executable = "replay-sidecar" .. (vim.fn.has("win32") == 1 and ".exe" or "")
local sidecar = vim.api.nvim_get_runtime_file("renderer/target/release/" .. executable, false)[1] or executable
local recording
local rendering
local last_path
local keystrokes = false
local render = {}

local function notify(message, opts, level)
  local ok, notifier = pcall(require, "notify")
  level = level or vim.log.levels.INFO
  if ok then
    opts = opts or {}
    opts.title = "Replay"
    return notifier(message, level, opts)
  end
  vim.notify(message, level, { title = "Replay" })
end

local function report(err, opts)
  local message = type(err) == "string" and err or vim.inspect(err)
  if opts then
    notify(message, opts, vim.log.levels.ERROR)
  else
    vim.notify(message, vim.log.levels.ERROR)
  end
end

local function connect()
  local ok, err = ipc.start(sidecar, function(reason)
    screen.stop()
    capture.stop(true)
    recording = nil
    report(reason)
  end)
  if not ok then
    report(err)
  end
  return ok
end

local function request(method, args, callback, on_progress)
  local ok, err = ipc.request(method, args, callback, on_progress)
  if not ok then
    callback(err)
  end
  return ok
end

local function absolute(path)
  return vim.fn.fnamemodify(path, ":p")
end

local function render_vimrc()
  local argv = vim.v.argv
  for i, arg in ipairs(argv) do
    if arg == "--" or arg == "-l" then
      break
    end
    if arg == "--clean" then
      return
    end
    if arg == "-u" then
      local path = argv[i + 1]
      if path == "NONE" then
        return
      end
      return path == "NORC" and path or absolute(path)
    end
  end
  return vim.env.MYVIMRC
end

function M.setup(opts)
  opts = opts or {}
  if opts.sidecar then
    sidecar = opts.sidecar
  end
  if opts.keystrokes ~= nil then
    keystrokes = opts.keystrokes == true
  end
  render = opts.render or render
end

function M.start(path)
  if recording then
    report("A replay recording is already active")
    return
  end
  if rendering then
    report("Wait for the replay export to finish before recording")
    return
  end
  if not connect() then
    return
  end
  path = absolute(path or "recording.nvimreplay")
  local current = { path = path }
  recording = current
  local started_at = vim.uv.hrtime()
  local metadata = { colorscheme = vim.g.colors_name, width = vim.o.columns, height = vim.o.lines }
  local queued = request("record_start", { path, metadata }, function(err)
    if err then
      if recording == current then
        screen.stop()
        capture.stop(true)
        recording = nil
      end
      report(err)
    else
      notify("Started recording replay to " .. path)
    end
  end)
  if queued then
    current.emit = function(kind, fields)
      fields.kind = kind
      -- Timestamp at capture so sidecar startup and IPC buffering preserve the timing.
      fields.timestamp_ns = vim.uv.hrtime() - started_at
      ipc.notify("record_event", { fields })
    end
    capture.start(current.emit, keystrokes)
    screen.start(current.emit)
  end
end

function M.stop()
  if not recording or recording.stopping then
    return
  end
  local current = recording
  current.stopping = true
  screen.stop()
  capture.stop()
  current.emit("recording_end", {})
  request("record_stop", {}, function(err, info)
    if recording == current then
      recording = nil
    end
    if err then
      report(err)
    else
      last_path = current.path
      notify(("Saved replay recording to %s (%.2f s)"):format(current.path, info.duration_ns / 1e9))
    end
  end)
end

function M.render(input_or_output, output, ...)
  if not input_or_output or select("#", ...) > 0 then
    report("Usage: :ReplayRender [recording.nvimreplay] <video.mp4 or video.mkv>")
    return
  end
  if recording or rendering then
    report("Finish the current recording or export before exporting a replay")
    return
  end
  local input = output and absolute(input_or_output) or last_path
  if not input then
    report("Specify a recording with :ReplayRender <recording.nvimreplay> <video.mp4 or video.mkv>")
    return
  end
  if connect() then
    output = absolute(output or input_or_output)
    rendering = true
    local message = "Rendering replay to " .. vim.fn.fnamemodify(output, ":t")
    local bar_width = 20
    local popup
    popup = notify(message .. "\n" .. string.rep("░", bar_width) .. "  Waiting for frames", {
      timeout = false,
      hide_from_history = true,
      on_close = function()
        popup = nil
      end,
    })
    local startup = {
      executable = vim.v.progpath,
      vimrc = render_vimrc(),
      cwd = vim.fn.getcwd(),
      keystrokes = keystrokes,
      font = render.font,
      height = render.height,
      scale = render.scale,
      progress = popup and true or nil,
    }
    local started_at = vim.uv.hrtime()
    request("replay_render", { input, output, startup }, function(err)
      rendering = false
      local opts = popup and { replace = popup, timeout = 5000, hide_from_history = false }
      if err then
        report(err, opts)
      else
        local elapsed = (vim.uv.hrtime() - started_at) / 1e9
        local stat = vim.uv.fs_stat(output)
        local size = stat and ("%d bytes"):format(stat.size) or "size unavailable"
        notify(("Saved replay video to %s (rendered in %.2f s, %s)"):format(output, elapsed, size), opts)
      end
    end, function(percent)
      if popup then
        local filled = math.floor(percent * bar_width / 100)
        local bar = string.rep("█", filled) .. string.rep("░", bar_width - filled)
        local status = percent == 100 and "100% · Finalizing video" or percent .. "%"
        popup = notify(message .. "\n" .. bar .. "  " .. status, { replace = popup })
      end
    end)
  end
end

vim.api.nvim_create_autocmd("VimLeavePre", {
  group = vim.api.nvim_create_augroup("ReplayIPC", { clear = true }),
  callback = function()
    M.stop()
    ipc.stop()
  end,
})

return M
