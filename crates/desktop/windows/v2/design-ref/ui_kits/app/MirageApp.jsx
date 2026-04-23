// MirageApp.jsx — single-button connect screen, Mirage brand.
// States: off → connecting → on → off
// Renders inside IOSDevice. Dark mode always.

const APP_PALETTE = {
  bg:       '#08090B',
  surface:  '#14151A',
  raised:   '#1C1D23',
  border:   'rgba(255,255,255,0.07)',
  borderS:  'rgba(255,255,255,0.12)',
  fg0:      '#F4F4F5',
  fg1:      '#D7D8DB',
  fg2:      '#878B91',
  fg3:      '#555860',
  accent:   '#FBB26A',
  accentD:  'rgba(251,178,106,0.12)',
  accentG:  'rgba(251,178,106,0.35)',
  l1:       '#6DB8F5',
  ok:       '#5CB87A',
  err:      '#EB5757',
  warn:     '#E8A838',
  mono:     "'JetBrains Mono', ui-monospace, SFMono-Regular, Menlo, monospace",
  display:  "'Space Grotesk', -apple-system, system-ui, sans-serif",
  sans:     "'PT Root UI', -apple-system, system-ui, sans-serif",
};

const S = APP_PALETTE;

// ── top strip: logo + session id + ping ──
function AppTopStrip({ state }) {
  return (
    <div style={{
      display: 'flex', alignItems: 'center', justifyContent: 'space-between',
      padding: '12px 22px', marginTop: 54,
      fontFamily: S.mono, fontSize: 10, letterSpacing: '0.1em',
      textTransform: 'uppercase', color: S.fg3,
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, color: S.accent, fontFamily: S.display, fontSize: 18, fontWeight: 500, letterSpacing: '-0.02em', textTransform: 'none' }}>
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none">
          <path d="M3 17 L12 4 L21 17 M7 17 L12 10 L17 17" stroke="currentColor" strokeWidth="1.75" strokeLinejoin="round"/>
        </svg>
        <span style={{ letterSpacing: 0 }}>mirage</span>
      </div>
      <div style={{ display: 'flex', gap: 14 }}>
        <span>ses · 7f2a</span>
        <span style={{ color: state === 'on' ? S.ok : state === 'connecting' ? S.warn : S.fg3 }}>
          {state === 'on' ? '● on' : state === 'connecting' ? '○ neg' : '· off'}
        </span>
      </div>
    </div>
  );
}

