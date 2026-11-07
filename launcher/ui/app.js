const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);
const status = (msg, kind = '') => { $('status').textContent = msg; $('status').className = kind; };

let declined = false;
let dxwLinks = [];
let projectLinks = [];
let dxwInstalled = false;
let mods = [];

// ------------------------------------------------------------------ tabs

const tabs = [...document.querySelectorAll('[role=tab]')];
function selectTab(tab, focus = true) {
  for (const t of tabs) {
    const on = t === tab;
    t.setAttribute('aria-selected', on);
    t.tabIndex = on ? 0 : -1;
    $(t.getAttribute('aria-controls')).hidden = !on;
  }
  if (focus) tab.focus();
  if (tab.id === 'tab-play') renderSummary();
  if (tab.id === 'tab-mods') refreshMods();
}
tabs.forEach((t, i) => {
  t.onclick = () => selectTab(t);
  t.onkeydown = (e) => {
    const next = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: tabs.length - 1 }[e.key];
    if (next === undefined) return;
    e.preventDefault();
    selectTab(tabs[(next + tabs.length) % tabs.length]);
  };
});

// ------------------------------------------------------------------ settings

const current = () => ({
  game_dir: $('dir').value.trim(),
  width: +$('w').value,
  height: +$('h').value,
  fullscreen: $('display').value !== 'windowed',
  borderless: $('display').value === 'borderless',
  dxwrapper_declined: declined,
  method: document.querySelector('input[name=method]:checked')?.value ?? 'inject',
  ui_scale: +$('scale').value,
  shadow_res: +$('shadows').value,
});

let saveTimer;
function saveSoon() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => invoke('save_settings', { settings: current() }).catch((e) => status(String(e), 'err')), 300);
}
document.querySelectorAll('#panel-settings input, #panel-settings select').forEach((el) => {
  el.addEventListener('change', saveSoon);
});

$('native').onclick = async () => {
  const [w, h] = await invoke('get_native_resolution');
  $('w').value = w;
  $('h').value = h;
  saveSoon();
};

// ------------------------------------------------------------------ play summary

const DISPLAY = { exclusive: 'Fullscreen (exclusive)', borderless: 'Borderless window', windowed: 'Windowed' };
function renderSummary() {
  const s = current();
  const scale = s.ui_scale > 1 && s.fullscreen ? `${Math.round(s.ui_scale * 100)}%` : 'Off';
  const shadows = s.shadow_res ? `${s.shadow_res} px` : 'Original method';
  const enabledMods = mods.filter((m) => m.enabled).length;
  const rows = [
    ['Game', s.game_dir || 'not found: set it in Settings', !s.game_dir],
    ['Resolution', `${s.width} × ${s.height}`],
    ['Display', DISPLAY[$('display').value]],
    ['UI scale', scale],
    ['Shadows', shadows],
    ['Renderer', dxwInstalled ? 'dxwrapper (Direct3D 9)' : 'Windows Direct3D 7 (max 2048 px wide)', !dxwInstalled && s.width > 2048],
    ['Mods', enabledMods ? `${enabledMods} enabled` : 'none'],
  ];
  $('summary').replaceChildren(...rows.flatMap(([k, v, warn]) => {
    const dt = document.createElement('dt'); dt.textContent = k;
    const dd = document.createElement('dd'); dd.textContent = v; if (warn) dd.className = 'warn';
    return [dt, dd];
  }));
}

$('play').onclick = async () => {
  const settings = current();
  // Above 2048 px wide the native renderer can't work; ask again even if declined before.
  if (settings.width > 2048 && !dxwInstalled && !(await askDxwrapper())) {
    status('Without dxwrapper, widths above 2048 crash. Pick 1920x1080 or install it.', 'err');
    return;
  }
  $('play').disabled = true;
  try {
    await invoke('launch', { settings: current() });
    status(`Launched at ${settings.width}×${settings.height}`, 'ok');
  } catch (e) {
    status(String(e), 'err');
  } finally {
    setTimeout(() => { $('play').disabled = false; }, 1500);
  }
};

// ------------------------------------------------------------------ mods

