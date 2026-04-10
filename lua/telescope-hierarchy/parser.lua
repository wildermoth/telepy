local state = require("telescope-hierarchy.state")

local M = {}
local REQUEST_TIMEOUT_MS = 30000
local BUILD_STARTUP_TIMEOUT_MS = 300000
local TRANSPORT_GENERATION = 2
local build_notifications = {}

local function json_decode(value)
    if vim.json and vim.json.decode then
        return vim.json.decode(value)
    end
    return vim.fn.json_decode(value)
end

local function json_encode(value)
    if vim.json and vim.json.encode then
        return vim.json.encode(value)
    end
    return vim.fn.json_encode(value)
end

local function plugin_root()
    local source = debug.getinfo(1, "S").source:sub(2)
    return vim.fn.fnamemodify(source, ":p:h:h:h")
end

---@param a vim.uv.fs_stat_result | nil
---@param b vim.uv.fs_stat_result | nil
---@return boolean
local function stat_is_newer(a, b)
    if a == nil or b == nil or a.mtime == nil or b.mtime == nil then
        return false
    end
    if a.mtime.sec ~= b.mtime.sec then
        return a.mtime.sec > b.mtime.sec
    end
    return (a.mtime.nsec or 0) > (b.mtime.nsec or 0)
end

---@param binary_path string
---@param repo_root string
---@return boolean
local function parser_binary_is_stale(binary_path, repo_root)
    local binary_stat = vim.uv.fs_stat(binary_path)
    if binary_stat == nil then
        return true
    end

    local dependency_paths = {
        repo_root .. "/parser/Cargo.toml",
        repo_root .. "/parser/src/lib.rs",
        repo_root .. "/parser/src/main.rs",
    }

    for _, path in ipairs(dependency_paths) do
        if stat_is_newer(vim.uv.fs_stat(path), binary_stat) then
            return true
        end
    end

    return false
end

local function parser_command(root_dir)
    local repo_root = plugin_root()
    local preferred_binary = repo_root .. "/parser/target/release/telepy-parser"
    local needs_build = parser_binary_is_stale(preferred_binary, repo_root)
    if vim.fn.executable(preferred_binary) == 1 and not needs_build then
        return {
            preferred_binary,
            "serve",
            "--root",
            root_dir,
        },
            repo_root,
            {
                uses_cargo = false,
                needs_build = false,
                binary_path = preferred_binary,
            }
    end

    if vim.fn.executable("cargo") == 1 then
        return {
            "cargo",
            "run",
            "--release",
            "--manifest-path",
            repo_root .. "/parser/Cargo.toml",
            "--",
            "serve",
            "--root",
            root_dir,
        },
            repo_root,
            {
                uses_cargo = true,
                needs_build = needs_build,
                binary_path = preferred_binary,
            }
    end

    return nil, repo_root
end

---@param command_info? { uses_cargo: boolean, needs_build: boolean, binary_path: string }
local function notify_build_start(command_info)
    if command_info == nil or not command_info.uses_cargo or not command_info.needs_build then
        return
    end

    local key = command_info.binary_path
    if build_notifications[key] then
        return
    end
    build_notifications[key] = true

    vim.schedule(function()
        vim.notify(
            "Building the telepy parser with cargo. The first hierarchy open may take a bit.",
            vim.log.levels.INFO,
            { title = "telepy" }
        )
    end)
end

local function fallback_root(filename)
    if vim.fs and vim.fs.root then
        return vim.fs.root(filename, {
            "pyproject.toml",
            "uv.lock",
            "poetry.lock",
            "requirements.txt",
            "setup.py",
            "setup.cfg",
            ".git",
        })
    end
end

local function project_root(filename)
    local rooted = fallback_root(filename)
    if rooted and rooted ~= "" then
        return rooted
    end
    return vim.fn.getcwd()
end

local function get_servers()
    return state.ensure("parser_servers", {})
end

local function fail_server(server, message)
    local pending = server.pending
    server.pending = {}
    server.queue = {}
    server.ready = false
    for _, request in pairs(pending) do
        request.callback(nil, message)
    end
