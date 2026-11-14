'use strict';

const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => Array.from(root.querySelectorAll(selector));
const escapeHTML = value => String(value ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const storedToken = () => { try { return sessionStorage.getItem('sharebox.admin') || ''; } catch { return ''; } };
let adminToken = storedToken();
let workspace = { workers: [], jobs: [], tokens: [] };
let refreshTimer;
let refreshing = false;
let toastTimer;
let selectedJobId = null;
let modalKind = '';
let network = { bindAddress: '', localOnly: false, urls: [] };
let selectedLanUrl = '';

function setToken(value) {
  adminToken = value;
  try { if (value) sessionStorage.setItem('sharebox.admin', value); else sessionStorage.removeItem('sharebox.admin'); } catch { /* Session still works in memory when browser storage is disabled. */ }
}
function showError(node, message) { node.textContent = message; node.classList.toggle('hidden', !message); }
function notify(message) { const toast = $('#toast'); toast.textContent = message; toast.classList.remove('hidden'); clearTimeout(toastTimer); toastTimer = setTimeout(() => toast.classList.add('hidden'), 5000); }
function numericTime(value) { return typeof value === 'number' ? value : new Date(value).getTime(); }
function relativeTime(value) {
  const time = numericTime(value); if (!Number.isFinite(time) || time <= 0) return 'Not seen yet';
  const seconds = Math.max(0, Math.floor((Date.now() - time) / 1000));
  if (seconds < 10) return 'Just now'; if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`; if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return new Date(time).toLocaleDateString();
}
function online(worker) { return !worker.revoked && Date.now() - numericTime(worker.lastSeen) < 20000; }
function statusClass(status) { return ['queued', 'running', 'cancelling', 'passed', 'failed', 'cancelled', 'lost'].includes(status) ? status : 'queued'; }
function terminal(job) { return ['passed', 'failed', 'cancelled', 'lost'].includes(job.status); }
function workerName(id) { return workspace.workers.find(w => w.id === id)?.name || id || 'Unassigned'; }
function shortId(id) { return String(id || '').slice(0, 12); }
function shellQuote(value) { return `'${String(value).replace(/'/g, `'"'"'`)}'`; }

async function api(path, options = {}) {
  const headers = { 'Authorization': `Bearer ${adminToken}`, ...options.headers };
  if (options.body !== undefined) headers['Content-Type'] = 'application/json';
  let response;
  try { response = await fetch(path, { ...options, headers, cache: 'no-store', body: options.body === undefined ? undefined : JSON.stringify(options.body) }); }
  catch { throw new Error('Cannot reach the master. Check that it is running and this device is on the same network.'); }
  const text = await response.text();
  let data = {}; try { data = text ? JSON.parse(text) : {}; } catch { if (response.ok) throw new Error('The master returned an unexpected response.'); }
  if (!response.ok) {
    if (response.status === 401 && !path.endsWith('/me')) disconnect('Your admin token is no longer valid. Connect again to continue.');
    throw new Error(typeof data.error === 'string' ? data.error : data.error?.message || data.message || `Request failed (${response.status}).`);
  }
  return data;
}

function disconnect(message = '') {
  clearInterval(refreshTimer); setToken(''); selectedJobId = null; selectedLanUrl = ''; network = { bindAddress: '', localOnly: false, urls: [] }; closeModal();
  $('#loading').classList.add('hidden'); $('#app').classList.add('hidden'); $('#login').classList.remove('hidden');
  $('#admin-token').value = ''; showError($('#login-error'), message);
}
async function connect() {
  const me = await api('/api/me');
  $('#workspace-name').textContent = me.name || 'Local workspace'; $('#master-host').textContent = location.host;
  $('#mcp-endpoint').textContent = `${location.origin}/mcp`;
  $('#loading').classList.add('hidden'); $('#login').classList.add('hidden'); $('#app').classList.remove('hidden');
  await Promise.all([refresh(), refreshNetwork()]); clearInterval(refreshTimer); refreshTimer = setInterval(refresh, 4000);
}

