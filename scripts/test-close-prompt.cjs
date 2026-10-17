#!/usr/bin/env node
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../src/ui/titlebar.js'), 'utf8');

function boot({ nativeTitlebar = false, closePromptEnabled = true } = {}) {
  let document;
  class Element {
    constructor(tag = 'div') {
      this.tag = tag; this.children = []; this.listeners = {}; this.dataset = {}; this.style = {};
      this.attributes = {}; this.isConnected = true;
      const classes = new Set();
      this.classList = {
        contains: value => classes.has(value),
        toggle(value, enabled) { if (enabled) classes.add(value); else classes.delete(value); }
      };
    }
    set innerHTML(html) {
      this.children = [...html.matchAll(/<(div|p|button|svg)\b([^>]*)>/g)].map(([, tag, attrs]) => {
        const child = new Element(tag);
        for (const [, name, value] of attrs.matchAll(/([\w-]+)="([^"]*)"/g)) {
          child.attributes[name] = value;
          if (name === 'id') child.id = value;
          else if (name === 'class') value.split(' ').forEach(value => child.classList.toggle(value, true));
          else if (name.startsWith('data-')) child.dataset[name.slice(5).replace(/-([a-z])/g, (_, letter) => letter.toUpperCase())] = value;
        }
        return child;
      });
    }
    all() { return this.children.flatMap(child => [child, ...child.all()]); }
    querySelectorAll(selector) {
      return this.all().filter(child => {
        if (selector.startsWith('#')) return child.id === selector.slice(1);
        if (selector.startsWith('.')) return child.classList.contains(selector.slice(1));
        const data = selector.match(/\[data-([\w-]+)(?:="([^"]*)")?\]/);
        if (data) {
          const key = data[1].replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());
          return child.dataset[key] !== undefined && (data[2] === undefined || child.dataset[key] === data[2]);
        }
        return child.tag === selector;
      });
    }
    querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
    appendChild(child) { this.children.push(child); }
    prepend(child) { this.children.unshift(child); }
    setAttribute(name, value) { this.attributes[name] = value; }
    getAttribute(name) { return this.attributes[name] || ''; }
    removeAttribute(name) { delete this.attributes[name]; }
    addEventListener(type, handler) { (this.listeners[type] ||= []).push(handler); }
    async dispatch(type, event = {}) { for (const handler of this.listeners[type] || []) await handler(event); }
    contains(target) { return target === this || this.all().includes(target); }
    focus() { document.activeElement = this; }
    getBoundingClientRect() { return { top: 0, bottom: 36, height: 36, right: 790 }; }
  }
  document = new Element('document');
  document.body = new Element('body'); document.head = new Element('head'); document.documentElement = new Element('html');
  document.children = [document.head, document.body];
  document.createElement = tag => new Element(tag);
  document.getElementById = id => document.querySelector('#' + id);
  document.activeElement = new Element('input');
  const calls = [];
  let choose = () => Promise.resolve();
  const window = {
    innerWidth: 800, addEventListener() {},
    __ALAS_NATIVE__: { invoke(command, args) { calls.push({ command, args }); return command === 'window_choose_close_behavior' ? choose(args) : Promise.resolve(false); } }
  };
  window.top = window;
  vm.runInNewContext(`(function(){const i18n = {closeRememberHint:'remember', closeSaveFailed:'save failed'};const nativeTitlebar=${nativeTitlebar};const closePromptEnabled=${closePromptEnabled};\n${source}\n})();`, {
    window, document, Element, console: { error() {} }, localStorage: { getItem() {} }, setTimeout() {},
    getComputedStyle: () => ({ getPropertyValue: () => '', cursor: 'default' })
  });
  return { window, document, calls, menu: document.getElementById('alas-close-menu'), setChoose: handler => { choose = handler; } };
}

test('X delegates to native close; native prompt hook only opens the menu', async () => {
  const app = boot();
  assert.equal(app.menu.hidden, true);
  await app.document.querySelector('[data-action="close"]').dispatch('click', { stopPropagation() {} });
  assert.equal(app.calls.at(-1).command, 'window_close');
  assert.equal(app.menu.hidden, true);
  const count = app.calls.length;
  app.window.__ALAS_OPEN_CLOSE_PROMPT();
  assert.equal(app.calls.length, count);
  assert.equal(app.menu.hidden, false);
  assert.equal(app.document.activeElement.dataset.closeAction, 'tray');
});

test('choices persist through native command, reject duplicates and expose recoverable errors', async () => {
  const app = boot();
  app.window.__ALAS_OPEN_CLOSE_PROMPT();
  let reject;
  app.setChoose(() => new Promise((_, failure) => { reject = failure; }));
  const tray = app.menu.querySelector('[data-close-action="tray"]');
  const exit = app.menu.querySelector('[data-close-action="exit"]');
  const pending = tray.dispatch('click');
  await exit.dispatch('click');
  assert.equal(tray.disabled, true);
  assert.equal(exit.disabled, true);
  assert.equal(app.calls.filter(call => call.command === 'window_choose_close_behavior').length, 1);
  assert.equal(app.calls.at(-1).args.behavior, 'tray');
  reject('Cannot save settings');
  await pending;
  const error = app.menu.querySelector('#alas-close-menu-error');
  assert.equal(error.attributes.role, 'alert');
  assert.equal(error.hidden, false);
  assert.equal(error.textContent, 'Cannot save settings');
  assert.equal(tray.disabled, false);
  assert.equal(app.menu.hidden, false);
  app.setChoose(() => Promise.resolve());
  await exit.dispatch('click');
  assert.equal(app.calls.at(-1).args.behavior, 'exit');
  assert.equal(app.menu.hidden, true);
});

test('Escape and outside clicks cancel without saving; Tab stays in the prompt', async () => {
  const app = boot();
  const original = app.document.activeElement;
  app.window.__ALAS_OPEN_CLOSE_PROMPT();
  const keys = key => app.document.dispatch('keydown', { key, preventDefault() {} });
  await keys('Tab');
  assert.equal(app.document.activeElement.dataset.closeAction, 'exit');
  await keys('Tab');
  assert.equal(app.document.activeElement.dataset.closeAction, 'tray');
  await keys('Escape');
  assert.equal(app.document.activeElement, original);
  assert.equal(app.menu.hidden, true);
  app.window.__ALAS_OPEN_CLOSE_PROMPT();
  await app.document.dispatch('pointerdown', { target: original });
  assert.equal(app.menu.hidden, true);
  assert.equal(app.calls.filter(call => call.command === 'window_choose_close_behavior').length, 0);
});

test('macOS gets a prompt without a custom titlebar; Linux has no tray action or prompt', () => {
  const mac = boot({ nativeTitlebar: true });
  assert.equal(mac.document.getElementById('alas-launcher-titlebar'), null);
  mac.window.__ALAS_OPEN_CLOSE_PROMPT();
  assert.equal(mac.menu.hidden, false);
  assert.equal(mac.menu.style.right, '12px');
  const linux = boot({ closePromptEnabled: false });
  assert.equal(linux.document.querySelector('[data-action="hide"]').hidden, true);
  linux.window.__ALAS_OPEN_CLOSE_PROMPT();
  assert.equal(linux.menu.hidden, true);
});
