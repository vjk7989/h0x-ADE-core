// Placeholder panel (the real one is REM-4): pair from #pair=, keep the
// session, show the terminals and the notices.
const out = document.getElementById('out');
const log = document.getElementById('log');
const say = (text) => {
  const p = document.createElement('p');
  p.textContent = text;
  log.prepend(p);
};
const pair = new URLSearchParams(location.hash.slice(1)).get('pair');
// The one-time token leaves the address bar (and the history) at once.
history.replaceState(null, '', location.pathname);
const ws = new WebSocket(`ws://${location.host}/ws`);
const send = (name, data) => ws.send(JSON.stringify({ name, data }));
ws.onopen = () => {
  const session = localStorage.getItem('h0x-session');
  if (pair) send('pair', { token: pair, label: navigator.platform || 'browser' });
  else if (session) send('auth', { session });
  else say('Not paired: open the pairing link that h0x-core bridge web prints.');
};
ws.onmessage = (event) => {
  const { name, data } = JSON.parse(event.data);
  if (name === 'paired') localStorage.setItem('h0x-session', data.session);
  if (name === 'snapshot') out.textContent = data.lines.join('\n') || 'no live terminals';
  if (name === 'notice') say(`${data.terminal} ${data.kind}: ${data.excerpt}`);
  if (name === 'error') say(data.message);
  if (name === 'reply') say(data.text || data.what || data.message || data.kind);
};
ws.onclose = () => say('Disconnected; reload to reconnect.');
