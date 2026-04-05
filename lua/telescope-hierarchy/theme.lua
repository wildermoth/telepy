local M = {}

--- The non-negotiable bits of config that will always be applied
M.apply = function(opts)
  opts = opts or {}

  local theme_opts = {
    theme = "hierarchy",

    sorting_strategy = "ascending",
    initial_mode = "normal",
  }

  return vim.tbl_deep_extend("force", theme_opts, opts)
end

return M
