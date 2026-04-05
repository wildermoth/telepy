local actions = require("telescope.actions")
local actions_state = require("telescope.actions.state")
-- local transform_mod = require("telescope.actions.mt").transform_mod
local Path = require("plenary.path")

local ui = require("telescope-hierarchy.ui")
local state = require("telescope-hierarchy.state")

local M = {}

---@return string | nil
local function direction_state_key()
    local mode = state.mode()
    local direction = state.direction()
    if mode == nil or direction == nil then
        return nil
    end
    return mode.val .. ":" .. direction.val
end

---@param prompt_bufnr number
---@return boolean
local function has_active_filter(prompt_bufnr)
    return vim.trim(actions_state.get_current_line() or "") ~= ""
end

---@param node Node
---@return boolean
local function filter_branch_visible(node)
    return node.filter_expanded ~= false
end

M.expand = function(prompt_bufnr)
    local function f()
        local picker = actions_state.get_current_picker(prompt_bufnr)
        ---@type Node
        local node = actions_state.get_selected_entry().value
        local filtered = has_active_filter(prompt_bufnr)
        if filtered then
            node.filter_expanded = true
            if node.expanded then
                ui.refresh(node, picker)
                return
            end
        end

        node:expand(function(tree)
            ui.refresh(tree, picker)
        end)
    end
    return f
end

M.multi_expand = function(prompt_bufnr)
    local function f()
        local picker = actions_state.get_current_picker(prompt_bufnr)
        ---@type Node
        local node = actions_state.get_selected_entry().value
        if has_active_filter(prompt_bufnr) then
            node.filter_expanded = true
        end

        node:expand_all(function(tree)
            ui.refresh(tree, picker)
        end)
    end
    return f
end

M.collapse = function(prompt_bufnr)
    local function f()
        local picker = actions_state.get_current_picker(prompt_bufnr)
        ---@type Node
        local node = actions_state.get_selected_entry().value
        local filtered = has_active_filter(prompt_bufnr)
        if filtered then
            node.filter_expanded = false
            if not node.expanded then
                ui.refresh(node, picker)
                return
            end
        end

        node:collapse(function(tree)
            ui.refresh(tree, picker)
        end)
    end
    return f
end

M.toggle = function(prompt_bufnr)
    local function f()
        local picker = actions_state.get_current_picker(prompt_bufnr)
        ---@type Node
        local node = actions_state.get_selected_entry().value
        local filtered = has_active_filter(prompt_bufnr)
        if filtered then
            local next_visible = not filter_branch_visible(node)
            node.filter_expanded = next_visible
            if next_visible then
                if node.expanded then
                    ui.refresh(node, picker)
                    return
                end
                node:expand(function(tree)
                    ui.refresh(tree, picker)
                end)
                return
            end

            if not node.expanded then
                ui.refresh(node, picker)
                return
            end
            node:collapse(function(tree)
                ui.refresh(tree, picker)
            end)
            return
        end

        node:toggle(function(tree)
            ui.refresh(tree, picker)
        end)
    end
    return f
end

M.switch = function(prompt_bufnr)
    local function f()
        local picker = actions_state.get_current_picker(prompt_bufnr)
        ---@type Node
        local node = actions_state.get_selected_entry().value
        local current_key = direction_state_key()
        local current_expanded = node.root:expanded_state_map()
        picker._hierarchy_expand_state_by_direction = picker._hierarchy_expand_state_by_direction or {}
        if current_key ~= nil then
            picker._hierarchy_expand_state_by_direction[current_key] = current_expanded
        end

        local next_key = nil
        local direction = state.direction()
        local mode = state.mode()
        if direction ~= nil and mode ~= nil then
            next_key = mode.val .. ":" .. direction:switch().val
        end

        node:switch_direction(function(tree)
            local restore = nil
            if next_key ~= nil then
                restore = picker._hierarchy_expand_state_by_direction[next_key]
            end
            if restore == nil then
                restore = current_expanded
            end
            tree:apply_expanded_state_map(restore)
            if next_key ~= nil then
                picker._hierarchy_expand_state_by_direction[next_key] = tree:expanded_state_map()
            end
            picker.results_border:change_title(ui.title())
            ui.refresh(tree, picker, false)
            require("telescope-hierarchy").schedule_parser_background_refresh(
                tree,
                picker,
                picker._hierarchy_opts or {}
            )
        end)
    end
    return f
end

M.goto_definition = function(prompt_bufnr)
    local function f()
        -- Shamelessly stolen from Telescope
        -- I had to copy paste as I needed to very slightly modify the inner workings
        -- of this one rather long function, so I was unable to import and call Telescope code
        local entry = actions_state.get_selected_entry()
        ---@type Node
        local node = entry.value
        local loc = node.cache.location
        local filename = vim.uri_to_fname(loc.textDocument.uri)
        local row = loc.position.line + 1
        local col = loc.position.character

        local picker = actions_state.get_current_picker(prompt_bufnr)
        require("telescope.pickers").on_close_prompt(prompt_bufnr)
        pcall(function()
            vim.api.nvim_set_current_win(picker.original_win_id)
        end)
        local win_id = picker.get_selection_window(picker, entry)

        -- Schedule navigation after Telescope fully flushes its cleanup (including
        -- the <Esc> it feeds when closing a normal-mode picker), so that the residual
        -- escape doesn't break the first keysequence the user types afterward
        vim.schedule(function()
            if picker.push_cursor_on_edit then
                vim.cmd("normal! m'")
            end

            if picker.push_tagstack_on_edit then
                local from = { vim.fn.bufnr("%"), vim.fn.line("."), vim.fn.col("."), 0 }
                local items = { { tagname = vim.fn.expand("<cword>"), from = from } }
                vim.fn.settagstack(vim.fn.win_getid(), { items = items }, "t")
            end

            if win_id ~= 0 and vim.api.nvim_get_current_win() ~= win_id then
                vim.api.nvim_set_current_win(win_id)
            end

            -- check if we didn't pick a different buffer
            -- prevents needlessly reopening the same buffer
            if vim.api.nvim_buf_get_name(0) ~= filename then
                filename = Path:new(filename):normalize(vim.uv.cwd())
                pcall(function()
                    vim.cmd(string.format("edit %s", vim.fn.fnameescape(filename)))
                end)
            end

            -- HACK: fixes folding: https://github.com/nvim-telescope/telescope.nvim/issues/699
            if vim.wo.foldmethod == "expr" then
                vim.schedule(function()
                    vim.opt.foldmethod = "expr"
                end)
            end

            if vim.api.nvim_buf_get_name(0) == filename then
                vim.cmd([[normal! m']])
            end
            pcall(function()
                vim.api.nvim_win_set_cursor(0, { row, col })
            end)
        end)
    end
    return f
end

M.toggle_methods = function(prompt_bufnr)
    local function f()
        local picker = actions_state.get_current_picker(prompt_bufnr)
        local entry = actions_state.get_selected_entry()
        if entry == nil then
            return
        end
        state.set("show_methods", not state.ensure("show_methods", true))
        ui.refresh(entry.value.root, picker)
    end
    return f
end

M.toggle_fields = function(prompt_bufnr)
    local function f()
        local picker = actions_state.get_current_picker(prompt_bufnr)
        local entry = actions_state.get_selected_entry()
        if entry == nil then
            return
        end
        state.set("show_fields", not state.ensure("show_fields", false))
        ui.refresh(entry.value.root, picker)
    end
    return f
end

M.quit = function(prompt_bufnr)
    local function f()
        actions.close(prompt_bufnr)
    end
    return f
end

-- return transform_mod(M)
return M