// ── mesh visualization: user → L1 → L2 dots arrayed around a center ──
function MeshRings({ state, t }) {
  // state ∈ 'off' | 'connecting' | 'on'
  const color = state === 'on' ? S.accent : state === 'connecting' ? S.warn : S.fg3;
  const glow  = state === 'on' ? S.accentG : 'rgba(232,168,56,0.2)';

  // radii
  const R = 140;     // outer (L2)
  const rL1 = 92;    // middle (L1)
  const cx = 0, cy = 0;

  // 6 L2 nodes, 3 L1 nodes (positions fixed)
  const L2 = [0, 1, 2, 3, 4, 5].map(i => {
    const a = (i / 6) * Math.PI * 2 - Math.PI / 2;
    return { x: Math.cos(a) * R, y: Math.sin(a) * R, i };
  });
  const L1 = [0, 1, 2].map(i => {
    const a = (i / 3) * Math.PI * 2 + Math.PI / 2;
    return { x: Math.cos(a) * rL1, y: Math.sin(a) * rL1, i };
  });

  // rotating highlight index while connecting/on
  const hot = state === 'off' ? -1 : Math.floor(t / 800) % 6;
  const hotL1 = state === 'off' ? -1 : Math.floor(t / 1200) % 3;

  return (
    <svg viewBox="-180 -180 360 360" style={{ width: 340, height: 340, overflow: 'visible' }}>
      {/* halo rings — only when on/connecting */}
      {state !== 'off' && (
        <>
          <circle cx={cx} cy={cy} r={R + ((t/20) % 30)} fill="none" stroke={glow} strokeWidth="0.5" opacity={1 - (((t/20) % 30)/30)} />
          <circle cx={cx} cy={cy} r={R + ((t/20 + 15) % 30)} fill="none" stroke={glow} strokeWidth="0.5" opacity={1 - (((t/20+15) % 30)/30)} />
        </>
      )}

      {/* dot-grid scaffold circles */}
      <circle cx={cx} cy={cy} r={R}   fill="none" stroke={S.border} strokeDasharray="1 4" />
      <circle cx={cx} cy={cy} r={rL1} fill="none" stroke={S.border} strokeDasharray="1 4" />

      {/* tick marks around outer ring */}
      {Array.from({length: 48}).map((_, i) => {
        const a = (i / 48) * Math.PI * 2;
        const r1 = R + 3, r2 = R + (i % 12 === 0 ? 10 : 6);
        return <line key={'t'+i} x1={Math.cos(a)*r1} y1={Math.sin(a)*r1} x2={Math.cos(a)*r2} y2={Math.sin(a)*r2} stroke={S.fg3} strokeWidth="0.6" opacity="0.5"/>;
      })}

      {/* connection lines user → L1 → L2 (shown when on) */}
      {state !== 'off' && L1.map((p, i) => (
        <line key={'lu'+i} x1={cx} y1={cy} x2={p.x} y2={p.y}
              stroke={i === hotL1 ? S.l1 : 'rgba(109,184,245,0.25)'}
              strokeWidth={i === hotL1 ? 1.4 : 0.8}
              strokeDasharray={i === hotL1 ? 'none' : '2 3'} />
      ))}
      {state !== 'off' && L2.map((p, i) => {
        // connect each L2 to nearest L1
        const nearest = L1.reduce((best, l) => {
          const d = Math.hypot(p.x - l.x, p.y - l.y);
          return d < best.d ? {l, d} : best;
        }, {l: L1[0], d: 99999}).l;
        return (
          <line key={'ll'+i} x1={nearest.x} y1={nearest.y} x2={p.x} y2={p.y}
                stroke={i === hot ? color : 'rgba(251,178,106,0.22)'}
                strokeWidth={i === hot ? 1.4 : 0.8}
                strokeDasharray={i === hot ? 'none' : '2 3'} />
        );
      })}

      {/* L2 nodes */}
      {L2.map((p, i) => (
        <g key={'l2'+i} transform={`translate(${p.x},${p.y})`}>
          <circle r="10" fill={S.surface} stroke={i === hot ? color : S.borderS} strokeWidth={i === hot ? 1.6 : 1}/>
          <circle r="2.5" fill={i === hot ? color : S.fg3}/>
          {i === hot && <circle r="14" fill="none" stroke={color} strokeWidth="1" opacity="0.5"/>}
        </g>
      ))}

      {/* L1 nodes */}
      {L1.map((p, i) => (
        <g key={'l1'+i} transform={`translate(${p.x},${p.y})`}>
          <rect x="-7" y="-7" width="14" height="14" fill={S.surface} stroke={i === hotL1 ? S.l1 : S.borderS} strokeWidth={i === hotL1 ? 1.6 : 1}/>
          <circle r="2" fill={i === hotL1 ? S.l1 : S.fg3}/>
        </g>
      ))}

      {/* center user */}
      <g>
        <circle r="6" fill={state === 'off' ? S.fg3 : color}/>
        {state !== 'off' && <circle r={10 + ((t/100) % 6)} fill="none" stroke={color} strokeWidth="1" opacity={1 - (((t/100) % 6)/6)}/>}
      </g>
    </svg>
  );
}

// ── big connect button ──
function ConnectButton({ state, onClick }) {
  const isOn = state === 'on';
  const isC  = state === 'connecting';
  const label = isOn ? 'DISCONNECT' : isC ? 'HANDSHAKE…' : 'CONNECT';
  const subLabel = isOn ? 'tap to stop' : isC ? 'negotiating route' : 'tap to route';

  const bg = isOn ? S.accent : isC ? S.raised : S.surface;
  const color = isOn ? '#1A1400' : isC ? S.warn : S.fg0;
  const border = isOn ? S.accent : isC ? 'rgba(232,168,56,0.5)' : S.borderS;
  const shadow = isOn ? `0 0 60px rgba(251,178,106,0.45), 0 0 16px rgba(251,178,106,0.6)` : 'none';

  return (
    <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 14 }}>
      <button onClick={onClick} style={{
        width: 220, height: 60,
        background: bg, color, border: `1.5px solid ${border}`,
        borderRadius: 4,
        fontFamily: S.mono, fontSize: 14, fontWeight: 700,
        letterSpacing: '0.18em', cursor: 'pointer',
        boxShadow: shadow,
        transition: 'all 200ms ease',
      }}>{label}</button>
      <div style={{
        fontFamily: S.mono, fontSize: 10, color: S.fg3,
        textTransform: 'uppercase', letterSpacing: '0.14em',
      }}>{subLabel}</div>
    </div>
  );
}

