import { describe, expect, test } from 'bun:test';
import { stripAnsi } from './fmt.js';

describe('stripAnsi', () => {
  test('removes CSI color sequences, including per-character coloring', () => {
    // The exact shape rolldown/vite emit: every character wrapped in a
    // 256-color set + reset pair.
    const colored = '\x1b[38;5;249mo\x1b[0m\x1b[38;5;249mn\x1b[0m\x1b[38;5;249ms\x1b[0m\x1b[38;5;249mt\x1b[0m';
    expect(stripAnsi(colored)).toBe('onst');
  });

  test('removes simple styling and cursor sequences', () => {
    expect(stripAnsi('\x1b[33m[UNRESOLVED_IMPORT] \x1b[0mCould not resolve')).toBe(
      '[UNRESOLVED_IMPORT] Could not resolve',
    );
    expect(stripAnsi('\x1b[1mBold\x1b[22m and \x1b[2K\x1b[1Gplain')).toBe('Bold and plain');
  });

  test('removes OSC sequences and lone escapes', () => {
    expect(stripAnsi('\x1b]0;window title\x07visible')).toBe('visible');
    expect(stripAnsi('reset\x1bc after')).toBe('reset after');
  });

  test('leaves clean text untouched', () => {
    const clean = '[build] starting bun run build — 100% plain';
    expect(stripAnsi(clean)).toBe(clean);
  });
});
