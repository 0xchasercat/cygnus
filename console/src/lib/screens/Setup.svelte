<script>
  import { store } from '../live.svelte.js';
  import SwanMark from '../components/SwanMark.svelte';

  // Three quiet steps. Progress reads as "1 · 2 · 3" hairline segments,
  // not a loud stepper — the wizard should feel like turning a precision knob.
  const STEPS = ['admin', 'listener', 'domain', 'ssl'];
  const STEP_LABEL = { admin: 'Create admin', listener: 'Listener', domain: 'Endpoint', ssl: 'Finish' };

  let step = $state(0); // 0..3

  // step 1 — admin
  let email = $state('');
  let password = $state('');
  let confirm = $state('');
  let showPw = $state(false);
  let adminErrors = $state({});

  // step 2 — dashboard domain + derived apex
  let dashboardDomain = $state('');
  let apexDomain = $state('');
  let apexTouched = $state(false);
  let domainError = $state('');

  // step 3 — ssl
  let sslAuto = $state(true);

  // Working-default ingress, with advanced modes disclosed only when chosen.
  let listenerMode = $state('integrated');
  let httpPort = $state(80);
  let httpsPort = $state(443);
  let dashboardPort = $state(3000);
  let appPortStart = $state(10000);
  let appPortEnd = $state(19999);
  let advertiseHost = $state('');
  let socketDir = $state('/run/cygnus/apps');
  let socketGroup = $state('www-data');
  let socketMode = $state('0660');
  let listenerError = $state('');

  let submitting = $state(false);
  let submitError = $state('');
  let emailEl = $state();

  $effect(() => {
    if (store.auth === 'setup') queueMicrotask(() => emailEl?.focus());
  });

  // Apex derivation: the entered host minus its leftmost label when it has
  // ≥3 labels, else the host itself. dashboard.cygnus.run → cygnus.run;
  // cygnus.run → cygnus.run. The user can override (co.uk-style cases).
  function deriveApex(host) {
    const h = (host || '').trim().toLowerCase().replace(/^https?:\/\//, '').replace(/\/.*$/, '');
    if (!h) return '';
    const labels = h.split('.');
    if (labels.length >= 3) return labels.slice(1).join('.');
    return h;
  }

  // When the dashboard domain changes and the user hasn't manually edited the
  // apex, recompute it so the derived line stays in lockstep.
  $effect(() => {
    if (!apexTouched) {
      apexDomain = deriveApex(dashboardDomain);
    }
  });

  const derivedApex = $derived(deriveApex(dashboardDomain));
  const effectiveApex = $derived((apexDomain || derivedApex || '').trim());

  const emailShape = $derived(/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email.trim()));
  const pwLong = $derived(password.length >= 8);
  const pwMatch = $derived(confirm.length > 0 && password === confirm);

  const adminValid = $derived(emailShape && pwLong && pwMatch);

  function validateAdmin() {
    const e = {};
    if (email && !emailShape) e.email = 'Enter a valid email address.';
    if (password && !pwLong) e.password = 'Use at least 8 characters.';
    if (confirm && password !== confirm) e.confirm = 'Passwords do not match.';
    adminErrors = e;
    return e;
  }

  function nextFromAdmin() {
    const e = validateAdmin();
    if (!emailShape) { adminErrors = { ...e, email: 'Enter a valid email address.' }; return; }
    if (!pwLong) { adminErrors = { ...e, password: 'Use at least 8 characters.' }; return; }
    if (!pwMatch) { adminErrors = { ...e, confirm: 'Passwords do not match.' }; return; }
    adminErrors = {};
    step = 1;
  }

  function back() {
    submitError = '';
    if (step > 0) step -= 1;
  }

  function apexInput(e) {
    apexTouched = true;
    apexDomain = e.currentTarget.value;
  }

  function resetApexToDerived() {
    apexTouched = false;
    apexDomain = deriveApex(dashboardDomain);
  }

  async function finish() {
    if (submitting) return;
    // Allow an empty dashboard domain — the dashboard stays reachable by IP
    // and apps default to apps.localhost.
    const dash = dashboardDomain.trim().toLowerCase();
    if (dash && !/^[a-z0-9.-]+\.[a-z]{2,}$/i.test(dash)) {
      domainError = 'Enter a domain like dashboard.example.com.';
      step = 2;
      return;
    }
    if (listenerMode === 'integrated' && effectiveApex && !/^[a-z0-9.-]+\.[a-z]{2,}$/i.test(effectiveApex)) {
      domainError = 'Apps domain looks off — check the spelling.';
      step = 2;
      return;
    }
    submitting = true;
    submitError = '';
    const r = await store.setup({
      email: email.trim(),
      password,
      dashboardDomain: dash,
      apexDomain: listenerMode === 'integrated' ? effectiveApex : '',
      ssl: listenerMode === 'integrated' && sslAuto,
      listener: listenerPayload(),
      dashboardListen: `0.0.0.0:${dashboardPort}`,
      httpsListen: listenerMode === 'integrated' ? `0.0.0.0:${httpsPort}` : null,
    });
    submitting = false;
    if (!r.ok) {
      if (r.status === 409 || r.error === 'already_setup') {
        // Already set up — hand back to the login screen.
        store.auth = 'signin';
        return;
      }
      submitError = r.error ?? 'Setup could not complete.';
    }
  }

  function listenerPayload() {
    if (listenerMode === 'tcp') {
      return {
        mode: 'tcp',
        host: '0.0.0.0',
        port_start: Number(appPortStart),
        port_end: Number(appPortEnd),
        advertise_host: advertiseHost.trim(),
      };
    }
    if (listenerMode === 'uds') {
      return {
        mode: 'uds',
        socket_dir: socketDir.trim(),
        socket_group: socketGroup.trim() || null,
        socket_mode: parseInt(socketMode, 8),
      };
    }
    return {
      mode: 'integrated',
      http_listen: `0.0.0.0:${httpPort}`,
    };
  }

  function nextFromListener() {
    listenerError = '';
    const ports = [dashboardPort, ...(listenerMode === 'integrated' ? [httpPort, httpsPort] : [])].map(Number);
    if (ports.some((p) => !Number.isInteger(p) || p < 1 || p > 65535) || new Set(ports).size !== ports.length) {
      listenerError = 'Ports must be unique whole numbers from 1 to 65535.';
      return;
    }
    if (listenerMode === 'tcp' && (!Number.isInteger(Number(appPortStart)) || !Number.isInteger(Number(appPortEnd)) || Number(appPortStart) < 1 || Number(appPortEnd) > 65535 || Number(appPortStart) > Number(appPortEnd))) {
      listenerError = 'Enter a valid app port range within 1–65535.';
      return;
    }
    if (listenerMode === 'tcp' && !advertiseHost.trim()) {
      listenerError = 'Enter the public host or IP users will connect to.';
      return;
    }
    if (listenerMode === 'uds' && (!socketDir.startsWith('/') || !/^0[0-7]{3}$/.test(socketMode))) {
      listenerError = 'Use an absolute socket directory and a four-digit octal mode such as 0660.';
      return;
    }
    step = 2;
  }

  function onKeydown(e) {
    if (e.key === 'Enter' && adminValid) {
      e.preventDefault();
      nextFromAdmin();
    }
  }
