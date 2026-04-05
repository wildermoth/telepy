local actions = require("telescope-hierarchy.actions")

local M = {}

M.opts = {
  initial_multi_expand = false,
  multi_depth = 5,
  collapse_external = true,
  warm_parser_on_bufenter = false,
  subtype_initial_render_depth = 2,
  subtype_initial_member_depth = 1,
  mappings = {
    i = {},
    n = {
      ["e"] = actions.expand,
      ["E"] = actions.multi_expand,
      ["l"] = actions.expand,
      ["<RIGHT>"] = actions.expand,

      ["c"] = actions.collapse,
      ["h"] = actions.collapse,
      ["<LEFT>"] = actions.collapse,

      ["t"] = actions.toggle,
      ["s"] = actions.switch,
      ["d"] = actions.goto_definition,
      ["m"] = actions.toggle_methods,
      ["f"] = actions.toggle_fields,

      ["q"] = actions.quit,
    },
  },
  layout_strategy = "horizontal",
}

return M
