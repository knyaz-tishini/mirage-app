// Providers — dense table instead of cards, with L1/L2 tabs

const PROVIDERS = {
  l1: [
    { name: 'Selectel',     url: 'selectel.ru',     price: 'от 300 ₽/мес', regions: 'MSK · SPB · LEN', desc: 'Партнёр ViPNet. IP в белых списках ТСПУ как корпоративный шифрованный трафик.', layer: 'l1' },
    { name: 'Yandex Cloud', url: 'yandex.cloud',    price: 'от 500 ₽/мес', regions: 'MSK · SPB',       desc: 'Лучший белосписочный статус. 84.201.x.x в федеральном реестре Минцифры.', layer: 'l1' },
    { name: 'VK Cloud',     url: 'cloud.vk.com',    price: 'от 500 ₽/мес', regions: 'MSK',             desc: 'Нестабильный белосписочный статус — зависит от ДЦ. Требуется проверка IP.', layer: 'l1', warn: true },
  ],
  l2: [
    { name: 'Timeweb Cloud', url: 'timeweb.cloud',  price: 'от 199 ₽/мес',  regions: 'AMS · FRA · KZ · PL', desc: 'Российское юрлицо, зарубежные ДЦ. IP не в РКН блок‑листе.', layer: 'l2' },
    { name: 'Serverspace',   url: 'serverspace.io', price: 'от €4.59/мес',  regions: 'KZ · RU · NL',         desc: 'Казахстанский ДЦ Kazteleport. Tier II+. Низкая задержка из РФ (~15‑25мс).', layer: 'l2' },
    { name: 'RUVDS',         url: 'ruvds.com',      price: 'от 139 ₽/мес',  regions: 'MSK · SPB · LON · ZRH · AMS', desc: 'Российский провайдер с зарубежными ДЦ. Алматы + Амстердам — оптимальные exit‑локации.', layer: 'l2' },
    { name: 'HOSTKEY',       url: 'hostkey.com',    price: 'от 300 ₽/мес',  regions: 'NL · DE · FI',         desc: 'Собственные ДЦ в Европе. Принимает BTC. Хорошая связность с РФ.', layer: 'l2' },
    { name: 'VDSina',        url: 'vdsina.com',     price: 'от $2.10/мес',  regions: 'MSK · AMS',            desc: 'Российский хостинг с ДЦ в Нидерландах. Амстердам идеален для exit‑нод.', layer: 'l2' },
    { name: 'Fornex',        url: 'fornex.com',     price: 'от €5.50/мес',  regions: 'DE · NL · SE · CH · US', desc: 'Европа + США. Принимает USDT. Стабильная работа с РФ.', layer: 'l2' },
  ],
};

const ProviderRow = ({ p }) => (
  <div className="prov-row">
    <div className="prov-name">
      <span className="n">{p.name} {p.warn && <span style={{ color: 'var(--warn)', fontSize: 11, marginLeft: 4 }}>⚠ нестабильный</span>}</span>
      <span className="url">{p.url} ↗</span>
    </div>
    <div className="prov-desc hide-sm">{p.desc}</div>
    <div className="prov-regions hide-sm">{p.regions}</div>
    <div className="prov-price">{p.price}</div>
    <div className="hide-sm"><LayerChip layer={p.layer}>{p.layer === 'l1' ? 'L1 RELAY' : 'L2 EXIT'}</LayerChip></div>
    <div>
      <span className="prov-go">
        <svg viewBox="0 0 12 12" fill="none" width="10" height="10"><path d="M2 10L10 2M10 2H4M10 2v6" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"/></svg>
      </span>
    </div>
  </div>
);

const Providers = () => {
  const [tab, setTab] = React.useState('l1');
  const list = PROVIDERS[tab];
  return (
    <section className="section" id="providers" style={{ background: 'linear-gradient(180deg, var(--bg) 0%, rgba(20,21,26,0.4) 50%, var(--bg) 100%)' }}>
      <div className="container">
        <div className="section__hd">
          <h2>Провайдеры VPS</h2>
          <span className="meta">§ 02 · aggregator · {list.length} of {PROVIDERS.l1.length + PROVIDERS.l2.length}</span>
        </div>
        <div className="tabs">
          <button className={'tab-btn' + (tab === 'l1' ? ' is-active' : '')} onClick={() => setTab('l1')}>
            <LayerChip layer="l1">L1</LayerChip> Relay — промежуточные
          </button>
          <button className={'tab-btn' + (tab === 'l2' ? ' is-active' : '')} onClick={() => setTab('l2')}>
            <LayerChip layer="l2">L2</LayerChip> Exit — выходные
          </button>
        </div>
        <div className="prov-table">
          <div className="prov-row prov-row--head">
            <div>provider</div>
            <div className="hide-sm">description</div>
            <div className="hide-sm">regions</div>
            <div>price</div>
            <div className="hide-sm">layer</div>
            <div></div>
          </div>
          {list.map(p => <ProviderRow key={p.name} p={p} />)}
        </div>
      </div>
    </section>
  );
};

Object.assign(window, { Providers });
