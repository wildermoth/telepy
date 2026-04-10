local state = require("telescope-hierarchy.state")
local Path = require("plenary.path")

--- Holds reference to a function location in the codebase that represents
--- a part of the call hierarchy
---@class Node
---@field text string The display name of the node
---@field filename string The filename that contains this node
---@field lnum integer The (1-based) line number of the reference
---@field col integer The (1-based) column number of the reference
---@field expanded boolean Is the node expanded in the current representation of the heirarchy tree
---@field recursive boolean Is this node recursive? Will be true if the same node exists in the parent chain
---@field cache table Cached metadata for the node location
---@field root Node The root of the tree this node is in
---@field parent Node | nil The parent node of this node
---@field children Node[] A list of the children of this node
local Node = {}
Node.__index = Node

local apply_override_flags

---@param value any
---@return any
local function denil(value)
    if value == vim.NIL then
        return nil
    end
    return value
end

local function is_external_path(filename)
    local normalized = (filename or ""):gsub("\\", "/")
    return normalized:match("/%.venv/") ~= nil
        or normalized:match("/site%-packages/") ~= nil
        or normalized:match("/dist%-packages/") ~= nil
        or normalized:match("/stdlib/") ~= nil
        or normalized:match("/parser/stubs/") ~= nil
end

--- Create a new (unattached) node
---@param uri string The URI representation of the filename where the node is found
---@param text string The display name of the node
---@param lnum integer The (l-based) line number of the reference
---@param col integer The (1-based) column number of the reference
---@param cache table
---@return Node
function Node.new(uri, text, lnum, col, cache)
    local node = {
        filename = vim.uri_to_fname(uri),
        text = text,
        lnum = lnum,
        col = col,
        expanded = false,
        recursive = false,
        cache = cache,
        parent = nil,
        children = {},
        node_kind = "class",
        is_overridden = false,
        method_names = nil,
        members_status = "No",
        member_callbacks = {},
    }
    -- We need to have a reference to a "root" node to make a valid node
    -- For an unattached node, this will be a self reference
    -- It gets over-written in `new_child`
    node.root = node
    setmetatable(node, Node)
    return node
end

---@param path string
---@param line integer
---@param col integer
---@return table
local function parser_position(path, line, col)
    return {
        textDocument = {
            uri = vim.uri_from_fname(path),
        },
        position = {
            line = math.max((line or 1) - 1, 0),
            character = math.max((col or 1) - 1, 0),
        },
    }
end

---@param inner table
---@return table
local function parser_cache(inner)
    return {
        location = parser_position(
            inner.target_path or inner.path,
            inner.target_line or inner.line,
            inner.target_col or inner.col
        ),
        name = inner.name,
        searched = "Yes",
        searched_node = nil,
        children = {},
        callbacks = {},
    }
end

---@param inner table
---@return Node
local function build_parser_node(inner)
    local path = assert(inner.path)
    local uri = vim.uri_from_fname(path)
    local kind = inner.kind or "class"
    local text = inner.name
    if (kind == "field" or kind == "field_object") and inner.required == true then
        text = text .. "*"
    end

    local node = Node.new(uri, text, inner.line or 1, inner.col or 1, parser_cache(inner))
    node.filename = path
    node.node_kind = kind
    node.recursive = inner.recursive == true
    node.expanded = kind == "method" or kind == "field"
    node.members_status = kind == "class" and "Yes" or "No"
    node.field_detail = denil(inner.detail) or ""
    node.field_type_ref = denil(inner.type_ref)
    node.field_type_name = denil(inner.type_name)
    node.parser_truncated = inner.truncated == true
    node.is_overridden = inner.is_overridden == true
    node.is_override = inner.is_override == true

    for _, child_data in ipairs(inner.children or {}) do
        local child = build_parser_node(child_data)
        child.parent = node
        table.insert(node.children, child)
    end

    return node
end

