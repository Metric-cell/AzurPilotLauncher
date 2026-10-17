#!/usr/bin/env node
'use strict';

// Execute the shipped document-start source. Rust tests separately exercise the
// real Tauri ACL resolver and Caller policy; this mock only models its rejection
// contract so a raw IPC call cannot accidentally pass this test without a token.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../src/webview_bridge.js'), 'utf8');
const rust = fs.readFileSync(path.join(__dirname, '../src/webview_security.rs'), 'utf8');
const commandSource = fs.readFileSync(path.join(__dirname, '../src/commands.rs'), 'utf8');
const projectRepository = commandSource.match(/pub const PROJECT_REPOSITORY_URL: &str = "([^"]+)";/)[1];
const commands = name => {
  const match = rust.match(new RegExp(`const ${name}: &\\[&str\\] = &\\[([\\s\\S]*?)\\];`));
  assert.ok(match, `missing Rust command matrix ${name}`);
  return [...match[1].matchAll(/"([a-z_]+)"/g)].map(item => item[1]);
};
const tokens = { splash: 's'.repeat(43), error: 'e'.repeat(43), backend: 'b'.repeat(43) };

function boot({
  href = 'http://127.0.0.1:22267/', label = 'main', webviewLabel = label,
  child = false, windows = false, backendNative = true,
  backendOrigin = 'http://127.0.0.1:22267', failProjectOpen = false
} = {}) {
  const policy = {
    splashUrl: windows ? 'http://alas-splash.localhost/' : 'alas-splash://localhost/',
    errorUrl: windows ? 'http://alas-error.localhost/backend' : 'alas-error://localhost/backend',
    backendOrigin, backendNative,
    projectRepositoryLinks: [projectRepository],
    commands: {
      splash: commands('SPLASH_COMMANDS'),
      error: commands('ERROR_COMMANDS'),
      backend: commands('BACKEND_COMMANDS')
    }
  };
  const listeners = new Map();
  const calls = [];
  const nativeCalls = [];
  const alerts = [];
  const location = { href };
  const internal = (url, expected, query) => !url.username && !url.password
    && url.protocol === expected.protocol && url.hostname === expected.hostname
    && url.port === expected.port && url.pathname === expected.pathname
    && (query || !url.search) && !url.hash;
  const currentRole = () => {
    const url = new URL(location.href);
    if (label === 'splash' && internal(url, new URL(policy.splashUrl), false)) return 'splash';
    if (label === 'main' && internal(url, new URL(policy.errorUrl), true)) return 'error';
    if (label === 'main' && backendNative && !url.username && !url.password
        && /^https?:$/.test(url.protocol) && url.origin === backendOrigin) return 'backend';
    return null;
  };
  const rawInvoke = (command, args = {}) => {
    calls.push({ command, args });
    const role = currentRole();
    if (label !== webviewLabel || !role || !policy.commands[role].includes(command)
        || args.launcherToken !== tokens[role]) {
      return Promise.reject(new Error('mock native boundary denied'));
    }
    nativeCalls.push({ command, args });
    if (command === 'open_project_repository' && failProjectOpen) return Promise.reject(new Error('Browser unavailable'));
    return Promise.resolve('accepted');
  };
  class Form {
    submit() { this.submitted = true; }
  }
  const sandbox = {
    URL, Promise, Error, Object, location, HTMLFormElement: Form,
    alert: message => alerts.push(message),
    document: { addEventListener: (type, handler) => listeners.set(type, handler) },
    __TAURI_INTERNALS__: {
      metadata: { currentWindow: { label }, currentWebview: { label: webviewLabel } },
      invoke: rawInvoke
    }
  };
  sandbox.window = sandbox;
  sandbox.top = child ? {} : sandbox;
  const script = source
    .replace('__ALAS_SECURITY_POLICY__', JSON.stringify(policy))
    .replace('__ALAS_SECURITY_TOKENS__', JSON.stringify(tokens));
  assert.ok(!/__ALAS_SECURITY_(?:POLICY|TOKENS)__/.test(script));
  vm.runInNewContext(script, sandbox, { filename: 'webview_bridge.js', timeout: 1000 });
  return { window: sandbox, calls, nativeCalls, alerts, listeners, Form, policy };
}

