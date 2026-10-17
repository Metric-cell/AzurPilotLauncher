;
        if (window.top !== window) return;
        const invoke =
            (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke)
            || (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
        if (typeof invoke !== 'function') {
            return;
        }
        const ensureTitlebar = () => {
            if (!document.body || document.getElementById('alas-launcher-titlebar')) {
                return;
            }
            if (!document.getElementById('alas-launcher-titlebar-style')) {
                const style = document.createElement('style');
                style.id = 'alas-launcher-titlebar-style';
                style.textContent = ':root{--alas-titlebar-height:56px}#alas-launcher-titlebar{position:fixed;top:0;left:0;right:0;height:var(--topbar-height,var(--alas-titlebar-height,56px));z-index:2147483647;user-select:none;pointer-events:none;background:transparent}#alas-launcher-titlebar *{box-sizing:border-box}.alas-titlebar-drag-zone{position:absolute;inset:0 148px 0 0;height:100%;pointer-events:none;background:transparent;touch-action:none}.header-icon,.header-icon *{app-region:no-drag;-webkit-app-region:no-drag}.header-icon{display:flex;align-items:center;gap:3px;padding:3px 6px;position:absolute;top:50%;transform:translateY(-50%);right:10px;height:36px;pointer-events:auto;border:1px solid rgba(255,255,255,.92);border-radius:18px;background:rgba(250,250,247,.78);box-shadow:0 4px 14px rgba(61,79,97,.1),inset 0 1px 0 rgba(255,255,255,.36);backdrop-filter:blur(16px) saturate(1.2);-webkit-backdrop-filter:blur(16px) saturate(1.2);transition:background .2s ease,border-color .2s ease,box-shadow .2s ease}.icon{width:28px;height:28px;min-width:28px;min-height:28px;margin:0;padding:0;line-height:1;border-radius:12px;border:none;background:transparent;color:#727b86;cursor:pointer;flex:0 0 auto;position:relative;transition:transform 140ms cubic-bezier(.23,1,.32,1),background-color 140ms ease,color 140ms ease;display:inline-flex;align-items:center;justify-content:center}.icon:hover{color:#202832;background:rgba(255,255,255,.72)}.icon:active{transform:scale(.96)}.icon-close{color:#e64f58}.icon-close:hover{color:#b5202e;background:rgba(244,91,91,.15)}.icon svg{width:11px;height:11px;stroke:currentColor;fill:none;stroke-width:1.2;stroke-linecap:round;stroke-linejoin:round;opacity:1;display:block;margin:auto}';
                style.textContent += '#alas-close-menu{position:fixed;top:8px;right:10px;z-index:2147483647;width:272px;padding:14px;border:1px solid rgba(255,255,255,.9);border-radius:20px;background:rgba(250,250,247,.94);box-shadow:0 18px 46px rgba(46,58,72,.2),inset 0 1px 0 rgba(255,255,255,.7);backdrop-filter:blur(20px) saturate(1.18);-webkit-backdrop-filter:blur(20px) saturate(1.18);color:#202832;opacity:0;pointer-events:none;transform:translateY(-6px) scale(.96);transform-origin:calc(100% - 64px) 0;transition:opacity 140ms ease,transform 180ms cubic-bezier(.23,1,.32,1),background .2s ease,border-color .2s ease;app-region:no-drag;-webkit-app-region:no-drag}#alas-close-menu.is-open{opacity:1;pointer-events:auto;transform:translateY(0) scale(1)}#alas-close-menu *{box-sizing:border-box;app-region:no-drag;-webkit-app-region:no-drag}#alas-close-menu-title{margin:0 0 12px;font:500 13px/1.55 "MiSans",sans-serif;color:rgba(32,40,50,.82)}#alas-close-menu-actions{display:grid;grid-template-columns:1fr 1fr;gap:8px}#alas-close-menu button{display:flex;align-items:center;justify-content:center;min-width:0;min-height:36px;margin:0;padding:0 12px;border:1px solid rgba(92,105,120,.16);border-radius:11px;background:rgba(105,118,133,.08);color:#394451;font:600 12px/1 "MiSans",sans-serif;cursor:pointer;transition:background-color 140ms ease,color 140ms ease,transform 140ms cubic-bezier(.23,1,.32,1)}#alas-close-menu button:hover{background:rgba(105,118,133,.14)}#alas-close-menu button:active{transform:scale(.98)}#alas-close-menu button:disabled{opacity:.55;cursor:default;transform:none}#alas-close-menu .alas-close-confirm{border-color:rgba(205,62,69,.28);background:#d94b4b;color:#fff}#alas-close-menu .alas-close-confirm:hover{background:#c93e3e;color:#fff}';
                style.textContent += '#alas-close-menu[hidden],#alas-close-menu [hidden],#alas-launcher-titlebar [hidden]{display:none}#alas-close-menu-hint{margin:0 0 12px;font:400 12px/1.55 \"MiSans\",sans-serif;opacity:.75}#alas-close-menu-error{margin:0 0 12px;font:500 12px/1.55 \"MiSans\",sans-serif;color:#df4653;overflow-wrap:anywhere}#alas-close-menu button:focus-visible{outline:2px solid #4589e8;outline-offset:2px}';
                style.textContent += ':root[data-theme*="dark"] .header-icon,:root[data-color-mode="dark"] .header-icon,:root.dark .header-icon,body[data-theme*="dark"] .header-icon,body[data-color-mode="dark"] .header-icon,body.dark .header-icon,[data-theme*="dark"] .header-icon,[data-color-mode="dark"] .header-icon,#alas-launcher-titlebar.is-dark .header-icon{background:rgba(30,34,42,.78);border:1px solid rgba(255,255,255,.14);box-shadow:0 4px 16px rgba(0,0,0,.4),inset 0 1px 0 rgba(255,255,255,.14)}';
                style.textContent += ':root[data-theme*="dark"] .icon,:root[data-color-mode="dark"] .icon,:root.dark .icon,body[data-theme*="dark"] .icon,body[data-color-mode="dark"] .icon,body.dark .icon,[data-theme*="dark"] .icon,[data-color-mode="dark"] .icon,#alas-launcher-titlebar.is-dark .icon{color:rgba(220,228,238,.82)}';
                style.textContent += ':root[data-theme*="dark"] .icon:hover,:root[data-color-mode="dark"] .icon:hover,:root.dark .icon:hover,body[data-theme*="dark"] .icon:hover,body[data-color-mode="dark"] .icon:hover,body.dark .icon:hover,[data-theme*="dark"] .icon:hover,[data-color-mode="dark"] .icon:hover,#alas-launcher-titlebar.is-dark .icon:hover{color:#fff;background:rgba(255,255,255,.14)}';
                style.textContent += ':root[data-theme*="dark"] .icon:active,:root[data-color-mode="dark"] .icon:active,:root.dark .icon:active,body[data-theme*="dark"] .icon:active,body[data-color-mode="dark"] .icon:active,body.dark .icon:active,[data-theme*="dark"] .icon:active,[data-color-mode="dark"] .icon:active,#alas-launcher-titlebar.is-dark .icon:active{background:rgba(255,255,255,.22)}';
                style.textContent += ':root[data-theme*="dark"] .icon-close,:root[data-color-mode="dark"] .icon-close,:root.dark .icon-close,body[data-theme*="dark"] .icon-close,body[data-color-mode="dark"] .icon-close,body.dark .icon-close,[data-theme*="dark"] .icon-close,[data-color-mode="dark"] .icon-close,#alas-launcher-titlebar.is-dark .icon-close{color:#ff6470}';
                style.textContent += ':root[data-theme*="dark"] .icon-close:hover,:root[data-color-mode="dark"] .icon-close:hover,:root.dark .icon-close:hover,body[data-theme*="dark"] .icon-close:hover,body[data-color-mode="dark"] .icon-close:hover,body.dark .icon-close:hover,[data-theme*="dark"] .icon-close:hover,[data-color-mode="dark"] .icon-close:hover,#alas-launcher-titlebar.is-dark .icon-close:hover{color:#fff;background:#e0444d}';
                style.textContent += ':root[data-theme*="dark"] .icon-close:active,:root[data-color-mode="dark"] .icon-close:active,:root.dark .icon-close:active,body[data-theme*="dark"] .icon-close:active,body[data-color-mode="dark"] .icon-close:active,body.dark .icon-close:active,[data-theme*="dark"] .icon-close:active,[data-color-mode="dark"] .icon-close:active,#alas-launcher-titlebar.is-dark .icon-close:active{background:#c7363f}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu,:root[data-color-mode="dark"] #alas-close-menu,:root.dark #alas-close-menu,body[data-theme*="dark"] #alas-close-menu,body[data-color-mode="dark"] #alas-close-menu,body.dark #alas-close-menu,[data-theme*="dark"] #alas-close-menu,[data-color-mode="dark"] #alas-close-menu,#alas-close-menu.is-dark{background:rgba(28,32,38,.94);border:1px solid rgba(255,255,255,.14);box-shadow:0 18px 46px rgba(0,0,0,.55),inset 0 1px 0 rgba(255,255,255,.12);color:#f0f3f6}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu-title,:root[data-color-mode="dark"] #alas-close-menu-title,:root.dark #alas-close-menu-title,body[data-theme*="dark"] #alas-close-menu-title,body[data-color-mode="dark"] #alas-close-menu-title,body.dark #alas-close-menu-title,[data-theme*="dark"] #alas-close-menu-title,[data-color-mode="dark"] #alas-close-menu-title,#alas-close-menu.is-dark #alas-close-menu-title{color:rgba(240,244,250,.9)}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu button,:root[data-color-mode="dark"] #alas-close-menu button,:root.dark #alas-close-menu button,body[data-theme*="dark"] #alas-close-menu button,body[data-color-mode="dark"] #alas-close-menu button,body.dark #alas-close-menu button,[data-theme*="dark"] #alas-close-menu button,[data-color-mode="dark"] #alas-close-menu button,#alas-close-menu.is-dark button{border:1px solid rgba(255,255,255,.12);background:rgba(255,255,255,.08);color:rgba(230,235,245,.88)}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu button:hover,:root[data-color-mode="dark"] #alas-close-menu button:hover,:root.dark #alas-close-menu button:hover,body[data-theme*="dark"] #alas-close-menu button:hover,body[data-color-mode="dark"] #alas-close-menu button:hover,body.dark #alas-close-menu button:hover,[data-theme*="dark"] #alas-close-menu button:hover,[data-color-mode="dark"] #alas-close-menu button:hover,#alas-close-menu.is-dark button:hover{background:rgba(255,255,255,.14);color:#fff}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu .alas-close-confirm,:root[data-color-mode="dark"] #alas-close-menu .alas-close-confirm,:root.dark #alas-close-menu .alas-close-confirm,body[data-theme*="dark"] #alas-close-menu .alas-close-confirm,body[data-color-mode="dark"] #alas-close-menu .alas-close-confirm,body.dark #alas-close-menu .alas-close-confirm,[data-theme*="dark"] #alas-close-menu .alas-close-confirm,[data-color-mode="dark"] #alas-close-menu .alas-close-confirm,#alas-close-menu.is-dark .alas-close-confirm{border-color:rgba(244,91,91,.35);background:#d94b4b;color:#fff}';
                style.textContent += ':root[data-theme*="dark"] #alas-close-menu .alas-close-confirm:hover,:root[data-color-mode="dark"] #alas-close-menu .alas-close-confirm:hover,:root.dark #alas-close-menu .alas-close-confirm:hover,body[data-theme*="dark"] #alas-close-menu .alas-close-confirm:hover,body[data-color-mode="dark"] #alas-close-menu .alas-close-confirm:hover,body.dark #alas-close-menu .alas-close-confirm:hover,[data-theme*="dark"] #alas-close-menu .alas-close-confirm:hover,[data-color-mode="dark"] #alas-close-menu .alas-close-confirm:hover,#alas-close-menu.is-dark .alas-close-confirm:hover{background:#c93e3e;color:#fff}';
                style.textContent += '@media (prefers-color-scheme:dark){:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .header-icon{background:rgba(30,34,42,.78);border:1px solid rgba(255,255,255,.14);box-shadow:0 4px 16px rgba(0,0,0,.4),inset 0 1px 0 rgba(255,255,255,.14)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon{color:rgba(220,228,238,.82)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon:hover{color:#fff;background:rgba(255,255,255,.14)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon:active{background:rgba(255,255,255,.22)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon-close{color:#ff6470}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon-close:hover{color:#fff;background:#e0444d}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) .icon-close:active{background:#c7363f}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu{background:rgba(28,32,38,.94);border:1px solid rgba(255,255,255,.14);box-shadow:0 18px 46px rgba(0,0,0,.55),inset 0 1px 0 rgba(255,255,255,.12);color:#f0f3f6}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu-title{color:rgba(240,244,250,.9)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu button{border:1px solid rgba(255,255,255,.12);background:rgba(255,255,255,.08);color:rgba(230,235,245,.88)}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu button:hover{background:rgba(255,255,255,.14);color:#fff}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu .alas-close-confirm{border-color:rgba(244,91,91,.35);background:#d94b4b;color:#fff}:root:not([data-theme*="light"]):not([data-color-mode="light"]):not(.light) #alas-close-menu .alas-close-confirm:hover{background:#c93e3e;color:#fff}}';
                document.head.appendChild(style);
            }
            const titlebar = document.createElement('div');
            titlebar.id = 'alas-launcher-titlebar';
            titlebar.innerHTML = '<div class="alas-titlebar-drag-zone" aria-hidden="true"></div><div class="header-icon"><button type="button" class="icon icon-hide" data-action="hide" aria-label="'+i18n.hideLabel+'" title="'+i18n.hideLabel+'"><svg viewBox="0 0 6 6"><rect x="1" y="1" width="4" height="4" rx="1"/><path d="M2 3h2"/></svg></button><button type="button" class="icon icon-minimize" data-action="minimize" aria-label="'+i18n.minimizeLabel+'" title="'+i18n.minimizeTitle+'"><svg viewBox="0 0 6 6"><line x1="1" y1="3" x2="5" y2="3"/></svg></button><button type="button" class="icon icon-maximize" data-action="maximize" aria-label="'+i18n.maximizeLabel+'" title="'+i18n.maximizeTitle+'"><svg viewBox="0 0 6 6" class="svg-restore" style="display:none"><polyline points="1,3 1,1 3,1"/><polyline points="3,5 5,5 5,3"/></svg><svg viewBox="0 0 6 6" class="svg-maximize"><polyline points="1,2.5 1,1 2.5,1"/><polyline points="3.5,5 5,5 5,3.5"/></svg></button><button type="button" class="icon icon-close" data-action="close" aria-label="'+i18n.closeLabel+'" title="'+i18n.closeTitle+'"><svg viewBox="0 0 6 6"><line x1="1" y1="1" x2="5" y2="5"/><line x1="5" y1="1" x2="1" y2="5"/></svg></button></div>';
            if (!nativeTitlebar) {
                document.body.dataset.alasCustomTitlebar = 'true';
                document.body.prepend(titlebar);
            }
            if (!closePromptEnabled) titlebar.querySelector('[data-action="hide"]').hidden = true;
            const maximizeButton = titlebar.querySelector('[data-action="maximize"]');
            let closeMenu = document.getElementById('alas-close-menu');
            if (!closeMenu) {
                closeMenu = document.createElement('div');
                closeMenu.id = 'alas-close-menu';
                closeMenu.setAttribute('role', 'dialog');
                closeMenu.setAttribute('aria-modal', 'true');
                closeMenu.setAttribute('aria-labelledby', 'alas-close-menu-title');
                closeMenu.setAttribute('aria-describedby', 'alas-close-menu-hint');
                closeMenu.hidden = true;
                closeMenu.innerHTML = '<p id="alas-close-menu-title"></p><p id="alas-close-menu-hint"></p><p id="alas-close-menu-error" role="alert" hidden></p><div id="alas-close-menu-actions"><button type="button" data-close-action="tray"></button><button type="button" class="alas-close-confirm" data-close-action="exit"></button></div>';
                closeMenu.querySelector('#alas-close-menu-title').textContent = i18n.closePrompt;
                closeMenu.querySelector('#alas-close-menu-hint').textContent = i18n.closeRememberHint;
                closeMenu.querySelector('[data-close-action="tray"]').textContent = i18n.minimizeToTrayAction;
                closeMenu.querySelector('[data-close-action="exit"]').textContent = i18n.exitAction;
                closeMenu.addEventListener('pointerdown', event => event.stopPropagation());
                document.body.appendChild(closeMenu);
            }
            const syncTitlebarLayout = () => {
                try {
                    const topbar = document.querySelector('.topbar, header, #topbar, [class*="topbar"], [class*="Header"]');
                    let h = 0;
                    let topOffset = 0;
                    if (topbar) {
                        const rect = topbar.getBoundingClientRect();
                        if (rect.height > 20) {
                            h = rect.height;
                            topOffset = rect.top;
                        }
                    }
                    if (!h) {
                        const computed = getComputedStyle(document.documentElement).getPropertyValue('--topbar-height').trim();
                        if (computed) {
                            const parsed = parseFloat(computed);
                            if (!isNaN(parsed) && parsed > 20) {
                                h = parsed;
                            }
                        }
                    }
                    if (h > 0) {
                        titlebar.style.height = h + 'px';
                        if (topOffset >= 0) {
                            titlebar.style.top = topOffset + 'px';
                        }
                    }
                } catch (_) {}
            };
            const syncTheme = () => {
                try {
                    const doc = document.documentElement;
                    const body = document.body;
                    let isDark = false;
                    const dt = (doc.getAttribute('data-theme') || (body && body.getAttribute('data-theme')) || '').toLowerCase();
                    const dcm = (doc.getAttribute('data-color-mode') || (body && body.getAttribute('data-color-mode')) || '').toLowerCase();
                    if (dt.includes('dark') || dcm === 'dark') {
                        isDark = true;
                    } else if (dt.includes('light') || dcm === 'light') {
                        isDark = false;
                    } else if (doc.classList.contains('dark') || (body && body.classList.contains('dark'))) {
                        isDark = true;
                    } else if (doc.classList.contains('light') || (body && body.classList.contains('light'))) {
                        isDark = false;
                    } else {
                        try {
                            const st = (localStorage.getItem('azurpilot.theme') || localStorage.getItem('theme') || '').toLowerCase();
                            const sm = (localStorage.getItem('azurpilot.color-mode') || '').toLowerCase();
                            if (st.includes('dark') || sm === 'dark') isDark = true;
                            else if (st.includes('light') || sm === 'light') isDark = false;
                        } catch (_) {}
                        if (!isDark && window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches) {
                            isDark = true;
                        }
                    }
                    titlebar.classList.toggle('is-dark', isDark);
                    if (closeMenu) closeMenu.classList.toggle('is-dark', isDark);
                } catch (e) {
                    console.error('Failed to sync titlebar theme', e);
                }
            };
            let closeChoicePending = false;
            let closePromptPreviousFocus = null;
            const closeButtons = Array.from(closeMenu.querySelectorAll('button'));
            const closeError = closeMenu.querySelector('#alas-close-menu-error');
            const setCloseMenuOpen = (open, restoreFocus = true) => {
                if (open) {
                    if (closeMenu.classList.contains('is-open')) return;
                    closePromptPreviousFocus = document.activeElement;
                    closeError.hidden = true;
                    closeError.textContent = '';
                    closeMenu.hidden = false;
                    syncTheme();
                    syncTitlebarLayout();
                    try {
                        const iconRect = titlebar.querySelector('.header-icon').getBoundingClientRect();
                        closeMenu.style.top = nativeTitlebar ? '12px' : Math.round(iconRect.bottom + 8) + 'px';
                        closeMenu.style.right = nativeTitlebar ? '12px' : Math.max(10, Math.round(window.innerWidth - iconRect.right)) + 'px';
                    } catch (_) {}
                }
                closeMenu.classList.toggle('is-open', open);
                closeMenu.hidden = !open;
                if (open) closeButtons[0].focus({ preventScroll: true });
                else if (restoreFocus && closePromptPreviousFocus && closePromptPreviousFocus.isConnected) {
                    closePromptPreviousFocus.focus({ preventScroll: true });
                }
            };
            // 原生 CloseRequested 统一决定关闭行为；此钩子只展示菜单，避免递归关闭。
            window.__ALAS_OPEN_CLOSE_PROMPT = () => {
                if (closePromptEnabled && !closeChoicePending) setCloseMenuOpen(true);
            };
            closeButtons.forEach(button => button.addEventListener('click', async () => {
                if (closeChoicePending) return;
                closeChoicePending = true;
                closeButtons.forEach(item => { item.disabled = true; });
                closeMenu.setAttribute('aria-busy', 'true');
                closeError.hidden = true;
                try {
                    await invoke('window_choose_close_behavior', { behavior: button.dataset.closeAction });
                    setCloseMenuOpen(false);
                } catch (error) {
                    closeError.textContent = typeof error === 'string' && error ? error : (error && error.message) || i18n.closeSaveFailed;
                    closeError.hidden = false;
                    console.error('Failed to save close behavior', error);
                } finally {
                    closeChoicePending = false;
                    closeButtons.forEach(item => { item.disabled = false; });
                    closeMenu.removeAttribute('aria-busy');
                    if (closeMenu.classList.contains('is-open')) button.focus({ preventScroll: true });
                }
            }));
            document.addEventListener('pointerdown', event => {
                if (!closeChoicePending && closeMenu.classList.contains('is-open') && !closeMenu.contains(event.target)) {
                    setCloseMenuOpen(false, false);
                }
            });
            document.addEventListener('keydown', event => {
                if (!closeMenu.classList.contains('is-open')) return;
                if (event.key === 'Escape') {
                    event.preventDefault();
                    if (!closeChoicePending) setCloseMenuOpen(false);
                } else if (event.key === 'Tab') {
                    event.preventDefault();
                    if (closeChoicePending) return;
                    const currentIndex = closeButtons.indexOf(document.activeElement);
                    const nextIndex = currentIndex < 0 ? (event.shiftKey ? closeButtons.length - 1 : 0)
                        : (currentIndex + (event.shiftKey ? -1 : 1) + closeButtons.length) % closeButtons.length;
                    closeButtons[nextIndex].focus({ preventScroll: true });
                }
            });
            const interactiveSelector = 'a[href],button,input,select,textarea,summary,label[for],[role="button"],[role="link"],[contenteditable="true"],[tabindex]:not([tabindex="-1"]),[onclick]';
            const isInteractiveElement = element => {
                if (!element || !(element instanceof Element)) return false;
                return Boolean(
                    element.closest(interactiveSelector)
                    || element.closest('[data-alas-no-drag]')
                    || (getComputedStyle(element).cursor === 'pointer' && !element.closest('#alas-launcher-titlebar'))
                );
            };

            document.addEventListener('pointerdown', event => {
                if (nativeTitlebar) return;
                syncTheme();
                syncTitlebarLayout();
                if (!event.isPrimary || event.button !== 0) return;
                const tbRect = titlebar.getBoundingClientRect();
                const titlebarHeight = tbRect.height || 56;
                const titlebarTop = tbRect.top || 0;
                const reservedRight = 148;
                if (event.clientY < titlebarTop || event.clientY > titlebarTop + titlebarHeight || event.clientX > window.innerWidth - reservedRight) return;
                const target = event.target;
                if (!target || !(target instanceof Element)) return;
                if (titlebar.contains(target) || target.closest('#alas-close-menu')) return;
                if (isInteractiveElement(target)) return;
                event.preventDefault();
                invoke('window_start_dragging').catch(error => {
                    console.error('Failed to start dragging from titlebar', error);
                });
            }, { capture: true });

            document.addEventListener('dblclick', async event => {
                if (nativeTitlebar) return;
                syncTheme();
                syncTitlebarLayout();
                const tbRect = titlebar.getBoundingClientRect();
                const titlebarHeight = tbRect.height || 56;
                const titlebarTop = tbRect.top || 0;
                const reservedRight = 148;
                if (event.clientY < titlebarTop || event.clientY > titlebarTop + titlebarHeight || event.clientX > window.innerWidth - reservedRight) return;
                const target = event.target;
                if (!target || !(target instanceof Element)) return;
                if (titlebar.contains(target) || target.closest('#alas-close-menu')) return;
                if (isInteractiveElement(target)) return;
                try {
                    await invoke('window_toggle_maximize');
                    await syncMaximizeState();
                } catch (error) {
                    console.error('Failed to toggle maximize from titlebar', error);
                }
            }, { capture: true });

            const syncMaximizeState = async () => {
                if (nativeTitlebar || !maximizeButton) return;
                try {
                    const maximized = await invoke('window_is_maximized');
                    maximizeButton.dataset.maximized = maximized ? 'true' : 'false';
                    maximizeButton.title = maximized ? i18n.restoreTitle : i18n.maximizeActionTitle;
                    maximizeButton.setAttribute('aria-label', maximized ? i18n.restoreLabel : i18n.maximizeLabelText);
                    maximizeButton.querySelector('.svg-maximize').style.display = maximized ? 'none' : '';
                    maximizeButton.querySelector('.svg-restore').style.display = maximized ? '' : 'none';
                } catch (e) {
                    console.error('Failed to sync maximize state', e);
                }
            };
            titlebar.querySelectorAll('button[data-action]').forEach(button => {
                button.addEventListener('click', async event => {
                    event.stopPropagation();
                    syncTheme();
                    try {
                        switch (button.dataset.action) {
                            case 'hide': await invoke('window_hide'); break;
                            case 'minimize': await invoke('window_minimize'); break;
                            case 'maximize': await invoke('window_toggle_maximize'); await syncMaximizeState(); break;
                            case 'close': await invoke('window_close'); break;
                        }
                    } catch (error) {
                        console.error('Failed to handle ' + button.dataset.action + ' window action', error);
                    }
                });
            });
            window.addEventListener('resize', () => { syncTitlebarLayout(); void syncMaximizeState(); syncTheme(); });
            if (window.matchMedia) {
                try {
                    window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', syncTheme);
                } catch (_) {}
            }
            void syncMaximizeState();
            syncTheme();
            syncTitlebarLayout();
            setTimeout(syncTitlebarLayout, 100);
            setTimeout(syncTitlebarLayout, 500);
        };
        ensureTitlebar();
        if (!document.body) {
            window.addEventListener('DOMContentLoaded', ensureTitlebar, { once: true });
        }