---@param node Node
---@param root Node
local function finalize_parser_node(node, root)
    node.root = root
    node.cache.searched_node = node
    node.cache.children = {}

    if node.node_kind == "class" then
        node.members_status = "Yes"
        node.method_names = {}
        node.field_names = {}
    end

    for _, child in ipairs(node.children) do
        child.parent = node
        finalize_parser_node(child, root)
        table.insert(node.cache.children, child.cache)
        if node.node_kind == "class" then
            if child.node_kind == "method" then
                node.method_names[child.cache.name or child.text] = true
            elseif child.node_kind == "field" or child.node_kind == "field_object" then
                node.field_names[child.cache.name or child.text] = true
            end
        end
    end
end

---@param node Node
local function clear_override_flags(node)
    node.is_override = false
    node.is_overridden = false
    for _, child in ipairs(node.children) do
        clear_override_flags(child)
    end
end

---@param class_node Node
local function apply_override_flags_recursive(class_node)
    if class_node.node_kind ~= "class" then
        return
    end

    class_node.method_names = class_node.method_names or {}
    class_node.field_names = class_node.field_names or {}

    for _, child in ipairs(class_node.children) do
        if child.node_kind == "class" then
            apply_override_flags_recursive(child)
            for _, member in ipairs(child.children) do
                if member.node_kind == "method" then
                    if class_node.method_names[member.cache.name or member.text] == true then
                        apply_override_flags(member, "method", class_node, member.cache.name or member.text)
                    end
                elseif member.node_kind == "field" or member.node_kind == "field_object" then
                    if class_node.field_names[member.cache.name or member.text] == true then
                        apply_override_flags(member, member.node_kind, class_node, member.cache.name or member.text)
                    end
                end
            end
        end
    end
end

---@param tree table
---@param opts? { request: table | nil, parser_needs_background_refresh: boolean | nil }
---@return Node
function Node.from_parser_tree(tree, opts)
    opts = opts or {}
    local root = build_parser_node(tree)
    root.parent = nil
    finalize_parser_node(root, root)
    root.parser_request = opts.request
    root.parser_needs_background_refresh = opts.parser_needs_background_refresh == true
    root.expanded = true
    clear_override_flags(root)
    apply_override_flags_recursive(root)
    return root
end

---@param node Node
---@return boolean
local function can_persist_expanded_state(node)
    return node.node_kind ~= "method" and node.node_kind ~= "field" and not node.recursive
end

---@param node Node
---@return string
local function expanded_state_key(node)
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

---@param map table<string, { expanded: boolean, filter_expanded: boolean | nil }> | nil
---@return table<string, { expanded: boolean, filter_expanded: boolean | nil }>
function Node:expanded_state_map(map)
    map = map or {}
    if can_persist_expanded_state(self) then
        map[expanded_state_key(self)] = {
            expanded = self.expanded == true,
            filter_expanded = self.filter_expanded,
        }
    end

    for _, child in ipairs(self.children) do
        child:expanded_state_map(map)
    end

    return map
end

---@param map table<string, { expanded: boolean, filter_expanded: boolean | nil }> | nil
function Node:apply_expanded_state_map(map)
    if map == nil then
        return
    end

    if can_persist_expanded_state(self) then
        local saved = map[expanded_state_key(self)]
        if saved ~= nil then
            self.expanded = saved.expanded == true
            self.filter_expanded = saved.filter_expanded
        end
    end

    for _, child in ipairs(self.children) do
        child:apply_expanded_state_map(map)
    end
end

---@return boolean
function Node:is_external()
    local session_opts = state.get("session_opts") or {}
    if session_opts.collapse_external == false then
        return false
    end
    return is_external_path(self.filename)
end