</script>

<div class="canvas-marks"></div>

<main class="setup">
  <section class="card" aria-labelledby="setup-title">
    <div class="mark"><SwanMark size={30} /></div>
    <h1 id="setup-title" class="word">CYGNUS</h1>
    <p class="line">First-run setup · create the admin account</p>

    <!-- quiet hairline progress: 1 · 2 · 3 -->
    <div class="steps" aria-label={`Step ${step + 1} of ${STEPS.length}`}>
      {#each STEPS as s, i}
        <span class="seg {i === step ? 'on' : i < step ? 'done' : ''}">
          <span class="seg-bar"></span>
          <span class="seg-num num">{i + 1}</span>
          <span class="seg-label">{STEP_LABEL[s]}</span>
        </span>
        {#if i < STEPS.length - 1}<span class="seg-dot">·</span>{/if}
      {/each}
    </div>

    {#if step === 0}
      <div class="body screen-enter">
        <p class="lede">You're the first person here. Create the admin account that owns this node — there's only one, and it's all you need.</p>
        <form class="form" onsubmit={(e) => { e.preventDefault(); nextFromAdmin(); }}>
          <label for="su-email">Admin email
            <input
              id="su-email"
              bind:this={emailEl}
              bind:value={email}
              onkeydown={onKeydown}
              type="email"
              autocomplete="email"
              autocapitalize="off"
              spellcheck="false"
              maxlength="254"
              placeholder="you@example.com"
              required
            />
          </label>
          {#if adminErrors.email}<p class="err" role="alert">{adminErrors.email}</p>{/if}

          <label for="su-pw" class="pwlab">
            <span>Password <span class="muted">· 8+ chars</span></span>
            <span class="pw-field">
              <input
                id="su-pw"
                bind:value={password}
                type={showPw ? 'text' : 'password'}
                autocomplete="new-password"
                maxlength="1024"
                required
              />
              <button type="button" class="reveal" onclick={() => (showPw = !showPw)} aria-label={showPw ? 'Hide password' : 'Show password'}>{showPw ? 'hide' : 'show'}</button>
            </span>
          </label>
          {#if adminErrors.password}<p class="err" role="alert">{adminErrors.password}</p>{/if}

          <label for="su-pw2">Confirm password
            <input
              id="su-pw2"
              bind:value={confirm}
              type={showPw ? 'text' : 'password'}
              autocomplete="new-password"
              maxlength="1024"
              required
            />
          </label>
          {#if adminErrors.confirm}<p class="err" role="alert">{adminErrors.confirm}</p>{/if}

          <button class="btn cobalt primary" type="submit" disabled={!adminValid}>
            Continue
          </button>
        </form>
      </div>
    {:else if step === 1}
      <div class="body screen-enter">
        <p class="lede">Choose how apps leave this node. Integrated is the fuss-free default: Cygnus owns ingress and HTTPS.</p>
        <form class="form" onsubmit={(e) => { e.preventDefault(); nextFromListener(); }}>
          <fieldset class="mode-picker">
            <legend>Listener mode</legend>
            {#each [
              { id: 'integrated', title: 'Integrated', copy: 'Domains + automatic HTTPS. Recommended.' },
              { id: 'tcp', title: 'TCP ports', copy: 'One host port per app; TLS stays upstream.' },
              { id: 'uds', title: 'Unix sockets', copy: 'No app TCP ports; connect a local proxy.' },
            ] as mode}
              <label class:chosen={listenerMode === mode.id}>
                <input type="radio" name="listener-mode" value={mode.id} bind:group={listenerMode} />
                <span><strong>{mode.title}</strong><small>{mode.copy}</small></span>
              </label>
            {/each}
          </fieldset>

          <div class="listener-fields">
            <label>Dashboard port<input bind:value={dashboardPort} type="number" min="1" max="65535" required /></label>
            {#if listenerMode === 'integrated'}
              <label>HTTP port<input bind:value={httpPort} type="number" min="1" max="65535" required /></label>
              <label>HTTPS port<input bind:value={httpsPort} type="number" min="1" max="65535" required /></label>
            {:else if listenerMode === 'tcp'}
              <label>App ports · from<input bind:value={appPortStart} type="number" min="1" max="65535" required /></label>
              <label>App ports · through<input bind:value={appPortEnd} type="number" min="1" max="65535" required /></label>
              <label class="wide">Public host or IP<input bind:value={advertiseHost} maxlength="253" placeholder="node.example.com or 203.0.113.10" required /></label>
              <p class="note mono">Apps receive the next free port in this range. Open the range in your firewall or proxy only as broadly as needed.</p>
            {:else}
              <label class="wide">Socket directory<input bind:value={socketDir} maxlength="4096" required /></label>
              <label>Proxy group<input bind:value={socketGroup} maxlength="64" placeholder="www-data" /></label>
              <label>Socket mode<input bind:value={socketMode} inputmode="numeric" pattern="0[0-7]{3}" maxlength="4" required /></label>
              <p class="note mono">Apps appear as {socketDir || '/run/cygnus/apps'}/&lt;app&gt;.sock · point Caddy or Nginx upstream at that path.</p>
            {/if}
          </div>
          {#if listenerError}<p class="err" role="alert">{listenerError}</p>{/if}
          <div class="rowbtns">
            <button class="btn" type="button" onclick={back}>Back</button>
            <button class="btn cobalt" type="submit">Continue</button>
          </div>
        </form>
      </div>
    {:else if step === 2}
      <div class="body screen-enter">
        <p class="lede">
          {listenerMode === 'integrated'
            ? 'Where will the console live? Add a domain now, or skip it — Cygnus remains reachable by IP.'
            : 'Your dashboard remains on TCP; app endpoints are allocated automatically from the listener policy.'}
        </p>
        <form class="form" onsubmit={(e) => { e.preventDefault(); step = 3; }}>
          <label for="su-dash">Dashboard domain
            <input
              id="su-dash"
              bind:value={dashboardDomain}
              type="text"
              inputmode="url"
              autocapitalize="off"
              spellcheck="false"
              maxlength="253"
              placeholder="dashboard.example.com"
            />
          </label>
          {#if domainError}<p class="err" role="alert">{domainError}</p>{/if}

          {#if listenerMode === 'integrated'}
            <div class="derived">
              <span class="dlabel">Apps will be served at</span>
              <span class="dvalue num">*.{effectiveApex || 'apps.localhost'}</span>
            </div>
            <label for="su-apex" class="apexlab">
              <span>Apps domain <span class="muted">· editable</span></span>
              <span class="apex-row">
                <input id="su-apex" value={apexDomain} oninput={apexInput} type="text" autocapitalize="off" spellcheck="false" maxlength="253" placeholder="example.com" />
                {#if apexTouched}<button type="button" class="reset" onclick={resetApexToDerived}>reset</button>{/if}
              </span>
            </label>
          {:else}
            <div class="derived">
              <span class="dlabel">App endpoint</span>
              <span class="dvalue num">{listenerMode === 'tcp' ? `node:${appPortStart}–${appPortEnd}` : `${socketDir}/<app>.sock`}</span>
            </div>
          {/if}

          <p class="note mono">You don't need to own this domain or have DNS configured yet — you can point it later.</p>

          <div class="rowbtns">
            <button class="btn" type="button" onclick={back}>Back</button>
            <button class="btn cobalt" type="submit">Continue</button>
          </div>
        </form>
      </div>
    {:else}
      <div class="body screen-enter">
        <p class="lede">{listenerMode === 'integrated' ? 'Last knob. HTTPS is on by default — Cygnus issues a trusted certificate when DNS propagates.' : 'Everything is ready. TLS for app traffic stays with your upstream proxy.'}</p>
        <form class="form" onsubmit={(e) => { e.preventDefault(); finish(); }}>
          {#if listenerMode === 'integrated'}
          <button type="button" class="toggle {sslAuto ? 'on' : ''}" onclick={() => (sslAuto = !sslAuto)} aria-pressed={sslAuto}>
            <span class="track"><span class="thumb"></span></span>
            <span class="tmeta">
              <span class="ttitle">Automatic HTTPS <span class="muted">· Let's Encrypt</span></span>
              <span class="tsub">Issued automatically once DNS resolves here.</span>
            </span>
          </button>

          <p class="note mono">
            {#if sslAuto}
              If your DNS isn't pointed here yet, Cygnus serves a self-signed certificate so your apps work instantly, then upgrades to a trusted certificate automatically once DNS propagates.
            {:else}
              Self-signed only. Browsers will warn until you switch to automatic HTTPS from Settings.
            {/if}
          </p>
          {:else}
            <div class="derived">
              <span class="dlabel">{listenerMode === 'tcp' ? 'App ports' : 'Socket directory'}</span>
              <span class="dvalue num">{listenerMode === 'tcp' ? `${appPortStart}–${appPortEnd}` : socketDir}</span>
            </div>
            <p class="note mono">Dashboard listens on port {dashboardPort}. App TLS termination and public routing remain under your control.</p>
          {/if}

          {#if submitError}<p class="err" role="alert">{submitError}</p>{/if}

          <div class="rowbtns">
            <button class="btn" type="button" onclick={back} disabled={submitting}>Back</button>
            <button class="btn cobalt primary" type="submit" disabled={submitting}>
              {submitting ? 'Provisioning…' : 'Finish setup'}
            </button>
          </div>
        </form>
      </div>
    {/if}
  </section>
</main>

<style>
  .setup {
    position: relative;
    z-index: 1;
    min-height: 100vh;
    display: grid;
    place-items: center;
    padding: 24px;
  }
  .card {
    width: 520px;
    max-width: 100%;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-xl);
    box-shadow: var(--shadow-pop);
    padding: 36px 36px 30px;
    text-align: center;
  }
  .mark {
    color: var(--ink);
    display: flex;
    justify-content: center;
    margin-bottom: 16px;
  }
  .word {
    font-size: 18px;
    font-weight: 700;
    letter-spacing: 0.34em;
    font-family: var(--mono);
    color: var(--ink);
  }
  .line {
    margin-top: 8px;
    font-size: 12px;
    color: var(--ink-3);
  }

  /* hairline progress — the only structural flourish */
  .steps {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 14px;
    margin: 22px 0 20px;
  }
  .seg {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    color: var(--ink-4);
    transition: color 0.18s ease;
  }
  .seg-label { font-size: 11.5px; font-weight: 500; white-space: nowrap; }
  .seg-bar {
    width: 22px;
    height: 2px;
    border-radius: 2px;
    background: var(--line-strong);
    transition: background 0.18s ease;
  }
  .seg-num { font-size: 11px; font-weight: 600; }
  .seg.done { color: var(--ink-3); }
  .seg.done .seg-bar { background: var(--ink-3); }
  .seg.on { color: var(--ink); }
  .seg.on .seg-bar { background: var(--cobalt); }
  .seg-dot { color: var(--ink-4); font-size: 11px; }

  .body { text-align: left; }
  .lede {
    font-size: 13px;
    line-height: 1.55;
    color: var(--ink-2);
    margin-bottom: 18px;
  }

  .form {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .mode-picker {
    display: grid;
    gap: 7px;
    margin: 0;
    padding: 0;
    border: 0;
  }
  .mode-picker legend {
    margin-bottom: 6px;
    font: 500 10px var(--mono);
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--ink-3);
  }
  .mode-picker label {
    display: grid;
    grid-template-columns: auto 1fr;
    align-items: start;
    gap: 10px;
    padding: 10px 11px;
    border: 1px solid var(--line);
    border-radius: 10px;
    cursor: pointer;
    text-transform: none;
    letter-spacing: 0;
  }
  .mode-picker label.chosen { border-color: color-mix(in srgb, var(--cobalt) 42%, var(--line)); background: var(--cobalt-ghost); }
  .mode-picker input { width: 15px; height: 15px; margin-top: 2px; accent-color: var(--cobalt); }
  .mode-picker span { display: grid; gap: 2px; }
  .mode-picker strong { font: 600 12.5px var(--sans); color: var(--ink); }
  .mode-picker small { font: 400 11px var(--sans); color: var(--ink-3); }
  .listener-fields {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 10px;
    padding-top: 4px;
  }
  .listener-fields .wide, .listener-fields .note { grid-column: 1 / -1; }
  label {
    display: grid;
    gap: 6px;
    font-family: var(--mono);
    font-size: 10px;
    font-weight: 500;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--ink-3);
    text-align: left;
  }
  .muted { text-transform: none; letter-spacing: 0; color: var(--ink-4); font-weight: 400; }
  input {
    width: 100%;
    box-sizing: border-box;
    border: 1px solid var(--line-strong);
    border-radius: 9px;
    background: var(--surface);
    color: var(--ink);
    padding: 11px 12px;
    font-family: var(--mono);
    font-size: 12.5px;
    text-transform: none;
    letter-spacing: 0;
  }
  input:focus-visible {
    outline: 2px solid var(--cobalt);
    outline-offset: 1px;
  }
  input::placeholder { color: var(--ink-4); }

  .pw-field, .apex-row {
    display: flex;
    align-items: stretch;
  }
  .pw-field input { border-radius: 9px 0 0 9px; }
  .apex-row input { border-radius: 9px 0 0 9px; }
  .reveal, .reset {
    font-family: var(--mono);
    font-size: 10.5px;
    color: var(--ink-3);
    background: var(--surface-3);
    border: 1px solid var(--line-strong);
    border-left: none;
    border-radius: 0 9px 9px 0;
    padding: 0 12px;
    letter-spacing: 0.04em;
  }
  .reveal:hover, .reset:hover { color: var(--ink); background: var(--surface-2); }

  .pwlab, .apexlab { gap: 6px; }

  .derived {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    padding: 11px 13px;
    border: 1px solid var(--line-2);
    border-radius: 9px;
    background: var(--surface-2);
  }
  .dlabel {
    font-family: var(--mono);
    font-size: 10px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--ink-3);
  }
  .dvalue {
    font-size: 13px;
    font-weight: 600;
    color: var(--ink);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .note {
    margin: 2px 0 0;
    font-size: 10.5px;
    line-height: 1.6;
    color: var(--ink-4);
    letter-spacing: 0.01em;
  }

  /* toggle — the one cobalt flourish on step 3 */
  .toggle {
    display: flex;
    align-items: center;
    gap: 14px;
    width: 100%;
    padding: 14px 15px;
    border: 1px solid var(--line-strong);
    border-radius: 12px;
    background: var(--surface);
    text-align: left;
    transition: border-color 0.16s ease, background 0.16s ease;
  }
  .toggle:hover { border-color: var(--ink-4); }
  .toggle.on { border-color: color-mix(in srgb, var(--cobalt) 40%, var(--line-strong)); background: var(--cobalt-ghost); }
  .track {
    flex: none;
    width: 38px;
    height: 22px;
    border-radius: 22px;
    background: var(--line-strong);
    position: relative;
    transition: background 0.18s ease;
  }
  .toggle.on .track { background: var(--cobalt); }
  .thumb {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: #fff;
    box-shadow: 0 1px 2px rgba(13, 18, 28, 0.2);
    transition: transform 0.18s cubic-bezier(0.22, 1, 0.36, 1);
  }
  .toggle.on .thumb { transform: translateX(16px); }
  .tmeta { display: flex; flex-direction: column; gap: 2px; }
  .ttitle { font-size: 13px; font-weight: 600; color: var(--ink); }
  .tsub { font-size: 11px; color: var(--ink-3); }

  .rowbtns {
    display: flex;
    gap: 10px;
    margin-top: 4px;
  }
  .rowbtns .btn { flex: 1; height: 38px; }
  .rowbtns .btn:first-child { flex: 0 0 110px; }

  .btn.cobalt.primary {
    width: 100%;
    height: 38px;
    font-size: 13px;
  }

  .err {
    color: var(--red);
    font-size: 11.5px;
    line-height: 1.5;
    text-align: left;
    overflow-wrap: anywhere;
    margin-top: -2px;
  }

  @media (max-width: 520px) {
    .card { padding: 28px 22px 24px; }
    .seg-label { display: none; }
    .rowbtns .btn:first-child { flex: 0 0 90px; }
    .listener-fields { grid-template-columns: 1fr; }
    .listener-fields .wide, .listener-fields .note { grid-column: auto; }
  }
</style>
