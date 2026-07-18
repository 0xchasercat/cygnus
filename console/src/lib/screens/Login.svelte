<script>
  import { store } from '../live.svelte.js';
  import SwanMark from '../components/SwanMark.svelte';

  // Primary auth is the admin account (email + password). The bootstrap token
  // stays as a recovery affordance — the backend keeps token login as a
  // fallback so a locked-out admin can re-enter with the installer token.
  let email = $state('');
  let password = $state('');
  let token = $state('');
  let error = $state('');
  let submitting = $state(false);
  let recovering = $state(false);
  let emailEl = $state();
  let tokenEl = $state();

  $effect(() => {
    if (store.auth === 'signin') queueMicrotask(() => emailEl?.focus());
  });

  async function submit(e) {
    e.preventDefault();
    if (submitting || !email || !password) return;
    submitting = true;
    error = '';
    const r = await store.signIn({ email: email.trim(), password });
    submitting = false;
    if (!r.ok) {
      error = r.error ?? 'Sign-in failed';
      password = '';
      queueMicrotask(() => emailEl?.focus());
    } else {
      email = '';
      password = '';
    }
  }

  async function submitToken(e) {
    e.preventDefault();
    if (submitting || !token) return;
    submitting = true;
    error = '';
    const r = await store.signInWithToken(token.trim());
    submitting = false;
    if (!r.ok) {
      error = r.error ?? 'Token sign-in failed';
      token = '';
      queueMicrotask(() => tokenEl?.focus());
    } else {
      token = '';
    }
  }

  function toggleRecover() {
    recovering = !recovering;
    error = '';
    if (recovering) queueMicrotask(() => tokenEl?.focus());
    else queueMicrotask(() => emailEl?.focus());
  }
</script>

<div class="canvas-marks"></div>

<main class="login">
  <section class="card" aria-labelledby="login-title">
    {#if store.auth === 'locked'}
      <div class="mark"><SwanMark size={28} /></div>
      <h1 id="login-title" class="word">CYGNUS</h1>
      <p class="line">Tenant zero console</p>
      <p class="locked">Console credentials are not configured on this host.</p>
      <p class="env mono">Set <code>CYGNUS_CONSOLE_BOOTSTRAP_TOKEN</code> and <code>CYGNUS_CONSOLE_SESSION_KEY</code> on the host, then reload.</p>
    {:else if recovering}
      <div class="mark"><SwanMark size={28} /></div>
      <h1 id="login-title" class="word">CYGNUS</h1>
      <p class="line">Recover access with the bootstrap token</p>

      <form onsubmit={submitToken} class="form">
        <label for="bootstrap-token">Bootstrap token</label>
        <input
          id="bootstrap-token"
          bind:this={tokenEl}
          bind:value={token}
          type="password"
          autocomplete="current-password"
          autocapitalize="off"
          spellcheck="false"
          maxlength="1024"
          required
        />
        {#if error}<p class="err" role="alert">{error}</p>{/if}
        <button class="btn cobalt primary" type="submit" disabled={submitting || !token}>
          {submitting ? 'Unlocking…' : 'Unlock with token'}
        </button>
      </form>

      <button class="back-link" onclick={toggleRecover}>← Back to sign in</button>
      <p class="hint mono">Token printed by the installer · rotate with install.sh --rotate-secrets</p>
    {:else}
      <div class="mark"><SwanMark size={28} /></div>
      <h1 id="login-title" class="word">CYGNUS</h1>
      <p class="line">Tenant zero console</p>

      <form onsubmit={submit} class="form">
        <label for="login-email">Admin email</label>
        <input
          id="login-email"
          bind:this={emailEl}
          bind:value={email}
          type="email"
          autocomplete="email"
          autocapitalize="off"
          spellcheck="false"
          maxlength="254"
          required
        />
        <label for="login-pw" class="pwlab">Password</label>
        <input
          id="login-pw"
          bind:value={password}
          type="password"
          autocomplete="current-password"
          maxlength="1024"
          required
        />
        {#if error}<p class="err" role="alert">{error}</p>{/if}
        <button class="btn cobalt primary" type="submit" disabled={submitting || !email || !password}>
          {submitting ? 'Unlocking…' : 'Unlock console'}
        </button>
      </form>

      <button class="recover" onclick={toggleRecover}>Lost access? Reset with the bootstrap token</button>
    {/if}
  </section>
</main>

<style>
  .login {
    position: relative;
    z-index: 1;
    min-height: 100vh;
    display: grid;
    place-items: center;
    padding: 24px;
  }
  .card {
    width: 380px;
    max-width: 100%;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-xl);
    box-shadow: var(--shadow-pop);
    padding: 34px 32px 30px;
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

  .form {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: 26px;
    text-align: left;
  }
  label {
    font-family: var(--mono);
    font-size: 10px;
    font-weight: 500;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--ink-3);
  }
  .pwlab { margin-top: 2px; }
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
  }
  input:focus-visible {
    outline: 2px solid var(--cobalt);
    outline-offset: 1px;
  }
  input::placeholder { color: var(--ink-4); }
  .btn.cobalt.primary {
    width: 100%;
    height: 38px;
    margin-top: 4px;
    font-size: 13px;
  }

  .err {
    color: var(--red);
    font-size: 11.5px;
    line-height: 1.5;
    text-align: left;
    overflow-wrap: anywhere;
  }

  .recover {
    margin-top: 16px;
    font-size: 11px;
    color: var(--ink-4);
    font-family: var(--mono);
    letter-spacing: 0.02em;
    transition: color 0.14s ease;
  }
  .recover:hover { color: var(--ink-2); }

  .back-link {
    margin-top: 16px;
    font-size: 11px;
    color: var(--ink-4);
    font-family: var(--mono);
    letter-spacing: 0.02em;
    transition: color 0.14s ease;
  }
  .back-link:hover { color: var(--ink-2); }

  .hint {
    margin-top: 14px;
    font-size: 10px;
    color: var(--ink-4);
    line-height: 1.6;
    letter-spacing: 0.02em;
  }
  .mono { font-family: var(--mono); }

  .locked {
    margin-top: 18px;
    font-size: 13px;
    color: var(--ink-2);
    line-height: 1.55;
  }
  .env {
    margin-top: 12px;
    font-size: 11px;
    color: var(--ink-3);
    line-height: 1.6;
    text-align: left;
  }
  .env code {
    font-family: var(--mono);
    font-size: 10.5px;
    background: var(--surface-3);
    border: 1px solid var(--line-2);
    border-radius: 5px;
    padding: 1px 5px;
    color: var(--ink-2);
  }
</style>