function renderNetwork() {
  const urls = network.localOnly ? [] : network.urls;
  if (!urls.includes(selectedLanUrl)) selectedLanUrl = urls[0] || '';
  $('#network-addresses').classList.toggle('hidden', !urls.length);
  $('#lan-address').innerHTML = urls.map(url => `<option value="${escapeHTML(url)}" ${url === selectedLanUrl ? 'selected' : ''}>${escapeHTML(url)}</option>`).join('');
  $('#copy-lan-address').disabled = !selectedLanUrl;
  $('#network-bind').textContent = network.bindAddress ? `Listening on ${network.bindAddress}` : '';
  $('#network-status').textContent = network.error || (network.localOnly
    ? 'This master accepts connections only from its own machine. To connect a remote worker, restart it with --bind 0.0.0.0:7331 (or your chosen port), keeping the same data directory. Allow Sharebox on private networks only.'
    : urls.length ? `${urls.length} LAN address${urls.length === 1 ? '' : 'es'} detected. Use the selected address when pairing another machine.`
      : 'No private LAN address was detected. Connect the master to your trusted network, then refresh addresses. Remote pairing is unavailable until a LAN address is found.');
}
async function refreshNetwork() {
  const button = $('#refresh-network'); button.disabled = true;
  try {
    const result = await api('/api/network');
    // Keep selectable addresses bounded, and never treat wildcard or loopback URLs as LAN URLs.
    const urls = (Array.isArray(result.urls) ? result.urls : []).slice(0, 64).filter(value => {
      try { if (typeof value !== 'string') return false; const url = new URL(value); return url.protocol === 'http:' && !url.username && !url.password && !['localhost', '0.0.0.0', '[::]', '[::1]'].includes(url.hostname) && !url.hostname.startsWith('127.'); } catch { return false; }
    });
    network = { bindAddress: result.bindAddress || '', localOnly: result.localOnly === true, urls, error: result.error || '' };
  } catch (error) { network = { bindAddress: '', localOnly: false, urls: [], error: `Unable to read network addresses: ${error.message}` }; }
  finally { renderNetwork(); button.disabled = false; }
}

async function refresh() {
  if (refreshing || !adminToken) return; refreshing = true;
  try {
    const result = await api('/api/workspace');
    workspace = { workers: result.workers || [], jobs: result.jobs || [], tokens: result.tokens || [] };
    render(); showError($('#page-error'), ''); $('#connection-state').textContent = 'Connected'; $('#connection-dot').className = 'dot'; $('#connection-caption').textContent = 'Local master connected';
    $('#last-updated').textContent = `Updated ${new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })}`;
    if (selectedJobId && $('#modal').open) await updateJob();
  } catch (error) {
    if (adminToken) { showError($('#page-error'), error.message); $('#connection-state').textContent = 'Reconnecting'; $('#connection-dot').className = 'dot error'; $('#connection-caption').textContent = 'Waiting for master connection'; }
  } finally { refreshing = false; }
}

