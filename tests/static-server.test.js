import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequestHandler } from "../assets/cygnus-static-server.ts";

let publicRoot;
let handleRequest;

async function put(relativePath, contents = relativePath) {
  const path = join(publicRoot, ...relativePath.split("/"));
  await mkdir(join(path, ".."), { recursive: true });
  await writeFile(path, contents);
}

function request(path, options) {
  return handleRequest(new Request(`http://static.test${path}`, options));
}

beforeEach(async () => {
  publicRoot = await mkdtemp(join(tmpdir(), "cygnus-static-"));
  handleRequest = createRequestHandler(publicRoot);
});

afterEach(async () => {
  await rm(publicRoot, { recursive: true, force: true });
});

describe("content types", () => {
  const cases = {
    html: "text/html; charset=utf-8",
    js: "text/javascript; charset=utf-8",
    mjs: "text/javascript; charset=utf-8",
    css: "text/css; charset=utf-8",
    json: "application/json; charset=utf-8",
    svg: "image/svg+xml",
    png: "image/png",
    jpg: "image/jpeg",
    jpeg: "image/jpeg",
    webp: "image/webp",
    avif: "image/avif",
    gif: "image/gif",
    ico: "image/x-icon",
    woff2: "font/woff2",
    woff: "font/woff",
    ttf: "font/ttf",
    wasm: "application/wasm",
    map: "application/json; charset=utf-8",
    txt: "text/plain; charset=utf-8",
    xml: "application/xml; charset=utf-8",
    webmanifest: "application/manifest+json; charset=utf-8",
  };

  for (const [extension, contentType] of Object.entries(cases)) {
    test(`serves .${extension} as ${contentType}`, async () => {
      await put(`file.${extension}`, "content");
      const response = await request(`/file.${extension}`);

      expect(response.status).toBe(200);
      expect(response.headers.get("Content-Type")).toBe(contentType);
      expect(await response.text()).toBe("content");
    });
  }
});

test("serves directory indexes and falls back to the root index for SPA routes", async () => {
  await put("index.html", "root app");
  await put("docs/index.html", "docs page");

  const directory = await request("/docs/");
  expect(directory.status).toBe(200);
  expect(await directory.text()).toBe("docs page");
  expect(directory.headers.get("Cache-Control")).toBe("no-cache");

  const fallback = await request("/client/route");
  expect(fallback.status).toBe(200);
  expect(fallback.headers.get("Content-Type")).toBe("text/html; charset=utf-8");
  expect(fallback.headers.get("Cache-Control")).toBe("no-cache");
  expect(await fallback.text()).toBe("root app");
});

test("rejects unsafe decoded paths", async () => {
  await put("index.html", "root app");

  for (const path of ["/..%2fsecret", "/%2e%2e%2fsecret", "/bad%5cpath", "/bad%00path", "/bad%ZZpath"]) {
    const response = await request(path);
    expect(response.status).toBe(400);
    expect(await response.text()).toBe("Bad Request");
  }
});

test("sets immutable caching for assets and hashed names but not ordinary files", async () => {
  await put("assets/app.js", "asset");
  await put("scripts/app-a1b2c3d4.js", "hashed");
  await put("scripts/app.js", "ordinary");
  await put("assets/page.html", "html wins");

  const asset = await request("/assets/app.js");
  expect(asset.headers.get("Cache-Control")).toBe("public, max-age=31536000, immutable");

  const hashed = await request("/scripts/app-a1b2c3d4.js");
  expect(hashed.headers.get("Cache-Control")).toBe("public, max-age=31536000, immutable");

  const ordinary = await request("/scripts/app.js");
  expect(ordinary.headers.has("Cache-Control")).toBe(false);

  const html = await request("/assets/page.html");
  expect(html.headers.get("Cache-Control")).toBe("no-cache");
});

test("HEAD matches GET status and headers without returning a body", async () => {
  await put("index.html", "root app");
  await put("assets/app-abcdef12.js", "console.log('hello')");

  const get = await request("/assets/app-abcdef12.js");
  const head = await request("/assets/app-abcdef12.js", { method: "HEAD" });

  expect(head.status).toBe(get.status);
  for (const name of ["Content-Type", "Content-Length", "Cache-Control"]) {
    expect(head.headers.get(name)).toBe(get.headers.get(name));
  }
  expect(await head.text()).toBe("");

  const fallbackHead = await request("/client/route", { method: "HEAD" });
  expect(fallbackHead.status).toBe(200);
  expect(fallbackHead.headers.get("Content-Type")).toBe("text/html; charset=utf-8");
  expect(fallbackHead.headers.get("Cache-Control")).toBe("no-cache");
  expect(await fallbackHead.text()).toBe("");
});

test("rejects methods other than GET and HEAD with an Allow header", async () => {
  const response = await request("/", { method: "POST" });

  expect(response.status).toBe(405);
  expect(response.headers.get("Allow")).toBe("GET, HEAD");
});

test("returns 404 for an unmatched path when the root index is missing", async () => {
  const get = await request("/missing");
  expect(get.status).toBe(404);
  expect(await get.text()).toBe("Not Found");

  const head = await request("/missing", { method: "HEAD" });
  expect(head.status).toBe(404);
  expect(await head.text()).toBe("");
});
