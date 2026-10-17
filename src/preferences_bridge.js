// Install before the backend app mounts so System Settings can discover the bridge.
(() => {
  if (window.top !== window || !['http:', 'https:'].includes(location.protocol)
      || !['127.0.0.1', 'localhost'].includes(location.hostname)
      || location.port !== '__ALAS_BACKEND_PORT__') return;
  const commands = new Set(['get_launcher_preferences', 'set_launcher_close_behavior']);
  const invoke = (command, args = {}) => {
    if (!commands.has(command)) return Promise.reject(new Error('Unsupported preferences command'));
    const rawInvoke = (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
      || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
    if (typeof rawInvoke !== 'function') return Promise.reject(new Error('Native bridge unavailable'));
    return rawInvoke(command, args);
  };
  Object.defineProperty(window, '__ALAS_NATIVE__', {
    value: Object.freeze({ invoke }), writable: false, configurable: false
  });
})();