// ── live stats block under button ──
function LiveStats({ state, t }) {
  const ping = state === 'on' ? (18 + Math.floor(Math.sin(t/400) * 4)) : state === 'connecting' ? Math.floor(Math.random()*99) : null;
  const up   = state === 'on' ? (84 + Math.floor(Math.sin(t/600) * 22)) : null;
  const dn   = state === 'on' ? (312 + Math.floor(Math.cos(t/500) * 78)) : null;

  const cell = (label, val, unit, color) => (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 4, minWidth: 0 }}>
      <div style={{ fontFamily: S.mono, fontSize: 9, color: S.fg3, letterSpacing: '0.12em', textTransform: 'uppercase' }}>{label}</div>
      <div style={{
        fontFamily: S.mono, fontWeight: 700, fontSize: 18, color: color || S.fg0,
        fontVariantNumeric: 'tabular-nums', lineHeight: 1,
      }}>{val ?? '— —'}<span style={{ color: S.fg3, fontSize: 11, fontWeight: 400, marginLeft: 2 }}>{val != null ? unit : ''}</span></div>
    </div>
  );

  return (
    <div style={{
      display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)',
      gap: 16, padding: '16px 24px',
      borderTop: `1px solid ${S.border}`, borderBottom: `1px solid ${S.border}`,
      background: S.bg,
    }}>
      {cell('ping',  ping, 'ms', state === 'on' ? S.ok : null)}
      {cell('down',  dn,   'KB/s')}
      {cell('up',    up,   'KB/s')}
    </div>
  );
}

// ── route line: fixed row showing "you → relay → exit" with live values ──
function RouteLine({ state }) {
  const active = state === 'on';
  const dim = !active;
  const stub = (label, value, color) => (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 3, flex: 1, minWidth: 0 }}>
      <div style={{ fontFamily: S.mono, fontSize: 9, color: S.fg3, letterSpacing: '0.12em', textTransform: 'uppercase' }}>{label}</div>
      <div style={{ fontFamily: S.mono, fontSize: 12, fontWeight: 700, color: dim ? S.fg3 : color,
                    whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
        {active ? value : '— — —'}
      </div>
    </div>
  );
  const arrow = <span style={{ color: S.fg3, fontFamily: S.mono, marginTop: 14 }}>→</span>;
  return (
    <div style={{ display: 'flex', alignItems: 'stretch', gap: 12, padding: '16px 24px' }}>
      {stub('origin', 'RU-MSK',       S.fg1)}
      {arrow}
      {stub('L1',     'yandex-msk-a', S.l1)}
      {arrow}
      {stub('L2',     'timeweb-ams',  S.accent)}
    </div>
  );
}

// ── rolling log strip (optional micro-status) ──
function MicroLog({ state, t }) {
  const lines = state === 'off'
    ? [{ t:'OK', m:'standby · tap to route' }]
    : state === 'connecting'
      ? [
          { t:'OK',   m:'requesting L1 ticket' },
          { t:'OK',   m:'rotating X25519 keys' },
          { t:'OK',   m:'mTLS · REALITY up' },
        ]
      : [
          { t:'OK',   m:'mesh sync · 3 L1 · 6 L2' },
          { t:'OK',   m:'XTLS-Vision · handshake ok' },
          { t:'OK',   m:'ТСПУ probe → marked corp TLS' },
          { t:'OK',   m:'key rot in 05:42' },
        ];
  const tagColor = { OK: S.ok, WARN: S.warn, ERR: S.err };

  return (
    <div style={{
      padding: '14px 24px 20px',
      fontFamily: S.mono, fontSize: 11, lineHeight: 1.65,
      color: S.fg2, background: S.bg,
    }}>
      {lines.map((l, i) => (
        <div key={i} style={{ display: 'flex', gap: 10 }}>
          <span style={{ color: S.fg3 }}>{pad2(Math.floor(t/60000)%60)}:{pad2(Math.floor(t/1000)%60 - i < 0 ? 0 : Math.floor(t/1000)%60 - i)}</span>
          <span style={{ color: tagColor[l.t] || S.fg2, fontWeight: 700, width: 28 }}>{l.t}</span>
          <span style={{ color: S.fg1, flex: 1 }}>{l.m}</span>
        </div>
      ))}
    </div>
  );
}
function pad2(n){ return (n<10?'0':'')+n; }