---Sort the children of the current node
function Node:sort_children()
    ---A comparsion function to compare any two nodes, such that we can sort them
    ---@param a Node
    ---@param b Node
    ---@return boolean
    local function cmp(a, b)
        ---@param node Node The node to encode
        local encode_node = function(node)
            -- Little trick; we prefer having the results concerning our current file at the top, since it's visually closer
            -- to the root node. We format the line number with 5 digits to make sure /my/file.c49 doesn't come after /my/file.c100
            -- since we do a lexical comparision '4' < '1' == false
            if node.filename == self.root.filename then
                return string.format("%05d", node.lnum)
            else
                return Path:new(node.filename):normalize(vim.uv.cwd()) .. string.format("%05d", node.lnum)
            end
        end
        return encode_node(a) < encode_node(b)
    end

    table.sort(self.children, cmp)
end

---@param uri string
---@param method table
---@return Node
local function build_method_node(uri, method)
    local stub_cache = {
        location = {
            textDocument = { uri = uri },
            position = {
                line = method.selectionRange.start.line,
                character = method.selectionRange.start.character,
            },
        },
        name = method.name,
        searched = "Yes",
        searched_node = { children = {} },
        children = {},
        callbacks = {},
    }
    local child =
        Node.new(uri, method.name, method.selectionRange.start.line + 1, method.selectionRange.start.character + 1, stub_cache)
    child.node_kind = "method"
    child.expanded = true
    return child
end

---@param uri string
---@param field table
---@param is_overridden boolean
---@return Node
local function build_field_node(uri, field, is_overridden)
    local label = field.name .. (field.is_required and "*" or "")
    local stub_cache = {
        location = {
            textDocument = { uri = uri },
            position = {
                line = field.selectionRange.start.line,
                character = field.selectionRange.start.character,
            },
        },
        name = field.name,
        searched = "Yes",
        searched_node = { children = {} },
        children = {},
        callbacks = {},
    }
    local child =
        Node.new(uri, label, field.selectionRange.start.line + 1, field.selectionRange.start.character + 1, stub_cache)
    local type_ref = field.type_ref or field.type_name
    local type_line = field.type_line or field.selectionRange.start.line
    local expandable = type_ref ~= nil
    child.node_kind = expandable and "field_object" or "field"
    child.field_detail = field.detail or ""
    child.field_type_name = field.type_name
    child.field_type_ref = type_ref
    child.field_type_lnum = type_line
    child.field_type_char = field.type_char
    child.expanded = not expandable
    child.is_overridden = is_overridden
    return child
end

---Pre-resolve field_object nodes to downgrade non-expandable ones to field kind.
---@param nodes Node[]
---@param callback fun()
local function pre_resolve_field_objects(nodes, callback)
    local parser = require("telescope-hierarchy.parser")
    local objects = {}
    for _, child in ipairs(nodes) do
        if child.node_kind == "field_object" then
            table.insert(objects, child)
        end
    end
    if #objects == 0 then
        callback()
        return
    end
    local remaining = #objects
    local function done()
        remaining = remaining - 1
        if remaining == 0 then
            callback()
        end
    end
    for _, child in ipairs(objects) do
        if child.field_type_ref == nil and child.field_type_name == nil then
            child.node_kind = "field"
            done()
        else
            local child_uri = vim.uri_from_fname(child.filename)
            parser.resolve_class_members(
                child_uri,
                child.field_type_ref or child.field_type_name,
                function(resolved_methods, resolved_fields, _, _, err)
                    local method_count = resolved_methods and #resolved_methods or 0
                    local field_count = resolved_fields and #resolved_fields or 0
                    if err ~= nil or (method_count == 0 and field_count == 0) then
                        child.node_kind = "field"
                    end
                    done()
                end
            )
        end
    end
end

---Walk up the tree skipping method nodes, returning the first class ancestor
---@param node Node
---@return Node | nil
local function find_parent_class_node(node)
    local p = node.parent
    while p do
        if p.node_kind ~= "method" then
            return p
        end
        p = p.parent
    end
end

