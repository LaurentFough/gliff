//= outline.js v1.0.0 — draw the red feature outlines around elements with
//= data-hl="group": one box per group, around all its elements, padded.
(function () {
  const groups = {};
  document.querySelectorAll('[data-hl]').forEach(el => {
    (groups[el.dataset.hl] = groups[el.dataset.hl] || []).push(el.getBoundingClientRect());
  });
  Object.values(groups).forEach(rs => {
    const pad = 3;
    const l = Math.min(...rs.map(r => r.left)) - pad, t = Math.min(...rs.map(r => r.top)) - pad;
    const r = Math.max(...rs.map(r => r.right)) + pad, b = Math.max(...rs.map(r => r.bottom)) + pad;
    const d = document.createElement('div'); d.className = 'hl';
    Object.assign(d.style, { left: l + 'px', top: t + 'px', width: (r - l) + 'px', height: (b - t) + 'px' });
    document.body.appendChild(d);
  });
})();