function render() {
  const activeWorkers = workspace.workers.filter(w => !w.revoked);
  const onlineWorkers = activeWorkers.filter(online);
  const running = workspace.jobs.filter(j => ['running', 'cancelling'].includes(j.status)).length;
  const queued = workspace.jobs.filter(j => j.status === 'queued').length;
  const completed = workspace.jobs.filter(terminal).length;
  $('#stat-workers').innerHTML = `${onlineWorkers.length} <span>/ ${activeWorkers.length}</span>`;
  $('#stat-workers-note').textContent = activeWorkers.length ? `${activeWorkers.length - onlineWorkers.length} offline · ${onlineWorkers.length} available on network` : 'Pair your first machine to get started';
  $('#stat-active').textContent = running + queued; $('#stat-active-note').textContent = `${running} running · ${queued} queued`;
  $('#stat-completed').textContent = completed; $('#stat-completed-note').textContent = `${workspace.jobs.filter(j => j.status === 'passed').length} passed · ${workspace.jobs.filter(j => j.status === 'failed').length} failed`;
  $('#worker-count').textContent = String(activeWorkers.length).padStart(2, '0');
  $('#topology-workers').innerHTML = activeWorkers.length ? activeWorkers.slice(0, 4).map(w => `<div class="topology-node"><span class="node-symbol">${w.os === 'windows' ? '⊞' : '▱'}</span><div><strong>${escapeHTML(w.name)}</strong><small><span class="dot ${online(w) ? '' : 'offline'}"></span>${escapeHTML(w.os)} · ${online(w) ? 'Online' : 'Offline'}</small></div></div>`).join('') : '<div class="topology-empty">Pair a machine to grow your network</div>';
  $('#worker-list').innerHTML = activeWorkers.length ? activeWorkers.map(w => {
    const jobs = workspace.jobs.filter(j => j.workerId === w.id && ['running', 'cancelling'].includes(j.status));
    return `<article class="panel worker"><div class="worker-header"><span class="worker-icon">${w.os === 'windows' ? '⊞' : w.os === 'linux' ? '◇' : '▱'}</span><span class="pill ${online(w) ? 'online' : ''}"><span class="dot ${online(w) ? '' : 'offline'}"></span>${online(w) ? 'Online' : 'Offline'}</span></div><h3>${escapeHTML(w.name)}</h3><p class="worker-meta">${escapeHTML(w.os || 'Unknown OS')} · ${escapeHTML(w.arch || 'Unknown architecture')}</p><div class="tags">${(w.tasks || []).map(t => `<span class="tag" title="${escapeHTML(t.description || '')}">${escapeHTML(t.name)}</span>`).join('') || '<span class="tag">No configured tasks</span>'}</div><div class="info-row"><span>Execution</span><strong>Owner-configured tasks</strong></div><div class="info-row"><span>Activity</span><strong>${jobs.length ? `${jobs.length} running` : 'Idle'}</strong></div><div class="info-row"><span>Last heartbeat</span><span>${escapeHTML(relativeTime(w.lastSeen))}</span></div><div class="worker-footer"><button class="btn" data-action="worker-job" data-id="${escapeHTML(w.id)}" ${!(w.tasks || []).length ? 'disabled' : ''}>Run task ↗</button><button class="text-btn danger" data-action="revoke-worker" data-id="${escapeHTML(w.id)}">Revoke access</button></div></article>`;
  }).join('') : '<div class="empty"><span>▣</span><h3>Your first worker is one command away</h3><p>Pair a Mac, Windows, or Linux machine to make its configured tasks available to your agent.</p><button class="btn primary" data-action="pair">+ Pair a worker</button></div>';
  renderJobs();
  $('#token-list').innerHTML = workspace.tokens.length ? workspace.tokens.map(t => `<div class="token-row"><div><strong>${escapeHTML(t.name || 'MCP token')}</strong><small>Created ${escapeHTML(relativeTime(t.createdAt))} · ${escapeHTML(shortId(t.id))}</small></div><button class="text-btn danger" data-action="revoke-token" data-id="${escapeHTML(t.id)}">Revoke</button></div>`).join('') : '<p class="tokens-empty">No MCP tokens yet. Create one to connect an agent.</p>';
}
function renderJobs() {
  const filter = $('#job-filter').value;
  const jobs = workspace.jobs.filter(j => filter === 'all' || j.status === filter).sort((a, b) => numericTime(b.createdAt) - numericTime(a.createdAt));
  $('#job-list').innerHTML = jobs.map(j => `<tr><td>${escapeHTML(j.task)}<small>${escapeHTML(shortId(j.id))}</small></td><td>${escapeHTML(workerName(j.workerId))}</td><td>${escapeHTML(relativeTime(j.createdAt))}</td><td><span class="status ${statusClass(j.status)}">${escapeHTML(j.status)}</span></td><td><button class="text-btn" data-action="job-details" data-id="${escapeHTML(j.id)}">View logs ↗</button></td></tr>`).join('');
  $('#jobs-empty').classList.toggle('hidden', jobs.length > 0);
  $('#jobs-empty h3').textContent = filter === 'all' ? 'No jobs yet' : `No ${filter} jobs`;
  $('#jobs-empty p').textContent = filter === 'all' ? 'Pair a worker, then delegate one of its configured tasks.' : 'Choose another status to see more jobs.';
}

