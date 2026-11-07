'use strict';

/* =============================================================
   Hardcoded provider data (fallback if API is unavailable)
   ============================================================= */

const PROVIDERS_L1 = [
  {
    name: 'Yandex Cloud',
    url: 'https://yandex.cloud/ru/prices?state=f2dac101335a#calculator',
    price: 'от 500 ₽/мес',
    regions: ['Москва', 'Санкт-Петербург'],
    description: 'Лучший белосписочный статус. IP 84.201.x.x в федеральном реестре Минцифры. Официальный процесс внесения в белый список. Управляемое ГОСТ-шифрование.',
    layer: 'l1',
  },
  {
    name: 'Selectel',
    url: 'https://selectel.ru/services/cloud/servers/',
    price: 'от 300 ₽/мес',
    regions: ['Москва', 'Санкт-Петербург', 'Ленобласть'],
    description: 'Крупнейший независимый российский облачный провайдер. Партнёр ViPNet Coordinator — IP уже в белых списках ТСПУ как корпоративный шифрованный трафик.',
    layer: 'l1',
  },
  {
    name: 'VK Cloud',
    url: 'https://cloud.vk.com/cloud-servers/#calculator',
    price: 'от 500 ₽/мес',
    regions: ['Москва'],
    description: 'Облачная платформа VK. Нестабильный белосписочный статус — зависит от ДЦ. Требуется проверка конкретного IP перед использованием.',
    layer: 'l1',
    warning: 'Нестабильный статус',
  },
];

const PROVIDERS_L2 = [
  {
    name: 'Timeweb Cloud',
    url: 'https://timeweb.cloud/?i=140853',
    price: 'от 199 ₽/мес',
    regions: ['Амстердам', 'Франкфурт', 'Польша', 'Казахстан'],
    description: 'Российское юрлицо, зарубежные ДЦ. IP не в РКН блок-листе — трафик проходит ТСПУ без ограничений.',
    layer: 'l2',
    payment: 'Рубли, карты РФ',
  },
  {
    name: 'VDSina',
    url: 'https://www.vdsina.com/?partner=c7bxkcrx6xvg',
    price: 'от $2.10/мес',
    regions: ['Москва', 'Амстердам'],
    description: 'Российский хостинг с ДЦ в Нидерландах. Амстердамский ДЦ идеален для exit-нод.',
    layer: 'l2',
    payment: 'Рубли',
  },
  {
    name: '4VPS',
    url: 'https://4vps.su',
    price: 'от 100 ₽/мес',
    regions: ['Глобальные локации'],
    description: 'Бюджетные VPS с почасовой и посуточной оплатой. Десятки локаций по всему миру.',
    layer: 'l2',
    payment: 'Рубли, Мир',
  },
  {
    name: 'Serverspace',
    url: 'https://serverspace.io/pricing/',
    price: 'от €4.59/мес',
    regions: ['Казахстан (Алматы)', 'Россия', 'Нидерланды'],
    description: 'Казахстанский ДЦ Kazteleport. Tier II+, PCI DSS, ISO. Низкая задержка из РФ (~15-25мс).',
    layer: 'l2',
    payment: 'Рубли, карты РФ',
  },
  {
    name: 'is*hosting',
    url: 'https://ishosting.com/affiliate/NzI5MSM2',
    price: 'от $3/мес',
    regions: ['40+ ДЦ'],
    description: 'До 10 Гбит/с. 40+ дата-центров по всему миру.',
    layer: 'l2',
    payment: 'Рубли, крипто',
  },
  {
    name: 'RUVDS',
    url: 'https://ruvds.com/en-usd',
    price: 'от 139 ₽/мес',
    regions: ['Москва', 'СПб', 'Лондон', 'Цюрих', 'Алматы', 'Амстердам'],
    description: 'Российский провайдер с зарубежными ДЦ. Алматы + Амстердам — оптимальные exit-локации.',
    layer: 'l2',
    payment: 'Рубли, карты РФ',
  },
  {
    name: 'HOSTKEY',
    url: 'https://hostkey.com/vps/premium/',
    price: 'от 300 ₽/мес',
    regions: ['Нидерланды', 'Германия', 'Финляндия'],
    description: 'Собственные ДЦ в НЛ/DE/FI. Принимает BTC. Хорошая связность с РФ.',
    layer: 'l2',
    payment: 'Рубли, BTC',
  },
  {
    name: 'Fornex',
    url: 'https://fornex.com/c/ft1f1e/ru/virtual-private-servers/',
    price: 'от €5.50/мес',
    regions: ['Германия', 'Нидерланды', 'Швеция', 'Швейцария', 'США'],
    description: 'DE/NL/SE серверы. Принимает USDT. Стабильная работа с РФ.',
    layer: 'l2',
    payment: 'USDT',
  },
];

