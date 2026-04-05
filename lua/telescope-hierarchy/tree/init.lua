local ts = require("telescope-hierarchy.treesitter")
local node = require("telescope-hierarchy.tree.node")
local state = require("telescope-hierarchy.state")
local parser = require("telescope-hierarchy.parser")

local Tree = {}

---@async
---@param mode Mode
---@param direction CallDirection | TypeDirection
---@param callback fun(root: Node)
---@param render_opts? table
---@param request_override? table
local function create_parser_tree(mode, direction, callback, render_opts, request_override)
    local request_opts = request_override
    local source_file = request_opts and vim.uri_to_fname(request_opts.uri) or vim.api.nvim_buf_get_name(0)

    if source_file == nil or source_file == "" or not parser.is_supported_file(source_file) then
        vim.notify(
            "telepy currently requires a supported Python file for parser-backed hierarchy queries",
            vim.log.levels.WARN
        )
        return
    end

    if request_opts == nil and mode:is_call() then
        local function_info = ts.current_function()
        if not function_info then
            vim.notify("No function found under the cursor for parser call hierarchy", vim.log.levels.WARN)
            return
        end
        request_opts = {
            uri = vim.uri_from_bufnr(0),
            symbol_name = function_info.name,
            line = function_info.lnum,
            col = function_info.col,
        }
    elseif request_opts == nil then
        local class_info = ts.current_class()
        if not class_info then
            vim.notify("No class found under the cursor for parser type hierarchy", vim.log.levels.WARN)
            return
        end
        request_opts = {
            uri = vim.uri_from_bufnr(0),
            symbol_name = class_info.name,
            line = class_info.lnum,
            col = class_info.col,
        }
    end

    parser.get_render_tree(mode, direction, request_opts, function(render_tree, err)
        if err then
            vim.notify("[hierarchy] " .. tostring(err), vim.log.levels.WARN)
            return
        end
        callback(node.from_parser_tree(render_tree, {
            request = request_opts,
            parser_needs_background_refresh = render_opts ~= nil,
        }))
    end, render_opts)
end

---@async
---@param mode Mode Either "Call" or "Type"
---@param direction CallDirection | TypeDirection The direction this tree is running in on startup. It can be changed later with a switch action.
---@param callback fun(root: Node) The code to be run once the tree is instantiated
---@param opts? { parser_render_opts: table | nil, request_override: table | nil }
function Tree.new(mode, direction, callback, opts)
    opts = opts or {}
    state.set("mode", mode)
    state.set("direction", direction)
    create_parser_tree(mode, direction, callback, opts.parser_render_opts, opts.request_override)
end

return Tree
