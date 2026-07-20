import { test, expect } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import http from "node:http";
import net from "node:net";

const SHIM = join(import.meta.dir, "..", "assets", "shim.js");
const BUN = process.execPath;

async function readStream(stream) {
  return stream ? new Response(stream).text() : "";
}

async function stopProcess(proc) {
  if (!proc) return;
  try {
    proc.kill();
  } catch {
    // The process may have exited between the request and cleanup.
  }
  await proc.exited;
}

async function unixHttpRequest(socketPath) {
  return new Promise((resolve, reject) => {
    const request = http.get(
      {
        path: "/",
        socketPath,
        timeout: 250,
      },
      (response) => {
        let body = "";
        response.setEncoding("utf8");
        response.on("data", (chunk) => {
          body += chunk;
        });
        response.on("end", () => resolve({ status: response.statusCode, body }));
      },
    );
    request.on("timeout", () => request.destroy(new Error("request timed out")));
    request.on("error", reject);
  });
}

async function unixRequestEventually(socketPath) {
  const deadline = Date.now() + 4_000;
  let lastError;
  while (Date.now() < deadline) {
    try {
      return await unixHttpRequest(socketPath);
    } catch (error) {
      lastError = error;
      await Bun.sleep(25);
    }
  }
  throw new Error(`timed out waiting for ${socketPath}: ${lastError}`);
}

async function unixRawRequest(socketPath) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ path: socketPath });
    let body = "";
    socket.setEncoding("utf8");
    socket.on("connect", () => {
      socket.write("GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    });
    socket.on("data", (chunk) => {
      body += chunk;
    });
    socket.on("end", () => resolve(body));
    socket.on("error", reject);
    socket.setTimeout(1_000, () => socket.destroy(new Error("request timed out")));
  });
}

async function unixRawRequestEventually(socketPath) {
  const deadline = Date.now() + 4_000;
  let lastError;
  while (Date.now() < deadline) {
    try {
      return await unixRawRequest(socketPath);
    } catch (error) {
      lastError = error;
      await Bun.sleep(25);
    }
  }
  throw new Error(`timed out waiting for ${socketPath}: ${lastError}`);
}

async function runServer(source, callback) {
  const directory = await mkdtemp(join(tmpdir(), "cygnus-shim-"));
  const app = join(directory, "app.js");
  const socket = join(directory, "app.sock");
  await writeFile(app, source);

  const environment = { ...process.env, CYGNUS_SOCKET: socket };
  const child = Bun.spawn([BUN, "--preload", SHIM, app], {
    env: environment,
    stderr: "pipe",
    stdout: "pipe",
  });

  try {
    return await callback({ process: child, socket });
  } finally {
    await stopProcess(child);
    await rm(directory, { force: true, recursive: true });
  }
}

test("redirects explicit Bun.serve to the configured UDS", async () => {
  await runServer(
    `
      let websocketRead = false;
      const options = {
        port: 3210,
        hostname: "127.0.0.1",
        get websocket() {
          websocketRead = true;
          return { open() {}, message() {} };
        },
        fetch() {
          return new Response(websocketRead ? "explicit-websocket" : "websocket-missing");
        },
        idleTimeout: 17,
      };
      Bun.serve(options);
    `,
    async ({ socket }) => {
      const response = await unixRequestEventually(socket);
      expect(response).toEqual({ status: 200, body: "explicit-websocket" });
    },
  );
});

test("redirects Bun's automatic default-export server to the configured UDS", async () => {
  await runServer(
    `
      export default {
        fetch() { return new Response("default-export"); },
        websocket: { open() {}, message() {} },
      };
    `,
    async ({ socket }) => {
      const response = await unixRequestEventually(socket);
      expect(response).toEqual({ status: 200, body: "default-export" });
    },
  );
});