/* =============================================================
   Render helpers
   ============================================================= */

/**
 * Build a single provider card element.
 * @param {Object} provider
 * @returns {HTMLElement}
 */
function buildProviderCard(provider) {
  const card = document.createElement('article');
  card.className = 'provider-card';

  const regionTags = provider.regions
    .map(r => `<span class="region-tag">${escapeHtml(r)}</span>`)
    .join('');

  const paymentBadge = provider.payment
    ? `<span class="privacy-badge">
         <svg viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
           <path d="M21 4H3a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h18a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2Z" stroke="currentColor" stroke-width="2"/>
           <path d="M1 10h22" stroke="currentColor" stroke-width="2"/>
         </svg>
         ${escapeHtml(provider.payment)}
       </span>`
    : '';

  const warningBadge = provider.warning
    ? `<span class="privacy-badge" style="color:#e8a838;">
         <svg viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
           <path d="M12 9v4m0 4h.01M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0Z" stroke="currentColor" stroke-width="2" stroke-linejoin="round"/>
         </svg>
         ${escapeHtml(provider.warning)}
       </span>`
    : '';

  const layerClass = provider.layer === 'l1' ? 'layer-badge--l1' : 'layer-badge--l2';
  const layerLabel = provider.layer === 'l1' ? 'L1 Relay — РФ' : 'L2 Exit — Зарубеж';

  card.innerHTML = `
    <div class="provider-card__header">
      <h3 class="provider-card__name">${escapeHtml(provider.name)}</h3>
      <span class="provider-card__price">${escapeHtml(provider.price)}</span>
    </div>
    <p class="provider-card__desc">${escapeHtml(provider.description)}</p>
    <div class="provider-card__regions">${regionTags}</div>
    <div class="provider-card__footer">
      <div style="display:flex;gap:6px;align-items:center;flex-wrap:wrap;">
        <span class="layer-badge ${layerClass}">${layerLabel}</span>
        ${paymentBadge}${warningBadge}
      </div>
      <a
        href="${escapeHtml(provider.url)}"
        target="_blank"
        rel="noopener noreferrer"
        class="provider-card__link"
        aria-label="Перейти на ${escapeHtml(provider.name)}"
      >
        Перейти
        <svg viewBox="0 0 12 12" fill="none" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
          <path d="M2 10L10 2M10 2H4M10 2v6" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>
        </svg>
      </a>
    </div>
  `;

  return card;
}

/**
 * Render a list of providers into a container element.
 * @param {HTMLElement} container
 * @param {Object[]} providers
 */
function renderProviders(container, providers) {
  container.innerHTML = '';
  if (!providers || providers.length === 0) {
    container.innerHTML = '<p class="providers-loading">Провайдеры не найдены.</p>';
    return;
  }
  const fragment = document.createDocumentFragment();
  providers.forEach(p => fragment.appendChild(buildProviderCard(p)));
  container.appendChild(fragment);
}

/**
 * Minimal XSS escape for user-provided or external strings.
 * @param {string} str
 * @returns {string}
 */