const denied = promise => assert.rejects(promise, /denied/);

test('真实项目链接点击通过固定命令打开浏览器，保留应用页面', async () => {
  for (const href of [projectRepository, projectRepository + '/']) {
    const context = boot();
    const anchor = {tagName: 'A', href, target: '_blank'};
    const click = {isTrusted: true, button: 0, composedPath: () => [anchor],
      preventDefault() {this.prevented = true;}, stopImmediatePropagation() {this.stopped = true;}};
    context.listeners.get('click')(click);
    assert.equal(click.prevented, true);
    assert.equal(click.stopped, true);
    assert.equal(context.window.location.href, 'http://127.0.0.1:22267/');
    assert.equal(context.nativeCalls.length, 1);
    assert.equal(context.nativeCalls[0].command, 'open_project_repository');
    assert.deepEqual(Object.keys(context.nativeCalls[0].args), ['launcherToken']);
  }
});

test('外部打开拒绝合成点击、其他链接和非后端角色，并显示打开失败', async () => {
  const click = (context, href, override = {}) => context.listeners.get('click')?.({
    isTrusted: true, button: 0, composedPath: () => [{tagName: 'A', href}],
    preventDefault() {}, stopImmediatePropagation() {}, ...override,
  });
  for (const href of ['https://evil.example/', projectRepository + '/issues', projectRepository + '?redirect=elsewhere',
    'https://github.com.evil.example/wess09/AzurPilot', 'https://user@github.com/wess09/AzurPilot',
    'javascript:alert(1)', 'file:///C:/Windows/notepad.exe']) {
    const context = boot();click(context, href);assert.equal(context.nativeCalls.length, 0, href);
  }
  for (const fixture of [{child: true}, {href: 'alas-error://localhost/backend'},
    {label: 'splash', href: 'alas-splash://localhost/'},
    {href: 'https://webui.example/', backendOrigin: 'https://webui.example', backendNative: false}]) {
    const context = boot(fixture);click(context, projectRepository);assert.equal(context.nativeCalls.length, 0);
  }
  for (const override of [{isTrusted: false}, {button: 1}, {button: 2}]) {
    const context = boot();click(context, projectRepository, override);assert.equal(context.nativeCalls.length, 0);
  }
  const failure = boot({failProjectOpen: true});
  click(failure, projectRepository);
  await Promise.resolve();
  assert.deepEqual(failure.alerts, ['Error: Browser unavailable']);
});

test('trusted top documents expose only the frozen native wrapper', async () => {
  for (const fixture of [
    { href: 'alas-splash://localhost/', label: 'splash', command: 'splash_ready', role: 'splash' },
    { href: 'alas-error://localhost/backend?error=offline', command: 'retry_backend_connection', role: 'error' },
    { command: 'save_as', role: 'backend' },
    { href: 'http://alas-splash.localhost/', label: 'splash', windows: true, command: 'splash_ready', role: 'splash' },
    { href: 'http://alas-error.localhost/backend', windows: true, command: 'retry_backend_connection', role: 'error' },
    { href: 'https://[::1]:22267/ui', backendOrigin: 'https://[::1]:22267', command: 'save_as', role: 'backend' }
  ]) {
    const context = boot(fixture);
    const bridge = context.window.__ALAS_NATIVE__;
    assert.ok(bridge);
    assert.equal(Object.isFrozen(bridge), true);
    assert.equal(await bridge.invoke(fixture.command, { filename: 'export.txt', data: 'YQ==' }), 'accepted');
    assert.equal(context.nativeCalls[0].args.launcherToken, tokens[fixture.role]);
    assert.equal(Object.getOwnPropertyDescriptor(context.window, '__ALAS_NATIVE__').writable, false);
    for (const token of Object.values(tokens)) assert.ok(!bridge.invoke.toString().includes(token));
    assert.equal(context.window.tokens, undefined);
    assert.equal(context.window.__ALAS_SECURITY_TOKENS__, undefined);
  }
});

