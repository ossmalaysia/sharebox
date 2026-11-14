/** Real browser smoke test. Run after cargo build with Playwright Chromium installed. */
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { chromium } from 'playwright';

const binary = path.resolve(process.env.SHAREBOX_BIN || `target/debug/sharebox${process.platform === 'win32' ? '.exe' : ''}`);
const output = path.resolve(process.env.SHAREBOX_UI_OUTPUT || 'test-output');
const temporary = await mkdtemp(path.join(os.tmpdir(), 'sharebox-ui-'));
const processes = [];
const browserMessages = [];
const sensitive = new Set();
let browser;
let page;
let failed = false;
let origin;
let admin;

function redact(value) {
  let text = String(value);
  for (const secret of sensitive) if (secret) text = text.replaceAll(secret, '[REDACTED]');
  return text
    .replace(/(admin(?:istrator)? token\s*[:=]\s*)\S+/gi, '$1[REDACTED]')
    .replace(/\b[0-9a-f]{64}\b/gi, '[REDACTED]');
}

function start(label, args) {
  const child = spawn(binary, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  const record = { label, child, log: '', error: null };
  const capture = chunk => { record.log = (record.log + chunk.toString()).slice(-200_000); };
  child.stdout.on('data', capture);
  child.stderr.on('data', capture);
  child.on('error', error => { record.error = error; });
  processes.push(record);
  return record;
}

async function until(check, description, timeout = 30_000) {
  const end = Date.now() + timeout;
  let lastError;
  while (Date.now() < end) {
    try { const result = await check(); if (result) return result; } catch (error) { lastError = error; }
    await delay(150);
  }
  throw new Error(`Timed out: ${description}${lastError ? ` (${lastError.message})` : ''}`);
}

async function unusedPort() {
  const server = net.createServer();
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  const { port } = server.address();
  await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  return port;
}

async function stop(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise(resolve => child.once('exit', resolve));
  child.kill('SIGTERM');
  await Promise.race([exited, delay(3000)]);
  if (child.exitCode === null && child.signalCode === null) {
    child.kill('SIGKILL');
    await Promise.race([exited, delay(3000)]);
  }
}

try {
  await mkdir(output, { recursive: true });
  const port = await unusedPort();
  origin = `http://127.0.0.1:${port}`;
  const state = path.join(temporary, 'state');
  const master = start('master', ['master', '--bind', `127.0.0.1:${port}`, '--data', state]);
  await until(async () => {
    if (master.error) throw master.error;
    if (master.child.exitCode !== null) throw new Error(`Master exited: ${redact(master.log)}`);
    const response = await fetch(`${origin}/health`, { signal: AbortSignal.timeout(1500) });
    return response.ok;
  }, 'master health');
  admin = (await readFile(path.join(state, 'admin-token'), 'utf8')).trim();
  sensitive.add(admin);

  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  page = await context.newPage();
  page.setDefaultTimeout(15_000);
  page.on('pageerror', error => browserMessages.push(`PAGE ERROR: ${error.message}`));
  page.on('console', message => { if (['error', 'warning'].includes(message.type())) browserMessages.push(`${message.type()}: ${message.text()}`); });

  await page.goto(origin, { waitUntil: 'domcontentloaded' });
  await page.getByRole('heading', { name: 'Connect to the master' }).waitFor();
  await page.getByLabel('Master admin token').fill(admin);
  await page.getByRole('button', { name: 'Open workspace' }).click();
  await page.getByRole('heading', { name: 'More machines. One agent.' }).waitFor();
  await page.getByRole('heading', { name: 'Your first worker is one command away' }).waitFor();
  assert.equal(await page.locator('#page-error').isVisible(), false, 'Workspace API should connect successfully');
  console.log('PASS: dashboard login and empty workspace');

  await page.locator('#workers').getByRole('button', { name: '+ Pair worker', exact: true }).click();
  await page.getByRole('heading', { name: 'Bring another machine online' }).waitFor();
  const commands = await page.locator('#modal .command').allTextContents();
  const pairingCommand = commands.find(command => command.includes(' worker '));
  assert.ok(pairingCommand, 'Pairing dialog should display a worker command');
  const code = pairingCommand.match(/--code\s+'([^']+)'/)?.[1];
  assert.ok(code, 'Worker command should contain a one-time pairing code');
  assert.ok(pairingCommand.includes(origin), 'Pairing command must point to this master');
  sensitive.add(code);
  await page.getByRole('button', { name: 'Done', exact: true }).click();

  const configPath = path.join(temporary, 'worker.json');
  const initializer = start('init', ['init', '--config', configPath]);
  await until(() => initializer.error ? Promise.reject(initializer.error) : initializer.child.exitCode !== null, 'worker initialization');
  assert.equal(initializer.child.exitCode, 0, redact(initializer.log));
  const config = JSON.parse(await readFile(configPath, 'utf8'));
  config.name = 'UI test worker';
  config.tasks['node-smoke'].argv[0] = process.execPath;
  await writeFile(configPath, JSON.stringify(config, null, 2), { mode: 0o600 });
  const worker = start('worker', ['worker', '--server', origin, '--code', code, '--config', configPath]);
  const workerCard = page.locator('#worker-list article').filter({ has: page.getByRole('heading', { name: 'UI test worker', exact: true }) });
  await workerCard.getByText('Online', { exact: true }).waitFor({ timeout: 30_000 });
  assert.equal(worker.child.exitCode, null, 'Worker should remain running');
  sensitive.add(JSON.parse(await readFile(configPath, 'utf8')).token);
  console.log('PASS: UI-generated code pairs a real Rust worker');

  await workerCard.getByRole('button', { name: 'Run task' }).click();
  await page.getByRole('heading', { name: 'Delegate a task', exact: true }).waitFor();
  await page.getByLabel('Allowed task').selectOption('node-smoke');
  await page.locator('#job-form').getByRole('button', { name: 'Delegate task' }).click();
  await page.locator('#modal-title').filter({ hasText: 'node-smoke' }).waitFor();
  await page.locator('#modal .status').waitFor();
  console.log(`Job details opened with status: ${await page.locator('#modal .status').innerText()}`);
  await page.locator('#modal .status').filter({ hasText: /^passed$/ }).waitFor({ timeout: 45_000 });
  await page.locator('#job-output').filter({ hasText: 'remote Node test' }).waitFor();
  assert.match(await page.locator('#job-output').innerText(), /pass 1|tests 1/);
  await page.locator('#modal').getByRole('button', { name: 'Close', exact: true }).click();
  await page.locator('#job-list tr').filter({ hasText: 'node-smoke' }).getByText('passed', { exact: true }).waitFor();
  await page.screenshot({ path: path.join(output, 'dashboard.png'), fullPage: true });
  console.log('PASS: UI task submission executes Node test and displays passing logs');

  await page.getByRole('button', { name: 'Connect MCP ↗', exact: true }).click();
  await page.getByLabel('Connection name').fill('UI smoke agent');
  await page.getByRole('button', { name: 'Create MCP token' }).click();
  await page.getByRole('heading', { name: 'Your agent’s connection is ready' }).waitFor();
  await page.locator('#modal summary').click();
  const token = (await page.locator('#modal .secret').innerText()).trim();
  sensitive.add(token);
  assert.ok(token.length >= 32, 'MCP token should contain sufficient random material');
  const clientConfig = JSON.parse(await page.locator('#mcp-config').innerText());
  assert.equal(clientConfig.mcpServers.sharebox.url, `${origin}/mcp`);
  assert.equal(clientConfig.mcpServers.sharebox.headers.Authorization, `Bearer ${token}`);
  assert.equal(clientConfig.mcpServers.sharebox.type, 'http');
  await page.getByLabel('Client and terminal').selectOption('codex-powershell');
  const windowsSetup = await page.locator('#mcp-setup').innerText();
  assert.ok(windowsSetup.includes(`[Environment]::SetEnvironmentVariable('SHAREBOX_MCP_TOKEN', '${token}', 'User')`));
  assert.ok(windowsSetup.includes(`$env:SHAREBOX_MCP_TOKEN = '${token}'`));
  assert.ok(windowsSetup.includes(`codex mcp add sharebox --url '${origin}/mcp' --bearer-token-env-var SHAREBOX_MCP_TOKEN`));
  await page.getByLabel('Client and terminal').selectOption('codex-shell');
  const shellSetup = await page.locator('#mcp-setup').innerText();
  assert.ok(shellSetup.startsWith(`export SHAREBOX_MCP_TOKEN='${token}'\n`));
  assert.ok(shellSetup.endsWith('\ncodex'));
  // Force the plain HTTP LAN path: modern clipboard unavailable, legacy copy succeeds.
  await page.evaluate(() => {
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: undefined });
    document.execCommand = command => {
      if (command !== 'copy') return false;
      window.copiedSetup = document.activeElement.value;
      return true;
    };
  });
  await page.locator('#copy-setup').click();
  assert.equal(await page.evaluate(() => window.copiedSetup), shellSetup);
  assert.equal(await page.locator('.clipboard-fallback').count(), 0);
  // A browser that blocks both methods should leave selected text for manual copy.
  await page.evaluate(() => { document.execCommand = () => false; });
  await page.locator('#copy-setup').click();
  assert.equal(await page.locator('.clipboard-fallback.manual-copy').inputValue(), shellSetup);
  await page.getByLabel('Client and terminal').selectOption('claude-powershell');
  assert.equal(await page.locator('#mcp-setup').innerText(), `claude mcp add --transport http --scope user sharebox '${origin}/mcp' --header 'Authorization: Bearer ${token}'`);
  await page.getByLabel('Client and terminal').selectOption('claude-shell');
  assert.equal(await page.locator('#mcp-setup').innerText(), `claude mcp add --transport http --scope user sharebox '${origin}/mcp' --header 'Authorization: Bearer ${token}'`);
  await page.getByLabel('Client and terminal').selectOption('json');
  assert.deepEqual(JSON.parse(await page.locator('#mcp-setup').innerText()), clientConfig);

  const probe = await fetch(`${origin}/mcp`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/list', params: {} }),
    signal: AbortSignal.timeout(5000),
  });
  assert.equal(probe.status, 200, 'UI-issued token must authenticate an MCP request');
  assert.ok((await probe.json()).result.tools.some(tool => tool.name === 'submit_job'));
  await page.getByRole('button', { name: 'I’ve saved the token' }).click();
  const tokenRow = page.locator('#token-list .token-row').filter({ hasText: 'UI smoke agent' });
  await tokenRow.getByRole('button', { name: 'Revoke', exact: true }).click();
  await page.getByRole('heading', { name: 'Revoke MCP token?' }).waitFor();
  await page.locator('#modal').getByRole('button', { name: 'Revoke access', exact: true }).click();
  await tokenRow.waitFor({ state: 'detached' });
  const revoked = await fetch(`${origin}/mcp`, { method: 'POST', headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 2, method: 'ping' }), signal: AbortSignal.timeout(5000) });
  assert.equal(revoked.status, 401, 'Revoked UI token must no longer authenticate');
  console.log('PASS: MCP token creation, client configuration, authorization and revocation');

  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole('heading', { name: 'More machines. One agent.' }).waitFor();
  await page.screenshot({ path: path.join(output, 'dashboard-mobile.png'), fullPage: true });
  const mobileLayout = await page.evaluate(() => ({
    viewport: innerWidth,
    documentWidth: document.documentElement.scrollWidth,
    overflowingElements: [...document.querySelectorAll('body *')].flatMap(element => {
      const rect = element.getBoundingClientRect();
      if (!rect.width || !rect.height || (rect.right <= innerWidth + 1 && rect.left >= -1)) return [];
      const style = getComputedStyle(element);
      return [{ tag: element.tagName, id: element.id, className: element.className, left: rect.left, right: rect.right, width: rect.width, position: style.position, overflowX: style.overflowX }];
    }).slice(0, 80),
  }));
  if (mobileLayout.documentWidth > mobileLayout.viewport + 1) {
    await writeFile(path.join(output, 'ui-overflow.json'), JSON.stringify(mobileLayout, null, 2));
    console.error('Mobile overflow diagnostics:', JSON.stringify(mobileLayout));
  }
  assert.ok(mobileLayout.documentWidth <= mobileLayout.viewport + 1, 'Mobile layout should not overflow horizontally');
  assert.deepEqual(browserMessages.filter(message => message.startsWith('PAGE ERROR:')), [], 'Dashboard should not throw uncaught JavaScript errors');
  console.log('PASS: responsive dashboard and no uncaught JavaScript errors');
} catch (error) {
  failed = true;
  console.error(redact(error.stack || error));
  if (origin && admin) {
    try {
      const response = await fetch(`${origin}/api/workspace`, { headers: { Authorization: `Bearer ${admin}` }, signal: AbortSignal.timeout(3000) });
      const data = await response.json();
      // Explicitly project diagnostics; never write workspace token records or credentials.
      const state = {
        httpStatus: response.status,
        error: typeof data.error === 'string' ? data.error : undefined,
        workers: (data.workers || []).map(({ id, name, lastSeen, revoked }) => ({ id, name, lastSeen, revoked })),
        jobs: (data.jobs || []).map(({ id, task, workerId, status, exitCode, createdAt, updatedAt }) => ({ id, task, workerId, status, exitCode, createdAt, updatedAt })),
      };
      await writeFile(path.join(output, 'ui-state.json'), redact(JSON.stringify(state, null, 2)));
      console.error('Failure workspace state:', redact(JSON.stringify(state)));
    } catch (diagnosticError) { browserMessages.push(`Workspace diagnostics unavailable: ${redact(diagnosticError.message)}`); }
  }
  if (page && !page.isClosed()) {
    try {
      const layout = await page.evaluate(() => ({ viewport: innerWidth, documentWidth: document.documentElement.scrollWidth, overflowingElements: [...document.querySelectorAll('body *')].flatMap(element => { const r = element.getBoundingClientRect(); return r.width && r.height && (r.right > innerWidth + 1 || r.left < -1) ? [{ tag: element.tagName, id: element.id, className: element.className, left: r.left, right: r.right, width: r.width }] : []; }).slice(0, 80) }));
      await writeFile(path.join(output, 'ui-failure-layout.json'), JSON.stringify(layout, null, 2));
    } catch (diagnosticError) { browserMessages.push(`Layout diagnostics unavailable: ${redact(diagnosticError.message)}`); }
    try { await writeFile(path.join(output, 'ui-page.txt'), redact(await page.locator('body').innerText({ timeout: 3000 }))); }
    catch (diagnosticError) { browserMessages.push(`Page diagnostics unavailable: ${redact(diagnosticError.message)}`); }
    try { await page.screenshot({ path: path.join(output, 'ui-failure.png'), fullPage: true, mask: [page.locator('#admin-token'), page.locator('#modal .secret'), page.locator('#modal .command'), page.locator('.clipboard-fallback')] }); } catch { /* Keep the original failure. */ }
  }
} finally {
  if (browser) await browser.close().catch(() => {});
  for (const record of processes.toReversed()) await stop(record.child);
  await mkdir(output, { recursive: true });
  await writeFile(path.join(output, 'ui.log'), redact([
    failed ? 'UI smoke test FAILED' : 'UI smoke test PASSED',
    ...browserMessages,
    ...processes.map(record => `\n--- ${record.label} ---\n${record.log}${record.error ? `\n${record.error.message}` : ''}`),
  ].join('\n')));
  await rm(temporary, { recursive: true, force: true });
}
if (failed) process.exitCode = 1;
