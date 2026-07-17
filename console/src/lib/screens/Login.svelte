<script>
  import { store } from '../live.svelte.js';
  import SwanMark from '../components/SwanMark.svelte';

  let token = $state('');
  let inputEl = $state();
  let error = $state('');
  let submitting = $state(false);

  $effect(() => {
    if (store.auth === 'signin') queueMicrotask(() => inputEl?.focus());
  });

  async function submit(e) {
    e.preventDefault();
    if (submitting || !token) return;
    submitting = true;
    error = '';
    const r = await store.signIn(token.trim());
    submitting = false;
    if (!r.ok) {
      error = r.error ?? 'Sign-in failed';
      token = '';
      queueMicrotask(() => inputEl?.focus());
    } else {
      token = '';
    }
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
    {:else}
      <div class="mark"><SwanMark size={28} /></div>
      <h1 id="login-title" class="word">CYGNUS</h1>
      <p class="line">Tenant zero console</p>

      <form onsubmit={submit} class="form">
        <label for="bootstrap-token">Bootstrap token</label>
        <input
          id="bootstrap-token"
          bind:this={inputEl}
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
          {submitting ? 'Unlocking…' : 'Unlock console'}
        </button>
      </form>

      <p class="hint mono">Token printed by the installer · rotate with install.sh --rotate-secrets</p>
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
  .hint {
    margin-top: 18px;
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