function openModal(html, kind = '') { selectedJobId = null; modalKind = kind; $('#modal-content').innerHTML = html; if (!$('#modal').open) $('#modal').showModal(); }
function closeModal() { selectedJobId = null; modalKind = ''; if ($('#modal').open) $('#modal').close(); }
function errorBox() { return '<div id="modal-error" class="alert hidden" role="alert"></div>'; }
function modalError(error) { const target = $('#modal-error'); if (target) showError(target, error.message || String(error)); else notify(error.message || String(error)); }
async function copy(text) {
  document.querySelectorAll('.clipboard-fallback').forEach(field => field.remove());
  try { await navigator.clipboard.writeText(text); notify('Copied to clipboard.'); }
  catch {
    // The modern Clipboard API is unavailable on plain HTTP LAN origins.
    const field = document.createElement('textarea');
    field.value = text; field.className = 'clipboard-fallback';
    field.setAttribute('aria-label', 'Text to copy'); field.readOnly = true;
    const previous = document.activeElement;
    ($('#modal').open ? $('#modal-content') : document.body).append(field);
    field.focus(); field.select();
    let copied = false;
    try { copied = document.execCommand('copy'); } catch { /* Offer manual copy below. */ }
    if (copied) { field.remove(); previous?.focus(); notify('Copied to clipboard.'); }
    else {
      field.classList.add('manual-copy');
      notify('Press Ctrl+C (Command+C on Mac) to copy the selected text.');
      // Keep the field until the dialog closes or another copy is requested.
      // Removing it during blur can shift the button receiving the next click.
    }
  }
}

async function pairing(sameMachine = false) {
  openModal(`<h2 id="modal-title">Pair a worker</h2><p>Creating a one-time pairing code for this workspace…</p>${errorBox()}`, 'pair');
  try {
    await refreshNetwork();
    if (modalKind !== 'pair') return;
    if (!sameMachine && !selectedLanUrl) {
      openModal(`<h2 id="modal-title">Remote pairing needs a LAN address</h2><p>${escapeHTML($('#network-status').textContent)}</p><p>You can still pair a worker on the master’s own machine using this dashboard address.</p><div class="actions"><button class="btn" data-action="close">Close</button><button class="btn primary" id="pair-same-machine">Pair on this machine only</button></div>`, 'pair');
      $('#pair-same-machine').onclick = () => pairing(true);
      return;
    }
    const serverUrl = sameMachine ? location.origin : selectedLanUrl;
    const result = await api('/api/pairing', { method: 'POST', body: {} });
    if (modalKind !== 'pair') return;
    const init = './sharebox init --config worker.json';
    const command = `./sharebox worker --server ${shellQuote(serverUrl)} --code ${shellQuote(result.code)} --config worker.json`;
    openModal(`<h2 id="modal-title">${sameMachine ? 'Pair a worker on this machine' : 'Bring another machine online'}</h2><p>Run the Sharebox binary on the target machine. Its configuration defines exactly which tasks your agent may execute.</p><div class="alert warning">Pairing code expires ${escapeHTML(new Date(numericTime(result.expiresAt)).toLocaleTimeString())}. Use it once and keep it private.</div><div class="command-label"><strong>1. Create a worker configuration</strong><button class="text-btn" id="copy-init">Copy</button></div><pre class="command">${escapeHTML(init)}</pre><p>Edit <code>worker.json</code> to select local directories and allowed tasks.</p><div class="command-label"><strong>2. Pair and start the worker</strong><button class="text-btn" id="copy-pair">Copy</button></div><pre class="command">${escapeHTML(command)}</pre><p>On Windows, use <code>.\\sharebox.exe</code> in PowerShell. The target machine must be able to reach <strong>${escapeHTML(serverUrl)}</strong>. ${sameMachine ? 'This command is for a worker on the same machine as the master.' : 'Open this URL from the worker first to check access; address detection does not verify firewall or network reachability.'}</p><div class="alert success">The worker will appear in your dashboard after it connects. Keep its terminal running.</div>${errorBox()}<div class="actions"><button class="btn" data-action="close">Done</button></div>`, 'pair');
    $('#copy-init').onclick = () => copy(init); $('#copy-pair').onclick = () => copy(command);
  } catch (error) { modalError(error); }
}

