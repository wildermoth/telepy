local tree = require("telescope-hierarchy.tree")
local ui = require("telescope-hierarchy.ui")
local call = require("telescope-hierarchy.enums.call")
local type = require("telescope-hierarchy.enums.type")
local mode = require("telescope-hierarchy.enums.mode")
local state = require("telescope-hierarchy.state")
local ts = require("telescope-hierarchy.treesitter")

local M = {}

local warm_augroup = vim.api.nvim_create_augroup("TelescopeHierarchyParserWarm", { clear = false })

---@param opts table
---@return integer
local function configured_multi_depth(opts)
    return opts.multi_depth or 5
end

---@return string | nil
local function direction_state_key()
    local mode = state.mode()
    local direction = state.direction()
    if mode == nil or direction == nil then
        return nil
    end
    return mode.val .. ":" .. direction.val
end

---@param mode_val Mode
---@param direction_val CallDirection | TypeDirection
---@param opts table
---@return table | nil
local function parser_render_opts(mode_val, direction_val, opts)
    if mode_val:is_call() or direction_val:is_super() then
        return nil
    end

    local staged_depth = opts.subtype_initial_render_depth or 2
    local staged_member_depth = opts.subtype_initial_member_depth or 1
    return {
        max_depth = staged_depth,
        member_depth = staged_member_depth,
    }
end

---@param root Node
---@param opts table
---@param callback fun(root: Node)
local function show_prebuilt_tree(root, opts, callback)
    root.expanded = true
    if opts.initial_multi_expand then
        root:multi_expand(configured_multi_depth(opts), function(expanded_tree)
            callback(expanded_tree)
        end)
        return
    end
    callback(root)
end

---@param root Node
---@param picker Picker | nil
---@param opts table
function M.schedule_parser_background_refresh(root, picker, opts)
    if picker == nil then
        return
    end

    local generation = (picker._parser_background_refresh_generation or 0) + 1
    picker._parser_background_refresh_generation = generation

    if root == nil or root.parser_request == nil or root.parser_needs_background_refresh ~= true then
        return
    end

    local mode_val = state.mode()
    local direction_val = state.direction()
    if mode_val == nil or direction_val == nil or mode_val:is_call() or direction_val:is_super() then
        return
    end

    local parser = require("telescope-hierarchy.parser")
    local node = require("telescope-hierarchy.tree.node")
    local expanded_state = root:expanded_state_map()
    parser.get_render_tree(mode_val, direction_val, root.parser_request, function(render_tree, err)
        if picker._parser_background_refresh_generation ~= generation then
            return
        end

        if err then
            vim.notify("[hierarchy] " .. tostring(err), vim.log.levels.WARN)
            return
        end

        local prompt_bufnr = picker.prompt_bufnr
        if prompt_bufnr == nil or not vim.api.nvim_buf_is_valid(prompt_bufnr) then
            return
        end

        local full_root = node.from_parser_tree(render_tree, { request = root.parser_request })
        full_root:apply_expanded_state_map(expanded_state)
        picker._hierarchy_expand_state_by_direction = picker._hierarchy_expand_state_by_direction or {}
        local key = direction_state_key()
        if key ~= nil then
            picker._hierarchy_expand_state_by_direction[key] = full_root:expanded_state_map()
        end
        show_prebuilt_tree(full_root, opts, function(prepared_root)
            if prompt_bufnr ~= nil and vim.api.nvim_buf_is_valid(prompt_bufnr) then
                ui.refresh(prepared_root, picker)
            end
        end)
    end)
end

local function open(mode_val, direction_val, opts)
    opts = opts or {}
    state.set("session_opts", vim.deepcopy(opts))
    state.ensure("show_methods", true)
    state.ensure("show_fields", false)
    local tree_opts = {
        request_override = opts.request_override,
    }
    if not mode_val:is_call() then
        tree_opts.parser_render_opts = parser_render_opts(mode_val, direction_val, opts)
    end

    tree.new(mode_val, direction_val, function(root, pending)
        if pending then
            return
        end
        show_prebuilt_tree(root, opts, function(prepared_root)
            local picker = ui.show(prepared_root, opts)
            M.schedule_parser_background_refresh(prepared_root, picker, opts)
        end)
    end, tree_opts)
end

M.incoming_calls = function(opts)
    open(mode.CALL, call.INCOMING, opts)
end

M.outgoing_calls = function(opts)
    open(mode.CALL, call.OUTGOING, opts)
end

M.supertypes = function(opts)
    open(mode.TYPE, type.SUPER, opts)
