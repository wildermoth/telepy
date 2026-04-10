local pickers = require("telescope.pickers")
local finders = require("telescope.finders")
local conf = require("telescope.config").values
local sorters = require("telescope.sorters")
local strings = require("plenary.strings")

local theme = require("telescope-hierarchy.theme")
local state = require("telescope-hierarchy.state")
local Path = require("plenary.path")

local M = {}

---@return string
local function overridden_suffix()
    local mode = state.mode()
    local direction = state.direction()
    if mode ~= nil and not mode:is_call() and direction ~= nil and not direction:is_super() then
        return "↓ "
    end
    return "↑ "
end

---@param picker Picker
---@param row integer
local function force_selection(picker, row)
    picker:reset_selection()
    picker:set_selection(row)
end

---@param picker Picker
local function center_results_view(picker)
    if picker == nil or picker.results_win == nil or not vim.api.nvim_win_is_valid(picker.results_win) then
        return
    end

    local wininfo = vim.fn.getwininfo(picker.results_win)[1]
    if wininfo == nil then
        return
    end

    local cursor_line = (vim.api.nvim_win_get_cursor(picker.results_win)[1] or 1)
    local height = vim.api.nvim_win_get_height(picker.results_win)
    if height <= 0 then
        return
    end

    local total_lines = picker.manager and picker.manager:num_results() or 0
    if total_lines <= 0 then
        return
    end

    local anchor = math.max(1, math.floor((height + 1) / 2))
    local max_topline = math.max(1, total_lines - height + 1)
    local desired_topline = math.max(1, math.min(cursor_line - anchor + 1, max_topline))
    if wininfo.topline == desired_topline then
        return
    end

    vim.api.nvim_win_call(picker.results_win, function()
        vim.fn.winrestview({ topline = desired_topline })
    end)
end

---@param picker Picker
local function request_center_results_view(picker)
    if picker == nil or picker._hierarchy_center_results_pending == true then
        return
    end

    picker._hierarchy_center_results_pending = true
    vim.schedule(function()
        if picker == nil then
            return
        end
        picker._hierarchy_center_results_pending = false
        center_results_view(picker)
    end)
end

---@param picker Picker
local function ensure_centered_selection_behavior(picker)
    if picker == nil or picker._hierarchy_center_selection_wrapped == true then
        return
    end

    local original_set_selection = picker.set_selection
    picker.set_selection = function(self, row)
        original_set_selection(self, row)
        request_center_results_view(self)
    end
    picker._hierarchy_center_selection_wrapped = true
end

---@param node Node | nil
---@return string | nil
local function node_selection_key(node)
    if node == nil then
        return nil
    end

    local location = node.cache and node.cache.location or nil
    local uri = location and location.textDocument and location.textDocument.uri or ""
    local line = location and location.position and location.position.line or -1
    local col = location and location.position and location.position.character or -1

    return table.concat({
        node.node_kind or "",
        node.text or "",
        node.filename or "",
        tostring(node.lnum or -1),
        tostring(node.col or -1),
        uri,
        tostring(line),
        tostring(col),
    }, "|")
end

---@param picker Picker
---@return string
local function picker_prompt(picker)
    local ok, prompt = pcall(function()
        return picker:_get_prompt()
    end)
    if ok and type(prompt) == "string" then
        return prompt
    end

    local prompt_bufnr = picker.prompt_bufnr
    if prompt_bufnr ~= nil and vim.api.nvim_buf_is_valid(prompt_bufnr) then
        local lines = vim.api.nvim_buf_get_lines(prompt_bufnr, 0, 1, false)
        return lines[1] or ""
    end

    return ""
end

---@param root Node
---@param prompt string
---@param selected_node Node | nil
---@return integer | nil
local function preferred_selection_index(root, prompt, selected_node)
    local visible = root:to_list(false, prompt)
    if #visible == 0 then
        return nil
    end

    if selected_node == nil then
        return 1
    end

    local visible_by_key = {}
    for index, entry in ipairs(visible) do
        if entry.node == selected_node then
            return index
        end

        local key = node_selection_key(entry.node)
        if key ~= nil and visible_by_key[key] == nil then
            visible_by_key[key] = index
        end
    end

    local candidate = selected_node
    while candidate ~= nil do
        local key = node_selection_key(candidate)
        if key ~= nil and visible_by_key[key] ~= nil then
            return visible_by_key[key]
        end
        candidate = candidate.parent
    end

    return 1