function delegate(workerId) {
  const workers = workspace.workers.filter(w => !w.revoked && (w.tasks || []).length > 0);
  if (!workers.length) { openModal('<h2 id="modal-title">A worker is needed first</h2><p>Pair a machine with at least one configured task before delegating work.</p><button class="btn primary" data-action="pair">Pair a worker →</button>'); return; }
  openModal(`<h2 id="modal-title">Delegate a task</h2><p>Local tasks use the worker’s checkout. Snapshot tasks use verified source uploaded with the Sharebox CLI. The worker owner chooses execution mode.</p><form id="job-form"><label for="target-worker">Worker</label><select id="target-worker">${workers.map(w => `<option value="${escapeHTML(w.id)}" ${w.id === workerId ? 'selected' : ''}>${escapeHTML(w.name)}${online(w) ? '' : ' (offline)'}</option>`).join('')}</select><label for="target-task">Allowed task</label><select id="target-task"></select><p class="input-help" id="task-description"></p><div id="snapshot-fields" class="hidden"><label for="snapshot-id">Uploaded snapshot ID</label><input id="snapshot-id" pattern="[a-f0-9]{64}" maxlength="64" spellcheck="false" autocomplete="off"><p class="input-help">Paste the SHA-256 snapshot ID returned by <code>sharebox snapshot upload</code>. Only tasks explicitly enabled by the worker owner accept uploaded source.</p></div><div id="worker-offline" class="alert warning hidden">This worker is offline. Start this worker before submitting.</div>${errorBox()}<div class="actions"><button type="button" class="btn" data-action="close">Cancel</button><button type="submit" class="btn primary">Delegate task →</button></div></form>`, 'delegate');
  const updateTasks = () => { const worker = workers.find(w => w.id === $('#target-worker').value); $('#target-task').innerHTML = worker.tasks.map(t => `<option value="${escapeHTML(t.name)}">${escapeHTML(t.name)}</option>`).join(''); $('#worker-offline').classList.toggle('hidden', online(worker)); updateDescription(); };
  const updateDescription = () => { const worker = workers.find(w => w.id === $('#target-worker').value); const task = worker.tasks.find(t => t.name === $('#target-task').value); $('#task-description').textContent = (task?.description || 'Runs the command configured by this machine’s owner.') + (task?.snapshot ? ` Snapshot execution: ${task.execution || 'owner configured'}.` : ' Uses the worker’s existing checkout.'); $('#snapshot-fields').classList.toggle('hidden', !task?.snapshot); $('#snapshot-id').disabled = !task?.snapshot; $('#snapshot-id').required = !!task?.snapshot; };
  $('#target-worker').onchange = updateTasks; $('#target-task').onchange = updateDescription; updateTasks();
  $('#job-form').onsubmit = async event => { event.preventDefault(); const button = $('#job-form button[type="submit"]'); button.disabled = true; try { const job = await api('/api/jobs', { method: 'POST', body: { workerId: $('#target-worker').value, task: $('#target-task').value, ...(!$('#snapshot-id').disabled ? { snapshotId: $('#snapshot-id').value.trim() } : {}) } }); closeModal(); notify('Task submitted to the worker queue.'); await refresh(); if (job.id) await jobDetails(job.id); } catch (error) { modalError(error); } finally { if (button.isConnected) button.disabled = false; } };
}

