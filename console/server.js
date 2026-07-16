import { AdminProtocolError, adminRequest } from "./admin-client.js";

const indexPath = `${import.meta.dir}/dist/index.html`;
const indexFile = Bun.file(indexPath);
const socketPath = process.env.CYGNUS_SOCKET?.trim();
const adminSocketPath = process.env.CYGNUS_ADMIN_SOCKET?.trim();
const requestedPort = process.env.PORT?.trim() || "3000";
const port = Number(requestedPort);

if (socketPath && !socketPath.startsWith("/")) {
  throw new Error(`CYGNUS_SOCKET must be an absolute path (received ${socketPath})`);
}
if (adminSocketPath && !adminSocketPath.startsWith("/")) {
  throw new Error(`CYGNUS_ADMIN_SOCKET must be an absolute path (received ${adminSocketPath})`);
}
if (!socketPath && (!Number.isInteger(port) || port < 0 || port > 65535)) {
  throw new Error(`PORT must be an integer between 0 and 65535 (received ${requestedPort})`);
}
if (!(await indexFile.exists())) {
  throw new Error(`Built console not found at ${indexPath}; run bun run build first`);
}

const server = Bun.serve({
  ...(socketPath ? { unix: socketPath } : { port }),
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/healthz") {
      return jsonResponse(
        {
          ok: true,
          service: "cygnus-console",
          tenant: "tenant-0",
          mode: adminSocketPath ? "live" : "preview",
          dataSource: adminSocketPath ? "daemon" : "unavailable",
          daemonBridge: adminSocketPath ? "configured" : "offline",
        },
        request.method === "HEAD",
      );
    }

    if (url.pathname.startsWith("/api/v1/")) {
      if (request.method !== "GET" && request.method !== "HEAD") {
        return new Response("Method Not Allowed\n", {
          status: 405,
          headers: { allow: "GET, HEAD", "content-type": "text/plain; charset=utf-8" },
        });
      }
      return handleApi(request, url);
    }

    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("Method Not Allowed\n", {
        status: 405,
        headers: { allow: "GET, HEAD", "content-type": "text/plain; charset=utf-8" },
      });
    }
    return new Response(request.method === "HEAD" ? null : indexFile, {
      headers: {
        "cache-control": "no-cache",
        "content-type": "text/html; charset=utf-8",
      },
    });
  },
});

async function handleApi(request, url) {
  if (!adminSocketPath) {
    return apiError(503, "unavailable", "daemon admin bridge unavailable");
  }
  let command;
  try {
    command = commandFor(url);
  } catch (error) {
    return apiError(422, "validation", error instanceof Error ? error.message : "invalid request");
  }
  if (!command) return apiError(404, "not_found", "API route not found");

  try {
    const { data, requestId } = await adminRequest(adminSocketPath, command);
    return jsonResponse(
      { ok: true, data, requestId },
      request.method === "HEAD",
      200,
      { "cache-control": "no-store" },
    );
  } catch (error) {
    const code = error instanceof AdminProtocolError ? error.code : "internal";
    const status = code === "not_found" ? 404 : code === "conflict" ? 409 : code === "validation" ? 422 : 503;
    const message = status === 503 ? "daemon admin bridge unavailable" : safeErrorMessage(code);
    return apiError(status, code, message);
  }
}

function commandFor(url) {
  const parts = url.pathname.split("/").filter(Boolean);
  if (parts.length === 3 && parts[2] === "status") return { type: "status" };
  if (parts.length === 3 && parts[2] === "apps") {
    return {
      type: "list_apps",
      cursor: optionalQuery(url, "cursor"),
      limit: listLimit(url),
    };
  }
  if (parts.length === 4 && parts[2] === "apps") {
    return { type: "get_app", app: decodeSegment(parts[3]) };
  }
  if (parts.length === 3 && parts[2] === "deployments") {
    return {
      type: "list_deployments",
      app: optionalQuery(url, "app"),
      cursor: optionalQuery(url, "cursor"),
      limit: listLimit(url),
    };
  }
  if (parts.length === 4 && parts[2] === "deployments") {
    return { type: "get_deployment", deployment: decodeSegment(parts[3]) };
  }
  return null;
}

function listLimit(url) {
  const raw = url.searchParams.get("limit") ?? "50";
  const limit = Number(raw);
  if (!Number.isInteger(limit) || limit < 1 || limit > 50) {
    throw new Error("limit must be an integer between 1 and 50");
  }
  return limit;
}

function optionalQuery(url, key) {
  const value = url.searchParams.get(key);
  if (value === null || value === "") return undefined;
  if (value.length > 128 || /[\u0000-\u001f\u007f]/u.test(value)) {
    throw new Error(`${key} is invalid`);
  }
  return value;
}

function decodeSegment(value) {
  const decoded = decodeURIComponent(value);
  if (!decoded || decoded.length > 128 || /[\u0000-\u001f\u007f/]/u.test(decoded)) {
    throw new Error("path identifier is invalid");
  }
  return decoded;
}

function safeErrorMessage(code) {
  if (code === "not_found") return "requested object was not found";
  if (code === "conflict") return "state changed; refresh and try again";
  if (code === "validation") return "request was rejected";
  return "daemon request failed";
}

function apiError(status, code, message) {
  return jsonResponse(
    { ok: false, error: { code, message } },
    false,
    status,
    { "cache-control": "no-store" },
  );
}

function jsonResponse(value, head = false, status = 200, headers = {}) {
  return new Response(head ? null : JSON.stringify(value), {
    status,
    headers: {
      ...headers,
      "content-type": "application/json; charset=utf-8",
    },
  });
}

console.log(
  `cygnus-console · tenant-0 · daemon bridge ${adminSocketPath ? "configured" : "offline"} · listening on ${
    socketPath ? `unix:${socketPath}` : server.url.href
  }`,
);
