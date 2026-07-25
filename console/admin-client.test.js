import { afterEach, describe, expect, test } from "bun:test";
import { unlinkSync } from "node:fs";
import { adminRequest } from "./admin-client.js";

let listener;
let socketPath;

afterEach(() => {
  listener?.stop(true);
  listener = undefined;
  if (socketPath) {
    try {
      unlinkSync(socketPath);
    } catch {}
  }
});

function listen(respond) {
  socketPath = `/tmp/cygnus-console-test-${process.pid}-${Date.now()}.sock`;
  listener = Bun.listen({
    unix: socketPath,
    socket: {
      data(socket, frame) {
        const bytes = Buffer.from(frame);
        const length = bytes.readUInt32BE(0);
        const request = JSON.parse(bytes.subarray(4, length + 4).toString("utf8"));
        const response = Buffer.from(JSON.stringify(respond(request)));
        const encoded = Buffer.allocUnsafe(response.length + 4);
        encoded.writeUInt32BE(response.length, 0);
        response.copy(encoded, 4);
        socket.write(encoded);
        socket.end();
      },
    },
  });
  return socketPath;
}

describe("adminRequest", () => {
  test("omits actor unless the authenticated caller supplies one", async () => {
    let received;
    const path = listen((request) => {
      received = request;
      return {
        version: 1,
        request_id: request.request_id,
        status: "ok",
        data: { kind: "health", service: "cygnus", isolation: "local" },
      };
    });

    await adminRequest(path, { type: "health" });
    expect(received.actor).toBeUndefined();
  });

  test("round-trips one correlated typed frame", async () => {
    const path = listen((request) => ({
      version: 1,
      request_id: request.request_id,
      status: "ok",
      data: { kind: "status", node: { listen: "127.0.0.1:3000", app_count: 2 } },
    }));

    const result = await adminRequest(path, { type: "status" });

    expect(result.requestId).toHaveLength(32);
    expect(result.data).toEqual({
      kind: "status",
      node: { listen: "127.0.0.1:3000", app_count: 2 },
    });
  });

  test("rejects an uncorrelated response", async () => {
    const path = listen(() => ({
      version: 1,
      request_id: "00000000000000000000000000000000",
      status: "ok",
      data: { kind: "health", service: "wrong", isolation: "test" },
    }));

    await expect(adminRequest(path, { type: "health" })).rejects.toThrow(
      "response envelope is invalid",
    );
  });

  test("surfaces the daemon's error for a reply it could not correlate", async () => {
    // The daemon answers an unparseable request with a synthetic request id
    // — the caller must see the daemon's explanation, not "envelope is
    // invalid".
    const path = listen(() => ({
      version: 1,
      request_id: "00000000000000000000000000000000",
      status: "error",
      error: { code: "invalid_request", message: "invalid admin request frame" },
    }));

    await expect(adminRequest(path, { type: "health" })).rejects.toThrow(
      "invalid admin request frame",
    );
  });

  test("delivers frames larger than the socket send buffer intact", async () => {
    // Deploy chunk requests are ~43 KiB framed; macOS unix sockets buffer
    // ~8 KiB, so the client must keep writing on drain. The server here
    // accumulates until one full frame arrived and echoes its length back.
    socketPath = `/tmp/cygnus-console-test-${process.pid}-${Date.now()}-big.sock`;
    let received = Buffer.alloc(0);
    listener = Bun.listen({
      unix: socketPath,
      socket: {
        data(socket, chunk) {
          received = Buffer.concat([received, Buffer.from(chunk)]);
          if (received.length < 4) return;
          const length = received.readUInt32BE(0);
          if (received.length < length + 4) return;
          const request = JSON.parse(received.subarray(4, length + 4).toString("utf8"));
          const response = Buffer.from(
            JSON.stringify({
              version: 1,
              request_id: request.request_id,
              status: "ok",
              data: { kind: "deploy_upload_chunk", received_bytes: request.command.chunk_base64.length },
            }),
          );
          const encoded = Buffer.allocUnsafe(response.length + 4);
          encoded.writeUInt32BE(response.length, 0);
          response.copy(encoded, 4);
          socket.write(encoded);
          socket.end();
        },
      },
    });

    const chunk = "A".repeat(256 * 1024);
    const result = await adminRequest(socketPath, {
      type: "deploy_upload_chunk",
      upload_id: "u-1",
      chunk_base64: chunk,
    });
    expect(result.data.received_bytes).toBe(chunk.length);
  });
});
