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
});