end

---A higher-ordered function, a function that returns a function
---This follows the pattern set out in "Telescope.make_entry" in that we contain all the
---logic for rendering a single row into a function.
---The higher-ordered function pattern is useful to 'cache' computation that applies to
---all rows and only needs to be done once per render cycle.
---Looking through the code, I'm not actually sure that this is so applicable to the code
---I have written: oops! It works better with "telescope.pickers.entry_display.create()"
---which sets up the fixed info for the layout of columns and their highlighting once at
---the start of the render cycle. We can't take advantage of that here as the variable
---size of the tree we need to render at the start of the row means we do not want to use
---a fixed column layout
---@param opts table
---@return fun(entry: NodeLevel) table
local function gen_make_entry(opts)
    opts = opts or {}

    local disable_devicons = opts.disable_devicons

    ---Create the tree string for jut one entry (row) in the list
    ---@param tree_state TreeStateLevel[] Series of flags used to render the tree prefix for each level
    ---@return string
    local function make_tree(tree_state)
        local tree = ""
        for idx, level in ipairs(tree_state) do
            local level_last = level.last
            local fold_state = level.fold_state
            if idx == #tree_state then
                local branch = level_last and " ╰─" or " ┆─"
                if fold_state == "collapsed" then
                    tree = tree .. branch .. " "
                elseif fold_state == "expanded" then
                    tree = tree .. branch .. " "
                else
                    tree = tree .. branch
                end
            else
                if level_last then
                    tree = tree .. (fold_state and "      " or "   ")
                else
                    tree = tree .. (fold_state and " ┆    " or " ┆ ")
                end
            end
        end
        return tree
    end

    ---Create the child count suffix
    ---@param node Node
    ---@return string
    local function make_suffix(node)
        if node.node_kind == "method" or node.node_kind == "field" then
            local override_suffix = ""
            if node.is_overridden then
                override_suffix = override_suffix .. overridden_suffix()
            end
            if node.is_override then
                override_suffix = override_suffix .. "󱞧 "
            end
            return override_suffix
        end
        if node.node_kind == "field_object" then
            local type_name = type(node.field_type_name) == "string" and node.field_type_name or "?"
            local override_suffix = ""
            if node.is_overridden then
                override_suffix = override_suffix .. overridden_suffix()
            end
            if node.is_override then
                override_suffix = override_suffix .. "󱞧 "
            end
            return "(" .. type_name .. ") " .. override_suffix
        end
        if node.cache.searched == "No" then
            return "? "
        end

        if node.cache.searched == "Pending" then
            return " "
        end

        if node.recursive then
            return "  "
        end

        assert(node.cache.searched == "Yes")
        local ref = assert(node.cache.searched_node)

        if node.node_kind == "class" then
            local method_count = 0
            local field_count = 0
            for _, child in ipairs(ref.children or {}) do
                if child.node_kind == "method" then
                    method_count = method_count + 1
                elseif child.node_kind == "field" or child.node_kind == "field_object" then
                    field_count = field_count + 1
                end
            end

            if method_count == 0 and field_count == 0 and node.parser_truncated then
                return "(?|?) "
            end

            return "(" .. method_count .. "|" .. field_count .. ") "
        end

        if node.node_kind == "call" then
            if node.parser_truncated then
                return "? "
            end
            return ""
        end

        local count = #ref.children
        if count == 0 then
            if node.parser_truncated then
                return "? "
            end
            return "(none) "
        end
        return "(" .. count .. ") "
    end

    ---@alias HighlightEntry [[integer, integer], string]

    ---@param results string[] A table holding the parts of the ultimate display string
    ---@param highlights HighlightEntry[] The highlights table that is being appended to
    ---@param start integer The current position in the display string
    ---@param text string|integer The text to be added to the display result & the highlight is being applied to
    ---@param hl string The highlight to be applied
    ---@return integer new_pos The new position in the display string
    local function add_part(results, highlights, start, text, hl)
        text = tostring(text) -- convert numbers to strings
        table.insert(results, text)
        local len = text:len()
        local new_pos = start + len
        ---@type HighlightEntry
        local highlight = { { start, new_pos }, hl }
        table.insert(highlights, highlight)
        return new_pos
    end

    ---Calculate the available width of the results window
    ---@param picker Picker
    ---@return integer
    local function results_width(picker)
        -- LuaLS does not like the call to selection_caret, which is in the metatable
        ---@diagnostic disable-next-line:undefined-field
        return vim.api.nvim_win_get_width(picker.results_win) - #picker.selection_caret
    end

    ---Compute a filemame that is padded and trimmed such that it is rendered right-justified
    ---in the results window. The trimming will occur if the filename (which includes the full path)
    ---would overflow the available space in the results window. If that is the case, we left trim
    ---on the basis that the right hand end of the filepath is the most interesting to users
    ---@param width integer The avialable width of the results window
    ---@param results string[] The text of the LHS of the result for this row, which will take precedence over any filename that is shown
    ---@param filename string The filename and path that is being trimmed and justified
    ---@return string
    local function padded_filename(width, results, filename)
        local prefix_len = 0
        for _, str in ipairs(results) do
            prefix_len = prefix_len + strings.strdisplaywidth(str)
        end

        local suffix_len = 0
        local available = width - prefix_len - suffix_len

        local truncated = strings.truncate(filename, available, "…", -1)
        return strings.align_str(truncated, available, true)
    end

    ---@class Entry
    ---@field value Node
    ---@field tree_state boolean[]
    ---@field ordinal string
    ---@field filename string
    ---@field lnum integer
    ---@field col integer

    ---Main UI rendering function that is used by the picker to render each row in the finder window
    ---It is the equivalant of the functions in "telescope.make_entry". I had to roll my own as the
    ---Telescope built in functions are focussed on displaying things in columns but the varying
    ---length of the tree rendered on the left hand side of the row means that this is not a good
    ---pattern for this add-in
    ---@param entry Entry
    ---@param picker Picker
    ---@return string final_str The text to be show in the results window for the row
    ---@return HighlightEntry[] highlights A table of highlights
    local make_display = function(entry, picker)
        local node = entry.value
        local width = results_width(picker)

        local results = {}
        local highlights = {}
        local position = 0
        local separator = " "
        local detail = (node.field_detail or ""):lower()

        position = add_part(results, highlights, position, make_tree(entry.tree_state), "TelescopeResultsMethod")
        position = add_part(results, highlights, position, separator, "")
        if not disable_devicons then
            local icon
            local mode = state.mode()
            if mode ~= nil and mode:is_call() then
                icon = "󰊕"
            elseif node.node_kind == "method" then
                icon = node.is_override and "󰡱" or "󰊕"
            elseif node.node_kind == "field_object" then
                icon = "󰅩"
            elseif node.node_kind == "field" then
                if detail:match("^bool") then
                    icon = "󰨙"
                elseif detail:match("^int") then
                    icon = "󰎠"
                elseif detail:match("^float") then
                    icon = "󱜩"
                elseif detail:match("^str") or detail == "" then
                    icon = "󰊄"
                elseif detail:match("^list") or detail:match("%[%]") then
                    icon = "󰅪"
                else
                    icon = "󰓹"
                end
            else
                icon = ""
            end
            position = add_part(results, highlights, position, icon, "TelescopeResultsFunction")
            position = add_part(results, highlights, position, separator, "")
        end
        position = add_part(results, highlights, position, node.text, "TelescopeResultsFunction")
        position = add_part(results, highlights, position, separator, "")
        position = add_part(results, highlights, position, make_suffix(node), "TelescopeResultsComment")
        position = add_part(results, highlights, position, "     ", "")

        local fname = entry.filename
        local display_fname = fname:match(".*/site%-packages/(.+)$")
            or fname:match(".*/stdlib/(.+)$")
            or Path:new(fname):normalize(vim.uv.cwd())
        local fname_no_ext = display_fname:gsub("%.[^./]+$", "")
        local fname_capped = strings.truncate(fname_no_ext, 30, "…", -1)
        local formatted_fname = padded_filename(width, results, fname_capped)
        _ = add_part(results, highlights, position, formatted_fname, "TelescopeResultsLineNr")

        local final_str = table.concat(results, "")
        return final_str, highlights
    end

    ---@param entry NodeLevel
    ---@return table
    local function output(entry)
        local node = entry.node

        return {
            value = node,
            tree_state = entry.tree_state,
            display = make_display,
            ordinal = node.text,
            filename = node.filename,
            lnum = node.lnum,
            col = node.col,
        }
    end

    return output
