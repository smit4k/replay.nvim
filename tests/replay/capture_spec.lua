local capture = require("replay.capture")

describe("replay keystrokes", function()
  it("captures typed keys during recording and skips masked input", function()
    local keys = {}
    vim.keymap.set("n", "<F5>", "l")
    vim.keymap.set("n", "<M-Up>", "k")
    capture.start(function(kind, event)
      if kind == "key_press" then
        keys[#keys + 1] = event.key
      end
    end)

    vim.api.nvim_feedkeys(vim.keycode("<F5><C-L><D-Right><M-Up><Down><Left><Space>:<CR>"), "xt", false)
    vim.defer_fn(function()
      vim.api.nvim_input("hidden<CR>")
    end, 10)
    local masked = vim.fn.inputsecret("Test prompt: ")
    capture.stop()
    vim.keymap.del("n", "<F5>")
    vim.keymap.del("n", "<M-Up>")
    vim.api.nvim_feedkeys(vim.keycode("<F6>"), "xt", false)

    assert.equals("hidden", masked)
    local mac = vim.fn.has("mac") == 1
    assert.same({
      "F5",
      mac and "⌃L" or "Ctrl+L",
      mac and "⌘→" or "Super+→",
      mac and "⌥↑" or "Alt+↑",
      "↓",
      "←",
      "␣",
      ":",
      "Enter",
    }, keys)
  end)
end)
