<script>
  import { store } from '../live.svelte.js';
  import SwanMark from '../components/SwanMark.svelte';

  // Three quiet steps. Progress reads as "1 · 2 · 3" hairline segments,
  // not a loud stepper — the wizard should feel like turning a precision knob.
  const STEPS = ['admin', 'domain', 'ssl'];
  const STEP_LABEL = { admin: 'Create admin', domain: 'Dashboard URL', ssl: 'Encryption' };

  let step = $state(0); // 0..2

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
      step = 1;
      return;
    }
    if (effectiveApex && !/^[a-z0-9.-]+\.[a-z]{2,}$/i.test(effectiveApex)) {
      domainError = 'Apps domain looks off — check the spelling.';
      step = 1;
      return;
    }
    submitting = true;
    submitError = '';
    const r = await store.setup({
      email: email.trim(),
      password,
      dashboardDomain: dash,
      apexDomain: effectiveApex,
      ssl: sslAuto,
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
    <div class="steps" aria-label={`Step ${step + 1} of 3`}>
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
        <p class="lede">Where will the console live? Add a domain now, or skip it — Cygnus is reachable by IP until you point DNS.</p>
        <form class="form" onsubmit={(e) => { e.preventDefault(); step = 2; }}>
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

          <div class="derived">
            <span class="dlabel">Apps will be served at</span>
            <span class="dvalue num">*.{effectiveApex || 'apps.localhost'}</span>
          </div>

          <label for="su-apex" class="apexlab">
            <span>Apps domain <span class="muted">· editable</span></span>
            <span class="apex-row">
              <input
                id="su-apex"
                value={apexDomain}
                oninput={apexInput}
                type="text"
                autocapitalize="off"
                spellcheck="false"
                maxlength="253"
                placeholder="example.com"
              />
              {#if apexTouched}
                <button type="button" class="reset" onclick={resetApexToDerived}>reset</button>
              {/if}
            </span>
          </label>

          <p class="note mono">You don't need to own this domain or have DNS configured yet — you can point it later.</p>

          <div class="rowbtns">
            <button class="btn" type="button" onclick={back}>Back</button>
            <button class="btn cobalt" type="submit">Continue</button>
          </div>
        </form>
      </div>
    {:else}
      <div class="body screen-enter">
        <p class="lede">Last knob. HTTPS is on by default — Cygnus issues a trusted certificate the moment DNS propagates.</p>
        <form class="form" onsubmit={(e) => { e.preventDefault(); finish(); }}>
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
    width: 460px;
    max-width: 100%;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-xl);
    box-shadow: var(--shadow-pop);
    padding: 36px 36px 30px;
    text-align: center;
  }
  .mark {
    color: var(--cobalt);
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
  }
</style>
