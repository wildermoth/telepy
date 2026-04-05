# telepy

`telepy` is a Telescope extension for Python call/type hierarchy navigation that uses a fast custom parser, avoiding lsp reliance

largely based off https://github.com/jmacadie/telescope-hierarchy.nvim 

## Usage

`:Telescope hierarchy` 


| Key | Action |
| --- | --- |
| `→` | Expand the current node: this will recursively find all incoming calls of the current node. It will only go the next level deep though |
| `←` | Collapse the current node: the child calls are still found, just hidden in the finder window |
| `E` | Expand all reachable nodes in the current tree |
| `i` | Enter insert mode on search bar to begin filtering nodes |
| `s` | Switch the direction of the Call hierarchy and toggle between incoming/outgoing or subtype/supertype |
| `m` | Toggle type-hierarchy method rows on and off |
| `f` | Toggle type-hierarchy field rows on and off |
| `CR` | Navigate to the target node |
| `q` or `ESC` | Quit the Telescope finder |


The `m` and `f` filter toggles are sticky for the rest of the current Neovim session, so reopening the hierarchy picker restores the last methods/fields visibility state.

When `initial_multi_expand` or `E` walks the call hierarchy tree, nodes under `.venv`, `site-packages`, and stdlib paths stay collapsed by default. They still appear in the hierarchy, but auto-expansion stops at that boundary unless you manually open the external node first.

## Install

Using Lazy, with a separate module for this extension's config:

```lua ...\lua\plugins\telepy.lua
return {
  "wiltdermoth/telepy",
  -- Optional: prebuild the Rust parser during plugin install/update instead of on first use.
  build = "make build-parser",
  ft = { "python" },
  dependencies = {
    {
      "nvim-telescope/telescope.nvim",
      dependencies = { "nvim-lua/plenary.nvim" },
    },
  },
  keys = {
    { 
      "<leader>ht",
      "<cmd>Telescope hierarchy<cr>",
      desc = "Heirarchy Tree",
    }
  },
  opts = {
    extensions = {
      hierarchy = {
          --- see below
      },
    },
  },
  config = function(_, opts)
    require("telescope").setup(opts)
    require("telescope").load_extension("hierarchy")
  end,
}
```

If you do not set a build step, the parser backend will fall back to `cargo run --release ...` on first use. That still works. The first open will pay for the build, and telepy now shows an info notification while that compile is happening.

The `ft = { "python" }` trigger is required for `warm_parser_on_bufenter` to work. A key-only lazy spec won't load the plugin until you press the key, so the `BufEnter` autocmd is never registered.

## Config

```lua
  opts = {
    extensions = {
      hierarchy = {
        -- hierarchy extension config
        initial_multi_expand = false, -- On open: call hierarchy visibly multi-expands; type hierarchy preloads deeper levels but keeps them collapsed
        multi_depth = 5, -- How many layers deep should a multi-expand go?
        collapse_external = true, -- Stop auto-expansion at .venv / library / stdlib nodes
        warm_parser_on_bufenter = false, -- Start the Python parser in the background when entering a supported buffer
        subtype_initial_render_depth = 2, -- How many subtype levels to request on first open (deeper levels are fetched in the background)
        subtype_initial_member_depth = 1, -- How many levels of class members to include in the initial subtype render
        layout_strategy = "horizontal",
      },
    },
  },
```