test('unknown origins, credentials, schemes, labels and remote backends receive no native wrapper', () => {
  for (const fixture of [
    { href: 'http://127.0.0.1:22268/' },
    { href: 'http://localhost:22267/' },
    { href: 'http://127.0.0.1.evil.example:22267/' },
    { href: 'http://user:password@127.0.0.1:22267/' },
    { href: 'https://evil.example/' },
    { href: 'about:blank' },
    { href: 'data:text/html,hello' },
    { href: 'blob:http://127.0.0.1:22267/uuid' },
    { href: 'javascript:void(0)' },
    { href: 'alas-error://evil.example/backend' },
    { href: 'alas-error://localhost/other' },
    { href: 'http://alas-error.localhost.evil.example/backend', windows: true },
    { href: 'http://alas-error.localhost:22267/backend', windows: true },
    { href: 'alas-splash://localhost/?extra=1', label: 'splash' },
    { label: 'hidden-app' },
    { webviewLabel: 'child' },
    { href: 'https://webui.example/', backendOrigin: 'https://webui.example', backendNative: false }
  ]) assert.equal(boot(fixture).window.__ALAS_NATIVE__, undefined, JSON.stringify(fixture));
});

test('cross-origin and same-origin child frames get no bridge or token-bearing hook', async () => {
  for (const href of ['https://evil.example/', 'http://127.0.0.1:22267/', 'alas-error://localhost/backend']) {
    const context = boot({ href, child: true });
    assert.equal(context.window.__ALAS_NATIVE__, undefined);
    assert.equal(context.listeners.size, 0);
    await denied(context.window.__TAURI_INTERNALS__.invoke('window_close'));
    assert.equal(context.nativeCalls.length, 0);
  }
});

test('raw invoke, unknown commands and a caller-provided token cannot widen authority', async () => {
  const context = boot();
  await denied(context.window.__TAURI_INTERNALS__.invoke('window_close'));
  await denied(context.window.__TAURI_INTERNALS__.invoke('window_close', { launcherToken: 'forged' }));
  const callCount = context.calls.length;
  await denied(context.window.__ALAS_NATIVE__.invoke('download_today_gui_log'));
  await denied(context.window.__ALAS_NATIVE__.invoke('retry_backend_connection'));
  await denied(context.window.__ALAS_NATIVE__.invoke('plugin:dialog|open'));
  await denied(context.window.__ALAS_NATIVE__.invoke('unknown_future_command'));
  assert.equal(context.calls.length, callCount);
  await context.window.__ALAS_NATIVE__.invoke('save_as', { launcherToken: tokens.error, filename: 'export.txt' });
  assert.equal(context.nativeCalls[0].args.launcherToken, tokens.backend);
});

test('a captured wrapper rejects foreign origin, cross-role navigation and later framing', async () => {
  const context = boot();
  const invoke = context.window.__ALAS_NATIVE__.invoke;
  for (const href of ['https://evil.example/', 'about:blank', 'alas-error://localhost/backend']) {
    context.window.location.href = href;
    await denied(invoke('window_close'));
    await denied(invoke('retry_backend_connection'));
  }
  assert.equal(context.calls.length, 0);
  context.window.location.href = 'http://127.0.0.1:22267/new-route';
  // Tokens are per session and role, not per document: this is the documented scope.
  await invoke('window_close');
  assert.equal(context.nativeCalls.length, 1);
  context.window.top = {};
  await denied(invoke('window_close'));
  assert.equal(context.nativeCalls.length, 1);
});

