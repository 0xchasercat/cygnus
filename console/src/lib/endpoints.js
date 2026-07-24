export function listenerMode(node) {
  return node?.listener?.mode ?? node?.listener_mode ?? 'integrated';
}

export function appEndpoint(node, app) {
  const mode = listenerMode(node);
  if (mode === 'tcp') {
    if (typeof app?.endpoint === 'string') return app.endpoint;
    if (app?.endpoint?.display) return app.endpoint.display;
    if (app?.endpoint?.value) return app.endpoint.value;
    if (app?.endpoint?.address) return app.endpoint.address;
    const port = app?.port ?? app?.endpoint?.port;
    const host = node?.listener?.advertise_host ?? node?.advertise_host ?? node?.hostname;
    const displayHost = host?.includes(':') && !host.startsWith('[') ? `[${host}]` : host;
    return port && displayHost ? `${displayHost}:${port}` : 'port assigned after deploy';
  }
  if (mode === 'uds') {
    if (typeof app?.endpoint === 'string') return app.endpoint;
    if (app?.endpoint?.display) return app.endpoint.display;
    if (app?.endpoint?.value) return app.endpoint.value;
    if (app?.endpoint?.socket_path) return app.endpoint.socket_path;
    const dir = node?.listener?.socket_dir ?? '/run/cygnus/apps';
    return app?.name ? `${dir}/${app.name}.sock` : dir;
  }
  return app?.domains?.[0]
    ?? (app?.name === 'tenant-0' ? node?.dashboard_domain : null)
    ?? 'unrouted';
}

export function endpointKind(node) {
  const mode = listenerMode(node);
  return mode === 'integrated' ? 'domain' : mode === 'tcp' ? 'address' : 'socket';
}