function escapeHtml(str) {
  if (typeof str !== 'string') return '';
  return str
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/* =============================================================
   API fetch (optional, falls back to hardcoded data)
   ============================================================= */

const API_URL = '/api/providers';
const API_TIMEOUT_MS = 2500;
const API_KEY = '';

/**
 * Attempt to load providers from local API.
 * Returns { l1: [], l2: [] } or null on failure.
 */
async function fetchProviders() {
    try {
        const controller = new AbortController();
        const timeout = setTimeout(() => controller.abort(), API_TIMEOUT_MS);
        const response = await fetch(API_URL, {
            signal: controller.signal,
            headers: API_KEY ? { 'X-Api-Key': API_KEY } : {}
        });
        clearTimeout(timeout);
        if (!response.ok) return null;
        return await response.json();
    } catch {
        return null;
    }
}

/* =============================================================
   Tab switching
   ============================================================= */

function initTabs() {
  const tabBtns = document.querySelectorAll('.tab-btn');
  const panels = {
    l1: document.getElementById('panel-l1'),
    l2: document.getElementById('panel-l2'),
  };

  tabBtns.forEach(btn => {
    btn.addEventListener('click', () => {
      const target = btn.dataset.tab;
      if (!target || !panels[target]) return;

      // Update button states
      tabBtns.forEach(b => b.classList.remove('tab-btn--active'));
      btn.classList.add('tab-btn--active');

      // Show/hide panels
      Object.entries(panels).forEach(([key, panel]) => {
        if (key === target) {
          panel.classList.remove('tab-panel--hidden');
        } else {
          panel.classList.add('tab-panel--hidden');
        }
      });
    });
  });
}

/* =============================================================
   Burger menu (mobile)
   ============================================================= */

function initBurger() {
  const burger = document.getElementById('burger');
  const nav = document.getElementById('nav');
  if (!burger || !nav) return;

  burger.addEventListener('click', () => {
    const isOpen = nav.classList.toggle('nav--open');
    burger.classList.toggle('burger--open', isOpen);
    burger.setAttribute('aria-expanded', String(isOpen));
  });

  // Close on nav link click
  nav.querySelectorAll('.nav__link').forEach(link => {
    link.addEventListener('click', () => {
      nav.classList.remove('nav--open');
      burger.classList.remove('burger--open');
      burger.setAttribute('aria-expanded', 'false');
    });
  });

  // Close on outside click
  document.addEventListener('click', (e) => {
    if (!burger.contains(e.target) && !nav.contains(e.target)) {
      nav.classList.remove('nav--open');
      burger.classList.remove('burger--open');
      burger.setAttribute('aria-expanded', 'false');
    }
  });
}

/* =============================================================
   Header scroll effect
   ============================================================= */

function initHeaderScroll() {
  const header = document.getElementById('header');
  if (!header) return;

  let ticking = false;

  window.addEventListener('scroll', () => {
    if (!ticking) {
      requestAnimationFrame(() => {
        if (window.scrollY > 10) {
          header.style.boxShadow = '0 4px 20px rgba(0,0,0,0.5)';
        } else {
          header.style.boxShadow = '';
        }
        ticking = false;
      });
      ticking = true;
    }
  }, { passive: true });
}

/* =============================================================
   Smooth scroll for anchor buttons (hero secondary CTA)
   ============================================================= */

function initSmoothScroll() {
  document.querySelectorAll('a[href^="#"]').forEach(link => {
    link.addEventListener('click', (e) => {
      const target = link.getAttribute('href');
      if (!target || target === '#') return;
      const el = document.querySelector(target);
      if (el) {
        e.preventDefault();
        el.scrollIntoView({ behavior: 'smooth' });
      }
    });
  });
}

/* =============================================================
   Main init
   ============================================================= */

async function init() {
  initTabs();
  initBurger();
  initHeaderScroll();
  initSmoothScroll();

  const containerL1 = document.getElementById('providers-l1');
  const containerL2 = document.getElementById('providers-l2');

  if (!containerL1 || !containerL2) return;

  // Show loading state
  containerL1.innerHTML = '<p class="providers-loading">Загрузка провайдеров...</p>';
  containerL2.innerHTML = '<p class="providers-loading">Загрузка провайдеров...</p>';

  // Try live API, fall back to hardcoded
  const apiData = await fetchProviders();

  // API returns different field names — normalize to card format
  function normalizeProvider(p) {
    return {
      name: p.name,
      url: p.affiliate_url || p.url,
      price: p.price_display || p.price || '',
      regions: typeof p.regions === 'string' ? JSON.parse(p.regions) : (p.regions || []),
      description: p.description || '',
      layer: p.tier || p.layer || 'l2',
      payment: p.payment_methods || p.payment || null,
      warning: p.warning || null,
    };
  }

  const l1 = apiData ? apiData.l1.map(normalizeProvider) : PROVIDERS_L1;
  const l2 = apiData ? apiData.l2.map(normalizeProvider) : PROVIDERS_L2;

  renderProviders(containerL1, l1);
  renderProviders(containerL2, l2);
}

// Run after DOM is ready
if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', init);
} else {
  init();
}