// ── paste-config sheet (client mode) ──
function PasteConfigSheet({ open, onClose }) {
  const [val, setVal] = React.useState('');
  const [status, setStatus] = React.useState('idle'); // idle | verify | ok
  if (!open) return null;
  const verify = () => {
    setStatus('verify');
    setTimeout(() => setStatus('ok'), 1200);
  };
  return (
    <div onClick={onClose} style={{
      position: 'absolute', inset: 0, zIndex: 60,
      background: 'rgba(0,0,0,0.55)', backdropFilter: 'blur(6px)',
      display: 'flex', alignItems: 'flex-end',
    }}>
      <div onClick={(e)=>e.stopPropagation()} style={{
        width: '100%', background: S.surface,
        borderTop: `1px solid ${S.borderS}`,
        borderRadius: '20px 20px 0 0',
        padding: '18px 22px 30px',
        display: 'flex', flexDirection: 'column', gap: 14,
        fontFamily: S.sans,
      }}>
        <div style={{ width: 40, height: 3, background: S.fg3, borderRadius: 2, margin: '0 auto 4px', opacity: 0.6 }}/>
        <div style={{ display:'flex', justifyContent:'space-between', alignItems:'baseline' }}>
          <div style={{ fontFamily: S.display, fontSize: 22, fontWeight: 500, letterSpacing: '-0.02em', color: S.fg0 }}>
            Paste config
          </div>
          <div style={{ fontFamily: S.mono, fontSize: 10, color: S.fg3, textTransform: 'uppercase', letterSpacing: '0.12em' }}>
            mirage:// link
          </div>
        </div>
        <div style={{ fontFamily: S.sans, fontSize: 13, color: S.fg2, lineHeight: 1.5 }}>
          Вставьте ссылку или конфиг из письма Mirage — мы сами развернём маршрут.
        </div>
        <textarea value={val} onChange={(e)=>setVal(e.target.value)} placeholder={'mirage://ticket/7f2a9c…'} style={{
          background: S.bg, color: S.fg0, fontFamily: S.mono, fontSize: 12,
          border: `1px solid ${S.borderS}`, borderRadius: 4, padding: 12,
          minHeight: 96, resize: 'none', outline: 'none',
        }}/>
        <div style={{ display: 'flex', gap: 10 }}>
          <button onClick={onClose} style={{
            flex: 1, height: 44, background: 'transparent', color: S.fg1,
            border: `1px solid ${S.borderS}`, borderRadius: 4,
            fontFamily: S.mono, fontSize: 12, fontWeight: 700, letterSpacing: '0.14em',
          }}>CANCEL</button>
          <button onClick={verify} style={{
            flex: 2, height: 44,
            background: status === 'ok' ? S.accent : S.raised,
            color: status === 'ok' ? '#1A1400' : S.fg0,
            border: `1px solid ${status === 'ok' ? S.accent : S.borderS}`, borderRadius: 4,
            fontFamily: S.mono, fontSize: 12, fontWeight: 700, letterSpacing: '0.14em',
          }}>
            {status === 'verify' ? 'VERIFYING…' : status === 'ok' ? '✓ ROUTE READY' : 'VERIFY & SAVE'}
          </button>
        </div>
      </div>
    </div>
  );
}

// ── mode switcher ──
function ModeSwitch({ mode, setMode }) {
  const tab = (k, label) => (
    <button onClick={()=>setMode(k)} style={{
      flex: 1, height: 30, background: mode === k ? S.raised : 'transparent',
      color: mode === k ? S.fg0 : S.fg3,
      border: 0, borderRadius: 3,
      fontFamily: S.mono, fontSize: 10, fontWeight: 700, letterSpacing: '0.14em',
      textTransform: 'uppercase', cursor: 'pointer',
      boxShadow: mode === k ? 'inset 0 0 0 1px rgba(255,255,255,0.08)' : 'none',
    }}>{label}</button>
  );
  return (
    <div style={{
      display: 'flex', gap: 3, padding: 3, margin: '6px 22px 0',
      background: S.bg, border: `1px solid ${S.border}`, borderRadius: 4,
    }}>
      {tab('client', 'Client')}
      {tab('admin',  'Admin')}
    </div>
  );
}

