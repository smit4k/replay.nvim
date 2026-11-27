if vim.g.loaded_replay then
  return
end
vim.g.loaded_replay = true

local commands = {
  ReplayStart = { method = "start", nargs = "?" },
  ReplayStop = { method = "stop", nargs = 0 },
  ReplayOpen = { method = "open", nargs = 1 },
  ReplayRender = { method = "render", nargs = 1 },
}
for name, command in pairs(commands) do
  vim.api.nvim_create_user_command(name, function(args)
    require("replay")[command.method](args.fargs[1])
  end, {
    nargs = command.nargs,
    complete = command.nargs ~= 0 and "file" or nil,
  })
end