function connectMcp() {
  openModal(`<h2 id="modal-title">Connect your LLM agent</h2><p>Create a dedicated bearer token for a client that supports Streamable HTTP MCP with authorization headers.</p><form id="token-form"><label for="token-name">Connection name</label><input id="token-name" required maxlength="80" placeholder="Windows coding agent" autocomplete="off"><label for="mcp-address">Master address for this agent</label><select id="mcp-address"><option value="${escapeHTML(location.origin)}">Dashboard address · ${escapeHTML(location.origin)}</option>${selectedLanUrl && selectedLanUrl !== location.origin ? `<option value="${escapeHTML(selectedLanUrl)}">Selected LAN address · ${escapeHTML(selectedLanUrl)}</option>` : ''}</select><p class="input-help">For an agent on another computer, choose the selected LAN address. A localhost address works only on the master’s own machine.</p>${errorBox()}<div class="actions"><button class="btn" type="button" data-action="close">Cancel</button><button class="btn primary" type="submit">Create MCP token →</button></div></form><p class="input-help">Your client must be able to reach this local master. A cloud-hosted LLM client cannot directly reach a private LAN address.</p>`, 'token');
  $('#token-form').onsubmit = async event => {
    event.preventDefault(); const button = $('#token-form button[type="submit"]'); button.disabled = true;
    try {
      const name = $('#token-name').value.trim(); if (!name) throw new Error('Enter a connection name.');
      const endpoint = `${$('#mcp-address').value}/mcp`;
      const result = await api('/api/tokens', { method: 'POST', body: { name } });
      if (modalKind !== 'token') return;
      const config = JSON.stringify({ mcpServers: { sharebox: { type: 'http', url: endpoint, headers: { Authorization: `Bearer ${result.token}` } } } }, null, 2);
      openModal(`<h2 id="modal-title">Your agent’s connection is ready</h2><p>Copy the setup command into a terminal on the machine running your agent. Your address and token are already filled in.</p><label for="mcp-client">Client and terminal</label><select id="mcp-client"><option value="codex-powershell">Codex · Windows PowerShell</option><option value="codex-shell">Codex · macOS / Linux (bash or zsh)</option><option value="claude-powershell">Claude Code · Windows PowerShell</option><option value="claude-shell">Claude Code · macOS / Linux</option><option value="json">Other client · JSON configuration</option></select><div class="command-label"><strong id="setup-label">Setup command</strong><button class="text-btn" id="copy-setup">Copy command</button></div><pre class="command" id="mcp-setup" tabindex="0"></pre><p class="input-help" id="setup-help"></p><p class="input-help">If your agent runs on another machine, use the master’s LAN IP instead of localhost. These commands contain a private token; do not commit or share them publicly.</p><details><summary>Token and JSON configuration</summary><p>Save this token now. It is shown only once.</p><div class="command-label"><strong>Bearer token</strong><button class="text-btn" id="copy-token">Copy token</button></div><div class="secret">${escapeHTML(result.token)}</div><div class="command-label"><strong>Example client configuration</strong><button class="text-btn" id="copy-config">Copy config</button></div><pre class="command" id="mcp-config">${escapeHTML(config)}</pre></details><p>The agent can discover workers, submit allowed tasks, inspect jobs and logs, and cancel jobs. Revoke this token from MCP connections to remove its access.</p><div class="actions"><button class="btn primary" data-action="close">I’ve saved the token</button></div>`, 'token-created');
      const select = $('#mcp-client');
      select.value = /Windows/i.test(navigator.userAgent) ? 'codex-powershell' : 'codex-shell';
      const updateSetup = () => {
        const client = select.value;
        const powershell = client.endsWith('powershell');
        const quote = powershell ? value => `'${value.replaceAll("'", "''")}'` : shellQuote;
        let command;
        let help;
        if (client.startsWith('codex-')) {
          const token = quote(result.token);
          command = `${powershell ? `[Environment]::SetEnvironmentVariable('SHAREBOX_MCP_TOKEN', ${token}, 'User')\n$env:SHAREBOX_MCP_TOKEN = ` : 'export SHAREBOX_MCP_TOKEN='}${token}\ncodex mcp add sharebox --url ${quote(endpoint)} --bearer-token-env-var SHAREBOX_MCP_TOKEN\ncodex`;
          help = powershell ? 'This saves the token in your Windows user environment and the connection in your Codex user configuration, then opens Codex. Exit an already running Codex session first. Restart other terminals and desktop apps to pick up the saved token. Use /mcp to check the connection.' : 'This registers Sharebox in your Codex user configuration, then opens Codex. The token lasts for this terminal session: repeat the export line before starting Codex in a new terminal. Exit an already running Codex session first and run this command in its terminal. Use /mcp to check the connection.';
        } else if (client.startsWith('claude-')) {
          const token = quote(result.token);
          const environment = powershell ? `[Environment]::SetEnvironmentVariable('SHAREBOX_MCP_TOKEN', ${token}, 'User')\n$env:SHAREBOX_MCP_TOKEN = ${token}` : `export SHAREBOX_MCP_TOKEN=${token}`;
          command = `${environment}\nclaude mcp add --transport http --scope user sharebox ${quote(endpoint)} --header ${quote(`Authorization: Bearer ${result.token}`)}`;
          help = 'This saves the MCP connection in Claude Code and sets SHAREBOX_MCP_TOKEN for the local snapshot uploader. ' + (powershell ? 'The token is saved in your Windows user environment; restart other terminals to pick it up. ' : 'Repeat the export line before uploading from a new terminal. ') + 'Restart Claude Code, then use /mcp to check the connection. If Sharebox is already registered, remove that entry with claude mcp remove sharebox before adding the replacement.';
        } else {
          command = config;
          help = 'Merge this entry into your client’s MCP configuration. JSON formats vary by client; Codex uses TOML, so choose a Codex command above for Codex.';
        }
        $('#mcp-setup').textContent = command;
        $('#setup-help').textContent = help;
        $('#setup-label').textContent = client === 'json' ? 'Client configuration' : 'Setup command';
        $('#copy-setup').textContent = client === 'json' ? 'Copy config' : 'Copy command';
        $('#copy-setup').onclick = () => copy(command);
      };
      select.onchange = updateSetup; updateSetup();
      $('#copy-token').onclick = () => copy(result.token); $('#copy-config').onclick = () => copy(config); await refresh();
    } catch (error) { modalError(error); } finally { if (button.isConnected) button.disabled = false; }
  };
}