// ── Client screen (current behaviour) ──
function ClientScreen({ t }) {
  const [state, setState] = React.useState('off');
  const [sheet, setSheet] = React.useState(false);
  const handleTap = () => {
    if (state === 'off') {
      setState('connecting');
      setTimeout(() => setState('on'), 1600);
    } else if (state === 'on') {
      setState('off');
    }
  };
  return (
    <>
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', alignItems: 'center', justifyContent: 'center', gap: 22, padding: '6px 0 8px' }}>
        <MeshRings state={state} t={t} />
        <ConnectButton state={state} onClick={handleTap} />
        <button onClick={()=>setSheet(true)} style={{
          background: 'transparent', border: 0,
          fontFamily: S.mono, fontSize: 10, color: S.fg2,
          textTransform: 'uppercase', letterSpacing: '0.14em',
          borderBottom: `1px dashed ${S.borderS}`, paddingBottom: 2, cursor: 'pointer',
        }}>paste config ↗</button>
      </div>
      <RouteLine state={state} />
      <LiveStats state={state} t={t} />
      <MicroLog state={state} t={t} />
      <PasteConfigSheet open={sheet} onClose={()=>setSheet(false)} />
    </>
  );
}

// ── Admin: small sparkline ──
function MiniSpark({ kind }) {
  const bars = React.useMemo(() => {
    const n = 24, out = [];
    for (let i = 0; i < n; i++) {
      let h;
      if (kind === 'err')      h = i > n-3 ? 1 : 3 + Math.random()*6;
      else if (kind === 'warn')h = 4 + Math.random()*7 + (i > n-5 ? 4 : 0);
      else                     h = 4 + Math.random()*9;
      out.push(h);
    }
    return out;
  }, [kind]);
  const color = kind === 'err' ? S.err : kind === 'warn' ? S.warn : kind === 'l1' ? S.l1 : S.accent;
  return (
    <div style={{ display:'flex', alignItems:'flex-end', gap: 1, height: 16 }}>
      {bars.map((h,i) => <span key={i} style={{ width: 2, height: h, background: color, opacity: 0.85, borderRadius: 1 }}/>)}
    </div>
  );
}

// ── Admin KPI strip ──
function AdminKPIs({ t }) {
  const cell = (label, val, unit, color) => (
    <div style={{ flex: 1, padding: '12px 14px', borderRight: `1px solid ${S.border}` }}>
      <div style={{ fontFamily: S.mono, fontSize: 9, color: S.fg3, letterSpacing: '0.12em', textTransform: 'uppercase', marginBottom: 4 }}>{label}</div>
      <div style={{ fontFamily: S.mono, fontWeight: 700, fontSize: 18, color: color || S.fg0, fontVariantNumeric: 'tabular-nums', lineHeight: 1 }}>
        {val}{unit && <span style={{ color: S.fg3, fontSize: 10, fontWeight: 400, marginLeft: 2 }}>{unit}</span>}
      </div>
    </div>
  );
  const ping = 18 + Math.floor(Math.sin(t/400) * 3);
  return (
    <div style={{ display: 'flex', borderTop: `1px solid ${S.border}`, borderBottom: `1px solid ${S.border}`, background: S.bg }}>
      {cell('nodes', '12', '/12', S.ok)}
      {cell('L1', '4', null, S.l1)}
      {cell('L2', '8', null, S.accent)}
      {cell('p50', ping, 'ms', null)}
    </div>
  );
}

