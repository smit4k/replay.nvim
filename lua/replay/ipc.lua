local uv = vim.uv
local M = {}
local child

local function close(handle)
  if not handle:is_closing() then
    handle:close()
  end
end

local function disconnect(state, reason)
  if child ~= state then
    return
  end

  child = nil
  close(state.input)
  close(state.output)
  close(state.stderr)
  for _, callback in pairs(state.pending) do
    vim.schedule(function()
      callback(reason)
    end)
  end
  if not state.stopping then
    vim.schedule(function()
      state.on_exit(reason)
    end)
  end
end

function M.start(command, on_exit)
  if child then
    return true
  end

  local state = {
    input = uv.new_pipe(false),
    output = uv.new_pipe(false),
    stderr = uv.new_pipe(false),
    pending = {},
    next_id = 0,
    on_exit = on_exit,
  }

  local function ended()
    if state.exit and state.eof then
      local reason = ("Replay sidecar exited (%d, signal %d)"):format(state.exit[1], state.exit[2])
      if state.diagnostic then
        reason = reason .. ": " .. state.diagnostic
      end
      disconnect(state, reason)
    end
  end

  local process, err = uv.spawn(command, {
    stdio = { state.input, state.output, state.stderr },
  }, function(code, signal)
    close(state.process)
    state.exit = { code, signal }
    ended()
  end)
  if not process then
    close(state.input)
    close(state.output)
    close(state.stderr)
    return nil, ("Could not start %s: %s"):format(command, err)
  end

  state.process = process
  child = state
  local unpack = vim.mpack.Unpacker()

  state.stderr:read_start(function(_, data)
    if data then
      state.diagnostic = data:sub(-512):gsub("%s+$", "")
    else
      close(state.stderr)
    end
  end)
  state.output:read_start(function(read_err, data)
    if read_err then
      process:kill("sigterm")
      disconnect(state, read_err)
      return
    end
    if not data then
      state.eof = true
      close(state.output)
      close(state.input)
      ended()
      return
    end

    local offset = 1
    while offset <= #data do
      local ok, message, next_offset = pcall(unpack, data, offset)
      if not ok or (message ~= nil and (type(message) ~= "table" or message[1] ~= 1 or #message ~= 4)) then
        process:kill("sigterm")
        disconnect(state, "Invalid Msgpack-RPC response from replay sidecar")
        return
      end
      offset = next_offset
      if message then
        local id, response_err, result = message[2], message[3], message[4]
        local callback = state.pending[id]
        state.pending[id] = nil
        if callback then
          if response_err == vim.NIL then
            response_err = nil
          end
          vim.schedule(function()
            callback(response_err, result)
          end)
        end
      end
    end
  end)
  return true
end

local function write(state, message)
  if not state or state.stopping or state.input:is_closing() then
    return nil, "Replay sidecar is disconnected"
  end
  return state.input:write(vim.mpack.encode(message), function(err)
    if err and not state.stopping then
      state.process:kill("sigterm")
      disconnect(state, err)
    end
  end)
end

function M.notify(method, params)
  return write(child, { 2, method, params })
end

function M.request(method, params, callback)
  local state = child
  if not state then
    return nil, "Replay sidecar is disconnected"
  end
  local id = state.next_id
  state.next_id = id + 1
  local request, err = write(state, { 0, id, method, params })
  if not request then
    return nil, err
  end
  state.pending[id] = callback
  return request
end

function M.stop()
  local state = child
  if not state then
    return
  end
  state.stopping = true

  -- Send EOF after queued writes so the sidecar can finalize the recording.
  if not state.input:is_closing() then
    local shutdown = state.input:shutdown(function()
      close(state.input)
    end)
    if not shutdown then
      close(state.input)
    end
  end
  if not vim.wait(1000, function()
    return state.exit and state.eof
  end, 10) then
    if not state.exit then
      state.process:kill("sigkill")
      vim.wait(200, function()
        return state.exit ~= nil
      end, 10)
    end
    disconnect(state, "Replay sidecar was stopped")
  end
end

return M