end

---@param root Node
---@param opts table
---@return Finder
local function make_finder(root, opts)
    return finders.new_dynamic({
        fn = function(prompt)
            return root:to_list(false, prompt)
        end,
        entry_maker = gen_make_entry(opts),
    })
end

---Convert the Tree direction into a display title for the Results window
---@return string
M.title = function()
    local mode = assert(state.mode())
    local direction = assert(state.direction())
    if mode:is_call() then
        return direction:is_incoming() and "Incoming Calls" or "Outgoing Calls"
    else
        return direction:is_super() and "Supertypes" or "Subtypes"
    end
end

---Show the Telescope UI based on the current tree.
---The tree is processed in `Node:to_list()` to convert the nested tree structure
---into a list format that Telescope can consume
---@param root Node
---@param opts table
---@return Picker | nil
M.show = function(root, opts)
    if root == nil then
        return
    end

    opts = theme.apply(opts or {})
    local picker_opts = vim.deepcopy(opts)
    local layout_strategy = picker_opts.layout_strategy

    local picker = pickers.new(picker_opts, {
        results_title = M.title(),
        prompt_title = "Filter",
        prompt_prefix = "  ",
        preview_title = "Preview",
        default_selection_index = 1,
        selection_strategy = "reset",
        cache_picker = false,
        finder = make_finder(root, opts),
        sorter = sorters.empty(),
        previewer = conf.qflist_previewer(opts),
        attach_mappings = function(prompt_bufnr, map)
            for _, mode in pairs({ "i", "n" }) do
                for key, action in pairs(opts.mappings[mode] or {}) do
                    map(mode, key, action(prompt_bufnr), { nowait = true })
                end
            end
            return true -- include defaults as well
        end,
    })

    picker._hierarchy_opts = vim.deepcopy(opts)
    picker.layout_strategy = layout_strategy or picker.layout_strategy
    ensure_centered_selection_behavior(picker)
    picker:register_completion_callback(function(self)
        request_center_results_view(self)
    end)
    local callbacks = { unpack(picker._completion_callbacks or {}) }
    picker:register_completion_callback(function(self)
        if self.manager and self.manager:num_results() > 0 then
            force_selection(self, self:get_row(1))
        end
        self._completion_callbacks = callbacks
    end)
    picker:find()
    return picker
end

---Refresh the picker, for use after the nodes tree has been updated
---@param node Node
---@param picker Picker
---@param keep_selection? boolean Retain the current selection after refresh. If ommitted will assume true
M.refresh = function(node, picker, keep_selection)
    local root = node.root
    local new_finder = make_finder(node.root, picker._hierarchy_opts or {})

    if keep_selection or keep_selection == nil then
        local selection_node = picker._selection_entry and picker._selection_entry.value or nil
        local callbacks = { unpack(picker._completion_callbacks or {}) } -- shallow copy
        picker:register_completion_callback(function(self)
            local results_count = self.manager and self.manager:num_results() or 0
            if results_count > 0 then
                local prompt = picker_prompt(self)
                local index = preferred_selection_index(root, prompt, selection_node) or 1
                force_selection(self, self:get_row(math.min(index, results_count)))
            end
            self._completion_callbacks = callbacks
        end)
    end
    picker:refresh(new_finder, {})
end

return M
