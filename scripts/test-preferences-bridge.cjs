#!/usr/bin/env node
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../src/preferences_bridge.js'), 'utf8')
  .replaceAll('__ALAS_BACKEND_PORT__', '22267');

function boot(url = 'http://127.0.0.1:22267/settings', frame = false) {
  const calls = [];
  const window = {};
  window.top = frame ? {} : window;
  vm.runInNewContext(source, {window, location: new URL(url)});
  // The initialization script may run before Tauri installs its public globals.
  window.__TAURI_INTERNALS__ = {invoke(command, args) {
    calls.push({command, args});
    return Promise.resolve({closeBehavior: args.behavior || 'ask', backgroundSupported: true});
  }};
  return {window, calls};
}

test('settings discover the bridge before mounting and can read and save preferences', async () => {
  const {window, calls} = boot();
  const native = window.__ALAS_NATIVE__;
  assert.equal((await native.invoke('get_launcher_preferences')).closeBehavior, 'ask');
  assert.equal((await native.invoke('set_launcher_close_behavior', {behavior: 'tray'})).closeBehavior, 'tray');
  assert.equal(calls[1].command, 'set_launcher_close_behavior');
  assert.equal(calls[1].args.behavior, 'tray');
  assert.equal(Object.isFrozen(native), true);
  assert.equal(Object.getOwnPropertyDescriptor(window, '__ALAS_NATIVE__').writable, false);
  await assert.rejects(native.invoke('window_exit_application'), /Unsupported/);
  assert.equal(calls.length, 2);
});

test('settings bridge is limited to the backend port and top frame', () => {
  for (const url of ['https://example.com:22267/', 'http://127.0.0.1:3333/', 'alas-error://localhost/backend']) {
    assert.equal(boot(url).window.__ALAS_NATIVE__, undefined);
  }
  assert.equal(boot(undefined, true).window.__ALAS_NATIVE__, undefined);
  assert.ok(boot('http://localhost:22267/settings').window.__ALAS_NATIVE__);
});
