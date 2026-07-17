// POSIX ustar tarball packer, pure JS, no deps.
// Used by ShipModal to bundle a picked folder for chunked upload.
// Skips node_modules/, .git/, .DS_Store, and symlinks (cages don't follow them).

const BLOCK = 512;

function octal(n, width) {
  const s = n.toString(8);
  return s.padStart(width, '0');
}

function clean(s) {
  return s.replace(/[\u0000-\u001f]/g, '');
}

function headerBlock(name, size, type, mode) {
  const buf = new Uint8Array(BLOCK);
  const enc = (str, off, len) => {
    const bytes = new TextEncoder().encode(clean(str));
    buf.set(bytes.subarray(0, len), off);
  };

  // ustar name: 100 bytes name, 155 bytes prefix
  let n = name;
  let prefix = '';
  if (n.length > 100) {
    const split = n.lastIndexOf('/', 155);
    if (split > 0 && n.length - split - 1 <= 100) {
      prefix = n.slice(0, split);
      n = n.slice(split + 1);
    } else {
      // Path too long to encode; caller should reject.
      throw new Error(`path too long for ustar: ${name}`);
    }
  }

  enc(n, 0, 100);
  enc(octal(mode, 7), 100, 8);
  enc(octal(0, 7), 108, 8); // uid
  enc(octal(0, 7), 116, 8); // gid
  enc(octal(size, 11), 124, 12); // size
  enc(octal(0, 11), 136, 12); // mtime
  // checksum placeholder (spaces)
  for (let i = 148; i < 156; i++) buf[i] = 0x20;
  buf[156] = type.charCodeAt(0);
  enc('ustar', 257, 6);
  buf[263] = 0x30; // version "00"
  buf[264] = 0x30;
  enc('', 265, 32); // uname
  enc('', 297, 32); // gname
  enc(octal(0, 7), 329, 8); // devmajor
  enc(octal(0, 7), 337, 8); // devminor
  enc(prefix, 345, 155);

  // checksum: sum of unsigned bytes with checksum field as spaces
  let sum = 0;
  for (let i = 0; i < BLOCK; i++) sum += buf[i];
  const sumStr = octal(sum, 6);
  enc(sumStr, 148, 6);
  buf[154] = 0;
  buf[155] = 0x20;

  return buf;
}

function padBlocks(len) {
  const rem = len % BLOCK;
  if (rem === 0) return new Uint8Array(0);
  return new Uint8Array(BLOCK - rem);
}

function zeroBlocks(n) {
  return new Uint8Array(BLOCK * n);
}

const SKIP = new Set(['node_modules', '.git', '.ds_store']);

function shouldSkip(name) {
  const lower = name.toLowerCase();
  return SKIP.has(lower) || lower === '.ds_store';
}

// Build a ustar tarball from a list of file entries.
// entries: [{ path, bytes }]  (path relative to archive root, no leading slash)
// Returns Uint8Array.
export function makeTarball(entries) {
  const parts = [];
  let total = 0;
  for (const entry of entries) {
    if (entry.dir) {
      const hdr = headerBlock(entry.path, 0, '5', 0o755);
      parts.push(hdr);
      total += hdr.length;
    } else {
      const data = entry.bytes;
      const hdr = headerBlock(entry.path, data.length, '0', 0o644);
      parts.push(hdr, data, padBlocks(data.length));
      total += hdr.length + data.length + padBlocks(data.length).length;
    }
  }
  parts.push(zeroBlocks(2)); // two zero blocks mark end of archive
  total += BLOCK * 2;

  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

// Walk a FileList from webkitdirectory input into tar entries.
// Returns { entries, fileCount, totalBytes, skipped }.
export function collectEntries(fileList, rootName) {
  const entries = [];
  let fileCount = 0;
  let totalBytes = 0;
  const seenDirs = new Set();
  const MAX = 64 * 1024 * 1024; // 64 MiB

  const sorted = Array.from(fileList).sort((a, b) =>
    (a.webkitRelativePath || a.name).localeCompare(b.webkitRelativePath || b.name),
  );

  for (const file of sorted) {
    const rel = file.webkitRelativePath || file.name;
    // Strip the leading root folder — archive root is the project contents.
    const slash = rel.indexOf('/');
    const path = slash >= 0 ? rel.slice(slash + 1) : rel;
    if (!path) continue;

    const segs = path.split('/');
    if (segs.some((seg, i) => i < segs.length - 1 && shouldSkip(seg))) continue;
    const last = segs[segs.length - 1];
    if (shouldSkip(last)) continue;

    // Record intermediate directories.
    let acc = '';
    for (let i = 0; i < segs.length - 1; i++) {
      acc = acc ? `${acc}/${segs[i]}` : segs[i];
      if (!seenDirs.has(acc)) {
        seenDirs.add(acc);
        entries.push({ path: acc, dir: true });
      }
    }

    totalBytes += file.size;
    if (totalBytes > MAX) {
      throw new Error('folder exceeds 64 MiB — remove large files or node_modules');
    }
    // Defer reading bytes; caller awaits file.arrayBuffer() when packing.
    entries.push({ path, file });
    fileCount += 1;
  }

  return { entries, fileCount, totalBytes, rootName };
}

// Resolve entry bytes (directories have none) into a Uint8Array tarball.
export async function packTarball(collected) {
  const resolved = [];
  for (const e of collected.entries) {
    if (e.dir) {
      resolved.push({ path: e.path, dir: true });
    } else {
      const buf = new Uint8Array(await e.file.arrayBuffer());
      resolved.push({ path: e.path, bytes: buf });
    }
  }
  return makeTarball(resolved);
}