async function jobDetails(id) {
  openModal(`<h2 id="modal-title">Job details</h2><p>Loading execution status…</p>${errorBox()}`, 'job'); selectedJobId = id;
  await updateJob();
}
async function updateJob() {
  const id = selectedJobId; if (!id) return;
  try {
    const job = await api(`/api/jobs/${encodeURIComponent(id)}`); if (selectedJobId !== id || !$('#modal').open) return;
    const priorLog = $('#job-output'); const atBottom = !priorLog || priorLog.scrollHeight - priorLog.scrollTop - priorLog.clientHeight < 35; const scroll = priorLog?.scrollTop || 0;
    $('#modal-content').innerHTML = `<h2 id="modal-title">${escapeHTML(job.task)}</h2><p>${escapeHTML(job.id)}</p><div class="job-summary"><div><small>Status</small><span class="status ${statusClass(job.status)}">${escapeHTML(job.status)}</span></div><div><small>Worker</small><strong>${escapeHTML(workerName(job.workerId))}</strong></div><div><small>Queued</small><strong>${escapeHTML(relativeTime(job.createdAt))}</strong></div><div><small>Exit code</small><strong>${job.exitCode === null || job.exitCode === undefined ? '—' : escapeHTML(job.exitCode)}</strong></div></div><div class="log-label"><span>Execution output</span><button class="text-btn" id="copy-log">Copy log</button></div><pre class="log-output" id="job-output"></pre><p class="input-help">${escapeHTML(job.execution || 'native')} · ${job.snapshotId ? `Verified snapshot <code class="snapshot-id">${escapeHTML(job.snapshotId)}</code>` : 'worker’s local checkout'}. Logs may be truncated by the worker’s configured limit.</p>${errorBox()}<div class="actions">${terminal(job) ? '' : '<button class="btn danger" id="cancel-job">Cancel job</button>'}<button class="btn" data-action="close">Close</button></div>`;
    const log = typeof job.log === 'string' ? job.log : Array.isArray(job.log) ? job.log.join('\n') : '';
    $('#job-output').textContent = log || (job.status === 'queued' ? 'Waiting for the worker to accept this job…' : 'No output captured yet.');
    $('#job-output').scrollTop = atBottom ? $('#job-output').scrollHeight : scroll;
    $('#copy-log').onclick = () => copy(log);
    if ($('#cancel-job')) $('#cancel-job').onclick = async () => { const button = $('#cancel-job'); button.disabled = true; try { await api(`/api/jobs/${encodeURIComponent(id)}/cancel`, { method: 'POST', body: {} }); notify('Cancellation requested.'); await refresh(); await updateJob(); } catch (error) { modalError(error); } finally { if (button.isConnected) button.disabled = false; } };
  } catch (error) { if (selectedJobId === id) modalError(error); }
}

