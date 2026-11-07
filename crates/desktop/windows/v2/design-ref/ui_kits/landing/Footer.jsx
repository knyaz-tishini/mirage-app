// Ticker, CTA, Footer

const Ticker = () => {
  const items = [
    ['ok', 'l1-msk-01', 'online · 18ms'],
    ['ok', 'l1-spb-02', 'online · 22ms'],
    ['a',  'l2-ams-03', 'exit · 842Mbps'],
    ['ok', 'l2-fra-01', 'online · 38ms'],
    ['w',  'l2-alm-02', 'degraded · 118ms'],
    ['ok', 'l2-zrh-01', 'online · 44ms'],
    ['a',  'global',    '2,417 nodes · 99.98% uptime'],
    ['ok', 'l1-msk-04', 'online · 19ms'],
    ['ok', 'l2-nl-02',  'exit · 620Mbps'],
  ];
  const all = [...items, ...items];
  return (
    <div className="ticker">
      <div className="ticker__track">
        {all.map(([tone, k, v], i) => (
          <span className="ticker__item" key={i}>
            <span className={`dot ${tone}`} />
            <b>{k}</b>
            <span>{v}</span>
          </span>
        ))}
      </div>
    </div>
  );
};

const CTA = () => (
  <section className="section" id="clients">
    <div className="container">
      <div className="cta">
        <div>
          <Eyebrow>§ 03 · download</Eyebrow>
          <h3 style={{ marginTop: 16 }}>Готовы поднять свою mesh‑сеть?</h3>
          <p>
            Клиент Mirage — open‑source. Конфигурация — ваша. Серверы — ваши.
            Мы только делаем так, чтобы всё работало.
          </p>
        </div>
        <div className="cta__actions">
          <Button variant="primary" mono>DOWNLOAD · macOS .dmg</Button>
          <Button variant="outline" mono>curl | sh</Button>
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--fg-3)', textAlign: 'center', textTransform: 'uppercase', letterSpacing: '0.08em' }}>
            sha256 · signed · reproducible build
          </span>
        </div>
      </div>
    </div>
  </section>
);

const Footer = () => (
  <footer className="footer">
    <div className="container">
      <div className="footer__grid">
        <div className="footer__brand">
          <a href="#" className="logo">
            <svg viewBox="0 0 24 24" fill="none" width="20" height="20">
              <path d="M12 2L3 7v10l9 5 9-5V7L12 2Z" stroke="currentColor" strokeWidth="1.8" strokeLinejoin="round"/>
              <path d="M12 2v20M3 7l9 5 9-5" stroke="currentColor" strokeWidth="1.8" strokeLinejoin="round"/>
            </svg>
            Mirage
          </a>
          <p className="footer__tag">
            Мы не продаём VPS.<br/>Мы настраиваем вашу инфраструктуру.
          </p>
        </div>
        <div className="footer__col">
          <h4>Продукт</h4>
          <ul>
            <li><a href="#">Клиент</a></li>
            <li><a href="#">Провайдеры</a></li>
            <li><a href="#">Документация</a></li>
            <li><a href="#">Changelog</a></li>
          </ul>
        </div>
        <div className="footer__col">
          <h4>Технологии</h4>
          <ul>
            <li><a href="#">WireGuard</a></li>
            <li><a href="#">obfs4</a></li>
            <li><a href="#">mesh‑routing</a></li>
            <li><a href="#">Threat model</a></li>
          </ul>
        </div>
        <div className="footer__col">
          <h4>Контакты</h4>
          <ul>
            <li><a href="#">GitHub</a></li>
            <li><a href="#">Telegram</a></li>
            <li><a href="#">security@mirage</a></li>
            <li><a href="#">Политика</a></li>
          </ul>
        </div>
      </div>
      <div className="footer__copy">
        <span>Mirage © 2025 · не VPN · не VPS</span>
        <span>build 0.2.0 · commit a8f3c21</span>
      </div>
    </div>
  </footer>
);

Object.assign(window, { Ticker, CTA, Footer });
