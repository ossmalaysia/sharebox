/** Exercise real local worker onboarding without touching installed services. */
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
const temporary = await mkdtemp(path.join(os.tmpdir(), 'sharebox-setup-ui-'));
const children = [];
const secrets = new Set();
const errors = [];
let browser;
let page;
let failed = false;
const redact = input => {
  let text = String(input);
  for (const secret of secrets) if (secret) text = text.replaceAll(secret, '[REDACTED]');
  return text.replace(/\b[a-f0-9]{64}\b/gi, '[REDACTED]');
};
function start(name, args, env = process.env) {
  const child = spawn(binary, args, { env, stdio: ['ignore', 'pipe', 'pipe'] });
  const record = { name, child, log: '', error: null };
  const capture = value => { record.log = (record.log + value.toString()).slice(-100_000); };
  child.stdout.on('data', capture); child.stderr.on('data', capture);
  child.on('error', error => { record.error = error; });
  children.push(record); return record;
}
async function until(check, description, timeout = 30_000) {
  const end = Date.now() + timeout;
  let last;
  while (Date.now() < end) {
    try { const value = await check(); if (value) return value; } catch (error) { last = error; }
    await delay(100);
  }
  throw new Error(`Timed out: ${description}${last ? ` (${last.message})` : ''}`);
}
async function stop(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const closed = new Promise(resolve => child.once('exit', resolve));
  child.kill('SIGTERM'); await Promise.race([closed, delay(3000)]);
  if (child.exitCode === null && child.signalCode === null) {
    child.kill('SIGKILL'); await Promise.race([closed, delay(3000)]);
  }
}
async function unusedPort() {
  const server = net.createServer();
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  const { port } = server.address(); await new Promise(resolve => server.close(resolve)); return port;
}
async function setupAddress(process) {
  return until(() => {
    if (process.error) throw process.error;
    if (process.child.exitCode !== null) throw new Error(`Setup exited: ${redact(process.log)}`);
    const match = /Worker setup: (http:\/\/127\.0\.0\.1:\d+)\/#token=([a-f0-9]+)/.exec(process.log);
    if (!match) return null;
    secrets.add(match[2]); return { origin: match[1], token: match[2], url: `${match[1]}/#token=${match[2]}` };
  }, 'private setup URL');
}
async function state(setup) {
  const response = await fetch(`${setup.origin}/api/setup/status`, { headers: { Authorization: `Bearer ${setup.token}` }, signal: AbortSignal.timeout(15_000) });
  assert.equal(response.status, 200); return response.json();
}
async function screenshot(name) {
  await page.screenshot({ path: path.join(output, name), fullPage: true, mask: [page.locator('#registration-link')] });
}
try {
  await mkdir(output, { recursive: true });
  const port = await unusedPort();
  const origin = `http://127.0.0.1:${port}`;
  const masterPath = path.join(temporary, 'master');
  start('master', ['master', '--bind', `127.0.0.1:${port}`, '--data', masterPath]);
  await until(async () => (await fetch(`${origin}/health`, { signal: AbortSignal.timeout(1500) })).ok, 'master health');
  const admin = (await readFile(path.join(masterPath, 'admin-token'), 'utf8')).trim(); secrets.add(admin);
  const codeResponse = await fetch(`${origin}/api/pairing`, { method: 'POST', headers: { Authorization: `Bearer ${admin}`, 'Content-Type': 'application/json' }, body: '{}' });
  assert.equal(codeResponse.status, 201);
  const { code } = await codeResponse.json(); secrets.add(code);
  const workspace = path.join(temporary, 'workspaces'); await mkdir(workspace);
  const customTask = { description: 'Existing owner task', argv: [process.execPath, '--version'], cwd: workspace, timeoutSeconds: 30 };
  const configPath = path.join(temporary, 'worker.json');
  await writeFile(configPath, JSON.stringify({ name: 'Existing local worker', server: origin, tasks: { 'custom-local': customTask } }), { mode: 0o600 });
  const fakePath = path.join(temporary, 'docker-unavailable'); await mkdir(fakePath);
  await writeFile(path.join(fakePath, process.platform === 'win32' ? 'docker.exe' : 'docker'), process.platform === 'win32' ? 'Not an executable' : '#!/bin/sh\nexit 127\n', { mode: 0o755 });
  const noDocker = start('setup without Docker', ['setup', '--config', configPath, '--no-open'], { ...process.env, PATH: fakePath + path.delimiter + process.env.PATH });
  const setup = await setupAddress(noDocker);
  browser = await chromium.launch({ headless: true });
  page = await browser.newPage({ viewport: { width: 1200, height: 1000 } });
  page.setDefaultTimeout(20_000);
  page.on('pageerror', error => errors.push(error.message));
  const requestUrls = [];
  page.on('request', request => requestUrls.push(request.url()));
  await page.goto(setup.url, { waitUntil: 'domcontentloaded' });
  await page.getByRole('heading', { name: 'Connect your worker', exact: true }).waitFor();
  await page.getByRole('button', { name: 'Check Docker', exact: true }).click();
  await until(async () => (await state(setup)).capabilities.docker.checked === true, 'Docker absence detection');
  await page.getByRole('button', { name: 'Refresh status', exact: true }).click();
  await page.locator('#docker-status').filter({ hasText: /not installed|not available|not on/ }).waitFor();
  assert.equal(new URL(page.url()).hash, '', 'Setup token must leave the address bar');
  assert.ok(requestUrls.every(url => !url.includes(setup.token)), 'Setup token is never in a request URL');
  assert.equal(await page.locator('#mode option[value="docker"]').isDisabled(), true, 'Unavailable Docker cannot be enabled');
  assert.equal(await page.getByLabel('Execution mode', { exact: true }).inputValue(), '', 'No automatic native fallback');
  await screenshot('worker-setup-unregistered.png');

  await page.getByLabel('Registration link', { exact: true }).fill(`${origin}/#pair=${code}`);
  await page.getByLabel('Worker name', { exact: true }).fill('Setup browser worker');
  await page.getByLabel('Test preset', { exact: true }).selectOption('node-test');
  await page.getByLabel('Execution mode', { exact: true }).selectOption('native');
  await page.locator('#native-warning').waitFor({ state: 'visible' });
  await page.getByText('Advanced workspace', { exact: true }).click();
  await page.getByLabel('Disposable workspace folder', { exact: true }).fill(workspace);
  await page.getByText('Advanced workspace', { exact: true }).click();
  await page.getByLabel('I allow this worker to execute uploaded source using the selected preset and mode.', { exact: true }).check();
  await page.getByRole('button', { name: 'Register worker & save task', exact: true }).click();
  await page.locator('#registration-status').filter({ hasText: 'Setup browser worker · Registered' }).waitFor();
  await page.getByRole('button', { name: 'Save snapshot task', exact: true }).waitFor();
  await page.locator('#tasks').getByText('custom-local', { exact: true }).waitFor();
  await page.locator('#tasks').getByText('sharebox-tests', { exact: true }).waitFor();
  assert.equal(await page.locator('#registration-fields').isVisible(), false);
  assert.equal(await page.locator('#consent').isChecked(), false, 'Consent resets after saving');
  let config = JSON.parse(await readFile(configPath, 'utf8')); secrets.add(config.token);
  assert.ok(config.token && config.workerId); assert.deepEqual(config.tasks['custom-local'], customTask);
  assert.equal(config.tasks['sharebox-tests'].snapshot.mode, 'native');
  assert.ok(!(await page.locator('body').innerText()).includes(config.token), 'Never render worker credential');
  const workspaceState = await (await fetch(`${origin}/api/workspace`, { headers: { Authorization: `Bearer ${admin}` } })).json();
  const registered = workspaceState.workers.find(worker => worker.id === config.workerId);
  assert.equal(registered.lastPollAt, 0, 'Registration does not start or claim work');
  await screenshot('worker-setup-registered.png');
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), 'Setup page must fit mobile width');
  await screenshot('worker-setup-mobile.png');
  console.log('PASS: real setup UI registration, explicit native consent, preserved custom task, private tokens, Docker unavailable disabled, responsive status');

  const dockerImage = process.env.SHAREBOX_SETUP_DOCKER_IMAGE;
  if (dockerImage) {
    await stop(noDocker.child);
    const dockerSetup = await setupAddress(start('setup with Docker', ['setup', '--config', configPath, '--no-open']));
    await page.setViewportSize({ width: 1200, height: 1000 });
    await page.goto(dockerSetup.url, { waitUntil: 'domcontentloaded' });
    await page.getByRole('button', { name: 'Check Docker', exact: true }).click();
    await until(async () => (await state(dockerSetup)).capabilities.docker.ready, 'real Docker ready');
    await page.getByRole('button', { name: 'Refresh status', exact: true }).click();
    await until(async () => !(await page.locator('#mode option[value="docker"]').isDisabled()), 'Docker choice enabled');
    await page.getByLabel('Execution mode', { exact: true }).selectOption('docker');
    await page.getByLabel('Docker image', { exact: true }).fill(dockerImage);
    await page.getByLabel('I allow this worker to execute uploaded source using the selected preset and mode.', { exact: true }).check();
    await page.getByRole('button', { name: 'Save snapshot task', exact: true }).click();
    await page.locator('#tasks').getByText('Docker sandbox · uploaded snapshot', { exact: true }).waitFor();
    const changed = JSON.parse(await readFile(configPath, 'utf8'));
    assert.equal(changed.token, config.token); assert.equal(changed.workerId, config.workerId);
    assert.deepEqual(changed.tasks['custom-local'], customTask);
    assert.equal(changed.tasks['sharebox-tests'].snapshot.mode, 'docker');
    assert.equal(changed.tasks['sharebox-tests'].snapshot.network, 'none');
    assert.equal(changed.tasks['sharebox-tests'].snapshot.image, dockerImage);
    await screenshot('worker-setup-docker.png');
    console.log('PASS: real Docker-ready UI enables an owner-selected sandbox task without changing custom tasks, pairing, or network permission');
  } else {
    console.log('SKIP optional Docker-ready UI save: set SHAREBOX_SETUP_DOCKER_IMAGE to a preloaded image');
  }
  assert.deepEqual(errors, [], 'No uncaught setup JavaScript errors');
} catch (error) {
  failed = true; console.error(redact(error.stack || error));
  if (page && !page.isClosed()) {
    try { await screenshot('worker-setup-failure.png'); } catch {}
    try { await writeFile(path.join(output, 'worker-setup-failure.txt'), redact(await page.locator('body').innerText({ timeout: 3000 }))); } catch {}
  }
} finally {
  if (browser) await browser.close().catch(() => {});
  for (const record of children.toReversed()) await stop(record.child);
  await mkdir(output, { recursive: true });
  await writeFile(path.join(output, 'worker-setup-ui.log'), redact([failed ? 'FAILED' : 'PASSED', ...errors, ...children.map(record => `${record.name}:\n${record.log}`)].join('\n')));
  await rm(temporary, { recursive: true, force: true });
}
if (failed) process.exitCode = 1;
