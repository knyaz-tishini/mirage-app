// Shared Mirage landing atoms.
// All components expose on window for cross-file Babel sharing.

const Logo = ({ size = 20, color = 'var(--accent)' }) => (
  <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8, color, fontFamily: 'var(--font-display)', fontWeight: 500, fontSize: 17, letterSpacing: '-0.02em' }}>
    <svg viewBox="0 0 24 24" fill="none" width={size} height={size}>
      <path d="M12 2L3 7v10l9 5 9-5V7L12 2Z" stroke="currentColor" strokeWidth="1.8" strokeLinejoin="round"/>
      <path d="M12 2v20M3 7l9 5 9-5" stroke="currentColor" strokeWidth="1.8" strokeLinejoin="round"/>
    </svg>
    <span>Mirage</span>
  </span>
);

const Icon = ({ name, size = 16, style }) => {
  const [svg, setSvg] = React.useState('');
  React.useEffect(() => {
    fetch(`../../assets/icons/${name}.svg`).then(r => r.text()).then(t =>
      setSvg(t.replace('<svg', `<svg width="${size}" height="${size}"`))
    );
  }, [name, size]);
  return <span style={{ display: 'inline-flex', ...style }} dangerouslySetInnerHTML={{ __html: svg }} />;
};

const Eyebrow = ({ children, muted, style }) => (
  <span className={'eyebrow' + (muted ? ' eyebrow--mute' : '')} style={style}>{children}</span>
);

const Button = ({ children, variant = 'primary', mono, sm, ...rest }) => (
  <button
    className={`btn btn--${variant}${mono ? ' btn--mono' : ''}${sm ? ' btn--sm' : ''}`}
    {...rest}
  >
    {children}
  </button>
);

const LayerChip = ({ layer, children }) => (
  <span className={`layer-chip layer-chip--${layer}`}>{children || layer.toUpperCase()}</span>
);

const StatusPill = ({ label = 'All systems online', tone = 'ok' }) => (
  <span className="status-pill">
    <span className="dot" />
    {label}
  </span>
);

const KPI = ({ label, value, unit }) => (
  <div className="kpi">
    <div className="kpi__label">{label}</div>
    <div className="kpi__value">{value}{unit && <span className="unit"> {unit}</span>}</div>
  </div>
);

// Header
const Header = () => (
  <header className="hdr">
    <div className="container hdr__inner">
      <a href="#" className="logo">
        <svg viewBox="0 0 24 24" fill="none" width="20" height="20">
          <path d="M12 2L3 7v10l9 5 9-5V7L12 2Z" stroke="currentColor" strokeWidth="1.8" strokeLinejoin="round"/>
          <path d="M12 2v20M3 7l9 5 9-5" stroke="currentColor" strokeWidth="1.8" strokeLinejoin="round"/>
        </svg>
        Mirage
        <span style={{ fontFamily: 'var(--font-mono)', fontSize: 10, color: 'var(--fg-3)', marginLeft: 4, letterSpacing: '0.08em' }}>v0.2.0</span>
      </a>
      <nav className="nav">
        <a href="#how">/how-it-works</a>
        <a href="#providers">/providers</a>
        <a href="#clients">/clients</a>
        <a href="#docs">/docs</a>
      </nav>
      <div className="hdr__right">
        <StatusPill />
        <Button variant="outline" sm>Войти</Button>
        <Button variant="primary" sm>Скачать</Button>
      </div>
    </div>
  </header>
);

Object.assign(window, { Logo, Icon, Eyebrow, Button, LayerChip, StatusPill, KPI, Header });