test("redirects node:http listen overloads and keeps the callback", async () => {
  await runServer(
    `
      import http from "node:http";
      let callbackRan = false;
      const server = http.createServer(async (request, response) => {
        // Bun may accept the first request before the listen callback runs.
        // Wait briefly so we assert the callback was wired, not callback-vs-accept order.
        const deadline = Date.now() + 2_000;
        while (!callbackRan && Date.now() < deadline) {
          await Bun.sleep(5);
        }
        response.end(callbackRan ? "node-http" : "callback-missed");
      });
      server.listen({ port: 3211, host: "127.0.0.1", backlog: 7 }, () => {
        callbackRan = true;
      });
    `,
    async ({ socket }) => {
      const response = await unixRequestEventually(socket);
      expect(response).toEqual({ status: 200, body: "node-http" });
    },
  );
});

test("redirects node:net path listen to the configured UDS", async () => {
  await runServer(
    `
      import net from "node:net";
      let callbackRan = false;
      const server = net.createServer((connection) => {
        connection.end(
          "HTTP/1.1 200 OK\\r\\nContent-Length: 8\\r\\nConnection: close\\r\\n\\r\\n" +
          (callbackRan ? "node-net" : "callback"),
        );
      });
      server.listen("/tmp/cygnus-legacy.sock", 5, () => {
        callbackRan = true;
      });
    `,
    async ({ socket }) => {
      const response = await unixRawRequestEventually(socket);
      expect(response).toContain("200 OK");
      expect(response).toContain("node-net");
    },
  );
});

test("redirects node:net TCP listen while preserving backlog and callback", async () => {
  await runServer(
    `
      import net from "node:net";
      let callbackRan = false;
      const server = net.createServer((connection) => {
        connection.end(
          "HTTP/1.1 200 OK\\r\\nContent-Length: 7\\r\\nConnection: close\\r\\n\\r\\n" +
          (callbackRan ? "net-tcp" : "callback"),
        );
      });
      server.listen(3212, "127.0.0.1", 6, () => {
        callbackRan = true;
      });
    `,
    async ({ socket }) => {
      let response = await unixRawRequestEventually(socket);
      if (!response.includes("net-tcp")) {
        await Bun.sleep(25);
        response = await unixRawRequestEventually(socket);
      }
      expect(response).toContain("200 OK");
      expect(response).toContain("net-tcp");
    },
  );
});

test("rejects a missing CYGNUS_SOCKET with a clear startup error", async () => {
  const directory = await mkdtemp(join(tmpdir(), "cygnus-shim-invalid-"));
  const app = join(directory, "app.js");
  await writeFile(app, "export default { fetch() { return new Response('nope') } };\n");
  const environment = { ...process.env };
  delete environment.CYGNUS_SOCKET;
  const child = Bun.spawn([BUN, "--preload", SHIM, app], {
    env: environment,
    stderr: "pipe",
    stdout: "pipe",
  });
  try {
    expect(await child.exited).not.toBe(0);
    const stderr = await readStream(child.stderr);
    expect(stderr).toContain("CYGNUS_SOCKET is required");
    expect(stderr).toContain("absolute Unix socket path");
  } finally {
    await stopProcess(child);
    await rm(directory, { force: true, recursive: true });
  }
});

test("rejects a relative CYGNUS_SOCKET with a clear startup error", async () => {
  const directory = await mkdtemp(join(tmpdir(), "cygnus-shim-invalid-"));
  const app = join(directory, "app.js");
  await writeFile(app, "export default { fetch() { return new Response('nope') } };\n");
  const environment = { ...process.env, CYGNUS_SOCKET: "relative.sock" };
  const child = Bun.spawn([BUN, "--preload", SHIM, app], {
    env: environment,
    stderr: "pipe",
    stdout: "pipe",
  });
  try {
    expect(await child.exited).not.toBe(0);
    const stderr = await readStream(child.stderr);
    expect(stderr).toContain("CYGNUS_SOCKET must be an absolute Unix socket path");
    expect(stderr).toContain('"relative.sock"');
  } finally {
    await stopProcess(child);
    await rm(directory, { force: true, recursive: true });
  }
});