function confirmRevoke(type, id) {
  const item = (type === 'worker' ? workspace.workers : workspace.tokens).find(i => i.id === id);
  openModal(`<h2 id="modal-title">Revoke ${type === 'worker' ? 'worker' : 'MCP token'}?</h2><p>${escapeHTML(item?.name || id)} will lose access to this workspace. ${type === 'worker' ? 'To connect this machine again, create a new pairing code.' : 'Agents using this token will need a new token to reconnect.'}</p>${errorBox()}<div class="actions"><button class="btn" data-action="close">Keep access</button><button class="btn danger" id="confirm-revoke">Revoke access</button></div>`, 'confirm');
  $('#confirm-revoke').onclick = async () => { const button = $('#confirm-revoke'); button.disabled = true; try { await api(type === 'worker' ? `/api/workers/${encodeURIComponent(id)}/revoke` : `/api/tokens/${encodeURIComponent(id)}`, type === 'worker' ? { method: 'POST', body: {} } : { method: 'DELETE' }); closeModal(); notify('Access revoked.'); await refresh(); } catch (error) { modalError(error); } finally { if (button.isConnected) button.disabled = false; } };
}

document.addEventListener('click', event => {
  const button = event.target.closest('[data-action]'); if (!button) return;
  const { action, id } = button.dataset;
  if (action === 'pair') void pairing();
  else if (action === 'new-job') delegate();
  else if (action === 'worker-job') delegate(id);
  else if (action === 'connect-mcp') connectMcp();
  else if (action === 'job-details') void jobDetails(id);
  else if (action === 'revoke-worker') confirmRevoke('worker', id);
  else if (action === 'revoke-token') confirmRevoke('token', id);
  else if (action === 'close') closeModal();
});
$('#login-form').onsubmit = async event => { event.preventDefault(); const button = $('#login-form button[type="submit"]'); button.disabled = true; showError($('#login-error'), ''); setToken($('#admin-token').value.trim()); try { await connect(); $('#admin-token').value = ''; } catch (error) { setToken(''); showError($('#login-error'), error.message); } finally { button.disabled = false; } };
$('#logout').onclick = () => disconnect();
$('#close-modal').onclick = closeModal;
$('#modal').addEventListener('close', () => {
  // A queued close event can arrive after an async action opens the next dialog.
  // Never erase that new dialog or stop its live job polling.
  if ($('#modal').open) return;
  selectedJobId = null; modalKind = ''; $('#modal-content').replaceChildren();
});
$('#modal').addEventListener('click', event => { if (event.target !== $('#modal')) return; const r = $('#modal').getBoundingClientRect(); if (event.clientX < r.left || event.clientX > r.right || event.clientY < r.top || event.clientY > r.bottom) closeModal(); });
$('#job-filter').onchange = renderJobs;
$('#refresh-network').onclick = () => refreshNetwork();
$('#lan-address').onchange = () => { selectedLanUrl = $('#lan-address').value; };
$('#copy-lan-address').onclick = () => { if (selectedLanUrl) void copy(selectedLanUrl); };
$('#copy-endpoint').onclick = () => copy(`${location.origin}/mcp`);
$$('nav a').forEach(a => a.onclick = () => { $$('nav a').forEach(n => n.classList.remove('active')); a.classList.add('active'); $('#breadcrumb').textContent = a.textContent.trim().replace(/^[^A-Za-z]+/, ''); });
if (adminToken) connect().catch(error => disconnect(error.message)); else disconnect();