end

local function flush_queue(server)
    if not server.ready or #server.queue == 0 then
        return
    end
    for _, line in ipairs(server.queue) do
        vim.fn.chansend(server.job_id, line .. "\n")
    end
    server.queue = {}
end

local function handle_line(server, line)
    if line == "" then
        return
    end

    local ok, decoded = pcall(json_decode, line)
    if not ok or type(decoded) ~= "table" then
        return
    end

    if decoded.ready or decoded.event == "ready" then
        server.ready = true
        flush_queue(server)
        return
    end

    local request_id = tonumber(decoded.id)
    if request_id == nil then
        return
    end

    local pending = server.pending[request_id]
    if not pending then
        return
    end
    server.pending[request_id] = nil

    if decoded.error then
        pending.callback(nil, decoded.error)
        return
    end

    pending.callback(decoded.result ~= nil and decoded.result or decoded, nil)
end

---@param server table
---@param line string
local function record_stderr_line(server, line)
    if line == "" then
        return
    end
    table.insert(server.stderr_lines, line)
    if #server.stderr_lines > 20 then
        table.remove(server.stderr_lines, 1)
    end
end

---@param server table
---@param base string
---@return string
local function format_server_error(server, base)
    if #server.stderr_lines == 0 then
        return base
    end
    return base .. ": " .. server.stderr_lines[#server.stderr_lines]
end

local function ensure_server(root_dir)
    local servers = get_servers()
    local existing = servers[root_dir]
    local command, cwd, command_info = parser_command(root_dir)
    local command_signature = nil
    if command then
        command_signature = table.concat(command, "\0") .. "\0" .. cwd
    end

    if existing and existing.job_id and vim.fn.jobwait({ existing.job_id }, 0)[1] == -1 then
        if existing.transport_generation == TRANSPORT_GENERATION then
            if command == nil or existing.command_signature == command_signature then
                return existing
            end
        end
        pcall(vim.fn.jobstop, existing.job_id)
        servers[root_dir] = nil
    end

    if existing and existing.job_id and vim.fn.jobwait({ existing.job_id }, 0)[1] ~= -1 then
        servers[root_dir] = nil
    end

    if not command then
        return nil, "No parser binary or cargo executable available"
    end

    notify_build_start(command_info)

    local server = {
        root_dir = root_dir,
        ready = false,
        queue = {},
        pending = {},
        stdout_partial = "",
        stderr_partial = "",
        stderr_lines = {},
        full_warm_requested = false,
        next_request_id = 0,
        transport_generation = TRANSPORT_GENERATION,
        command_signature = command_signature,
        startup_timeout_ms = command_info and command_info.needs_build and BUILD_STARTUP_TIMEOUT_MS
            or REQUEST_TIMEOUT_MS,
    }

    local function on_stdout(_, data)
        if not data or #data == 0 then
            return
        end
        data[1] = server.stdout_partial .. (data[1] or "")
        server.stdout_partial = table.remove(data) or ""
        for _, line in ipairs(data) do
            handle_line(server, line)
        end
    end

    local function on_stderr(_, data)
        if not data then
            return
        end
        if #data == 0 then
            return
        end
        data[1] = server.stderr_partial .. (data[1] or "")
        server.stderr_partial = table.remove(data) or ""
        for _, line in ipairs(data) do
            record_stderr_line(server, line)
        end
    end

    local function on_exit(_, code)
        local message = "Hierarchy parser exited"
        if code and code ~= 0 then
            message = message .. " with code " .. tostring(code)
        end
        fail_server(server, format_server_error(server, message))
        if servers[root_dir] == server then
            servers[root_dir] = nil
        end
    end

    local job_id = vim.fn.jobstart(command, {
        cwd = cwd,
        stdout_buffered = false,
        stderr_buffered = false,
        on_stdout = on_stdout,
        on_stderr = on_stderr,
        on_exit = on_exit,
    })

    if job_id <= 0 then
        return nil, "Failed to start hierarchy parser"
    end

    server.job_id = job_id
    servers[root_dir] = server
    return server, nil
end

local function send_request(server, filename, payload, callback)
    server.next_request_id = server.next_request_id + 1
    local request_id = server.next_request_id
    payload.id = request_id
    payload.file = filename
    local encoded = json_encode(payload)
    local timeout_ms = server.ready and REQUEST_TIMEOUT_MS or server.startup_timeout_ms or REQUEST_TIMEOUT_MS

    server.pending[request_id] = {
        callback = callback,
    }
    vim.defer_fn(function()
        local pending = server.pending[request_id]
        if pending == nil then
            return
        end
        server.pending[request_id] = nil
        pending.callback(nil, "Hierarchy parser request timed out after " .. timeout_ms .. "ms")
    end, timeout_ms)
    if server.ready then
        vim.fn.chansend(server.job_id, encoded .. "\n")
    else
        table.insert(server.queue, encoded)
    end
end

---@param filename string
---@return boolean, string | nil
function M.prewarm_file(filename)
    if filename == nil or filename == "" then
        return false, "missing filename"
    end
    if not M.is_supported_file(filename) then
        return false, "unsupported file"
    end

    local root_dir = project_root(filename)
    local server, err = ensure_server(root_dir)
    if not server then
        return false, err
    end

    if not server.full_warm_requested then
        server.full_warm_requested = true
        send_request(server, filename, { action = "prewarm_full" }, function(_, warm_err)
            if warm_err then
                server.full_warm_requested = false
            end
        end)
    end

    return true, nil
end

function M.refresh_file(filename)
    if filename == nil or filename == "" then
        return
    end
    if not M.is_supported_file(filename) then
        return
    end

    local root_dir = project_root(filename)
    local server = get_servers()[root_dir]
    if not server then
        return
    end

    server.full_warm_requested = false
    send_request(server, filename, { action = "refresh" }, function(_, _) end)
end

local function request(uri, payload, callback)
    local filename = vim.uri_to_fname(uri)
    local root_dir = project_root(filename)
    local server, start_err = ensure_server(root_dir)
    if not server then
        callback(nil, start_err)
        return
    end

    send_request(server, filename, payload, callback)
end

local function symbol_range(line, col, name)
    local zero_line = math.max((line or 1) - 1, 0)
    local zero_col = math.max((col or 1) - 1, 0)
    local finish = zero_col + math.max(vim.fn.strchars(name or ""), 1)
    return {
        start = {
            line = zero_line,
            character = zero_col,
        },
        ["end"] = {
            line = zero_line,
            character = finish,
        },
    }
end

local function to_type_item(node)
    local range = symbol_range(node.line, node.col, node.name)
    return {
        name = node.name,
        kind = node.kind or 5,
        uri = vim.uri_from_fname(node.path),
        range = range,
        selectionRange = range,
    }
end

local function to_method_symbol(method)
    local range = symbol_range(method.line, method.col, method.name)
    return {
        kind = 12,
        name = method.name,
        range = range,
        selectionRange = range,
    }
end

local function display_type_name(type_ref)
    if type_ref == nil or type_ref == vim.NIL or type_ref == "" then
        return nil
    end
    return type_ref:match("([_%w]+)$") or type_ref
end

local function to_field_symbol(field)
    local range = symbol_range(field.line, field.col, field.name)
    local type_ref = field.type_ref
    if type_ref == vim.NIL or type_ref == "" then
        type_ref = nil
    end
    local type_line = field.type_line
    if type_line == vim.NIL then
        type_line = nil
    end
    local type_col = field.type_col
    if type_col == vim.NIL then
        type_col = nil
    end
    return {
        kind = 8,
        name = field.name,
        detail = field.annotation,
        is_required = field.required == true,
        type_ref = type_ref,
        type_name = display_type_name(type_ref),
        type_line = type_line and math.max(type_line - 1, 0) or nil,
        type_char = type_col and math.max(type_col - 1, 0) or nil,
        range = range,
        selectionRange = range,
    }
end

local function decode_class_member_response(response)
    local methods = {}
    for _, method in ipairs((response and response.methods) or {}) do
        table.insert(methods, to_method_symbol(method))
    end

    local fields = {}
    for _, field in ipairs((response and response.fields) or {}) do
        table.insert(fields, to_field_symbol(field))
    end

    local resolved_uri = nil
    if response and response.file and response.file ~= vim.NIL then
        resolved_uri = vim.uri_from_fname(response.file)
    end

    return methods, fields, resolved_uri, response and response.class_name or nil
end

local function to_lsp_range(range)
    return {
        start = {
            line = math.max((range.start.line or 1) - 1, 0),
            character = math.max((range.start.col or 1) - 1, 0),
        },
        ["end"] = {
            line = math.max((range["end"].line or range.start.line or 1) - 1, 0),
            character = math.max((range["end"].col or range.start.col or 1) - 1, 0),
        },
    }
end

local function to_call_item(item)
    local range = symbol_range(item.line, item.col, item.name)
    return {
        name = item.name,
        kind = item.kind or 6,
        uri = vim.uri_from_fname(item.path),
        range = range,
        selectionRange = range,
    }
end

local function request_type_items(action, uri, symbol_name, callback)
    request(uri, {
        action = action,
        class = symbol_name,
    }, function(response, err)
        if err then
            callback(nil, err)
            return
        end

        local items = {}
        if action == "supertypes" then
            local hierarchy = response and response.hierarchy
            if type(hierarchy) ~= "table" then
                callback(nil, "Hierarchy parser returned an invalid supertype response")
                return
            end
            for _, ancestor in ipairs(hierarchy.ancestors or {}) do
                if not ancestor.path or ancestor.path == vim.NIL then
                    callback(nil, "Parser ancestor requires fallback")
                    return
                end
                table.insert(items, to_type_item(ancestor))
            end
        else
            for _, item in ipairs((response and response.items) or {}) do
                if not item.path or item.path == vim.NIL then
                    callback(nil, "Parser subtype requires fallback")
                    return
                end
                table.insert(items, to_type_item(item))
            end
        end

        callback(items, nil)
    end)
end

local function request_call_items(action, position, symbol_name, callback)
    request(position.textDocument.uri, {
        action = action,
        class = symbol_name,
        line = (position.position.line or 0) + 1,
        col = (position.position.character or 0) + 1,
    }, function(response, err)
        if err then
            callback(nil, err)
            return
        end

        local items = {}
        for _, edge in ipairs((response and response.items) or {}) do
            local inner = to_call_item(edge.item or {})
            local from_ranges = {}
            for _, range in ipairs(edge.from_ranges or {}) do
                table.insert(from_ranges, to_lsp_range(range))
            end
            table.insert(items, {
                from = action == "incoming_calls" and inner or nil,
                to = action == "outgoing_calls" and inner or nil,
                fromRanges = from_ranges,
            })
        end
        callback(items, nil)
    end)
end

---@param mode Mode
---@param direction CallDirection | TypeDirection
---@param request_opts { uri: string, symbol_name: string, line: integer | nil, col: integer | nil }
---@param callback fun(tree: table | nil, err: string | nil)
---@param render_opts? { max_depth: integer | nil, member_depth: integer | nil }
function M.get_render_tree(mode, direction, request_opts, callback, render_opts)
    local action
    if mode:is_call() then
        action = direction:is_incoming() and "incoming_calls_tree" or "outgoing_calls_tree"
    else
        action = direction:is_super() and "supertypes_tree" or "subtypes_tree"
    end

    local payload = {
        action = action,
        class = request_opts.symbol_name,
    }
    if mode:is_call() then
        payload.line = request_opts.line
        payload.col = request_opts.col
    end
    if render_opts ~= nil then
        if render_opts.max_depth ~= nil then
            payload.max_depth = render_opts.max_depth
        end
        if render_opts.member_depth ~= nil then
            payload.member_depth = render_opts.member_depth
        end
    end

    request(request_opts.uri, payload, function(response, err)
        if err then
            callback(nil, err)
            return
        end

        local tree = response and response.tree or nil
        if type(tree) ~= "table" then
            callback(nil, "Hierarchy parser returned an invalid render tree response")
            return
        end

        callback(tree, nil)
    end)
end

---@param uri string
---@param class_name string
---@param callback fun(items: lsp.TypeHierarchyItem[] | nil, err: string | nil)
function M.get_supertypes(uri, class_name, callback)
    request_type_items("supertypes", uri, class_name, callback)
end

---@param uri string
---@param class_name string
---@param callback fun(items: lsp.TypeHierarchyItem[] | nil, err: string | nil)
function M.get_subtypes(uri, class_name, callback)
    request_type_items("subtypes", uri, class_name, callback)
end

---@param position lsp.TextDocumentPositionParams
---@param symbol_name string
---@param callback fun(items: lsp.CallHierarchyIncomingCall[] | nil, err: string | nil)
function M.get_incoming_calls(position, symbol_name, callback)
    request_call_items("incoming_calls", position, symbol_name, callback)
end

---@param position lsp.TextDocumentPositionParams
---@param symbol_name string
---@param callback fun(items: lsp.CallHierarchyOutgoingCall[] | nil, err: string | nil)
function M.get_outgoing_calls(position, symbol_name, callback)
    request_call_items("outgoing_calls", position, symbol_name, callback)
end

---@param uri string
---@param class_name string
---@param callback fun(methods: lsp.DocumentSymbol[] | nil, fields: lsp.DocumentSymbol[] | nil, err: string | nil)
function M.get_class_members(uri, class_name, callback)
    request(uri, {
        action = "class_members",
        class = class_name,
    }, function(response, err)
        if err then
            callback(nil, nil, err)
            return
        end

        local methods, fields = decode_class_member_response(response)
        callback(methods, fields, nil)
    end)
end

---@param uri string
---@param type_ref string
---@param callback fun(methods: lsp.DocumentSymbol[] | nil, fields: lsp.DocumentSymbol[] | nil, resolved_uri: string | nil, resolved_name: string | nil, err: string | nil)
function M.resolve_class_members(uri, type_ref, callback)
    request(uri, {
        action = "resolve_class_fields",
        class = type_ref,
    }, function(response, err)
        if err then
            callback(nil, nil, nil, nil, err)
            return
        end

        local methods, fields, resolved_uri, resolved_name = decode_class_member_response(response)
        callback(methods, fields, resolved_uri, resolved_name, nil)
    end)
end

---@param uri string
---@param type_ref string
---@param callback fun(fields: lsp.DocumentSymbol[] | nil, resolved_uri: string | nil, resolved_name: string | nil, err: string | nil)
function M.resolve_class_fields(uri, type_ref, callback)
    M.resolve_class_members(uri, type_ref, function(_, fields, resolved_uri, resolved_name, err)
        callback(fields, resolved_uri, resolved_name, err)
    end)
end

---@param position lsp.TextDocumentPositionParams
---@param callback fun(reference: table | nil, err: string | nil)
function M.resolve_callable_reference(position, callback)
    request(position.textDocument.uri, {
        action = "resolve_callable_reference",
        line = position.position.line + 1,
        col = position.position.character + 1,
    }, function(response, err)
        if err then
            callback(nil, err)
            return
        end

        local resolved_path = nil
        if response ~= nil then
            if response.path ~= nil and response.path ~= vim.NIL and response.path ~= "" then
                resolved_path = response.path
            elseif response.file ~= nil and response.file ~= vim.NIL and response.file ~= "" then
                resolved_path = response.file
            end
        end

        if response == nil or resolved_path == nil or response.name == nil or response.name == "" then
            callback(nil, "callable reference could not be resolved")
            return
        end

        callback({
            uri = vim.uri_from_fname(resolved_path),
            symbol_name = response.name,
            line = response.line,
            col = response.col,
            kind = response.kind,
            external = response.external == true,
        }, nil)
    end)
end

---@param filename string
---@return boolean
function M.is_supported_file(filename)
    local filetype = vim.filetype.match({ filename = filename }) or ""
    if filetype ~= "python" then
        return false
    end
    local root_dir = project_root(filename)
    local command = parser_command(root_dir)
    return command ~= nil
end

return M
