local TreeSitter = {}

---@param node TSNode | nil
---@param pattern string
---@return TSNode | nil
local function find_outer_named_node(node, pattern)
  while true do
    if not node then
      return
    end
    local type = node:type()
    if type:find(pattern) then
      return node
    end
    node = node:parent()
  end
end

---Navigate outwards until we find a node that is of type that has the word function in it
---@param node TSNode | nil
---@return TSNode | nil
local function find_outer_function_node(node)
  return find_outer_named_node(node, "function")
end

---Find the function name node inside an outer node of function type
---@param node TSNode | nil
---@return TSNode | nil
local function find_function_name_node(node)
  local outer = find_outer_function_node(node)
  if not outer then
    return
  end
  local names = outer:field("name")
  if #names == 0 then
    return
  end
  if #names > 1 then
    -- Ambiguous name field; use the first match
  end
  return names[1]
end

---@param fallback boolean
---@return { name: string, lnum: integer, col: integer } | nil
local function current_function_impl(fallback)
  local node = vim.treesitter.get_node()
  local target = find_function_name_node(node)
  if not target then
    if not fallback then
      return
    end
    local cursor = vim.api.nvim_win_get_cursor(0)
    local lines = vim.api.nvim_buf_get_lines(0, 0, cursor[1], false)
    for index = #lines, 1, -1 do
      local line = lines[index]
      local name = line:match("^%s*async%s+def%s+([%a_][%w_]*)") or line:match("^%s*def%s+([%a_][%w_]*)")
      if name then
        local col = line:find(name, 1, true) or 1
        return {
          name = name,
          lnum = index,
          col = col,
        }
      end
    end
    return
  end

  local start_row, start_col, _ = target:start()
  local text = vim.treesitter.get_node_text(target, 0)
  if not text or text == "" then
    return
  end
  return {
    name = text,
    lnum = start_row + 1,
    col = start_col + 1,
  }
end

---Use treesitter to find an outer node from the current location that
---is a function, then find the function name within that & move the
---cursor to the function name location. This is useful as we need to be
---on a function name in order to find incoming or outgoing calls
function TreeSitter.find_function()
  local node = vim.treesitter.get_node()
  local target = find_function_name_node(node)
  if target then
    local start_row, start_col, _ = target:start()
    vim.api.nvim_win_set_cursor(0, { start_row + 1, start_col })
  end
end

---@return { name: string, lnum: integer, col: integer } | nil
function TreeSitter.current_function()
  return current_function_impl(true)
end

---@param node TSNode | nil
---@return TSNode | nil
local function find_outer_class_node(node)
  return find_outer_named_node(node, "class")
end

---@param node TSNode | nil
---@return TSNode | nil
local function find_class_name_node(node)
  local outer = find_outer_class_node(node)
  if not outer then
    return
  end
  local names = outer:field("name")
  if #names == 0 then
    return
  end
  return names[1]
end

---@return { name: string, lnum: integer, col: integer } | nil
function TreeSitter.current_class()
  local node = vim.treesitter.get_node()
  local target = find_class_name_node(node)
  if not target then
    local cursor = vim.api.nvim_win_get_cursor(0)
    local lines = vim.api.nvim_buf_get_lines(0, 0, cursor[1], false)
    for index = #lines, 1, -1 do
      local line = lines[index]
      local name = line:match("^%s*class%s+([%a_][%w_]*)")
      if name then
        local col = line:find(name, 1, true) or 1
        return {
          name = name,
          lnum = index,
          col = col,
        }
      end
    end
    return
  end
  local start_row, start_col, _ = target:start()
  local text = vim.treesitter.get_node_text(target, 0)
  if not text or text == "" then
    return
  end
  return {
    name = text,
    lnum = start_row + 1,
    col = start_col + 1,
  }
end

---@return { name: string, lnum: integer, col: integer } | nil
function TreeSitter.enclosing_function()
  return current_function_impl(false)
end

---@return { name: string, lnum: integer, col: integer } | nil
function TreeSitter.enclosing_class()
  local node = vim.treesitter.get_node()
  local target = find_class_name_node(node)
  if not target then
    return
  end
  local start_row, start_col, _ = target:start()
  local text = vim.treesitter.get_node_text(target, 0)
  if not text or text == "" then
    return
  end
  return {
    name = text,
    lnum = start_row + 1,
    col = start_col + 1,
  }
end

---@return { name: string, start_col: integer, end_col: integer, line: string } | nil
function TreeSitter.symbol_under_cursor_info()
  local cursor = vim.api.nvim_win_get_cursor(0)
  local line = vim.api.nvim_buf_get_lines(0, cursor[1] - 1, cursor[1], false)[1] or ""
  if line == "" then
    return
  end

  local function symbol_char(ch)
    return ch ~= nil and ch:match("[%w_%.]") ~= nil
  end

  local pos = cursor[2] + 1
  if pos > #line then
    pos = #line
  end

  if pos < 1 then
    return
  end

  if not symbol_char(line:sub(pos, pos)) and symbol_char(line:sub(pos + 1, pos + 1)) then
    pos = pos + 1
  end
  if not symbol_char(line:sub(pos, pos)) and symbol_char(line:sub(pos - 1, pos - 1)) then
    pos = pos - 1
  end
  if not symbol_char(line:sub(pos, pos)) then
    return
  end

  local start_col = pos
  while start_col > 1 and symbol_char(line:sub(start_col - 1, start_col - 1)) do
    start_col = start_col - 1
  end

  local end_col = pos
  while end_col < #line and symbol_char(line:sub(end_col + 1, end_col + 1)) do
    end_col = end_col + 1
  end

  local symbol = line:sub(start_col, end_col)
  if symbol == "" or symbol:match("^%.+$") then
    return
  end

  return {
    name = symbol,
    start_col = start_col,
    end_col = end_col,
    line = line,
  }
end

---@return string | nil
function TreeSitter.symbol_under_cursor()
  local info = TreeSitter.symbol_under_cursor_info()
  return info and info.name or nil
end

return TreeSitter
