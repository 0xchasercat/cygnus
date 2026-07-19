import { randomBytes } from "node:crypto";

export const ADMIN_PROTOCOL_VERSION = 1;
export const MAX_ADMIN_FRAME_BYTES = 64 * 1024;
// Deploy builds and large upload finishes can take longer than a UI poll.
// Keep the default short enough to surface dead daemons quickly, but well
// above a busy SQLite busy_timeout (5s) so concurrent admin traffic survives.
const ADMIN_TIMEOUT_MS = 30_000;

export class AdminProtocolError extends Error {
  constructor(message, code = "transport") {
    super(message);
    this.name = "AdminProtocolError";
    this.code = code;
  }
}

export function adminRequest(socketPath, command, actor) {
  if (typeof socketPath !== "string" || !socketPath.startsWith("/")) {
    return Promise.reject(new AdminProtocolError("admin socket path must be absolute"));
  }
  const requestId = randomBytes(16).toString("hex");
  const envelope = {
    version: ADMIN_PROTOCOL_VERSION,
    request_id: requestId,
    command,
  };
  if (actor !== undefined) envelope.actor = actor;
  const payload = Buffer.from(
    JSON.stringify(envelope),
    "utf8",
  );
  if (payload.length === 0 || payload.length > MAX_ADMIN_FRAME_BYTES) {
    return Promise.reject(new AdminProtocolError("admin request exceeds frame limit"));
  }
  const frame = Buffer.allocUnsafe(payload.length + 4);
  frame.writeUInt32BE(payload.length, 0);
  payload.copy(frame, 4);

  return new Promise((resolve, reject) => {
    let settled = false;
    let connection = null;
    let expectedLength = null;
    let received = Buffer.alloc(0);
    const timer = setTimeout(() => {
      finish(null, new AdminProtocolError("daemon admin request timed out", "timeout"));
    }, ADMIN_TIMEOUT_MS);

    const finish = (socket, error, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket?.end();
      if (error) reject(error);
      else resolve(value);
    };

    const acceptChunk = (socket, chunk) => {
      if (settled) return;
      if (received.length + chunk.length > MAX_ADMIN_FRAME_BYTES + 4) {
        finish(socket, new AdminProtocolError("daemon admin response exceeds frame limit"));
        return;
      }
      received = Buffer.concat([received, Buffer.from(chunk)]);
      if (expectedLength === null && received.length >= 4) {
        expectedLength = received.readUInt32BE(0);
        if (expectedLength === 0 || expectedLength > MAX_ADMIN_FRAME_BYTES) {
          finish(socket, new AdminProtocolError("daemon admin response has invalid frame length"));
          return;
        }
      }
      if (expectedLength === null || received.length < expectedLength + 4) return;
      if (received.length !== expectedLength + 4) {
        finish(socket, new AdminProtocolError("daemon admin response contains trailing bytes"));
        return;
      }

      let response;
      try {
        response = JSON.parse(received.subarray(4).toString("utf8"));
      } catch {
        finish(socket, new AdminProtocolError("daemon admin response is malformed JSON"));
        return;
      }
      if (
        response?.version !== ADMIN_PROTOCOL_VERSION ||
        response?.request_id !== requestId ||
        (response?.status !== "ok" && response?.status !== "error")
      ) {
        finish(socket, new AdminProtocolError("daemon admin response envelope is invalid"));
        return;
      }
      if (response.status === "error") {
        finish(
          socket,
          new AdminProtocolError(
            response.error?.message || "daemon admin request failed",
            response.error?.code || "internal",
          ),
        );
        return;
      }
      finish(socket, null, { data: response.data, requestId });
    };

    Bun.connect({
      unix: socketPath,
      socket: {
        open(socket) {
          socket.write(frame);
        },
        data(socket, data) {
          acceptChunk(socket, data);
        },
        close(socket) {
          if (!settled) {
            finish(
              socket,
              new AdminProtocolError("daemon admin response ended before one complete frame"),
            );
          }
        },
        end(socket) {
          if (!settled) {
            finish(
              socket,
              new AdminProtocolError("daemon admin response ended before one complete frame"),
            );
          }
        },
        connectError(socket, error) {
          finish(
            socket,
            new AdminProtocolError(
              `daemon admin bridge unavailable: ${error?.code ?? "error"}`,
            ),
          );
        },
        error(socket, error) {
          finish(
            socket,
            new AdminProtocolError(
              `daemon admin bridge unavailable: ${error?.code ?? "error"}`,
            ),
          );
        },
      },
    })
      .then((socket) => {
        connection = socket;
      })
      .catch((error) => {
        finish(
          connection,
          new AdminProtocolError(
            `daemon admin bridge unavailable: ${error?.code ?? "error"}`,
          ),
        );
      });
  });
}