test('document-start popup and form guards preserve only allowed same-window navigation', () => {
  const context = boot();
  assert.equal(context.window.open('https://evil.example/'), null);
  assert.equal(Object.getOwnPropertyDescriptor(context.window, 'open').writable, false);
  const event = target => ({
    target, prevented: false, stopped: false,
    composedPath() { return [target]; },
    preventDefault() { this.prevented = true; },
    stopImmediatePropagation() { this.stopped = true; }
  });
  for (const href of ['javascript:void(0)', 'data:text/html,x', 'blob:http://127.0.0.1:22267/id', 'about:blank', 'https://evil.example/']) {
    const click = event({ tagName: 'A', href, target: '_blank' });
    context.listeners.get('click')(click);
    assert.equal(click.prevented, true, href);
  }
  const anchor = { tagName: 'A', href: 'http://127.0.0.1:22267/tasks', target: '_blank' };
  context.listeners.get('click')(event(anchor));
  assert.equal(anchor.target, '_self');
  assert.equal(anchor.rel, 'noopener noreferrer');
  const auxiliary = event(anchor);
  context.listeners.get('auxclick')(auxiliary);
  assert.equal(auxiliary.prevented, true);
  const form = new context.Form();
  form.action = 'https://evil.example/';
  form.submit();
  assert.equal(form.submitted, undefined);
  form.action = 'http://127.0.0.1:22267/tasks';
  form.target = '_blank';
  form.submit();
  assert.equal(form.submitted, true);
  assert.equal(form.target, '_self');
  const submitEvent = event(form);
  submitEvent.submitter = { hasAttribute: () => true, formAction: 'https://evil.example/' };
  context.listeners.get('submit')(submitEvent);
  assert.equal(submitEvent.prevented, true);
});


test('close preferences use the backend token and preserve action arguments', async () => {
  const context = boot();
  const invoke = context.window.__ALAS_NATIVE__.invoke;
  await invoke('get_launcher_preferences');
  await invoke('set_launcher_close_behavior', { behavior: 'ask', launcherToken: tokens.error });
  await invoke('window_choose_close_behavior', { behavior: 'tray' });
  assert.deepEqual(context.nativeCalls.map(call => call.command), [
    'get_launcher_preferences', 'set_launcher_close_behavior', 'window_choose_close_behavior'
  ]);
  assert.equal(context.nativeCalls[1].args.behavior, 'ask');
  assert.equal(context.nativeCalls[2].args.behavior, 'tray');
  for (const call of context.nativeCalls) assert.equal(call.args.launcherToken, tokens.backend);
});

test('only the error close chooser gets preference authority outside the backend', async () => {
  const preferenceCommands = ['get_launcher_preferences', 'set_launcher_close_behavior', 'window_choose_close_behavior'];
  const splash = boot({ href: 'alas-splash://localhost/', label: 'splash' });
  for (const command of preferenceCommands) await denied(splash.window.__ALAS_NATIVE__.invoke(command, { behavior: 'exit' }));
  assert.equal(splash.calls.length, 0);
  const error = boot({ href: 'alas-error://localhost/backend?error=offline' });
  for (const command of preferenceCommands.slice(0, 2)) await denied(error.window.__ALAS_NATIVE__.invoke(command));
  assert.equal(error.calls.length, 0);
  await error.window.__ALAS_NATIVE__.invoke('window_choose_close_behavior', { behavior: 'exit' });
  assert.equal(error.nativeCalls[0].args.behavior, 'exit');
  assert.equal(error.nativeCalls[0].args.launcherToken, tokens.error);
  const backend = boot();
  const stale = backend.window.__ALAS_NATIVE__.invoke;
  backend.window.location.href = 'alas-error://localhost/backend';
  for (const command of preferenceCommands) await denied(stale(command, { behavior: 'tray' }));
  assert.equal(backend.calls.length, 0);
});