// ── Admin node row ──
function AdminNodeRow({ n }) {
  const dotColor = n.s === 'err' ? S.err : n.s === 'warn' ? S.warn : S.ok;
  const layerColor = n.layer === 'L1' ? S.l1 : S.accent;
  const layerBg    = n.layer === 'L1' ? 'rgba(109,184,245,0.12)' : 'rgba(251,178,106,0.12)';
  return (
    <div style={{
      display: 'grid', gridTemplateColumns: '8px 1fr auto 44px',
      gap: 10, alignItems: 'center',
      padding: '10px 22px', borderBottom: `1px solid ${S.border}`,
      fontFamily: S.mono, fontSize: 11,
    }}>
      <span style={{ width: 7, height: 7, borderRadius: 99, background: dotColor, boxShadow: n.s === 'ok' ? `0 0 6px ${dotColor}` : 'none' }}/>
      <div style={{ minWidth: 0 }}>
        <div style={{ color: S.fg0, fontWeight: 700, display: 'flex', gap: 6, alignItems: 'center', overflow: 'hidden' }}>
          <span style={{
            fontSize: 9, padding: '1px 5px', borderRadius: 3,
            color: layerColor, background: layerBg,
            border: `1px solid ${layerColor === S.l1 ? 'rgba(109,184,245,0.3)' : 'rgba(251,178,106,0.3)'}`,
            letterSpacing: '0.08em',
          }}>{n.layer}</span>
          <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{n.name}</span>
        </div>
        <div style={{ color: S.fg3, fontSize: 10, marginTop: 2 }}>{n.addr} · {n.region}</div>
      </div>
      <div style={{ textAlign: 'right', color: S.fg1, fontVariantNumeric: 'tabular-nums' }}>
        {n.s === 'err' ? <span style={{ color: S.err }}>— —</span> : <>{n.rtt}<span style={{ color: S.fg3 }}>ms</span></>}
      </div>
      <MiniSpark kind={n.s === 'err' ? 'err' : n.s === 'warn' ? 'warn' : (n.layer === 'L1' ? 'l1' : 'amber')}/>
    </div>
  );
}

const ADMIN_NODES = [
  { layer:'L1', name:'yandex-moscow-a',   addr:'84.201.146.12', region:'MSK',   rtt:12, s:'ok' },
  { layer:'L1', name:'yandex-moscow-b',   addr:'84.201.146.44', region:'MSK',   rtt:14, s:'ok' },
  { layer:'L1', name:'selectel-spb-01',   addr:'188.93.154.9',  region:'SPB',   rtt:16, s:'ok' },
  { layer:'L1', name:'vk-cloud-msk',      addr:'95.163.201.77', region:'MSK',   rtt:23, s:'warn' },
  { layer:'L2', name:'timeweb-ams-01',    addr:'149.154.71.12', region:'AMS',   rtt:42, s:'ok' },
  { layer:'L2', name:'timeweb-fra-02',    addr:'149.154.71.98', region:'FRA',   rtt:38, s:'ok' },
  { layer:'L2', name:'ruvds-ala-kz',      addr:'85.202.194.3',  region:'ALA',   rtt:21, s:'ok' },
  { layer:'L2', name:'serverspace-ala',   addr:'93.95.217.55',  region:'ALA',   rtt:19, s:'ok' },
  { layer:'L2', name:'hostkey-nl-03',     addr:'213.183.63.201',region:'AMS',   rtt:45, s:'ok' },
  { layer:'L2', name:'fornex-waw-01',     addr:'37.220.86.19',  region:'WAW',   rtt:34, s:'ok' },
  { layer:'L2', name:'vdsina-ams-02',     addr:'45.80.158.4',   region:'AMS',   rtt:0,  s:'err' },
  { layer:'L2', name:'ishosting-hel-04',  addr:'91.239.115.27', region:'HEL',   rtt:29, s:'ok' },
];

