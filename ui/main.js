const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const els = {
  toggle: $('toggle'), pill: $('pill'), hint: $('hint'), liveBox: $('liveBox'),
  mainUrl: $('mainUrl'), copyBtn: $('copyBtn'), qr: $('qr'), viewers: $('viewers'),
  fps: $('fps'), latency: $('latency'), bw: $('bw'), resLine: $('resLine'), otherBox: $('otherBox'), otherUrls: $('otherUrls'),
  error: $('error'), monitor: $('monitor'), fpsSel: $('fpsSel'), resSel: $('resSel'),
  quality: $('quality'), qualityVal: $('qualityVal'), port: $('port'),
  previewToggle: $('previewToggle'), preview: $('preview'),
};

const STORE_KEY = 'local-stream-settings-v3';
const defaults = { monitorId: null, fps: 60, quality: 75, maxWidth: 1920, port: 8080, preview: false };

function loadPrefs() {
  try { return { ...defaults, ...JSON.parse(localStorage.getItem(STORE_KEY) || '{}') }; }
  catch { return { ...defaults }; }
}
function savePrefs(p) {
  try { localStorage.setItem(STORE_KEY, JSON.stringify(p)); } catch {}
}

let prefs = loadPrefs();
let running = false;
let busy = false;
let currentPort = null;
let lastUrlsKey = '';

function readForm() {
  const m = els.monitor.value;
  prefs = {
    ...prefs,
    monitorId: m === '' ? null : Number(m),
    fps: Number(els.fpsSel.value),
    quality: Number(els.quality.value),
    maxWidth: Number(els.resSel.value),
    port: Math.min(65000, Math.max(1024, Number(els.port.value) || 8080)),
    preview: els.previewToggle.checked,
  };
  savePrefs(prefs);
  return prefs;
}

async function pushSettings() {
  const p = readForm();
  els.qualityVal.textContent = `(${p.quality})`;
  await invoke('update_settings', {
    settings: { monitorId: p.monitorId, fps: p.fps, quality: p.quality, maxWidth: p.maxWidth },
  });
}

function setError(text) {
  els.error.hidden = !text;
  els.error.textContent = text || '';
}

async function loadMonitors() {
  try {
    const list = await invoke('list_monitors');
    els.monitor.innerHTML = '';
    list.forEach((m, i) => {
      const o = document.createElement('option');
      o.value = String(m.id);
      o.textContent = `${m.primary ? '★ ' : ''}${m.name || 'Display ' + (i + 1)} — ${m.width}×${m.height}`;
      els.monitor.appendChild(o);
    });
    const wanted = list.find((m) => m.id === prefs.monitorId) || list.find((m) => m.primary) || list[0];
    if (wanted) els.monitor.value = String(wanted.id);
  } catch (e) {
    setError('Could not list displays: ' + e);
  }
}

function applyFormFromPrefs() {
  els.fpsSel.value = String(prefs.fps);
  els.resSel.value = String(prefs.maxWidth);
  els.quality.value = String(prefs.quality);
  els.qualityVal.textContent = `(${prefs.quality})`;
  els.port.value = String(prefs.port);
  els.previewToggle.checked = !!prefs.preview;
}

function updatePreview() {
  const show = running && els.previewToggle.checked && currentPort;
  els.preview.hidden = !show;
  const want = show ? `http://127.0.0.1:${currentPort}/?embed=1` : '';
  if (show && els.preview.getAttribute('src') !== want) els.preview.src = want;
  if (!show && els.preview.getAttribute('src')) els.preview.removeAttribute('src');
}

function render(status) {
  running = status.running;
  currentPort = status.port;
  els.pill.textContent = running ? 'Live' : 'Off';
  els.pill.className = 'pill ' + (running ? 'live' : 'off');
  els.toggle.textContent = running ? 'Stop sharing' : 'Start sharing';
  els.toggle.classList.toggle('stop', running);
  els.hint.hidden = running;
  els.liveBox.hidden = !running;
  els.port.disabled = running;

  if (running) {
    const key = status.urls.map((u) => u.url).join('|');
    if (key !== lastUrlsKey) {
      lastUrlsKey = key;
      const [main, ...others] = status.urls;
      els.mainUrl.textContent = main.url.replace(/^http:\/\//, '');
      els.mainUrl.href = main.url;
      els.mainUrl.title = main.interface;
      els.qr.innerHTML = main.qrSvg;
      els.otherBox.hidden = others.length === 0;
      els.otherUrls.innerHTML = '';
      for (const u of others) {
        const li = document.createElement('li');
        li.innerHTML = `<code></code> <small></small>`;
        li.querySelector('code').textContent = u.url;
        li.querySelector('small').textContent = u.interface;
        els.otherUrls.appendChild(li);
      }
    }
    const watching = status.viewers > 0;
    els.viewers.textContent = status.viewers;
    els.latency.textContent = watching && status.latencyMs ? `${status.latencyMs} ms` : '–';
    els.fps.textContent = watching ? status.fps.toFixed(0) : '–';
    els.bw.textContent = !watching ? '–' : status.kbps >= 1000 ? `${(status.kbps / 1000).toFixed(1)} Mb/s` : `${status.kbps} kb/s`;
    const parts = [];
    if (status.videoPeers > 0 && status.videoWidth) parts.push(`Video: ${status.videoPeers} viewer${status.videoPeers > 1 ? 's' : ''} at ${status.videoWidth}×${status.videoHeight}, ${status.encoder || 'encoder'} ${status.encodeMs.toFixed(0)} ms`);
    const sharp = status.viewers - status.videoPeers;
    if (sharp > 0 && status.width) parts.push(`Sharp: ${sharp} at ${status.width}×${status.height}`);
    els.resLine.textContent = parts.join(' · ');
    setError(status.error);
  } else {
    lastUrlsKey = '';
  }
  updatePreview();
}

async function refresh() {
  try { render(await invoke('get_status')); } catch (e) { console.error(e); }
}

els.toggle.addEventListener('click', async () => {
  if (busy) return;
  busy = true;
  els.toggle.disabled = true;
  setError('');
  try {
    if (running) {
      await invoke('stop_sharing');
      await refresh();
    } else {
      await pushSettings();
      const status = await invoke('start_sharing', { port: prefs.port });
      render(status);
    }
  } catch (e) {
    setError(String(e));
  } finally {
    busy = false;
    els.toggle.disabled = false;
  }
});

// Clicking the URL in the app copies it (links can't open an external browser without extra plugins).
els.mainUrl.addEventListener('click', (e) => { e.preventDefault(); copy(els.mainUrl.href.replace(/\/$/, '')); });
els.copyBtn.addEventListener('click', () => copy(els.mainUrl.href.replace(/\/$/, '')));

async function copy(text) {
  let ok = false;
  try { await navigator.clipboard.writeText(text); ok = true; } catch {}
  if (!ok) {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    try { ok = document.execCommand('copy'); } catch {}
    ta.remove();
  }
  els.copyBtn.textContent = ok ? 'Copied!' : 'Copy failed';
  setTimeout(() => (els.copyBtn.textContent = 'Copy'), 1400);
}

[els.monitor, els.fpsSel, els.resSel, els.quality].forEach((el) =>
  el.addEventListener(el === els.quality ? 'input' : 'change', () => pushSettings().catch(console.error))
);
els.port.addEventListener('change', readForm);
els.previewToggle.addEventListener('change', () => { readForm(); updatePreview(); });

(async function init() {
  applyFormFromPrefs();
  await loadMonitors();
  await pushSettings();
  await refresh();
  setInterval(refresh, 1000);
})();
