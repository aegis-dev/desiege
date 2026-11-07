const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);
const status = (msg, err = false) => { $('status').textContent = msg; $('status').className = err ? 'err' : ''; };

let declined = false;
let links = [];

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
        status('dxwrapper installed.');
        resolve(true);
      } catch (e) {
        status(String(e), true);
        resolve(false);
      } finally {
        $('consent').hidden = true;
        $('download').disabled = $('skip').disabled = false;
        $('download').textContent = 'Download';
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

document.querySelectorAll('[data-link]').forEach((a) => {
  a.onclick = (ev) => { ev.preventDefault(); invoke('open_link', { url: links[+a.dataset.link] }); };
});

async function init() {
  links = await invoke('dxwrapper_links');
  const s = await invoke('load_settings');
  $('dir').value = s.game_dir;
  $('w').value = s.width;
  $('h').value = s.height;
  $('display').value = !s.fullscreen ? 'windowed' : s.borderless ? 'borderless' : 'exclusive';
  $('scale').value = String(s.ui_scale);
  $('shadows').value = String(s.shadow_res);
  document.querySelector(`input[name=method][value=${s.method}]`).checked = true;
  declined = s.dxwrapper_declined;
  if (!s.game_dir) status('Dungeon Siege not found, enter the game folder.', true);
  if (!declined && !(await invoke('dxwrapper_installed'))) await askDxwrapper();
}

$('native').onclick = async () => {
  const [w, h] = await invoke('get_native_resolution');
  $('w').value = w;
  $('h').value = h;
};

$('play').onclick = async () => {
  const settings = current();
  // Above 2048 px wide the native renderer can't work; ask again even if declined before.
  if (settings.width > 2048 && !(await invoke('dxwrapper_installed')) && !(await askDxwrapper())) {
    status('Without dxwrapper, widths above 2048 crash. Pick 1920x1080 or install it.', true);
    return;
  }
  try {
    await invoke('launch', { settings });
    status(`Launched at ${settings.width}x${settings.height}`);
  } catch (e) {
    status(String(e), true);
  }
};

init();
