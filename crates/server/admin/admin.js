// The operator console's script (served by tokumai-admin as /admin.js; see admin.html).
(function(){
  const $ = id => document.getElementById(id);
  const fmt = n => (n === undefined || n === null || isNaN(n)) ? '–' : Number(n).toLocaleString('en-US');
  const usd = n => '$' + Number(n).toLocaleString('en-US', {maximumFractionDigits: 2, minimumFractionDigits: 2});
  const ago = s => s < 90 ? s + ' s' : s < 5400 ? Math.round(s/60) + ' min' : s < 172800 ? (s/3600).toFixed(1) + ' h' : Math.round(s/86400) + ' d';
  const day = ms => new Date(ms).toISOString().slice(0,10);
  // Everything a row shows is text: what the host says over SSH and what AWS answers are
  // data, and a host that answered `<img onerror=…>` for "is-active" would otherwise run
  // in this page (audit M9).
  const row = (k, v, cls) => `<div class="row"><span>${esc(k)}</span><b class="${esc(cls||'')}">${esc(v)}</b></div>`;
  // Every call to this process carries the token the page was served with: a page from
  // elsewhere (a name pointed at 127.0.0.1, a cross-site POST) has no token and gets
  // nothing (audit M10).
  const TOKEN = (document.querySelector('meta[name="tokumai-token"]') || {}).content || '';
  const api = (path, init) => fetch(path, Object.assign({}, init || {}, { headers: Object.assign({ 'x-admin-token': TOKEN }, (init && init.headers) || {}) }));
  let data = null, win = 'today';

  function pill(el, text, cls){ el.textContent = text; el.className = 'pill ' + (cls||''); }

  function render(){
    const d = data; if(!d) return;
    const errs = [];
    const h = d.enclave.health, u = d.enclave.usage, p = d.enclave.plans, host = d.host, al = d.alarm;
    $('hClock').textContent = new Date(d.clock).toUTCString().replace(' GMT',' UTC');
    $('hImage').textContent = (host.image || '') + '  ' + d.target.pcr0.slice(0,12) + '…';
    if(h.error){ errs.push('enclave: ' + h.error); pill($('hEnclave'), 'enclave unreachable', 'bad'); }
    else pill($('hEnclave'), 'enclave ' + (h.working > 0 ? h.working + ' working' : 'idle'), 'ok');
    // A sandbox image takes Apple's test purchases beside real ones: its numbers are test
    // numbers until the production image runs, and the page says so.
    $('hSandbox').hidden = !(h.appleSandbox === true);
    if(al.error){ errs.push('alarm: ' + al.error); pill($('hAlarm'), 'alarm ?', 'warn'); }
    else pill($('hAlarm'), 'alarm ' + al.state.toLowerCase(), al.state === 'OK' ? 'ok' : al.state === 'ALARM' ? 'bad' : 'warn');
    if(host.error) errs.push('host: ' + host.error);
    $('errs').innerHTML = errs.map(e => `<div class="err">${esc(e)}</div>`).join('');

    // Enclave card
    $('cEnclave').innerHTML = h.error ? '' :
      row('up', ago(h.uptimeS)) + row('working now', fmt(h.working), h.working > 40 ? 'r' : '') +
      row('replies kept', fmt(h.replies.count) + ' · ' + (h.replies.bytes/1048576).toFixed(1) + ' MiB') +
      row('cover queue', fmt(h.coverWaiting)) + row('strikes today', fmt(h.strikesToday), h.strikesToday ? 'a' : '') +
      row('pricing', h.pricingVersion) + row('stripe', h.stripe.configured ? 'keys · prices ' + (h.stripe.pricesReadMs ? ago((d.clock - h.stripe.pricesReadMs)/1000) + ' old' : 'not read') : 'none', h.stripe.configured ? 's' : '') +
      row('apple api', h.appleApi ? 'yes' : 'no') + row('mode', h.devMode ? 'DEV' : 'sealed', h.devMode ? 'r' : 's');
    // Book card
    $('cBook').innerHTML = h.error ? '' :
      row('position', 'gen ' + h.book.generation + ' · rec ' + fmt(h.book.record)) +
      row('on the host', 'gen ' + h.book.flushedGeneration + ' · rec ' + fmt(h.book.flushedRecord), (h.book.flushedGeneration === h.book.generation && h.book.flushedRecord === h.book.record) ? 's' : 'a') +
      row('since snapshot', fmt(h.book.sinceSnapshot) + ' / 2,000') + row('holds open', fmt(h.book.holdsOpen)) +
      row('replayed at start', fmt(h.replayed)) +
      (host.error ? '' : row('journal on disk', (host.journal/1024).toFixed(0) + ' KiB') + row('snapshot', host.snapshot > 0 ? ago(d.clock/1000 - host.snapshot) + ' old' : '–') + row('volume', host.bookvol, host.bookvol === 'root' ? 'r' : 's'));
    // Host card
    const svc = (name, v) => row(name, v, v === 'active' ? 's' : 'r');
    $('cHost').innerHTML = host.error ? '' :
      svc('egress', host.egress) + svc('enclave unit', host.enclave) + row('enclaves running', host.running, host.running === '1' ? 's' : 'r') +
      row('pulse age', ago(+host.pulseage), +host.pulseage > 300 ? 'r' : 's') + svc('pulse watch', host.pulsewatch) + svc('pulse timer', host.pulsetimer) +
      row('host up', ago(+host.uptime)) + row('disk', host.disk) + row('memory', host.mem) + row('ip', host.ip);
    // Alarm card
    $('cAlarm').innerHTML = (al.error ? '' : row('state', al.state, al.state === 'OK' ? 's' : 'r') + row('since', (al.since||'').slice(0,16)) + `<div class="hint">${esc(al.reason||'')}</div>`) +
      (h.error ? '' : row('notes window', 'months ' + h.notesWindow[0] + '–' + h.notesWindow[1]) + row('door', (h.address||'').slice(0,10) + '…'));

    renderUsage(u, h);
    renderPlans(p, d.clock);
    $('log').textContent = host.error ? host.error : (host.log || []).join('\n');
  }

  function renderUsage(u, h){
    if(u.error){ $('kpis').innerHTML = ''; return; }
    const today = Math.floor(u.now / 86400000);
    const from = win === 'today' ? today : win === 'd7' ? today - 6 : today - 29;
    // Days from the book plus today's pending deltas, so the current day is complete.
    const by = {};
    const add = (model, kind, c) => { const k = model + '|' + kind; const r = by[k] || (by[k] = {model, kind, requests:0, declined:0, toku:0}); r.requests += c.requests||0; r.declined += c.declined||0; r.toku += c.toku||0; };
    for(const r of u.days){ if(r.day >= from) add(r.model, r.kind, r); }
    for(const pnd of u.pending || []){ const m = /^d:(\d+):([^:]+):([^:]+):(requests|toku|declined)$/.exec(pnd.key); if(!m) continue; const dd = +m[1]; if(dd < from) continue; const c = {}; c[m[4]] = pnd.n; add(m[2], m[3], c); }
    const rows = Object.values(by).sort((a,b) => b.toku - a.toku);
    const tot = rows.reduce((t, r) => ({requests: t.requests + r.requests, declined: t.declined + r.declined, toku: t.toku + r.toku}), {requests:0, declined:0, toku:0});
    const cost = t => t / u.margin / u.tokuPerUsd;
    $('kpis').innerHTML =
      `<div class="kpi"><span>requests</span><b>${fmt(tot.requests)}</b></div>` +
      `<div class="kpi"><span>TOKU spent</span><b class="g">${fmt(tot.toku)}</b></div>` +
      `<div class="kpi"><span>≈ provider cost</span><b>${usd(cost(tot.toku))}</b></div>` +
      `<div class="kpi"><span>declined</span><b class="${tot.declined ? 'r' : ''}">${fmt(tot.declined)}</b></div>` +
      `<div class="kpi"><span>accounts active today</span><b class="s">${fmt(u.activeToday)}</b></div>` +
      `<div class="kpi"><span>yesterday</span><b>${fmt(u.activeYesterday)}</b></div>`;
    $('models').querySelector('tbody').innerHTML = rows.length ? rows.map(r =>
      `<tr><td>${esc(r.model)}</td><td class="d">${r.kind}</td><td>${fmt(r.requests)}</td><td class="${r.declined?'r':'d'}">${fmt(r.declined)}</td><td class="g">${fmt(r.toku)}</td><td>${usd(cost(r.toku))}</td></tr>`).join('')
      : '<tr><td colspan="6" class="d">nothing in this window</td></tr>';
    // Hourly bars: the last 48 hours, text and pictures stacked.
    const nowH = Math.floor(u.now / 3600000);
    const hrs = Array.from({length: 48}, (_, i) => ({hour: nowH - 47 + i, t: 0, p: 0}));
    for(const r of u.hours){ const i = r.hour - (nowH - 47); if(i >= 0 && i < 48){ if(r.kind === 'picture') hrs[i].p += r.requests; else hrs[i].t += r.requests; } }
    const max = Math.max(1, ...hrs.map(x => x.t + x.p));
    const W = 960, H = 120, bw = W / 48;
    let svg = `<svg class="chart" viewBox="0 0 ${W} ${H + 18}" role="img" aria-label="requests per hour, last 48 hours">`;
    hrs.forEach((x, i) => {
      const ht = Math.round(x.t / max * H), hp = Math.round(x.p / max * H);
      const X = i * bw + 1, w = bw - 2;
      if(ht) svg += `<rect class="t" x="${X}" y="${H - ht}" width="${w}" height="${ht}" rx="1"/>`;
      if(hp) svg += `<rect class="p" x="${X}" y="${H - ht - hp}" width="${w}" height="${hp}" rx="1"/>`;
      if(i % 6 === 0) svg += `<text x="${X}" y="${H + 13}">${String(x.hour % 24).padStart(2,'0')}h</text>`;
    });
    svg += `<text x="${W}" y="10" text-anchor="end">max ${max}/h</text></svg>`;
    $('hours').innerHTML = svg;
  }

  function renderPlans(p, clock){
    if(p.error){ $('cAccounts').innerHTML = $('cPlans').innerHTML = $('cMoney').innerHTML = ''; return; }
    const plans = p.plans, active = plans.filter(x => x.active);
    const soon = active.filter(x => x.paidUntil - clock < 7 * 86400000).length;
    const month = new Date(clock); month.setUTCDate(1); month.setUTCHours(0,0,0,0);
    const newThisMonth = plans.filter(x => x.periodStart >= month.getTime()).length;
    $('cAccounts').innerHTML = row('known to the book', fmt(p.accounts)) + row('with a plan', fmt(plans.length)) + row('active plans', fmt(active.length), 's') +
      row('started this month', fmt(newThisMonth)) + row('prepaid lots', fmt(p.lots.count) + ' · ' + fmt(p.lots.left) + ' TOKU');
    const byRail = r => active.filter(x => x.rail === r).length;
    $('cPlans').innerHTML = row('App Store', fmt(byRail('appstore'))) + row('notes', fmt(byRail('note')), 'g') + row('Stripe', fmt(byRail('stripe'))) +
      row('yearly', fmt(active.filter(x => x.yearly).length)) + row('ending within 7 days', fmt(soon), soon ? 'a' : '') +
      row('disputed', fmt(plans.filter(x => x.disputed).length), plans.some(x => x.disputed) ? 'r' : '') + row('ended, still in book', fmt(plans.length - active.length), 'd');
    // Money: list price per active plan per month.
    let monthly = 0;
    for(const x of active){ const cents = p.tiers[x.tier] ? p.tiers[x.tier].cents : 0; monthly += x.yearly ? cents * 12 * 0.9 / 12 : cents; }
    $('cMoney').innerHTML = row('active plans at list', usd(monthly / 100) + ' / month', 'g') + row('per active plan', active.length ? usd(monthly / 100 / active.length) : '–') +
      row('yearly share', active.length ? Math.round(100 * active.filter(x => x.yearly).length / active.length) + ' %' : '–');
    // Per tier table
    const tb = $('tiers').querySelector('tbody'); tb.innerHTML = p.tiers.map((t, i) => {
      const of = plans.filter(x => x.tier === i), act = of.filter(x => x.active);
      return `<tr><td>${fmt(t.toku)} TOKU · ${usd(t.cents/100)}</td><td>${fmt(act.filter(x=>x.rail==='appstore').length)}</td><td class="g">${fmt(act.filter(x=>x.rail==='note').length)}</td><td>${fmt(act.filter(x=>x.rail==='stripe').length)}</td><td>${fmt(act.filter(x=>x.yearly).length)}</td><td class="s">${fmt(act.length)}</td><td>${fmt(act.filter(x => x.paidUntil - clock < 7*86400000).length)}</td><td class="${of.some(x=>x.disputed)?'r':'d'}">${fmt(of.filter(x=>x.disputed).length)}</td></tr>`;
    }).join('');
    // Notes per month and tier
    const nm = {};
    for(const n of p.notes){ const m = /^notes:(minted|spent):(\d+):(\d+)(:sandbox)?$/.exec(n.key); if(!m) continue; const k = m[2] + '|' + m[3]; const r = nm[k] || (nm[k] = {epoch:+m[2], tier:+m[3], minted:0, test:0, spent:0}); r[m[1]] += n.n; if(m[4]) r.test += n.n; }
    const nrows = Object.values(nm).sort((a,b) => b.epoch - a.epoch || a.tier - b.tier);
    const monthName = e => { const y = 2026 + Math.floor(e / 12), mo = e % 12; return y + '-' + String(mo + 1).padStart(2,'0'); };
    $('notes').querySelector('tbody').innerHTML = nrows.length ? nrows.map(r => `<tr><td>${monthName(r.epoch)}</td><td class="d">${fmt(p.tiers[r.tier] ? p.tiers[r.tier].toku : r.tier)} TOKU</td><td>${fmt(r.minted)}</td><td class="${r.test ? 'a' : 'd'}">${fmt(r.test)}</td><td class="g">${fmt(r.spent)}</td><td class="${r.minted - r.spent > 0 ? 'a' : 'd'}">${fmt(r.minted - r.spent)}</td></tr>`).join('')
      : '<tr><td colspan="6" class="d">no notes minted yet</td></tr>';
  }

  function esc(s){ return String(s === undefined || s === null ? '' : s).replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }

  async function load(){
    try{
      const r = await api('/api/state'); data = await r.json(); render();
    }catch(e){ $('errs').innerHTML = `<div class="err">${esc(e.message || e)}</div>`; }
  }
  document.querySelectorAll('#win .chip').forEach(c => c.addEventListener('click', () => {
    document.querySelectorAll('#win .chip').forEach(x => x.classList.remove('on')); c.classList.add('on'); win = c.dataset.win; render();
  }));
  $('bLook').addEventListener('click', async () => {
    const account = $('acct').value.trim(); if(!account) return;
    $('lookup').innerHTML = '<p class="hint">asking…</p>';
    try{
      const r = await api('/api/account', {method:'POST', headers:{'content-type':'application/json'}, body: JSON.stringify({account})});
      const a = await r.json();
      if(a.error){ $('lookup').innerHTML = `<div class="err">${esc(a.error)}</div>`; return; }
      const b = a.balance, pl = a.plan;
      $('lookup').innerHTML = '<div style="margin-top:10px">' +
        row('balance', fmt(b.total) + ' TOKU', 'g') + row('allowance', fmt(b.allowance) + (b.allowance_ends_ms ? ' · until ' + day(b.allowance_ends_ms) : '')) +
        row('prepaid lots', b.prepaid.length ? b.prepaid.map(l => fmt(l[0]) + ' until ' + day(l[1])).join(', ') : 'none') +
        (pl ? row('plan', (pl.active ? 'active' : 'ended') + ' · tier ' + pl.tier + (pl.yearly ? ' yearly' : ' monthly') + ' · ' + pl.rail, pl.active ? 's' : 'r') + row('paid until', day(pl.paidUntil)) + row('disputed', pl.disputed ? 'yes' : 'no', pl.disputed ? 'r' : '') : row('plan', 'none', 'd')) +
        (a.usage && a.usage.length ? row('past periods', a.usage.map(x => day(x.period * 1000) + ': ' + x.drawn).join(' · ')) : '') + '</div>';
    }catch(e){ $('lookup').innerHTML = `<div class="err">${esc(e.message || e)}</div>`; }
  });
  load(); setInterval(load, 60000);
})();