function fmtSize(n) {
  return n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`;
}

async function refreshMods() {
  try {
    mods = await invoke('list_mods');
  } catch (e) {
    status(String(e), 'err');
    return;
  }
  const enabled = mods.filter((m) => m.enabled).length;
  $('mod-count').hidden = enabled === 0;
  $('mod-count').textContent = enabled;
  if (!mods.length) {
    const li = document.createElement('li');
    li.className = 'empty';
    li.textContent = 'No mods installed yet.';
    $('mods').replaceChildren(li);
    return;
  }
  $('mods').replaceChildren(...mods.map((m) => {
    const li = document.createElement('li');
    if (!m.enabled) li.className = 'off';
    const sw = document.createElement('label');
    sw.className = 'switch';
    sw.title = m.enabled ? 'Enabled' : 'Disabled';
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = m.enabled;
    cb.setAttribute('aria-label', `Enable ${m.file}`);
    cb.onchange = async () => {
      let trusted = false;
      if (m.kind === 'code' && cb.checked) {
        trusted = await askTrust(m.file);
        if (!trusted) { cb.checked = false; return; }
      }
      try { await invoke('set_mod_enabled', { file: m.file, enabled: cb.checked, trusted }); } catch (e) { status(String(e), 'err'); }
      refreshMods();
    };
    sw.append(cb, document.createElement('span'));
    const name = document.createElement('span'); name.className = 'name'; name.textContent = m.file; name.title = m.file;
    const badge = document.createElement('span');
    badge.className = m.kind === 'code' ? 'badge code' : 'badge';
    badge.textContent = m.kind;
    if (m.kind === 'code') badge.title = 'Runs code inside the game';
    const meta = document.createElement('span'); meta.className = 'meta'; meta.textContent = fmtSize(m.size);
    const del = document.createElement('button');
    del.type = 'button'; del.className = 'btn small danger'; del.textContent = 'Remove';
    del.setAttribute('aria-label', `Remove ${m.file}`);
    del.onclick = async () => {
      if (del.dataset.armed !== '1') { del.dataset.armed = '1'; del.textContent = 'Confirm'; setTimeout(() => { del.dataset.armed = ''; del.textContent = 'Remove'; }, 3000); return; }
      try { await invoke('remove_mod', { file: m.file }); status(`Removed ${m.file}`); } catch (e) { status(String(e), 'err'); }
      refreshMods();
    };
    li.append(sw, name, badge, meta, del);
    return li;
  }));
}

async function installDropped(paths) {
  status('Installing…');
  try {
    const r = await invoke('install_mods', { paths });
    const parts = [];
    if (r.installed.length) parts.push(`Installed ${r.installed.join(', ')}`);
    if (r.needs_trust.length) parts.push(`Installed ${r.needs_trust.join(', ')} (disabled: code mods need you to switch them on)`);
    for (const [file, why] of r.skipped) parts.push(`${file}: ${why}`);
    status(parts.join(' · ') || 'Nothing to install', r.skipped.length ? 'err' : 'ok');
  } catch (e) {
    status(String(e), 'err');
  }
  refreshMods();
}

// File drag-and-drop comes from the window (the OS gives us real paths, unlike the DOM).
window.__TAURI__.webview.getCurrentWebview().onDragDropEvent((event) => {
  const t = event.payload.type;
  if (t === 'enter' || t === 'over') {
    $('drop').classList.add('over');
    if (t === 'enter' && $('tab-mods').getAttribute('aria-selected') !== 'true') selectTab($('tab-mods'), false);
  } else {
    $('drop').classList.remove('over');
    if (t === 'drop' && event.payload.paths?.length) installDropped(event.payload.paths);
  }
});

$('open-mods').onclick = () => invoke('open_mods_folder').catch((e) => status(String(e), 'err'));

// Resolves true only if the user explicitly trusts this code mod.
function askTrust(file) {
  $('trust-name').textContent = file;
  $('trust').hidden = false;
  $('trust-no').focus(); // the safe choice is the default
  return new Promise((resolve) => {
    const done = (v) => { $('trust').hidden = true; resolve(v); };
    $('trust-yes').onclick = () => done(true);
    $('trust-no').onclick = () => done(false);
    $('trust').onkeydown = (e) => { if (e.key === 'Escape') done(false); };
  });
}

// ------------------------------------------------------------------ dxwrapper

// Resolves true if dxwrapper ended up installed, false if the user said "not now".
function askDxwrapper() {
  $('consent').hidden = false;
  $('download').focus();
  return new Promise((resolve) => {
    $('download').onclick = async () => {
      $('download').disabled = $('skip').disabled = true;
      $('download').textContent = 'Downloading…';
      try {
        await invoke('install_dxwrapper');
        dxwInstalled = true;
        status('dxwrapper installed.', 'ok');
        resolve(true);
      } catch (e) {
        status(String(e), 'err');
        resolve(false);
      } finally {
        $('consent').hidden = true;
        $('download').disabled = $('skip').disabled = false;
        $('download').textContent = 'Download';
        renderDxw();
        renderSummary();
      }
    };
    $('skip').onclick = async () => {
      declined = true;
      $('consent').hidden = true;
      await invoke('save_settings', { settings: current() }).catch(() => {});
      resolve(false);
    };
  });
}

function renderDxw() {
  $('dxw-status').textContent = dxwInstalled
    ? 'dxwrapper is installed: Direct3D 7 is translated to Direct3D 9, so resolutions above 2048 px and all performance fixes are available.'
    : 'dxwrapper is not installed: the game uses Windows\' own Direct3D 7, limited to 2048 px wide and without most performance fixes.';
  $('dxw-install').hidden = dxwInstalled;
}
$('dxw-install').onclick = () => askDxwrapper();

document.querySelectorAll('[data-dxw-link]').forEach((a) => {
  a.onclick = (ev) => { ev.preventDefault(); invoke('open_link', { url: dxwLinks[+a.dataset.dxwLink] }); };
});
document.querySelectorAll('[data-link]').forEach((a) => {
  a.onclick = (ev) => { ev.preventDefault(); invoke('open_link', { url: projectLinks[+a.dataset.link] }); };
});

// ------------------------------------------------------------------ start

async function init() {
  dxwLinks = await invoke('dxwrapper_links');
  const info = await invoke('app_info');
  projectLinks = info.links;
  $('version').textContent = `v${info.version}`;
  const s = await invoke('load_settings');
  $('dir').value = s.game_dir;
  $('w').value = s.width;
  $('h').value = s.height;
  $('display').value = !s.fullscreen ? 'windowed' : s.borderless ? 'borderless' : 'exclusive';
  $('scale').value = String(s.ui_scale);
  $('shadows').value = String(s.shadow_res);
  document.querySelector(`input[name=method][value=${s.method}]`).checked = true;
  declined = s.dxwrapper_declined;
  dxwInstalled = await invoke('dxwrapper_installed');
  renderDxw();
  await refreshMods();
  renderSummary();
  if (!s.game_dir) status('Dungeon Siege not found: set the game folder in Settings.', 'err');
  if (!declined && !dxwInstalled) await askDxwrapper();
}

init();