---@param kind "method" | "field" | "field_object"
---@param parent_class Node | nil
---@param member_name string
---@return Node | nil
local function find_parent_member_node(kind, parent_class, member_name)
    if parent_class == nil then
        return nil
    end

    for _, child in ipairs(parent_class.children) do
        local same_kind = child.node_kind == kind
            or (
                (kind == "field" or kind == "field_object")
                and (child.node_kind == "field" or child.node_kind == "field_object")
            )
        if same_kind and (child.cache.name or child.text) == member_name then
            return child
        end
    end
end

---@return boolean
local function subtype_override_direction()
    local mode = state.mode()
    local direction = state.direction()
    return mode ~= nil and not mode:is_call() and direction ~= nil and not direction:is_super()
end

---@param node Node
---@param kind "method" | "field" | "field_object"
---@param parent_class Node | nil
---@param member_name string
apply_override_flags = function(node, kind, parent_class, member_name)
    local parent_member = find_parent_member_node(kind, parent_class, member_name)
    if parent_member == nil then
        return
    end

    if subtype_override_direction() then
        node.is_override = true
        parent_member.is_overridden = true
    else
        node.is_overridden = true
        parent_member.is_override = true
    end
end

---Load method and field children for a class node exactly once.
---@async
---@param callback fun(node: Node)
function Node:load_members(callback)
    if self.node_kind ~= "class" or self.recursive then
        callback(self)
        return
    end

    if self.members_status == "Yes" then
        callback(self)
        return
    end

    if self.members_status == "Pending" then
        table.insert(self.member_callbacks, callback)
        return
    end

    self.members_status = "Pending"
    self.member_callbacks = {}
    local parser = require("telescope-hierarchy.parser")
    local uri = vim.uri_from_fname(self.filename)

    parser.get_class_members(uri, self.text, function(methods, fields, err)
        if err ~= nil then
            self.members_status = "Yes"
            vim.notify("[hierarchy] " .. tostring(err), vim.log.levels.WARN)
            callback(self)

            local pending = table.remove(self.member_callbacks)
            while pending do
                pending(self)
                pending = table.remove(self.member_callbacks)
            end
            return
        end

        methods = methods or {}
        fields = fields or {}
        self.method_names = {}
        for _, m in ipairs(methods) do
            self.method_names[m.name] = true
        end

        self.field_names = {}
        for _, f in ipairs(fields) do
            self.field_names[f.name] = true
        end

        local parent_class = find_parent_class_node(self)
        local method_nodes = {}
        for _, m in ipairs(methods) do
            local child = build_method_node(uri, m)
            if
                parent_class ~= nil
                and parent_class.method_names ~= nil
                and parent_class.method_names[m.name] == true
            then
                apply_override_flags(child, "method", parent_class, m.name)
            end
            child.parent = self
            child.root = self.root
            child.expanded = true
            table.insert(method_nodes, child)
        end

        table.sort(method_nodes, function(a, b)
            if a.lnum == b.lnum then
                return a.col < b.col
            end
            return a.lnum < b.lnum
        end)
        for i = #method_nodes, 1, -1 do
            table.insert(self.children, 1, method_nodes[i])
        end

        local field_nodes = {}
        for _, f in ipairs(fields) do
            local child = build_field_node(uri, f, false)
            if parent_class ~= nil and parent_class.field_names ~= nil and parent_class.field_names[f.name] == true then
                apply_override_flags(child, child.node_kind, parent_class, f.name)
            end
            child.parent = self
            child.root = self.root
            table.insert(field_nodes, child)
        end

        for i = #field_nodes, 1, -1 do
            table.insert(self.children, 1, field_nodes[i])
        end

        pre_resolve_field_objects(field_nodes, function()
            self.members_status = "Yes"
            callback(self)

            local pending = table.remove(self.member_callbacks)
            while pending do
                pending(self)
                pending = table.remove(self.member_callbacks)
            end
        end)
    end)
end

