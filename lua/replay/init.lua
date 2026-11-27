local ipc = require("replay.ipc")
local capture = require("replay.capture")
local scrubber = require("replay.scrubber")
local M = {}
local sidecar = "replay-sidecar"
local recording
local last_path
local open_path

local function report(err)
  local message = type(err) == "string" and err or vim.inspect(err)
  vim.notify(message, vim.log.levels.ERROR)
end

local function connect()
  local ok, err = ipc.start(sidecar, function(reason)
    capture.stop(true)
    recording = nil
    report(reason)
  end)
  if not ok then
    report(err)
  end
  return ok
end

local function request(method, args, callback)
  local ok, err = ipc.request(method, args, callback)
  if not ok then
    callback(err)
  end
  return ok
end

local function absolute(path)
  return vim.fn.fnamemodify(path, ":p")
end

function M.setup(opts)
  opts = opts or {}
  if opts.sidecar then
    sidecar = opts.sidecar
  end
end

function M.start(path)
  if recording then
    report("A replay recording is already active")
    return
  end
  if not connect() then
    return
  end
  path = absolute(path or "recording.nvimreplay")
  local current = { path = path }
  recording = current
  local queued = request("record_start", { path }, function(err)
    if err then
      if recording == current then
        capture.stop(true)
        recording = nil
      end
      report(err)
    end
  end)
  if queued then
    capture.start(function(kind, fields)
      fields.kind = kind
      ipc.notify("record_event", { fields })
    end)
  end
end

function M.stop()
  if not recording or recording.stopping then
    return
  end
  local current = recording
  current.stopping = true
  capture.stop()
  request("record_stop", {}, function(err)
    if err then
      report(err)
    else
      last_path = current.path
    end
    if recording == current then
      recording = nil
    end
  end)
end

function M.open(path)
  if not connect() then
    return
  end
  path = absolute(path)
  request("replay_open", { path }, function(err, info)
    if err then
      report(err)
      return
    end
    local ok, open_err = scrubber.open(info, function(position, callback)
      request("replay_seek", { position }, function(seek_err, frame)
        callback(seek_err and (type(seek_err) == "string" and seek_err or vim.inspect(seek_err)), frame)
      end)
    end)
    if ok then
      open_path = path
    else
      report(open_err)
    end
  end)
end

function M.render(path)
  local input = open_path or last_path
  if not input then
    report("Open or finish recording a replay before rendering")
    return
  end
  if connect() then
    request("replay_render", { input, absolute(path) }, function(err)
      if err then
        report(err)
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
