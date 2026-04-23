// Hero — split: left marketing, right live terminal / config

function useTickingLog() {
  const [i, setI] = React.useState(0);
  React.useEffect(() => {
    const id = setInterval(() => setI(x => x + 1), 1400);
    return () => clearInterval(id);
  }, []);
  return i;
}

const Terminal = () => {
  const tick = useTickingLog();
  const rows = [
    { t: '[14:02:17.182]', tag: 'OK',   cls: 'c-str',  msg: 'handshake ',  k: 'l1-msk-01', arrow: true, k2: 'l2-ams-03', tail: '  rtt=18ms' },
    { t: '[14:02:17.201]', tag: 'OK',   cls: 'c-str',  msg: 'wireguard peer established  ', k: '185.82.x.x:51820' },
    { t: '[14:02:17.409]', tag: 'WARN', cls: 'c-key',  msg: 'obfs4 bridge latency p95=142ms ', tail: '(target <120)' },
    { t: '[14:02:18.003]', tag: 'OK',   cls: 'c-str',  msg: 'tunnel up  ', k: 'mirage0', tail: '  mtu=1420  keepalive=25s' },
    { t: '[14:02:19.551]', tag: 'OK',   cls: 'c-str',  msg: 'dns via l2  1.1.1.1  dot  verified' },
    { t: '[14:02:22.118]', tag: 'OK',   cls: 'c-str',  msg: 'traffic obfs4→wg  throughput=842Mbps' },
  ];
  const shown = rows.slice(0, Math.min(rows.length, (tick % (rows.length + 2)) + 1));
  return (
    <div className="hero__right">
      <div className="term__bar">
        <div className="dots"><span/><span/><span/></div>
        <span className="title">mirage@node-01 — mirage --connect --watch</span>
        <span style={{ fontFamily: 'var(--font-mono)', fontSize: 10, color: 'var(--fg-3)' }}>PID 4012</span>
      </div>
      <div className="term__tabs">
        <div className="term__tab term__tab--active">wireguard.conf</div>
        <div className="term__tab">obfs4.toml</div>
        <div className="term__tab">peers.json</div>
        <div className="term__tab">~log</div>
      </div>
      <div className="term__body">
        <div><span className="c-mute"># L1 Relay · Москва · Selectel</span></div>
        <div><span className="c-key">[Interface]</span></div>
        <div>PrivateKey = <span className="c-str">"WG/k2...==redacted"</span></div>
        <div>Address    = <span className="c-str">10.13.13.2/32</span></div>
        <div>DNS        = <span className="c-str">1.1.1.1</span></div>
        <div style={{ height: 6 }}/>
        <div><span className="c-key">[Peer]</span>  <span className="c-mute"># L2 Exit · Amsterdam · Timeweb</span></div>
        <div>Endpoint   = <span className="c-l1">185.82.x.x:51820</span></div>
        <div>AllowedIPs = <span className="c-str">0.0.0.0/0</span></div>
        <div>obfs4      = <span className="c-str">enabled</span> <span className="c-mute">iat-mode=1</span></div>
        <div style={{ height: 10 }}/>
        <div className="c-mute">── live ──</div>
        {shown.map((r, idx) => (
          <div key={idx} style={{ opacity: idx === shown.length - 1 ? 1 : 0.75 }}>
            <span className="c-mute">{r.t}</span>{' '}
            <span className={r.cls} style={{ fontWeight: 700 }}>{r.tag.padEnd(4)}</span>{' '}
            {r.msg}
            {r.k && <span className="c-l1">{r.k}</span>}
            {r.arrow && <span className="c-mute"> → </span>}
            {r.k2 && <span className="c-key">{r.k2}</span>}
            {r.tail}
          </div>
        ))}
        <div><span className="prompt">mirage@node-01:~$</span><span className="cur"/></div>
      </div>
    </div>
  );
};

const Hero = () => (
  <section className="hero">
    <div className="container">
      <div className="hero__grid">
        <div className="hero__left">
          <div className="hero__meta-row">
            <span className="accent">● двухузловая маршрутизация</span>
            <span>·</span>
            <span>wireguard + obfs4</span>
            <span>·</span>
            <span>v0.2.0 beta</span>
          </div>
          <h1 className="hero__title">
            Ваша инфраструктура.<br/>
            <em>Наша конфигурация.</em>
          </h1>
          <p className="hero__sub">
            Mirage настраивает и обслуживает частные mesh‑сети на ваших VPS.
            Двухузловая маршрутизация, обфускация трафика, активная защита — без централизованного провайдера.
          </p>
          <div className="hero__actions">
            <Button variant="primary">Скачать клиент</Button>
            <Button variant="outline">Выбрать провайдеров →</Button>
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--fg-3)', marginLeft: 8, textTransform: 'uppercase', letterSpacing: '0.08em' }}>
              macOS · Linux · iOS · Android
            </span>
          </div>
          <div className="hero__kpis">
            <KPI label="uptime 30d" value="99.98" unit="%" />
            <KPI label="latency p50" value="18" unit="ms" />
            <KPI label="configured" value="2,417" unit="nodes" />
          </div>
        </div>
        <Terminal />
      </div>
    </div>
  </section>
);

Object.assign(window, { Hero, Terminal });
