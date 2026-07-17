// Thin fetch client — ported from LiveConsole.svelte. Same-origin,
// JSON envelope {ok, data, error:{message,code}}. ApiError carries status+code.

export class ApiError extends Error {
  constructor(message, status, code) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

export async function api(path, options = {}) {
  const response = await fetch(path, {
    credentials: 'same-origin',
    ...options,
    headers: {
      accept: 'application/json',
      ...(options.body ? { 'content-type': 'application/json' } : {}),
      ...(options.headers ?? {}),
    },
  });

  const envelope = await response.json().catch(() => null);
  if (!response.ok || !envelope?.ok) {
    const err = envelope?.error;
    const status = response.status;
    const retryAfter = response.headers.get('retry-after');
    let message = err?.message || `Request failed (${status})`;
    // Surface the rate-limit wait so the login screen can quote it.
    if (status === 429 && retryAfter && /^\d+$/.test(retryAfter)) {
      message = `too many attempts — retry in ${retryAfter}s`;
    }
    throw new ApiError(message, status, err?.code);
  }
  return envelope.data;
}

// POST helper that JSON-encodes the body.
export function post(path, body) {
  return api(path, {
    method: 'POST',
    body: body == null ? undefined : JSON.stringify(body),
  });
}