---Expand a field_object node by resolving its class type and fetching its fields and methods.
---@async
---@param callback fun(node: Node)
function Node:_load_object_fields(callback)
    self.children = {} -- clear on re-expand
    local parser = require("telescope-hierarchy.parser")
    local source_uri = vim.uri_from_fname(self.filename)
    parser.resolve_class_members(
        source_uri,
        self.field_type_ref or self.field_type_name,
        function(methods, fields, uri, _, err)
            methods = methods or {}
            fields = fields or {}
            if err ~= nil or not uri or (#methods == 0 and #fields == 0) then
                self.node_kind = "field"
                callback(self)
                return
            end

            for _, f in ipairs(fields) do
                local child = build_field_node(uri, f, false)
                child.parent = self
                child.root = self.root
                table.insert(self.children, child)
            end

            local method_nodes = {}
            for _, m in ipairs(methods) do
                local child = build_method_node(uri, m)
                child.parent = self
                child.root = self.root
                table.insert(method_nodes, child)
            end

            table.sort(method_nodes, function(a, b)
                if a.lnum == b.lnum then
                    return a.col < b.col
                end
                return a.lnum < b.lnum
            end)
            for _, child in ipairs(method_nodes) do
                table.insert(self.children, child)
            end

            self.expanded = true
            pre_resolve_field_objects(self.children, function()
                callback(self)
            end)
        end
    )
end

---Ensure the current class node has had its hierarchy children resolved without
---changing its visible fold state.
---@async
---@param callback fun(node: Node, pending: boolean | nil)
function Node:ensure_hierarchy_loaded(callback)
    if self.node_kind ~= "class" or self.recursive then
        callback(self)
        return
    end
    callback(self)
end

---Ensure the current class node has both hierarchy children and class members loaded.
---@async
---@param callback fun(node: Node, pending: boolean | nil)
function Node:ensure_loaded(callback)
    self:ensure_hierarchy_loaded(function(node, pending)
        if pending then
            callback(node, pending)
            return
        end
        node:load_members(function(loaded)
            callback(loaded)
        end)
    end)
end

---Expand the node, searching for children if not already done
---The callback will not be called if the node is already expanded or is recursive
---@async
---@param callback fun(node: Node, pending: boolean | nil) Function to be run once children have been found (async) & the node expanded
---@param force_cb boolean | nil
function Node:expand(callback, force_cb)
    if self.node_kind == "method" or self.node_kind == "field" then
        if force_cb then
            callback(self)
        end
        return
    end
    if self.node_kind == "field_object" then
        if self.expanded then
            if force_cb then
                callback(self)
            end
            return
        end
        if force_cb then
            callback(self)
            return
        end -- skip in multi_expand
        self:_load_object_fields(callback)
        return
    end
    if self.expanded or self.recursive then
        if force_cb then
            callback(self)
        end
        return
    end

    self:ensure_loaded(function(node, pending)
        if pending then
            callback(node, pending)
            return
        end

        node.expanded = true
        callback(node)
    end)
end

---Recursively expand the current node
---Since this could be quite expensive, it takes a depth parameter
---and will only expand to that many layers deep
---@async
---@param depth integer The depth to which to expand the current node
---@param refresh_cb fun(node: Node) A callback to trigger a repaint of the picker window
function Node:multi_expand(depth, refresh_cb)
    ---Recursive heart of this function
    ---@async
    ---@param level integer A counter for which level (counting down towards 1) we are in
    ---@param frontier Node[] A list of nodes that are to be processed at the current level
    local function process_level(level, frontier)
        ---@type Node[]
        local next = {}
        local remaining = #frontier

        local function finish_level()
            remaining = remaining - 1
            if remaining == 0 then
                if level > 1 and #next > 0 then
                    process_level(level - 1, next)
                else
                    refresh_cb(self)
                end
            end
        end

        ---Callback function to be run once the node expansion work has resolved
        ---@async
        ---@param expanded Node
        ---@param pending boolean
        local once_expanded = function(expanded, pending)
            -- This allows us to repaint the picker window if the node is only in
            -- a pending state
            -- The early return will ensure that the remaining processing,
            -- which is intended for the node once expanded, is skipped
            if pending then
                refresh_cb(self)
                return
            end

            for _, child in ipairs(expanded.children) do
                table.insert(next, child)
            end

            finish_level()
        end

        for _, node in ipairs(frontier) do
            if node:is_external() and not node.expanded then
                finish_level()
            else
                -- Pass force_cb as true to ensure that even nodes that
                -- are known to have no children or be recursive trigger the callback
                -- This is necessary to ensure that the remaining counter above
                -- counts down to zero correctly and we don't hang mid-processing
                node:expand(once_expanded, true)
            end
        end
    end

    process_level(depth, { self })
end

---Recursively resolve class hierarchy descendants without changing their
---collapsed/expanded state in the picker.
---@async
---@param depth integer The depth to which to resolve the current node
---@param refresh_cb fun(node: Node) A callback to trigger a repaint of the picker window
---@param opts? { refresh_pending: boolean | nil }
function Node:multi_resolve(depth, refresh_cb, opts)
    opts = opts or {}
    local refresh_pending = opts.refresh_pending ~= false
    local function process_level(level, frontier)
        ---@type Node[]
        local next = {}
        local remaining = #frontier

        if remaining == 0 then
            refresh_cb(self)
            return
        end

        local function finish_level()
            remaining = remaining - 1
            if remaining == 0 then
                if level > 1 and #next > 0 then
                    process_level(level - 1, next)
                else
                    refresh_cb(self)
                end
            end
        end

        ---@async
        ---@param loaded Node
        ---@param pending boolean | nil
        local once_loaded = function(loaded, pending)
            if pending then
                if refresh_pending then
                    refresh_cb(self)
                end
                return
            end

            for _, child in ipairs(loaded.children) do
                if child.node_kind == "class" then
                    table.insert(next, child)
                end
            end

            finish_level()
        end

        for _, node in ipairs(frontier) do
            node:ensure_hierarchy_loaded(once_loaded)
        end
    end

    process_level(depth, { self })
end

---@return Node[][]
function Node:class_levels()
    ---@type Node[][]
    local levels = {}
    ---@type Node[]
    local frontier = { self.root }

    while #frontier > 0 do
        ---@type Node[]
        local level = {}
        ---@type Node[]
        local next = {}
        for _, node in ipairs(frontier) do
            if node.node_kind == "class" then
                table.insert(level, node)
                for _, child in ipairs(node.children) do
                    if child.node_kind == "class" then
                        table.insert(next, child)
                    end
                end
            end
        end
        if #level > 0 then
            table.insert(levels, level)
        end
        frontier = next
    end

    return levels
end

---@param levels Node[][]
---@param callback fun(node: Node)
function Node:load_member_levels(levels, callback)
    local level_idx = 1

    local function process_level()
        while level_idx <= #levels and #levels[level_idx] == 0 do
            level_idx = level_idx + 1
        end

        if level_idx > #levels then
            callback(self.root)
            return
        end

        local nodes = levels[level_idx]
        level_idx = level_idx + 1
        local remaining = #nodes

        if remaining == 0 then
            process_level()
            return
        end

        local function done()
            remaining = remaining - 1
            if remaining == 0 then
                process_level()
            end
        end

        for _, node in ipairs(nodes) do
            node:load_members(function()
                done()
            end)
        end
    end

    process_level()
end

---Recursively expand every expandable node in the tree.
---@async
---@param refresh_cb fun(node: Node)
function Node:expand_all(refresh_cb)
    local function process_frontier(frontier)
        if #frontier == 0 then
            refresh_cb(self)
            return
        end

        ---@type Node[]
        local next = {}
        local remaining = #frontier

        local function finish_node()
            remaining = remaining - 1
            if remaining == 0 then
                process_frontier(next)
            end
        end

        ---@param expanded Node
        ---@param pending boolean | nil
        local once_expanded = function(expanded, pending)
            if pending then
                refresh_cb(self)
                return
            end

            for _, child in ipairs(expanded.children) do
                table.insert(next, child)
            end

            finish_node()
        end

        for _, node in ipairs(frontier) do
            node:expand(once_expanded, true)
        end
    end

    process_frontier({ self })
end

---Collapse the node.
---This function is not actually async but it makes sense to write it this way so it can be
---composed with `expand` in a `toggle` method. It also allows the same pattern of not running
---the callback if the node is already collapsed
---@async
---@param callback fun(node: Node)
function Node:collapse(callback)
    if self.node_kind == "method" or self.node_kind == "field" then
        return
    end
    if not self.expanded then
        return
    end

    self.expanded = false
    callback(self)
end

---Toggle the expanded state of the node
---Since expanding requires searching for child nodes on the first pass, which is async,
---the entire function is written with the async pattern. The callback contains the following
---code to be run once the node's expanded state has been toggled
---@async
---@param callback fun(node: Node)
function Node:toggle(callback)
    if self.node_kind == "method" or self.node_kind == "field" then
        return
    end
    if self.expanded then
        self:collapse(callback)
    else
        self:expand(callback)
    end
end

---@async
---@param callback fun(node: Node)
function Node:switch_direction(callback)
    local parser = require("telescope-hierarchy.parser")
    local mode = assert(state.mode())
    local session_opts = state.get("session_opts") or {}
    state.switch_direction()
    local new_direction = assert(state.direction())
    local render_opts = nil
    if not mode:is_call() and not new_direction:is_super() then
        local staged_depth = session_opts.subtype_initial_render_depth or 2
        local staged_member_depth = session_opts.subtype_initial_member_depth or 1
        render_opts = {
            max_depth = staged_depth,
            member_depth = staged_member_depth,
        }
    end
    parser.get_render_tree(mode, new_direction, self.root.parser_request, function(render_tree, err)
        if err then
            state.switch_direction()
            vim.notify("[hierarchy] " .. tostring(err), vim.log.levels.WARN)
            return
        end
        local new_root = Node.from_parser_tree(render_tree, {
            request = self.root.parser_request,
            parser_needs_background_refresh = render_opts ~= nil,
        })
        callback(new_root)
    end, render_opts)
end

---@alias TreeStateLevel {last: boolean, fold_state: "collapsed" | "expanded" | nil}
---@alias NodeLevel {node: Node, tree_state: TreeStateLevel[]}
---@alias NodeList NodeLevel[]

---@param node Node
---@return "collapsed" | "expanded" | nil
local function fold_state(node)
    if node.node_kind == "method" or node.node_kind == "field" or node.recursive then
        return nil
    end

    if node.node_kind == "field_object" then
        return node.expanded and "expanded" or "collapsed"
    end

    if node.cache.searched == "No" or node.cache.searched == "Pending" then
        return "collapsed"
    end

    if #node.children == 0 then
        return nil
    end

    return node.expanded and "expanded" or "collapsed"
end

---@param node Node
---@return Node[]
local function filtered_children_by_toggle(node)
    local show_methods = state.ensure("show_methods", true)
    local show_fields = state.ensure("show_fields", false)
    local visible = {}
    for _, child in ipairs(node.children) do
        local kind = child.node_kind
        if
            (kind == "method" and not show_methods) or ((kind == "field" or kind == "field_object") and not show_fields)
        then
        -- hidden by toggle
        else
            table.insert(visible, child)
        end
    end
    return visible
end

---@param prompt string | nil
---@return string[]
local function prompt_terms(prompt)
    if prompt == nil then
        return {}
    end

    local trimmed = vim.trim(prompt)
    if trimmed == "" then
        return {}
    end

    local terms = {}
    for term in trimmed:lower():gmatch("%S+") do
        table.insert(terms, term)
    end
    return terms
end

---@param node Node
---@return string
local function node_search_text(node)
    local parts = {
        node.text,
        node.field_detail,
        node.field_type_name,
    }
    if node.cache and node.cache.name and node.cache.name ~= node.text then
        table.insert(parts, node.cache.name)
    end
    return table.concat(parts, " "):lower()
end

---@param node Node
---@param terms string[]
---@return boolean
local function node_matches_terms(node, terms)
    if #terms == 0 then
        return true
    end

    local haystack = node_search_text(node)
    for _, term in ipairs(terms) do
        if not haystack:find(term, 1, true) then
            return false
        end
    end
    return true
end

---@class FilteredNode
---@field node Node
---@field children FilteredNode[]
---@field has_matching_children boolean

---@param node Node
local function clear_filter_overrides(node)
    node.filter_expanded = nil
    for _, child in ipairs(node.children) do
        clear_filter_overrides(child)
    end
end

---@param node Node
---@param terms string[]
---@return FilteredNode | nil
local function collect_filtered_tree(node, terms)
    local matching_children = {}
    for _, child in ipairs(filtered_children_by_toggle(node)) do
        local child_desc = collect_filtered_tree(child, terms)
        if child_desc ~= nil then
            table.insert(matching_children, child_desc)
        end
    end

    local has_matching_children = #matching_children > 0
    if node_matches_terms(node, terms) or has_matching_children then
        local show_children = has_matching_children and node.filter_expanded ~= false
        return {
            node = node,
            children = show_children and matching_children or {},
            has_matching_children = has_matching_children,
        }
    end

    return nil
end

---@param filtered FilteredNode
---@return "collapsed" | "expanded" | nil
local function filtered_fold_state(filtered)
    if filtered.has_matching_children then
        return #filtered.children > 0 and "expanded" or "collapsed"
    end
    return nil
end

---@param list NodeList
---@param filtered FilteredNode
---@param tree_state TreeStateLevel[]
local function add_filtered_node_to_list(list, filtered, tree_state)
    table.insert(list, {
        node = filtered.node,
        tree_state = tree_state,
    })

    for idx, child in ipairs(filtered.children) do
        local last_child = idx == #filtered.children
        local new_state = { unpack(tree_state) }
        table.insert(new_state, {
            last = last_child,
            fold_state = filtered_fold_state(child),
        })
        add_filtered_node_to_list(list, child, new_state)
    end
end

---Add a node to the list reprsentation of the tree
---There is no return as the list is mutated in place.
---The mutated list is the effective return of this function
---@param list NodeList The list being built up
---@param node Node
---@param tree_state TreeStateLevel[] A list of per-level render flags used to draw the tree in the Telescope finder
local function add_node_to_list(list, node, tree_state)
    local entry = {
        node = node,
        tree_state = tree_state,
    }
    table.insert(list, entry)
    if node.expanded and #node.children > 0 then
        local visible = filtered_children_by_toggle(node)
        for idx, child in ipairs(visible) do
            local last_child = idx == #visible
            local new_state = { unpack(tree_state) }
            table.insert(new_state, {
                last = last_child,
                fold_state = fold_state(child),
            })
            add_node_to_list(list, child, new_state)
        end
    end
end

---Convert a tree of nodes into a list representation
---This is needed for Telescope which only works with lists. We retain a memory of the nestedness
---through the level field of the inner table
---@param from_root? boolean Optional flag to render from the node's root, if missing will assume the root is wanted
---@param prompt? string Optional tree filter prompt. When set, matching descendants keep their parent chain visible.
---@return NodeList
function Node:to_list(from_root, prompt)
    ---@type NodeList
    local results = {}
    local render_root = (from_root == nil or from_root) and self.root or self
    local terms = prompt_terms(prompt)
    local prompt_key = #terms > 0 and table.concat(terms, "\n") or nil
    if render_root._last_filter_prompt ~= prompt_key then
        clear_filter_overrides(render_root)
        render_root._last_filter_prompt = prompt_key
    end
    if #terms > 0 then
        local filtered = collect_filtered_tree(render_root, terms)
        if filtered ~= nil then
            add_filtered_node_to_list(results, filtered, {})
        end
        return results
    end
    add_node_to_list(results, render_root, {})
    return results
end

return Node
