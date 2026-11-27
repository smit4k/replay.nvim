if vim.g.loaded_replay then
  return
end
vim.g.loaded_replay = true

local commands = {
  ReplayStart = { method = "start", nargs = "?" },
  ReplayStop = { method = "stop", nargs = 0 },
  ReplayRender = { method = "render", nargs = "+" },
}
for name, command in pairs(commands) do
  vim.api.nvim_create_user_command(name, function(args)
    require("replay")[command.method](unpack(args.fargs))
  end, {
    nargs = command.nargs,
    complete = command.nargs ~= 0 and "file" or nil,
  })
end