end

M.subtypes = function(opts)
    open(mode.TYPE, type.SUB, opts)
end

M.hierarchy = function(opts)
    opts = opts or {}

    local filename = vim.api.nvim_buf_get_name(0)
    local parser = require("telescope-hierarchy.parser")
    if filename == "" or not parser.is_supported_file(filename) then
        vim.notify("telepy currently supports parser-backed hierarchy only for Python files", vim.log.levels.WARN)
        return
    end

    local symbol_info = ts.symbol_under_cursor_info()
    local symbol = symbol_info and symbol_info.name or nil
    local cursor = vim.api.nvim_win_get_cursor(0)
    local position = {
        textDocument = {
            uri = vim.uri_from_bufnr(0),
        },
        position = {
            line = cursor[1] - 1,
            character = cursor[2],
        },
    }

    local function cursor_is_on_named_target(target)
        if target == nil or symbol_info == nil then
            return false
        end
        if cursor[1] ~= target.lnum then
            return false
        end
        local target_end = target.col + vim.fn.strchars(target.name or "") - 1
        return symbol_info.start_col >= target.col and symbol_info.end_col <= target_end
    end

    local function symbol_looks_like_call()
        if symbol_info == nil then
            return false
        end
        local remainder = symbol_info.line:sub(symbol_info.end_col + 1)
        local next_char = remainder:match("^%s*(.)")
        return next_char == "("
    end

    local function fallback_open()
        if ts.enclosing_function() ~= nil then
            M.outgoing_calls(opts)
            return
        end

        if ts.enclosing_class() ~= nil then
            M.supertypes(opts)
            return
        end

        if ts.current_function() ~= nil then
            M.outgoing_calls(opts)
            return
        end

        M.supertypes(opts)
    end

    local enclosing_class = ts.enclosing_class()
    if cursor_is_on_named_target(enclosing_class) then
        M.supertypes(opts)
        return
    end

    local enclosing_function = ts.enclosing_function()
    if cursor_is_on_named_target(enclosing_function) then
        M.outgoing_calls(opts)
        return
    end

    local function try_field_target()
        if symbol == nil or symbol == "" then
            fallback_open()
            return
        end

        parser.resolve_class_fields(vim.uri_from_bufnr(0), symbol, function(_, resolved_uri, resolved_name, err)
            if not err and resolved_uri ~= nil and resolved_name ~= nil and resolved_name ~= "" then
                local direct_opts = vim.deepcopy(opts)
                direct_opts.request_override = {
                    uri = resolved_uri,
                    symbol_name = resolved_name,
                }
                M.supertypes(direct_opts)
                return
            end

            fallback_open()
        end)
    end

    if symbol_looks_like_call() then
        parser.resolve_callable_reference(position, function(reference, call_err)
            if not call_err and reference ~= nil then
                local direct_opts = vim.deepcopy(opts)
                direct_opts.request_override = {
                    uri = reference.uri,
                    symbol_name = reference.symbol_name,
                    line = reference.line,
                    col = reference.col,
                }
                M.incoming_calls(direct_opts)
                return
            end

            try_field_target()
        end)
        return
    end

    try_field_target()
end

function M.prewarm_file(filename)
    local parser = require("telescope-hierarchy.parser")
    return parser.prewarm_file(filename)
end

function M.prewarm_current_buffer()
    local filename = vim.api.nvim_buf_get_name(0)
    if filename == nil or filename == "" then
        return false, "missing filename"
    end
    return M.prewarm_file(filename)
end

function M.configure(opts)
    opts = opts or {}

    vim.api.nvim_clear_autocmds({ group = warm_augroup })

    if opts.warm_parser_on_bufenter then
        vim.api.nvim_create_autocmd({ "BufEnter", "BufWinEnter" }, {
            group = warm_augroup,
            callback = function(args)
                local filename = vim.api.nvim_buf_get_name(args.buf)
                if filename == nil or filename == "" then
                    return
                end
                vim.schedule(function()
                    M.prewarm_file(filename)
                end)
            end,
        })

        vim.api.nvim_create_autocmd("BufWritePost", {
            group = warm_augroup,
            callback = function(args)
                local filename = vim.api.nvim_buf_get_name(args.buf)
                if filename == nil or filename == "" then
                    return
                end
                vim.schedule(function()
                    require("telescope-hierarchy.parser").refresh_file(filename)
                end)
            end,
        })

        vim.schedule(function()
            M.prewarm_current_buffer()
        end)
    end
end

return M
