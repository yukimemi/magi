// Opt-in browser regression: npm ci --prefix tools/demo && node tests/web_run_layout.mjs
// Uses shipped HTML/CSS and renderers with controlled records, without a daemon
// or control requests. MAGI_CHROME selects an installed browser; shots are optional.
import assert from 'node:assert/strict';
import { readFileSync, mkdirSync } from 'node:fs';
import { chromium } from '../tools/demo/node_modules/playwright-core/index.mjs';
const html = readFileSync(new URL('../assets/ui/index.html', import.meta.url), 'utf8');
const css = readFileSync(new URL('../assets/ui/app.css', import.meta.url), 'utf8');
const js = readFileSync(new URL('../assets/ui/app.js', import.meta.url), 'utf8').replace(/boot\(\);\s*$/, '')
  // Modes are driven directly, including standalone desktop. Screenshot viewport
  // changes must not navigate the unbooted fixture's empty URL back to Runs.
  .replace('const relayout = () => applyRoute();', 'const relayout = () => {};');
const browser = await chromium.launch({executablePath: process.env.MAGI_CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true});
const shots = process.env.MAGI_LAYOUT_SHOTS;
if (shots) mkdirSync(shots, {recursive: true});
try {
  let page;
  async function init(width, height) {
    page = await browser.newPage({viewport: {width, height}, reducedMotion: 'reduce'});
    await page.route('http://layout.test/**', route => route.fulfill({contentType: 'text/html', body: html.replace(/<script[^>]*src=[^>]*><\/script>/g, '').replace(/<link[^>]*>/g, '')}));
    await page.goto('http://layout.test');
    await page.addStyleTag({content: css});
    await page.addScriptTag({content: js + `
      window.layout = {
        setup(split, master) {
          document.querySelectorAll('main > .view').forEach(v => show(v, false));
          show($('view-run'), true); show($('view-runs'), split);
          document.body.toggleAttribute('data-split', split);
          const main = document.querySelector('main');
          main.toggleAttribute('data-split', split);
          main.style.setProperty('--split-master', master || '30%');
          setText($('run-h'), 'Review the existing branch');
          setText($('run-status'), 'REVIEWING');
          setText($('run-meta'), 'Review-only run · e2e verification');
          state.route = {name: 'run', id: 'layout', tab: 'overview'};
          state.streamOpen = true;
          state.detail = {run: {
            id: 'layout', status: 'reviewing', live: 'live', created_at: '2026-11-03T07:29:46Z', updated_at: '2026-11-03T08:50:00Z',
            instruction: 'Review the branch',
            active: {e2e: {task: true, node: 'verify', command: 'cargo test --locked --all-targets', index: 1, total: 1, timeout_secs: 2400, started_at: '2026-11-03T08:38:00Z'}},
            candidates: [{label: 'A', agent: 'existing-branch', duration_ms: 0, commits: 1, files: 1}, {label: 'B', agent: 'retained', duration_ms: 12000, commits: 2}],
            events: [{at: '2026-11-03T08:20:00Z', node: 'review', message: 'Panel answered'}, {at: '2026-11-03T08:30:00Z', node: 'verify', message: 'Waiting for shared build cache; verification command remains active'}],
            progress: {review_only: true, activity: 'reviewing · e2e', phases: [
              {key: 'prep', label: 'Prepare', state: 'complete', detail: 'Preparation complete'},
              {key: 'advice', label: 'Design advice', state: 'skipped', detail: 'Not applicable to this run'},
              {key: 'implement', label: 'Parallel implementation', state: 'skipped', detail: 'Not applicable to this run'},
              {key: 'selection', label: 'Selection', state: 'skipped', detail: 'Not applicable to this run'},
              {key: 'review', label: 'Review & verification', state: 'current', detail: 'e2e'},
              {key: 'gate', label: 'Final gate', state: 'upcoming', detail: 'No result yet'},
              {key: 'land', label: 'Publish & land', state: 'upcoming', detail: 'No result yet'}], rounds: [{round: 1, detail: '3/3 reviewers answered'}]}
          }, progressObservedAt: Date.now()};
          document.documentElement.dataset.theme = 'dark';
          state.runs = [{...state.detail.run, short: 'view', repo: 'project', title: 'Review the existing branch', base: 'main', candidates: 2, viable: 2, judges: 3, activity: 'reviewing · e2e'}];
          renderRuns();
          wireRunPanels();
          wireRunTabs();
          window.addEventListener('hashchange', () => { state.route.tab = parseRoute().tab; applyRunTab(); });
          this.render();
          setText($('run-tab-timeline-count'), '17');
          setText($('run-report'), 'Recorded report');
          setText($('run-events'), 'Recorded timeline');
        },
        render() { const r = state.detail.run; syncRunPanels(r); renderRunProgress(r); renderActive(r); renderCandidates(r); applyRunTab(); },
        refresh() { state.detail.run.events.push({at: '2026-11-03T08:40:00Z', node: 'verify', message: 'Verification still running'}); renderRunProgress(state.detail.run); syncRunPanels(state.detail.run); tickActive(); },
        legacy() { delete state.detail.run.progress; renderRunProgress(state.detail.run); renderActive(state.detail.run); },
      };
    `});
  }
  const rect = selector => page.locator(selector).boundingBox();
  const near = (a, b, label) => assert.ok(Math.abs(a - b) < 2, `${label}: ${a} vs ${b}`);
  for (const [width, height, split, master] of [
    [1531, 1518, true], [1530, 1518, true, '1050px'], [1499, 900, true], [1500, 900, true],
    [1080, 900, true], [1079, 900, false], [1530, 1000, false], [1280, 900, false], [390, 844, false], [320, 740, false],
  ]) {
    await init(width, height);
    await page.evaluate(([s, m]) => layout.setup(s, m), [split, master]);
    assert.equal(await page.locator('#progress-pipeline').evaluate(e => e.open), false, 'pipeline starts folded');
    await page.waitForTimeout(30);
    const label = `${width}-${split ? master ? 'narrow-split' : 'split' : 'standalone'}`;
    const pulse = await rect('#run-progress'), tabs = await rect('#run-tabs'), active = await rect('#run-active-panel');
    near(pulse.x, tabs.x, label + ' tabs align'); near(pulse.width, tabs.width, label + ' tabs width');
    near(pulse.width, active.width, label + ' active width');
    assert.ok(tabs.y >= pulse.y + pulse.height, label + ' tabs below pulse');
    assert.ok(active.y >= tabs.y + tabs.height && active.y < tabs.y + tabs.height + 50, label + ' operational content follows tabs');
    assert.ok(active.y + Math.min(active.height, 70) < height, label + ' active visible above fold');
    assert.equal(await page.locator('#progress-recent li').count(), 1);
    assert.equal(await page.locator('#progress-phases .progress-implementers li').count(), 0);
    assert.match(await page.locator('#run-cands-panel').innerText(), /Reviewed candidate records[\s\S]*not an implementation stage/i);
    assert.equal(await page.locator('#run-cands .cand').count(), 2);
    assert.match(await page.locator('#run-active').innerText(), /cargo test --locked --all-targets/);
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), label + ' no page overflow: ' + JSON.stringify(await page.evaluate(() => [...document.querySelectorAll('body *')].filter(e => e.getBoundingClientRect().right > innerWidth + 1).map(e => [e.id || e.className, Math.round(e.getBoundingClientRect().right)]).slice(0, 15))));
    assert.ok(await page.locator('#view-run').evaluate(e => e.scrollWidth <= e.clientWidth), label + ' no pane overflow');
    await page.locator('#progress-pipeline').evaluate(e => { e.open = true; });
    near((await rect('#run-active-panel')).y, active.y, label + ' expanded pipeline leaves active in place');
    for (const tab of ['timeline', 'report', 'overview']) {
      await page.locator('#run-tab-' + tab).click();
      await page.waitForFunction(t => document.getElementById('run-tab-' + t).getAttribute('aria-selected') === 'true', tab);
      near((await rect('#run-progress')).height, pulse.height, label + ' stable pulse height');
      for (const name of ['overview', 'timeline', 'report']) assert.equal(await page.locator('#run-tabpanel-' + name).isVisible(), tab === name);
      if (shots && [1531, 390].includes(width) && tab !== 'overview') await page.screenshot({path: `${shots}/${label}-${tab}.png`, fullPage: true});
    }
    if (shots && [1531, 1530, 390, 320].includes(width)) await page.screenshot({path: `${shots}/${label}-expanded.png`, fullPage: true});
    await page.locator('#run-tab-overview').focus();
    await page.keyboard.press('ArrowRight');
    await page.waitForFunction(() => document.getElementById('run-tab-timeline').getAttribute('aria-selected') === 'true');
    assert.ok(await page.locator('#run-tab-timeline').evaluate(e => e === document.activeElement));
    await page.keyboard.press('ArrowLeft');
    await page.waitForFunction(() => document.getElementById('run-tab-overview').getAttribute('aria-selected') === 'true');
    // Updates preserve the disclosure, phase elements, keyboard focus and scroll.
    await page.locator('#progress-pipeline summary').focus();
    const before = await page.evaluate(() => {
      window.savedPhase = $('progress-phases').firstElementChild;
      return {pane: $('view-run').scrollTop, page: scrollY};
    });
    await page.evaluate(() => layout.refresh());
    assert.ok(await page.locator('#progress-pipeline').evaluate(e => e.open));
    assert.ok(await page.evaluate(() => savedPhase === $('progress-phases').firstElementChild));
    assert.ok(await page.locator('#progress-pipeline summary').evaluate(e => e === document.activeElement));
    assert.deepEqual(await page.evaluate(() => ({pane: $('view-run').scrollTop, page: scrollY})), before);
    await page.evaluate(() => { $('view-run').scrollTop = 0; scrollTo(0, 0); $('progress-pipeline').open = false; });
    if (shots && [1531, 1530, 390, 320].includes(width)) await page.screenshot({path: `${shots}/${label}-collapsed.png`, fullPage: true});
    await page.waitForTimeout(30);
    assert.equal(await page.evaluate(() => runPanelOpen('layout', 'pipeline')), false, 'collapse is persisted');
    await page.locator('#progress-pipeline').evaluate(e => { e.open = true; });
    await page.waitForTimeout(30);
    await page.evaluate(() => { state.detail.run.id = 'another'; syncRunPanels(state.detail.run); });
    assert.equal(await page.locator('#progress-pipeline').evaluate(e => e.open), false);
    await page.waitForTimeout(30);
    await page.evaluate(() => { state.detail.run.id = 'layout'; syncRunPanels(state.detail.run); });
    assert.equal(await page.locator('#progress-pipeline').evaluate(e => e.open), true, 'open state returns per run');
    assert.equal(await page.locator('#progress-recent li').evaluate(e => getComputedStyle(e).animationName), 'none', 'reduced motion');
    await page.evaluate(() => {
      const run = state.detail.run;
      run.progress.review_only = false;
      run.progress.phases.find(p => p.key === 'implement').state = 'current';
      renderRunProgress(run);
    });
    assert.equal(await page.locator('#progress-phases .progress-implementers li').count(), 2, 'ordinary runs retain unknown and recorded results');
    assert.match(await page.locator('#progress-phases').innerText(), /Duration unrecorded/);
    assert.ok(await page.locator('#view-run').evaluate(e => e.scrollWidth <= e.clientWidth), label + ' expanded pipeline wraps');
    await page.evaluate(() => layout.legacy());
    assert.equal(await page.locator('#run-progress').isVisible(), false);
    assert.equal(await page.locator('#progress-pipeline').isVisible(), false);
    assert.equal(await page.locator('#run-active-panel').isVisible(), true);
    console.log('PASS', label);
    await page.close();
  }
} finally { await browser.close(); }
