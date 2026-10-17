// This is a document-start plugin script, never a DOM script or an HTML asset.
;(function () {
  'use strict';
  // WebView2 adds initialization scripts to subframes despite the Tauri flag.
  if (window.top !== window) return;

  const policy = __ALAS_SECURITY_POLICY__;
  const tokens = __ALAS_SECURITY_TOKENS__;
  const metadata = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.metadata;
  if (!metadata || metadata.currentWindow.label !== metadata.currentWebview.label) return;
  const label = metadata.currentWindow.label;
  if (label !== 'main' && label !== 'splash') return;
  const rawInvoke = window.__TAURI_INTERNALS__.invoke;
  const splashUrl = new URL(policy.splashUrl);
  const errorUrl = new URL(policy.errorUrl);
  const credentialFree = url => !url.username && !url.password;
  const internal = (url, expected, query) => credentialFree(url)
    && url.protocol === expected.protocol && url.hostname === expected.hostname
    && url.port === expected.port && url.pathname === expected.pathname
    && (query || !url.search) && !url.hash;
  const backend = url => credentialFree(url)
    && (url.protocol === 'http:' || url.protocol === 'https:')
    && url.origin === policy.backendOrigin;
  const role = () => {
    const url = new URL(window.location.href);
    if (label === 'splash' && internal(url, splashUrl, false)) return 'splash';
    if (label === 'main' && internal(url, errorUrl, true)) return 'error';
    if (label === 'main' && policy.backendNative && backend(url)) return 'backend';
    return null;
  };
  const allowsNavigation = target => {
    try {
      const url = new URL(target, window.location.href);
      return label === 'splash' ? internal(url, splashUrl, false)
        : internal(url, errorUrl, true) || backend(url);
    } catch (_) { return false; }
  };
  const isProjectRepository = target => {
    try {
      const url = new URL(target);
      return policy.projectRepositoryLinks.some(link => url.href === link || url.href === link + '/');
    } catch (_) { return false; }
  };

  // Tauri 2.5.1 has no public on_new_window builder callback. These document-start
  // guards complement the Rust navigation guard; they do not replace a native veto.
  Object.defineProperty(window, 'open', {
    value: function () { return null; }, writable: false, configurable: false
  });
  document.addEventListener('click', event => {
    const path = event.composedPath ? event.composedPath() : [];
    const anchor = path.find(node => node && node.tagName && /^(A|AREA)$/.test(node.tagName));
    if (!anchor) return;
    if (!allowsNavigation(anchor.href)) {
      event.preventDefault(); event.stopImmediatePropagation();
      // 固定项目链接交给系统浏览器；只有可信主页面中的真实点击可以触发。
      if (event.isTrusted && event.button === 0 && role() === 'backend' && isProjectRepository(anchor.href)) {
        window.__ALAS_NATIVE__.invoke('open_project_repository').catch(error => {
          window.alert(String(error));
        });
      }
      return;
    }
    anchor.target = '_self';
    anchor.rel = 'noopener noreferrer';
  }, true);
  document.addEventListener('auxclick', event => { event.preventDefault(); }, true);
  const prepareForm = form => {
    if (!allowsNavigation(form.action || window.location.href)) return false;
    form.target = '_self';
    return true;
  };
  document.addEventListener('submit', event => {
    const submitter = event.submitter;
    if (submitter && submitter.hasAttribute('formaction') && !allowsNavigation(submitter.formAction)) {
      event.preventDefault(); event.stopImmediatePropagation(); return;
    }
    if (!prepareForm(event.target)) {
      event.preventDefault(); event.stopImmediatePropagation(); return;
    }
    if (submitter) submitter.formTarget = '_self';
  }, true);
  const submit = HTMLFormElement.prototype.submit;
  Object.defineProperty(HTMLFormElement.prototype, 'submit', {
    value: function () { if (prepareForm(this)) return submit.call(this); },
    writable: false, configurable: false
  });

  const initialRole = role();
  if (!initialRole || typeof rawInvoke !== 'function') return;
  const token = tokens[initialRole];
  // The token is captured outside this function so Function#toString cannot reveal it.
  const invoke = function (command, args = {}) {
    if (window.top !== window || role() !== initialRole || !policy.commands[initialRole].includes(command)) {
      return Promise.reject(new Error('Native command denied for this document'));
    }
    return rawInvoke(command, Object.assign({}, args, { launcherToken: token }));
  };
  Object.defineProperty(window, '__ALAS_NATIVE__', {
    value: Object.freeze({ invoke }), writable: false, configurable: false
  });
})();
