// Renders the fake demo illustrations used by `iris demo-seed` (feed photos,
// a chat photo, a result card) to JPEG. They are simple drawn scenes, not
// photos, so nothing copyrighted ships with the repo.
//
//   npm i playwright   (in any temp folder), then:
//   node scripts/demo/make-images.mjs <out dir>
import { chromium } from 'playwright';
import { mkdirSync } from 'node:fs';

const out = process.argv[2] || 'scripts/demo/images';
mkdirSync(out, { recursive: true });

const sky = (a, b) => `<linearGradient id="sky" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${a}"/><stop offset="1" stop-color="${b}"/></linearGradient>`;
const svg = (defs, body) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 600" width="800" height="600"><defs>${defs}</defs>${body}</svg>`;

const scenes = {
  // A potted chrysanthemum on a porch.
  'plant.jpg': svg(sky('#f6e7d3', '#e9cfae'), `<rect width="800" height="600" fill="url(#sky)"/>
    <rect y="420" width="800" height="180" fill="#b98b5e"/>${[0, 1, 2, 3, 4, 5, 6, 7].map((i) => `<rect x="${i * 100}" y="420" width="96" height="180" fill="#c49768"/>`).join('')}
    <path d="M300 430 L500 430 L470 560 L330 560 Z" fill="#a4553a"/><rect x="290" y="410" width="220" height="30" rx="8" fill="#b9644a"/>
    ${Array.from({ length: 34 }, (_, i) => { const a = i * 2.4; const r = 40 + (i % 5) * 18; const x = 400 + Math.cos(a) * r * 1.4; const y = 330 + Math.sin(a) * r * 0.8; return `<circle cx="${x.toFixed(0)}" cy="${y.toFixed(0)}" r="${22 + (i % 3) * 4}" fill="${['#f2b134', '#e8962f', '#f5c84c'][i % 3]}"/><circle cx="${x.toFixed(0)}" cy="${y.toFixed(0)}" r="7" fill="#8a5a1c"/>`; }).join('')}
    <path d="M250 400 Q400 300 550 400" stroke="#4f7a3a" stroke-width="14" fill="none"/>`),
  // A restaurant patio at dusk with string lights.
  'kindred.jpg': svg(sky('#2c2547', '#e9875a'), `<rect width="800" height="600" fill="url(#sky)"/>
    <rect y="380" width="800" height="220" fill="#3b2a26"/>
    <path d="M0 120 Q200 190 400 130 T800 140" stroke="#222" stroke-width="2" fill="none"/>
    ${Array.from({ length: 16 }, (_, i) => `<circle cx="${25 + i * 50}" cy="${140 + Math.sin(i * 0.8) * 25}" r="9" fill="#ffd27a"/><circle cx="${25 + i * 50}" cy="${140 + Math.sin(i * 0.8) * 25}" r="22" fill="#ffd27a" opacity=".18"/>`).join('')}
    ${[150, 400, 650].map((x) => `<ellipse cx="${x}" cy="420" rx="90" ry="22" fill="#f4ede2"/><rect x="${x - 6}" y="420" width="12" height="90" fill="#2a1d19"/><rect x="${x - 120}" y="430" width="30" height="70" rx="6" fill="#6b4a3b"/><rect x="${x + 90}" y="430" width="30" height="70" rx="6" fill="#6b4a3b"/><rect x="${x - 10}" y="380" width="20" height="34" rx="4" fill="#ffcf73" opacity=".9"/>`).join('')}
    <rect x="0" y="250" width="800" height="12" fill="#2a1d19"/>${Array.from({ length: 9 }, (_, i) => `<rect x="${i * 100 + 40}" y="200" width="18" height="60" rx="8" fill="#4d7d43"/><circle cx="${i * 100 + 49}" cy="195" r="26" fill="#5d9450"/>`).join('')}`),
  // Walnut slab bench, three views.
  'bench-1.jpg': svg(sky('#efe6da', '#d9c8b2'), `<rect width="800" height="600" fill="url(#sky)"/><rect y="440" width="800" height="160" fill="#a8927a"/>
    <path d="M120 300 Q400 270 690 305 L700 350 Q400 330 110 350 Z" fill="#6b4226"/><path d="M120 300 Q400 270 690 305" stroke="#8a5a36" stroke-width="6" fill="none"/>
    ${[0, 1, 2, 3, 4, 5].map((i) => `<path d="M${160 + i * 90} 318 q30 -6 70 2" stroke="#4f2f1b" stroke-width="3" fill="none" opacity=".6"/>`).join('')}
    <path d="M180 350 L200 470 M200 350 L180 470 M600 350 L620 470 M620 350 L600 470" stroke="#1d1d1f" stroke-width="7"/>`),
  'bench-2.jpg': svg(sky('#e8dccd', '#cdb79c'), `<rect width="800" height="600" fill="url(#sky)"/>
    <rect x="80" y="180" width="640" height="240" rx="30" fill="#5e3a22"/>${Array.from({ length: 12 }, (_, i) => `<path d="M100 ${200 + i * 18} Q400 ${180 + i * 20} 700 ${205 + i * 17}" stroke="#7d4f30" stroke-width="5" fill="none" opacity=".7"/>`).join('')}
    <ellipse cx="300" cy="300" rx="40" ry="18" fill="#3e2414"/><ellipse cx="300" cy="300" rx="22" ry="9" fill="#5e3a22"/>`),
  'bench-3.jpg': svg(sky('#f3ece2', '#e0d0bb'), `<rect width="800" height="600" fill="url(#sky)"/>
    <rect x="160" y="160" width="140" height="280" rx="16" fill="#c9b08f"/><rect x="190" y="110" width="80" height="60" rx="10" fill="#d8c3a5"/><text x="230" y="320" font-size="34" text-anchor="middle" fill="#6b4a2b" font-family="Georgia">OIL</text>
    <path d="M420 420 L700 420 L690 450 L430 450 Z" fill="#6b4226"/><path d="M420 420 L700 420" stroke="#9a6a43" stroke-width="5"/><circle cx="560" cy="300" r="70" fill="#f1e6d6"/><path d="M520 300 h80 M560 260 v80" stroke="#c9a77c" stroke-width="8" stroke-linecap="round"/>`),
  // Farm corn maze and pumpkins.
  'maze-1.jpg': svg(sky('#9fd0f0', '#f5e6c4'), `<rect width="800" height="600" fill="url(#sky)"/><circle cx="640" cy="110" r="50" fill="#fff3c4"/>
    <rect y="300" width="800" height="300" fill="#d6b45a"/>${Array.from({ length: 40 }, (_, i) => `<path d="M${i * 20} 300 l8 -60 l8 60" fill="#b8963d"/>`).join('')}
    <path d="M0 470 Q200 420 400 470 T800 450" stroke="#a4823a" stroke-width="40" fill="none"/><path d="M380 300 L420 600" stroke="#a4823a" stroke-width="34"/>`),
  'maze-2.jpg': svg(sky('#f7d9a8', '#e9a86a'), `<rect width="800" height="600" fill="url(#sky)"/><rect y="360" width="800" height="240" fill="#7a8a3a"/>
    ${[[160, 430, 70], [330, 470, 90], [520, 440, 60], [660, 490, 80], [250, 540, 50], [450, 550, 55]].map(([x, y, r]) => `<ellipse cx="${x}" cy="${y}" rx="${r}" ry="${r * 0.78}" fill="#e8742a"/><path d="M${x} ${y - r * 0.78} q-${r * 0.4} ${r * 0.78} 0 ${r * 1.56} M${x} ${y - r * 0.78} q${r * 0.4} ${r * 0.78} 0 ${r * 1.56}" stroke="#c45a1d" stroke-width="4" fill="none"/><rect x="${x - 5}" y="${y - r * 0.78 - 18}" width="10" height="20" rx="3" fill="#4f6b2a"/>`).join('')}`),
  // Abstract "AI this week" graphic.
  'ai.jpg': svg(`<linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#2b2d6e"/><stop offset="1" stop-color="#7c5cf6"/></linearGradient>`, `<rect width="800" height="600" fill="url(#g)"/>
    ${Array.from({ length: 9 }, (_, i) => Array.from({ length: 7 }, (_, j) => `<circle cx="${100 + i * 75}" cy="${80 + j * 75}" r="${3 + ((i * j) % 4)}" fill="#c9bbff" opacity="${0.3 + ((i + j) % 4) * 0.15}"/>`).join('')).join('')}
    <rect x="300" y="200" width="200" height="200" rx="28" fill="#1b1840" stroke="#a78bfa" stroke-width="6"/><rect x="345" y="245" width="110" height="110" rx="14" fill="#a78bfa"/>
    ${[0, 1, 2, 3].map((k) => `<path d="M${330 + k * 46} 200 v-40 M${330 + k * 46} 400 v40 M300 ${230 + k * 46} h-40 M500 ${230 + k * 46} h40" stroke="#a78bfa" stroke-width="6" stroke-linecap="round"/>`).join('')}`),
  // Porch garden planters.
  'garden-1.jpg': svg(sky('#dff0e6', '#bfdcc8'), `<rect width="800" height="600" fill="url(#sky)"/><rect y="430" width="800" height="170" fill="#9c7b5b"/>
    ${[[170, 'M'], [400, 'L'], [630, 'M']].map(([x]) => `<rect x="${x - 90}" y="330" width="180" height="110" rx="10" fill="#3f6f78"/>${Array.from({ length: 7 }, (_, i) => `<ellipse cx="${x - 70 + i * 23}" cy="${310 - (i % 3) * 25}" rx="20" ry="46" fill="${['#4f9a52', '#3f8a46', '#69b05d'][i % 3]}" transform="rotate(${(i - 3) * 12} ${x - 70 + i * 23} 330)"/>`).join('')}`).join('')}`),
  'garden-2.jpg': svg(sky('#f4efe2', '#e2d7bd'), `<rect width="800" height="600" fill="url(#sky)"/>
    ${Array.from({ length: 5 }, (_, i) => `<rect x="${90 + i * 130}" y="${380 - (i % 2) * 40}" width="100" height="120" rx="12" fill="${['#c8714e', '#d9a066', '#b85c43'][i % 3]}"/><circle cx="${140 + i * 130}" cy="${330 - (i % 2) * 40}" r="56" fill="${['#5e9b4f', '#7bb662', '#4c8a45'][i % 3]}"/><circle cx="${125 + i * 130}" cy="${315 - (i % 2) * 40}" r="12" fill="#f5d04c"/>`).join('')}`),
  // A product deal: headphones.
  'deal.jpg': svg(sky('#f1f1f4', '#dcdce4'), `<rect width="800" height="600" fill="url(#sky)"/>
    <path d="M250 330 Q250 150 400 150 Q550 150 550 330" stroke="#2b2b33" stroke-width="30" fill="none" stroke-linecap="round"/>
    <rect x="200" y="300" width="100" height="160" rx="40" fill="#34343e"/><rect x="500" y="300" width="100" height="160" rx="40" fill="#34343e"/>
    <rect x="220" y="320" width="60" height="120" rx="28" fill="#7c5cf6"/><rect x="520" y="320" width="60" height="120" rx="28" fill="#7c5cf6"/>
    <rect x="600" y="80" width="150" height="70" rx="35" fill="#e0245e"/><text x="675" y="128" font-size="36" font-weight="700" text-anchor="middle" fill="#fff" font-family="Arial">-30%</text>`),
  // A forest trail.
  'trail.jpg': svg(sky('#cfe6f2', '#f2efe0'), `<rect width="800" height="600" fill="url(#sky)"/>
    <path d="M0 330 L160 210 L300 300 L460 170 L640 290 L800 220 L800 600 L0 600Z" fill="#8fb0a8"/>
    <path d="M0 400 L800 360 L800 600 L0 600Z" fill="#5f8a55"/>
    <path d="M330 600 Q380 480 430 420 Q450 395 470 380" stroke="#d9c49a" stroke-width="70" fill="none"/>
    ${Array.from({ length: 12 }, (_, i) => `<path d="M${30 + i * 68} ${430 - (i % 3) * 20} l34 -120 l34 120z" fill="${['#2f5d3a', '#3d6e44', '#285233'][i % 3]}"/>`).join('')}`),
};

const browser = await chromium.launch({ executablePath: process.env.CHROME || undefined });
const page = await browser.newPage({ viewport: { width: 800, height: 600 } });
for (const [name, markup] of Object.entries(scenes)) {
  await page.setContent(`<html><body style="margin:0">${markup}</body></html>`);
  await page.screenshot({ path: `${out}/${name}`, type: 'jpeg', quality: 72 });
}
await browser.close();
console.log(`wrote ${Object.keys(scenes).length} images to ${out}`);
