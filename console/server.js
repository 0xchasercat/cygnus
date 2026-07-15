const indexPath = `${import.meta.dir}/dist/index.html`;
const indexFile = Bun.file(indexPath);
const socketPath = process.env.CYGNUS_SOCKET?.trim();
const requestedPort = process.env.PORT?.trim() || '3000';
const port = Number(requestedPort);

if (!socketPath && (!Number.isInteger(port) || port < 0 || port > 65535)) {
  throw new Error(`PORT must be an integer between 0 and 65535 (received ${requestedPort})`);
}

if (!(await indexFile.exists())) {
  throw new Error(`Built console not found at ${indexPath}; run bun run build first`);
}

const health = JSON.stringify({
  ok: true,
  service: 'cygnus-console',
  tenant: 'tenant-0',
  mode: 'preview',
  dataSource: 'fixture',
  daemonBridge: 'offline',
});

const server = Bun.serve({
  ...(socketPath ? { unix: socketPath } : { port }),
  fetch(request) {
    if (request.method !== 'GET' && request.method !== 'HEAD') {
      return new Response('Method Not Allowed\n', {
        status: 405,
        headers: { allow: 'GET, HEAD', 'content-type': 'text/plain; charset=utf-8' },
      });
    }

    const { pathname } = new URL(request.url);
    if (pathname === '/healthz') {
      return new Response(request.method === 'HEAD' ? null : health, {
        headers: {
          'cache-control': 'no-store',
          'content-type': 'application/json; charset=utf-8',
        },
      });
    }

    return new Response(request.method === 'HEAD' ? null : indexFile, {
      headers: {
        'cache-control': 'no-cache',
        'content-type': 'text/html; charset=utf-8',
      },
    });
  },
});

console.log(
  `cygnus-console · tenant-0 preview · daemon bridge offline · listening on ${
    socketPath ? `unix:${socketPath}` : server.url.href
  }`,
);
