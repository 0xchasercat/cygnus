import { realpath, stat } from "node:fs/promises";
import { dirname, extname, isAbsolute, relative, resolve, sep } from "node:path";

// Bun inlines import.meta.dir to the daemon staging path when producing CJS
// bytecode. Resolve from the launched artifact entry so content-addressed moves
// and macOS host paths keep the public root beside the generated server.
export const PUBLIC_ROOT = resolve(dirname(process.argv[1]), "public");

const CONTENT_TYPES: Readonly<Record<string, string>> = Object.freeze({
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".webp": "image/webp",
  ".avif": "image/avif",
  ".gif": "image/gif",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
  ".woff": "font/woff",
  ".ttf": "font/ttf",
  ".wasm": "application/wasm",
  ".map": "application/json; charset=utf-8",
  ".txt": "text/plain; charset=utf-8",
  ".xml": "application/xml; charset=utf-8",
  ".webmanifest": "application/manifest+json; charset=utf-8",
});

const IMMUTABLE_CACHE = "public, max-age=31536000, immutable";
const NO_CACHE = "no-cache";
const HASHED_NAME = /-[a-z0-9]{6,}(?=\.[^./]+$)/i;

function isWithin(root: string, candidate: string): boolean {
  const pathFromRoot = relative(root, candidate);
  return pathFromRoot === "" || (!isAbsolute(pathFromRoot) && pathFromRoot !== ".." && !pathFromRoot.startsWith(`..${sep}`));
}

function decodeRequestPath(url: string): string | null {
  let pathname: string;
  try {
    pathname = new URL(url).pathname;
    pathname = decodeURIComponent(pathname);
  } catch {
    return null;
  }

  if (pathname.includes("\0") || pathname.includes("\\")) return null;

  const parts = pathname.split("/");
  if (parts.some((part) => part === "." || part === "..")) return null;

  return pathname;
}

function cacheControl(pathname: string, extension: string): string | null {
  if (extension === ".html") return NO_CACHE;
  const parts = pathname.split("/").filter(Boolean);
  if (parts[0] === "assets" || HASHED_NAME.test(parts.at(-1) ?? "")) {
    return IMMUTABLE_CACHE;
  }
  return null;
}

async function existingFile(root: string, candidate: string): Promise<{ path: string; size: number } | null> {
  try {
    const info = await stat(candidate);
    let filePath = candidate;
    if (info.isDirectory()) filePath = resolve(candidate, "index.html");

    const fileInfo = info.isDirectory() ? await stat(filePath) : info;
    if (!fileInfo.isFile()) return null;

    const canonicalPath = await realpath(filePath);
    const canonicalRoot = await realpath(root);
    if (!isWithin(canonicalRoot, canonicalPath)) return null;

    return { path: canonicalPath, size: fileInfo.size };
  } catch {
    return null;
  }
}

function textResponse(
  request: Request,
  body: string,
  status: number,
  extraHeaders?: HeadersInit,
): Response {
  const headers = new Headers(extraHeaders);
  headers.set("Content-Type", "text/plain; charset=utf-8");
  headers.set("Content-Length", String(new TextEncoder().encode(body).byteLength));
  return new Response(request.method === "HEAD" ? null : body, { status, headers });
}

function fileResponse(
  request: Request,
  file: { path: string; size: number },
  requestPath: string,
): Response {
  const extension = extname(file.path).toLowerCase();
  const headers = new Headers();
  const contentType = CONTENT_TYPES[extension];
  if (contentType) headers.set("Content-Type", contentType);
  headers.set("Content-Length", String(file.size));

  const caching = cacheControl(requestPath, extension);
  if (caching) headers.set("Cache-Control", caching);

  return new Response(request.method === "HEAD" ? null : Bun.file(file.path), {
    status: 200,
    headers,
  });
}

export function createRequestHandler(publicRoot: string = PUBLIC_ROOT) {
  const root = resolve(publicRoot);

  return async function handleStaticRequest(request: Request): Promise<Response> {
    if (request.method !== "GET" && request.method !== "HEAD") {
      return textResponse(request, "Method Not Allowed", 405, { Allow: "GET, HEAD" });
    }

    const pathname = decodeRequestPath(request.url);
    if (pathname === null) return textResponse(request, "Bad Request", 400);

    const candidate = resolve(root, `.${pathname}`);
    if (!isWithin(root, candidate)) return textResponse(request, "Bad Request", 400);

    const requestedFile = await existingFile(root, candidate);
    if (requestedFile) return fileResponse(request, requestedFile, pathname);

    const indexFile = await existingFile(root, resolve(root, "index.html"));
    if (indexFile) return fileResponse(request, indexFile, "/index.html");

    return textResponse(request, "Not Found", 404);
  };
}

export const handleRequest = createRequestHandler();

if (import.meta.main) {
  Bun.serve({ fetch: handleRequest });
}
