import { describe, expect, test } from 'bun:test';
import { appEndpoint, endpointKind, listenerMode } from './endpoints.js';

describe('app endpoint presentation', () => {
  test('keeps domains as the integrated default', () => {
    const node = { listener: { mode: 'integrated' }, dashboard_domain: 'console.example.com' };
    expect(listenerMode(node)).toBe('integrated');
    expect(appEndpoint(node, { name: 'web', domains: ['web.example.com'] })).toBe('web.example.com');
    expect(endpointKind(node)).toBe('domain');
  });

  test('uses daemon-provided TCP endpoints and brackets IPv6 fallbacks', () => {
    const node = { listener: { mode: 'tcp', advertise_host: '2001:db8::1' } };
    expect(appEndpoint(node, { name: 'api', endpoint: 'node.example.com:10004' })).toBe('node.example.com:10004');
    expect(appEndpoint(node, { name: 'api', port: 10004 })).toBe('[2001:db8::1]:10004');
    expect(endpointKind(node)).toBe('address');
  });

  test('uses explicit or derived Unix socket paths', () => {
    const node = { listener: { mode: 'uds', socket_dir: '/run/cygnus/apps' } };
    expect(appEndpoint(node, { name: 'api', endpoint: '/custom/api.sock' })).toBe('/custom/api.sock');
    expect(appEndpoint(node, { name: 'web' })).toBe('/run/cygnus/apps/web.sock');
    expect(endpointKind(node)).toBe('socket');
  });
});
