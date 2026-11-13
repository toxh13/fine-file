// site.js — 안내 사이트 공용: 스크롤 등장 효과, 기능 안내의 번호 켜고 끄기·크게 보기, 다운로드 정보.
// 스크립트가 없어도 모든 내용은 보인다 (효과만 빠진다).
(() => {
  const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  // ── 스크롤하면 부드럽게 올라오기 (한 번만, 처음부터 보이는 곳은 바로) ──
  const reveals = document.querySelectorAll('[data-reveal]');
  if (!reduce && 'IntersectionObserver' in window && reveals.length) {
    document.documentElement.classList.add('reveal-on');
    const io = new IntersectionObserver((entries) => {
      for (const e of entries) if (e.isIntersecting) { e.target.classList.add('is-in'); io.unobserve(e.target); }
    }, { rootMargin: '0px 0px -8% 0px', threshold: 0.08 });
    reveals.forEach((el) => io.observe(el));
    // 안전장치: 어떤 이유로든 2.5초 뒤에는 모두 보이게
    setTimeout(() => reveals.forEach((el) => el.classList.add('is-in')), 2500);
  }

  // ── 기능 안내: 번호 숨기기, 설명에 올리면 그 번호만 강조, 크게 보기 ──
  const box = document.querySelector('dialog.lightbox');
  for (const sec of document.querySelectorAll('.tour-sec')) {
    const fig = sec.querySelector('.tour');
    const pins = sec.querySelectorAll('.pin');
    const set = (n) => {
      fig.classList.toggle('has-focus', n != null);
      pins.forEach((p) => p.classList.toggle('is-on', p.dataset.n === n));
    };
    for (const li of sec.querySelectorAll('.tour__legend li')) {
      li.tabIndex = 0;
      li.addEventListener('mouseenter', () => set(li.dataset.n));
      li.addEventListener('focus', () => set(li.dataset.n));
      li.addEventListener('mouseleave', () => set(null));
      li.addEventListener('blur', () => set(null));
    }
    const toggle = sec.querySelector('[data-pins-toggle]');
    toggle?.addEventListener('click', () => {
      const off = fig.classList.toggle('pins-off');
      toggle.setAttribute('aria-pressed', String(off));
      toggle.textContent = off ? '번호 보이기' : '번호 숨기기';
    });
    const zoom = sec.querySelector('[data-zoom]');
    const img = sec.querySelector('.tour__stage img');
    const open = () => {
      if (!box || !box.showModal) { window.open(img.currentSrc || img.src, '_blank', 'noopener'); return; }
      let big = box.querySelector('img');
      if (!big) { big = document.createElement('img'); box.append(big); }
      big.src = img.currentSrc || img.src; big.alt = img.alt;
      box.querySelector('[data-lightbox-title]').textContent = sec.querySelector('h2').textContent;
      box.showModal();
    };
    zoom?.addEventListener('click', open);
    img?.addEventListener('click', open);
  }
  if (box) {
    box.addEventListener('click', (e) => { if (e.target === box || e.target.closest('[data-close]')) box.close(); });
  }

  // ── 다운로드: GitHub에서 최신 버전 정보 읽기 (실패하면 페이지에 적힌 값 그대로) ──
  const dl = document.querySelector('[data-latest]');
  if (dl) {
    fetch('https://api.github.com/repos/' + (dl.dataset.repo || 'toxh13/office-kit') + '/releases/latest', { headers: { Accept: 'application/vnd.github+json' } })
      .then((r) => (r.ok ? r.json() : Promise.reject(r.status)))
      .then((rel) => {
        const exe = (rel.assets || []).find((a) => new RegExp(dl.dataset.exe || '^OfficeKit-Setup-.*\.exe$').test(a.name));
        if (!exe) return;
        const ver = String(rel.tag_name || '').replace(/^v/, '');
        const mb = `${Math.round(exe.size / 1048576)}MB`;
        const date = (rel.published_at || '').slice(0, 10);
        document.querySelectorAll('[data-dl-href]').forEach((a) => { a.href = exe.browser_download_url; });
        document.querySelectorAll('[data-dl-version]').forEach((el) => { el.textContent = ver; });
        document.querySelectorAll('[data-dl-size]').forEach((el) => { el.textContent = mb; });
        document.querySelectorAll('[data-dl-date]').forEach((el) => { el.textContent = date; });
        document.querySelectorAll('[data-dl-file]').forEach((el) => { el.textContent = exe.name; });
        const m = /SHA-256[^`]*`([0-9A-Fa-f]{64})`/.exec(rel.body || '');
        // 릴리스 설명에 SHA-256이 있고 판이 바뀌었을 때만 바꾼다 (없으면 페이지에 적힌 값 그대로)
        if (m) document.querySelectorAll('[data-dl-sha]').forEach((el) => { el.textContent = m[1].toUpperCase(); });
      })
      .catch(() => {});
  }
})();
