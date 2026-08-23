-- M11a Neovim smoke test (docs/10 §14): drive graphy-lsp through Neovim's
-- built-in LSP client, headless. Per language: semantic tokens full (plus the
-- automatic semantic_tokens module's highlights), documentSymbol,
-- foldingRange, then a live buffer edit followed by full/delta against the
-- previous resultId (exercises incremental didChange + the delta endpoint).
-- Requires nvim 0.11+ (client:request_sync method form).

local bin = assert(os.getenv('GRAPHY_LSP_BIN'), 'GRAPHY_LSP_BIN not set')
local fixtures = assert(os.getenv('SMOKE_FIXTURES'), 'SMOKE_FIXTURES not set')
local report_path = assert(os.getenv('SMOKE_REPORT'), 'SMOKE_REPORT not set')

local report = { pass = false, nvim = tostring(vim.version()), docs = {} }

-- Turtle lines like `ex:s ex:p …` match the legacy `ex:` modeline prefix and
-- would be parsed as option settings (E518); this test is about LSP, not
-- modelines. No swapfile: a stale swap prompt would hang a headless run.
vim.o.modeline = false
vim.o.swapfile = false

-- Step trace so a hang in a headless run is attributable (stderr → run.log).
local function step(msg)
  io.stderr:write('[smoke] ' .. msg .. '\n')
  io.stderr:flush()
end

local function request(client, buf, method, params)
  local resp = client:request_sync(method, params, 10000, buf)
  assert(resp, method .. ': no response')
  assert(not resp.err, method .. ': ' .. vim.inspect(resp.err))
  assert(resp.result ~= nil, method .. ': null result')
  return resp.result
end

local function contains(list, want)
  for _, v in ipairs(list) do
    if v == want then return true end
  end
  return false
end

local function check(file, ft, expects)
  step(file .. ': edit')
  vim.cmd.edit(fixtures .. '/' .. file)
  local buf = vim.api.nvim_get_current_buf()
  vim.bo[buf].filetype = ft
  step(file .. ': lsp.start')
  local client_id = assert(
    vim.lsp.start({ name = 'graphy-lsp', cmd = { bin } }, { bufnr = buf }),
    'vim.lsp.start failed'
  )
  assert(
    vim.wait(10000, function()
      local c = vim.lsp.get_client_by_id(client_id)
      return c ~= nil and c.initialized
    end),
    file .. ': client never initialized'
  )
  local client = vim.lsp.get_client_by_id(client_id)
  local entry = { file = file, languageId = ft }
  local td = { uri = vim.uri_from_bufnr(buf) }

  -- The built-in semantic_tokens module's highlights prove the stock-Neovim
  -- integration path, not just raw RPC. It auto-attaches on the first buffer;
  -- for reused clients start it explicitly (idempent for already-attached).
  step(file .. ': builtin semantic_tokens highlight')
  pcall(vim.lsp.semantic_tokens.start, buf, client_id)
  local row, col = expects.hlpos[1], expects.hlpos[2]
  assert(
    vim.wait(10000, function()
      local toks = vim.lsp.semantic_tokens.get_at_pos(buf, row, col)
      return toks ~= nil and #toks > 0
    end),
    ('%s: built-in semantic_tokens produced no highlight at (%d,%d)'):format(file, row, col)
  )
  entry.highlightAt = vim.lsp.semantic_tokens.get_at_pos(buf, row, col)[1].type

  -- Stop the module's own full/delta refresher: it races the raw requests
  -- below and its fresher resultIds would (correctly) make the server answer
  -- our stale-id deltas with full-token fallbacks.
  vim.lsp.semantic_tokens.stop(buf, client_id)

  -- Semantic tokens: raw full request.
  step(file .. ': semanticTokens/full')
  local full = request(client, buf, 'textDocument/semanticTokens/full', { textDocument = td })
  assert(#full.data > 0, file .. ': no semantic tokens')
  assert(#full.data % 5 == 0, file .. ': token data not 5-aligned')
  assert(full.resultId, file .. ': full carries no resultId')
  entry.semanticTokenInts = #full.data

  -- Outline.
  step(file .. ': documentSymbol')
  local syms = request(client, buf, 'textDocument/documentSymbol', { textDocument = td })
  entry.symbols = vim.tbl_map(function(s) return s.name end, syms)
  for _, want in ipairs(expects.symbols) do
    assert(contains(entry.symbols, want),
      file .. ': missing symbol ' .. want .. ', got ' .. vim.inspect(entry.symbols))
  end

  -- Folding.
  step(file .. ': foldingRange')
  local folds = request(client, buf, 'textDocument/foldingRange', { textDocument = td })
  assert(#folds >= 1, file .. ': no folding ranges')
  entry.folds = vim.tbl_map(function(f) return { f.startLine, f.endLine } end, folds)

  -- Hover: find the token by text so fixture edits don't shift positions.
  if expects.hover then
    step(file .. ': hover')
    local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
    local row, col
    for i, l in ipairs(lines) do
      local c = l:find(expects.hover[1], 1, true)
      if c then
        row, col = i - 1, c -- one char inside the token
        break
      end
    end
    assert(row, file .. ': hover needle missing: ' .. expects.hover[1])
    local hr = client:request_sync('textDocument/hover',
      { textDocument = td, position = { line = row, character = col } }, 10000, buf)
    local value = hr and hr.result and hr.result.contents and hr.result.contents.value
    assert(value and value:find(expects.hover[2], 1, true),
      file .. ': hover mismatch: ' .. vim.inspect(hr))
    entry.hover = value
  end

  -- Completion at the end of a token.
  if expects.complete then
    step(file .. ': completion')
    local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
    local row, col
    for i, l in ipairs(lines) do
      local c = l:find(expects.complete[1], 1, true)
      if c then
        row, col = i - 1, c - 1 + #expects.complete[1]
        break
      end
    end
    assert(row, file .. ': completion needle missing')
    local cr = client:request_sync('textDocument/completion',
      { textDocument = td, position = { line = row, character = col } }, 10000, buf)
    assert(cr and cr.result, file .. ': completion failed')
    local found = false
    for _, it in ipairs(cr.result) do
      if it.label == expects.complete[2] then found = true end
    end
    assert(found, file .. ': completion missing `' .. expects.complete[2] .. '`')
    entry.completions = #cr.result
  end

  -- Live edit -> incremental didChange -> full/delta vs the previous result.
  -- Each delta response mints a fresh resultId, so the poll must chain ids:
  -- an empty-edit delta (didChange not seen yet) hands its id to the next try.
  step(file .. ': edit + full/delta')
  vim.api.nvim_buf_set_lines(buf, -1, -1, false, { expects.append })
  local prev = full.resultId
  local last
  assert(
    vim.wait(10000, function()
      local d = client:request_sync('textDocument/semanticTokens/full/delta',
        { textDocument = td, previousResultId = prev }, 5000, buf)
      if not (d and d.result) then return false end
      last = d.result
      prev = d.result.resultId or prev
      if d.result.edits and #d.result.edits > 0 and #d.result.edits[1].data > 0 then
        entry.deltaEditData = #d.result.edits[1].data
        return true
      end
      return false
    end, 300),
    file .. ': no delta edits after buffer edit; last: ' .. vim.inspect(last)
  )

  table.insert(report.docs, entry)
end

-- Watchdog: a wedged wait must still produce a report and a nonzero exit.
local watchdog = (vim.uv or vim.loop).new_timer()
watchdog:start(90000, 0, function()
  local f = io.open(report_path, 'w')
  if f then
    f:write(vim.json.encode({ pass = false, error = 'watchdog: hung after 90s', docs = report.docs }))
    f:close()
  end
  os.exit(3)
end)

local ok, err = pcall(function()
  check('smoke.ttl', 'turtle', {
    symbols = { 'ex:' },
    hlpos = { 0, 1 }, -- inside @prefix
    hover = { 'ex:p', 'http://example.org/p' },
    complete = { 'ex:q', 'p' }, -- local names under ex:
    append = 'ex:added ex:p "new" .',
  })
  check('smoke.rq', 'sparql', {
    symbols = { 'ex:', 'SELECT' },
    hlpos = { 0, 1 }, -- inside PREFIX
    hover = { 'FILTER', 'restricts solutions' },
    complete = { '?o', '?s' }, -- variables in scope
    append = 'ORDER BY ?s',
  })
  check('smoke.jsonld', 'jsonld', {
    symbols = { '@context', '@id', 'name' },
    hlpos = { 1, 4 }, -- inside "@context" (line 0 is just `{`, no token)
    hover = { '@context', 'maps terms' },
    append = '{"@id": "http://example.org/y", "extra": 42}',
  })
end)

report.pass = ok
if not ok then report.error = tostring(err) end
local f = assert(io.open(report_path, 'w'))
f:write(vim.json.encode(report))
f:close()
if ok then vim.cmd('qa!') else vim.cmd('cq!') end
