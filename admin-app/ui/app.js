// Access Portal Admin: the page. All portal calls go through the native side
// (`request`), which holds the session; this file only draws and reacts.

(function () {
  'use strict';

  var invoke = window.__TAURI__.core.invoke;
  var $ = function (id) { return document.getElementById(id); };

  // ---- small helpers ----------------------------------------------------

  /** Build an element: h('div', {class: 'x', onclick: fn}, child, 'text', ...) */
  function h(tag, attrs) {
    var el = document.createElement(tag);
    Object.keys(attrs || {}).forEach(function (k) {
      var v = attrs[k];
      if (v === false || v == null) return;
      if (k.slice(0, 2) === 'on') el.addEventListener(k.slice(2), v);
      else if (k === 'class') el.className = v;
      else if (k in el && k !== 'list') el[k] = v;
      else el.setAttribute(k, v);
    });
    for (var i = 2; i < arguments.length; i++) {
      var c = arguments[i];
      if (c == null || c === false) continue;
      if (Array.isArray(c)) c.forEach(function (x) { if (x) el.append(x); });
      else el.append(c);
    }
    return el;
  }

  function remember(key, value) { try { localStorage.setItem(key, value); } catch (e) { /* optional */ } }
  function recall(key) { try { return localStorage.getItem(key) || ''; } catch (e) { return ''; } }

  var toastTimer = 0;
  function toast(message, bad) {
    var t = $('toast');
    t.textContent = message;
    t.className = 'toast' + (bad ? ' bad' : '');
    t.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { t.hidden = true; }, bad ? 5000 : 2400);
  }

  /** One API call. Resolves with the portal's JSON, rejects with a message. */
  function api(method, path, body) {
    return invoke('request', { method: method, path: path, body: body === undefined ? null : body })
      .then(function (res) {
        if (res.status >= 200 && res.status < 300) return res.data;
        if (res.status === 401 && path !== '/api/login') showSignin('Your sign-in expired. Sign in again.');
        var err = new Error((res.data && res.data.error) || 'The portal refused that (' + res.status + ').');
        err.data = res.data;
        throw err;
      }, function (message) { throw new Error(String(message)); });
  }

  /** Disable a button while its action runs; report failures as a toast. */
  function run(button, action) {
    if (button) button.disabled = true;
    return Promise.resolve().then(action).catch(function (e) { toast(e.message, true); })
      .then(function () { if (button) button.disabled = false; });
  }

  // ---- state --------------------------------------------------------------

  var state = { me: null, users: [], apps: [] };
  var selected = null;   // id of the user shown in the drawer
  var server = '';

  function load() {
    return api('GET', '/api/admin/state').then(function (s) {
      state = s;
      if (selected != null && !userById(selected)) selected = null;
      render();
    });
  }

  function userById(id) { return state.users.filter(function (u) { return u.id === id; })[0]; }
  function appById(id) { return state.apps.filter(function (a) { return a.id === id; })[0]; }

  // ---- sign in ------------------------------------------------------------

  function showSignin(message) {
    $('shell').hidden = true;
    closeDialog();
    $('signin').hidden = false;
    $('in-server').value = recall('server') || 'http://127.0.0.1:8088';
    $('in-user').value = recall('user');
    $('in-pass').value = '';
    $('in-code').value = '';
    $('code-row').hidden = true;
    var err = $('signin-error');
    err.hidden = !message;
    err.textContent = message || '';
    ($('in-user').value ? $('in-pass') : $('in-user')).focus();
  }

  $('signin-form').addEventListener('submit', function (ev) {
    ev.preventDefault();
    var err = $('signin-error');
    err.hidden = true;
    var btn = $('signin-btn');
    btn.disabled = true;
    invoke('set_server', { url: $('in-server').value })
      .then(function (url) {
        server = url;
        return api('POST', '/api/login', {
          username: $('in-user').value,
          password: $('in-pass').value,
          code: $('in-code').value || null
        });
      })
      .then(function () {
        remember('server', server);
        remember('user', $('in-user').value.trim());
        return load();
      })
      .then(function () {
        $('signin').hidden = true;
        $('shell').hidden = false;
      })
      .catch(function (e) {
        if (e.data && e.data.need_code) {
          $('code-row').hidden = false;
          $('in-code').focus();
        }
        err.textContent = e.message || String(e);
        err.hidden = false;
      })
      .then(function () { btn.disabled = false; });
  });

  $('signout').addEventListener('click', function () {
    api('POST', '/api/logout').catch(function () { /* signing out regardless */ })
      .then(function () { selected = null; showSignin(); });
  });

  // ---- navigation ---------------------------------------------------------

  Array.prototype.forEach.call(document.querySelectorAll('.nav'), function (b) {
    b.addEventListener('click', function () {
      Array.prototype.forEach.call(document.querySelectorAll('.nav'), function (x) { x.classList.toggle('on', x === b); });
      $('view-users').hidden = b.dataset.view !== 'users';
      $('view-apps').hidden = b.dataset.view !== 'apps';
      if (b.dataset.view !== 'users') { selected = null; renderDrawer(); renderUsers(); }
    });
  });

  Array.prototype.forEach.call(document.querySelectorAll('[data-refresh]'), function (b) {
    b.addEventListener('click', function () { run(b, function () { return load().then(function () { toast('Up to date'); }); }); });
  });

  $('user-filter').addEventListener('input', renderUsers);

  // ---- rendering ----------------------------------------------------------

  function render() {
    $('who').textContent = state.me.username;
    $('where').textContent = server.replace(/^https?:\/\//, '');
    $('n-users').textContent = state.users.length;
    $('n-apps').textContent = state.apps.length;
    renderUsers();
    renderApps();
    renderDrawer();
  }

  function renderUsers() {
    var q = $('user-filter').value.trim().toLowerCase();
    var rows = $('user-rows');
    rows.replaceChildren();
    var shown = state.users.filter(function (u) {
      return !q || u.username.toLowerCase().indexOf(q) >= 0 || u.win_username.toLowerCase().indexOf(q) >= 0;
    });
    shown.forEach(function (u) {
      var apps = u.app_ids.map(appById).filter(Boolean);
      var open = function () { selected = u.id; renderUsers(); renderDrawer(); };
      rows.append(h('tr', {
        class: 'pick' + (u.id === selected ? ' sel' : ''), tabIndex: 0, onclick: open,
        onkeydown: function (ev) { if (ev.key === 'Enter') open(); }
      },
        h('td', { class: 'name' }, u.username,
          u.is_admin && h('span', { class: 'pill' }, 'Admin'),
          u.totp && h('span', { class: 'pill ok' }, '2-step'),
          u.locked && h('span', { class: 'pill warn' }, 'Locked')),
        h('td', { class: 'dim' }, u.win_username),
        h('td', {}, apps.length
          ? apps.map(function (a) { return h('span', { class: 'pill plain' }, a.name); })
          : h('span', { class: 'dim' }, 'None')),
        h('td', { class: 'end dim' }, '›')));
    });
    var empty = $('users-empty');
    empty.hidden = shown.length > 0;
    empty.textContent = q ? 'No user matches “' + q + '”.' : 'No users yet.';
  }

  function renderApps() {
    var rows = $('app-rows');
    rows.replaceChildren();
    state.apps.forEach(function (a) {
      var count = state.users.filter(function (u) { return u.app_ids.indexOf(a.id) >= 0; }).length;
      rows.append(h('tr', {},
        h('td', { class: 'name' }, a.name),
        h('td', {}, h('code', {}, a.key)),
        h('td', { class: 'dim' }, a.hostname + ':' + a.port),
        h('td', { class: 'dim' }, count === 1 ? '1 user' : count + ' users'),
        h('td', { class: 'end' }, h('button', {
          class: 'btn danger small',
          onclick: function () {
            confirmDialog('Delete ' + a.name + '?',
              count ? 'It will disappear for the ' + (count === 1 ? 'user' : count + ' users') + ' who can open it now.'
                    : 'No user has it assigned.',
              'Delete app',
              function () { return api('DELETE', '/api/admin/apps/' + a.id).then(load).then(function () { toast('App deleted'); }); });
          }
        }, 'Delete'))));
    });
    $('apps-empty').hidden = state.apps.length > 0;
  }

  // ---- user drawer --------------------------------------------------------

  function renderDrawer() {
    var d = $('drawer');
    var u = selected == null ? null : userById(selected);
    d.hidden = !u;
    d.replaceChildren();
    if (!u) return;
    var isMe = u.id === state.me.id;

    d.append(h('div', { class: 'drawer-head' },
      h('div', {},
        h('h1', {}, u.username),
        h('p', { class: 'hint' }, (u.is_admin ? 'Administrator' : 'User') + (isMe ? ' · you' : ''))),
      h('button', { class: 'btn ghost small', onclick: function () { selected = null; renderUsers(); renderDrawer(); } }, 'Close')));

    // Apps: each tick saves straight away.
    var appsBox = h('section', {}, h('h2', {}, 'Apps this user can open'));
    if (!state.apps.length) appsBox.append(h('p', { class: 'hint' }, 'No apps exist yet. Add one under Apps.'));
    state.apps.forEach(function (a) {
      var box = h('input', {
        type: 'checkbox', checked: u.app_ids.indexOf(a.id) >= 0,
        onchange: function () {
          var ids = u.app_ids.filter(function (i) { return i !== a.id; });
          if (box.checked) ids.push(a.id);
          run(box, function () {
            return api('PUT', '/api/admin/users/' + u.id + '/apps', { app_ids: ids })
              .then(load).then(function () { toast(box.checked ? a.name + ' allowed' : a.name + ' removed'); })
              .catch(function (e) { box.checked = !box.checked; throw e; });
          });
        }
      });
      appsBox.append(h('label', { class: 'check' }, box, a.name, h('small', {}, a.hostname)));
    });
    d.append(appsBox);

    // Locked out
    if (u.locked || u.failed_count > 0) {
      d.append(h('section', {},
        h('h2', {}, 'Sign-in attempts'),
        h('div', { class: 'status-line' },
          h('span', {}, u.locked ? 'Locked after too many wrong passwords.' : u.failed_count + ' wrong password' + (u.failed_count === 1 ? '' : 's') + ' so far.'),
          h('button', {
            class: 'btn small',
            onclick: function (ev) {
              run(ev.target, function () {
                return api('POST', '/api/admin/users/' + u.id + '/unlock').then(load).then(function () { toast(u.locked ? 'Account unlocked' : 'Counter reset'); });
              });
            }
          }, u.locked ? 'Unlock' : 'Reset'))));
    }

    // Portal password
    var pass = h('input', { type: 'password', placeholder: 'New password', autocomplete: 'new-password' });
    d.append(h('section', {},
      h('h2', {}, 'Portal password'),
      pass,
      h('p', { class: 'hint' }, isMe ? 'Changes the password you sign in with.' : 'The user is signed out of the portal everywhere.'),
      h('button', {
        class: 'btn row-btn',
        onclick: function (ev) {
          if (!pass.value) { toast('Type the new password first.', true); pass.focus(); return; }
          run(ev.target, function () {
            return api('PUT', '/api/admin/users/' + u.id + '/password', { password: pass.value })
              .then(function () { pass.value = ''; toast('Password changed'); return load(); });
          });
        }
      }, 'Set password')));

    // Windows login
    var wu = h('input', { value: u.win_username, spellcheck: false });
    var wp = h('input', { type: 'password', placeholder: 'Windows password', autocomplete: 'new-password' });
    d.append(h('section', {},
      h('h2', {}, 'Windows login'),
      h('label', {}, 'Windows username', wu),
      h('label', {}, 'Windows password', wp),
      h('p', { class: 'hint' }, 'The account on the RDP host this user is signed in as. The password is stored encrypted and never shown again.'),
      h('button', {
        class: 'btn row-btn',
        onclick: function (ev) {
          if (!wu.value.trim() || !wp.value) { toast('Enter both the Windows username and password.', true); return; }
          run(ev.target, function () {
            return api('PUT', '/api/admin/users/' + u.id + '/windows', { win_username: wu.value, win_password: wp.value })
              .then(load).then(function () { toast('Windows login saved'); });
          });
        }
      }, 'Save Windows login')));

    // Two-step verification
    d.append(h('section', {},
      h('h2', {}, 'Two-step verification'),
      h('div', { class: 'status-line' },
        h('span', {}, u.totp ? 'On. A code is asked at every sign-in.' : 'Off.'),
        u.totp
          ? h('span', {},
              h('button', { class: 'btn small', onclick: function (ev) { setTotp(ev.target, u, true, 'New code set up'); } }, 'Reset'), ' ',
              h('button', { class: 'btn small danger', onclick: function (ev) { setTotp(ev.target, u, false, 'Two-step verification turned off'); } }, 'Turn off'))
          : h('button', { class: 'btn small', onclick: function (ev) { setTotp(ev.target, u, true, 'Two-step verification turned on'); } }, 'Turn on'))));

    // Delete
    if (!isMe) {
      d.append(h('section', {},
        h('button', {
          class: 'btn danger',
          onclick: function () {
            confirmDialog('Delete ' + u.username + '?',
              'Their portal account and app permissions are removed. The Windows account on the host is not touched.',
              'Delete user',
              function () {
                return api('DELETE', '/api/admin/users/' + u.id).then(function () { selected = null; return load(); })
                  .then(function () { toast('User deleted'); });
              });
          }
        }, 'Delete user')));
    }
  }

  function setTotp(button, u, enabled, done) {
    run(button, function () {
      return api('PUT', '/api/admin/users/' + u.id + '/totp', { enabled: enabled }).then(function (res) {
        return load().then(function () {
          if (res.totp) secretDialog(u.username, res.totp); else toast(done);
        });
      });
    });
  }

  // ---- dialogs ------------------------------------------------------------

  function openDialog() {
    var d = $('dialog');
    d.replaceChildren.apply(d, arguments);
    $('scrim').hidden = false;
    var first = d.querySelector('input, button');
    if (first) first.focus();
  }
  function closeDialog() { $('scrim').hidden = true; $('dialog').replaceChildren(); }

  $('scrim').addEventListener('mousedown', function (ev) { if (ev.target === $('scrim') && !$('dialog').dataset.sticky) closeDialog(); });
  document.addEventListener('keydown', function (ev) {
    if (ev.key === 'Escape' && !$('scrim').hidden && !$('dialog').dataset.sticky) closeDialog();
  });

  function dialogError() { return h('p', { class: 'error', role: 'alert', hidden: true }); }

  /** Run a dialog's action; keep the dialog open and show the reason on failure. */
  function submit(button, errorEl, action) {
    errorEl.hidden = true;
    button.disabled = true;
    Promise.resolve().then(action).then(null, function (e) {
      errorEl.textContent = e.message;
      errorEl.hidden = false;
    }).then(function () { button.disabled = false; });
  }

  function confirmDialog(title, detail, actionLabel, action) {
    delete $('dialog').dataset.sticky;
    var err = dialogError();
    var go = h('button', { class: 'btn danger', onclick: function () { submit(go, err, function () { return action().then(closeDialog); }); } }, actionLabel);
    openDialog(h('h1', {}, title), h('p', { class: 'hint' }, detail), err,
      h('div', { class: 'actions' }, h('button', { class: 'btn ghost', onclick: closeDialog }, 'Cancel'), go));
  }

  function copyRow(label, value) {
    var btn = h('button', {
      class: 'btn small',
      onclick: function () {
        navigator.clipboard.writeText(value).then(function () {
          btn.textContent = 'Copied';
          setTimeout(function () { btn.textContent = 'Copy'; }, 1500);
        }, function () { toast('Could not copy. Select the text and copy it by hand.', true); });
      }
    }, 'Copy');
    return h('div', {}, h('label', {}, label), h('div', { class: 'secret' }, h('code', { class: 'selectable' }, value), btn));
  }

  /** The authenticator secret is shown once; the dialog only closes on purpose. */
  function secretDialog(username, totp) {
    $('dialog').dataset.sticky = '1';
    openDialog(
      h('h1', {}, 'Set up ' + username + '’s authenticator'),
      h('p', { class: 'hint' }, 'Add this key to an authenticator app now. It is not shown again; if it is lost, reset two-step verification for this user.'),
      copyRow('Setup key', totp.secret),
      copyRow('Or the full setup link', totp.uri),
      h('div', { class: 'actions' }, h('button', {
        class: 'btn primary',
        onclick: function () { delete $('dialog').dataset.sticky; closeDialog(); }
      }, 'I’ve saved it')));
  }

  $('new-user').addEventListener('click', function () {
    delete $('dialog').dataset.sticky;
    var name = h('input', { spellcheck: false });
    var pass = h('input', { type: 'password', autocomplete: 'new-password' });
    var wu = h('input', { spellcheck: false, placeholder: 'Same as username' });
    var wp = h('input', { type: 'password', autocomplete: 'new-password', placeholder: 'Leave empty to set later' });
    var admin = h('input', { type: 'checkbox' });
    var totp = h('input', { type: 'checkbox' });
    var appBoxes = state.apps.map(function (a) {
      var box = h('input', { type: 'checkbox', value: String(a.id) });
      return { id: a.id, box: box, row: h('label', { class: 'check' }, box, a.name) };
    });
    var err = dialogError();
    var go = h('button', { class: 'btn primary' }, 'Create user');

    var form = h('form', {
      autocomplete: 'off',
      onsubmit: function (ev) {
        ev.preventDefault();
        submit(go, err, function () {
          return api('POST', '/api/admin/users', {
            username: name.value, password: pass.value,
            win_username: wu.value, win_password: wp.value,
            is_admin: admin.checked, enable_totp: totp.checked,
            app_ids: appBoxes.filter(function (b) { return b.box.checked; }).map(function (b) { return b.id; })
          }).then(function (res) {
            selected = res.id;
            return load().then(function () {
              if (res.totp) secretDialog(name.value.trim(), res.totp);
              else { closeDialog(); toast('User created'); }
            });
          });
        });
      }
    },
      h('h1', {}, 'New user'),
      h('div', { class: 'two' },
        h('label', {}, 'Username', name),
        h('label', {}, 'Portal password', pass)),
      h('div', { class: 'group' },
        h('h2', {}, 'Windows login on the RDP host'),
        h('div', { class: 'two' },
          h('label', {}, 'Windows username', wu),
          h('label', {}, 'Windows password', wp))),
      h('div', { class: 'group' },
        h('h2', {}, 'Apps'),
        appBoxes.length ? appBoxes.map(function (b) { return b.row; }) : h('p', { class: 'hint' }, 'No apps exist yet.')),
      h('div', { class: 'group' },
        h('label', { class: 'check' }, totp, 'Ask for a two-step verification code'),
        h('label', { class: 'check' }, admin, 'Administrator', h('small', {}, 'can manage users and apps'))),
      err,
      h('div', { class: 'actions' },
        h('button', { class: 'btn ghost', type: 'button', onclick: closeDialog }, 'Cancel'), go));
    openDialog(form);
  });

  $('new-app').addEventListener('click', function () {
    delete $('dialog').dataset.sticky;
    var name = h('input', { placeholder: 'Miracle' });
    var key = h('input', { placeholder: 'miracle', spellcheck: false });
    var host = h('input', { value: '127.0.0.1', spellcheck: false });
    var port = h('input', { placeholder: '3389', inputMode: 'numeric' });
    var typedKey = false;
    key.addEventListener('input', function () { typedKey = true; });
    name.addEventListener('input', function () {
      if (!typedKey) key.value = name.value.trim().toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
    });
    var err = dialogError();
    var go = h('button', { class: 'btn primary' }, 'Create app');

    openDialog(h('form', {
      autocomplete: 'off',
      onsubmit: function (ev) {
        ev.preventDefault();
        submit(go, err, function () {
          return api('POST', '/api/admin/apps', { name: name.value, key: key.value, hostname: host.value, port: port.value })
            .then(load).then(function () { closeDialog(); toast('App created'); });
        });
      }
    },
      h('h1', {}, 'New app'),
      h('p', { class: 'hint' }, 'An app is an RDP connection to a host, opened as the user’s Windows account.'),
      h('div', { class: 'two' },
        h('label', {}, 'Name', name),
        h('label', {}, 'Key (used in the link)', key),
        h('label', {}, 'RDP host', host),
        h('label', {}, 'Port', port)),
      err,
      h('div', { class: 'actions' },
        h('button', { class: 'btn ghost', type: 'button', onclick: closeDialog }, 'Cancel'), go)));
  });

  // ---- start --------------------------------------------------------------

  showSignin();
})();
