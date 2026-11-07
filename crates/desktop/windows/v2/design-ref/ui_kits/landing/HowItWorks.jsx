// How it works — 3 steps + ASCII mesh diagram

const MeshDiagram = () => (
  <div className="mesh">
    <div className="mesh__ascii">
{`     ВЫ              L1 RELAY            L2 EXIT           ИНТЕРНЕТ
  ┌──────┐         ┌─────────┐         ┌─────────┐         ┌──────────┐
  │      │ ═obfs4═▶│         │ ══wg═══▶│         │ ══tls══▶│          │
  │  УЗ  │         │  `}<span className="l1">MOSCOW</span>{` │         │  `}<span className="a">AMS·NL</span>{` │         │  TARGETS │
  │      │◀══════  │         │◀════════│         │◀════════│          │
  └──────┘         └─────────┘         └─────────┘         └──────────┘
                   `}<span className="m">Selectel · v0.2</span>{`       `}<span className="m">Timeweb · v0.2</span>{`
                   `}<span className="m">185.82.x.x</span>{`           `}<span className="m">45.12.x.x</span>{`
`}
    </div>
    <div className="mesh__notes">
      <div className="mesh__note l1">
        <div>
          <b>L1 Relay</b> — промежуточный узел в РФ‑юрисдикции с белосписочным IP.
          Принимает обфусцированный трафик клиента и реле­ит на L2 по WireGuard.
        </div>
      </div>
      <div className="mesh__note">
        <div>
          <b>L2 Exit</b> — выходной узел в офшорной юрисдикции.
          Финальный выход в интернет. Ни ваш ISP, ни ТСПУ не видят содержимое и целевые IP.
        </div>
      </div>
      <div className="mesh__note" style={{ borderLeftColor: 'var(--fg-3)' }}>
        <div>
          <b>Никакого Mirage в маршруте.</b> Мы только разворачиваем конфигурацию. Серверы, ключи, трафик — ваши.
        </div>
      </div>
    </div>
  </div>
);

const Steps = () => (
  <div className="steps">
    <div className="step">
      <div className="step__num">01</div>
      <div className="step__title">Выберите VPS</div>
      <p className="step__text">
        Возьмите L1 (relay) и L2 (exit) из нашего агрегатора проверенных провайдеров.
        L1 в РФ с белосписочным IP, L2 в юрисдикции без блокировок.
      </p>
      <div className="step__meta">≈ 10 минут · оплата у провайдера</div>
    </div>
    <div className="step">
      <div className="step__num">02</div>
      <div className="step__title">Дайте доступ — мы настроим</div>
      <p className="step__text">
        Разворачиваем WireGuard, двухузловую маршрутизацию и обфускацию трафика.
        Вы получаете конфиги, ключи и control‑plane. Никаких третьих лиц в маршруте.
      </p>
      <div className="step__meta">≈ 15 минут автодеплой · ssh + ansible</div>
    </div>
    <div className="step">
      <div className="step__num">03</div>
      <div className="step__title">Подключайтесь</div>
      <p className="step__text">
        Клиент Mirage на macOS, Linux, iOS и Android. Автопереключение узлов,
        мониторинг здоровья сети, обновление конфигурации без простоя.
      </p>
      <div className="step__meta">kill‑switch · split‑tunnel · per‑app routing</div>
    </div>
  </div>
);

const HowItWorks = () => (
  <section className="section" id="how">
    <div className="container">
      <div className="section__hd">
        <h2>Как это работает</h2>
        <span className="meta">§ 01 · topology</span>
      </div>
      <MeshDiagram />
      <div style={{ height: 48 }} />
      <Steps />
    </div>
  </section>
);

Object.assign(window, { MeshDiagram, Steps, HowItWorks });