// ── Admin events/log ──
function AdminEvents({ t }) {
  const items = [
    { tag:'ERR',  m:'vdsina-ams-02 unreachable · failover → timeweb-ams-01', k:'err' },
    { tag:'WARN', m:'loss 0.4% on vk-cloud-msk · throttling',                k:'warn' },
    { tag:'OK',   m:'probe → ТСПУ · marked corp TLS on 4/4 L1',              k:'ok' },
    { tag:'OK',   m:'key rot · selectel-spb-01 · next in 6h',                k:'ok' },
    { tag:'OK',   m:'XTLS-Vision · handshake ok on all L2',                  k:'ok' },
  ];
  const tagColor = { OK: S.ok, WARN: S.warn, ERR: S.err };
  return (
    <div style={{ padding: '8px 22px 20px', fontFamily: S.mono, fontSize: 10.5, lineHeight: 1.6, background: S.bg }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', padding: '10px 0 8px', color: S.fg3, letterSpacing: '0.14em', textTransform: 'uppercase', fontSize: 9, borderBottom: `1px solid ${S.border}` }}>
        <span>EVENTS</span>
        <span>live · tail -f</span>
      </div>
      {items.map((l, i) => (
        <div key={i} style={{ display: 'flex', gap: 8, padding: '5px 0', borderBottom: i < items.length-1 ? `1px solid ${S.border}` : 0 }}>
          <span style={{ color: S.fg3, flexShrink: 0, width: 38 }}>{pad2(Math.floor(t/60000)%60)}:{pad2((Math.floor(t/1000)-i*7+60)%60)}</span>
          <span style={{ color: tagColor[l.tag], fontWeight: 700, width: 32, flexShrink: 0 }}>{l.tag}</span>
          <span style={{ color: S.fg1, flex: 1 }}>{l.m}</span>
        </div>
      ))}
    </div>
  );
}

// ── Admin screen ──
function AdminScreen({ t }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', flex: 1, overflow: 'auto' }}>
      {/* header */}
      <div style={{ padding: '14px 22px 10px' }}>
        <div style={{ fontFamily: S.mono, fontSize: 9, color: S.fg3, letterSpacing: '0.14em', textTransform: 'uppercase', marginBottom: 4 }}>
          MESH / eu-prod-01
        </div>
        <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between' }}>
          <div style={{ fontFamily: S.display, fontSize: 26, fontWeight: 500, letterSpacing: '-0.02em', color: S.fg0, lineHeight: 1 }}>
            Topology
          </div>
          <div style={{ fontFamily: S.mono, fontSize: 10, color: S.ok }}>● healthy</div>
        </div>
      </div>
      <AdminKPIs t={t} />

      {/* probes/sentries callout */}
      <div style={{ padding: '10px 22px', borderBottom: `1px solid ${S.border}`, display: 'flex', gap: 10, alignItems: 'center', background: S.bg }}>
        <div style={{ width: 8, height: 8, borderRadius: 99, background: S.ok, boxShadow: `0 0 6px ${S.ok}` }}/>
        <div style={{ fontFamily: S.mono, fontSize: 10, color: S.fg2, lineHeight: 1.45, flex: 1 }}>
          <span style={{ color: S.fg0, fontWeight: 700 }}>12 probes · 4 sentries</span> active<br/>
          <span style={{ color: S.fg3 }}>metadata only · traffic not inspected</span>
        </div>
      </div>

      {/* node list */}
      <div style={{ padding: '10px 22px 6px', fontFamily: S.mono, fontSize: 9, color: S.fg3, letterSpacing: '0.14em', textTransform: 'uppercase', display: 'flex', justifyContent: 'space-between' }}>
        <span>NODES · 12</span>
        <span>sort: layer ↓</span>
      </div>
      <div>
        {ADMIN_NODES.map(n => <AdminNodeRow key={n.name} n={n} />)}
      </div>

      <AdminEvents t={t} />
    </div>
  );
}

// ── main app ──
function MirageApp() {
  const [mode, setMode] = React.useState('client');
  const [t, setT] = React.useState(0);

  React.useEffect(() => {
    let raf;
    const start = performance.now();
    const tick = (now) => { setT(now - start); raf = requestAnimationFrame(tick); };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, []);

  return (
    <div style={{
      background: S.bg, color: S.fg1, minHeight: '100%',
      fontFamily: S.sans,
      backgroundImage: 'radial-gradient(rgba(255,255,255,0.035) 1px, transparent 1px)',
      backgroundSize: '20px 20px',
      display: 'flex', flexDirection: 'column',
      position: 'relative',
    }}>
      <AppTopStrip state={'on'} />
      <ModeSwitch mode={mode} setMode={setMode} />

      {mode === 'client' ? <ClientScreen t={t}/> : <AdminScreen t={t}/>}

      <div style={{
        padding: '10px 24px 26px', display: 'flex', justifyContent: 'space-between',
        fontFamily: S.mono, fontSize: 10, color: S.fg3,
        textTransform: 'uppercase', letterSpacing: '0.1em',
        borderTop: `1px solid ${S.border}`,
      }}>
        <span>mesh · eu-prod-01</span>
        <span>v 2.4.1 · build 2814</span>
      </div>
    </div>
  );
}

Object.assign(window, { MirageApp });
