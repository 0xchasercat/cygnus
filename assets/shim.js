import path from "node:path";
import * as http from "node:http";
import * as net from "node:net";

const CYGNUS_STATE = Symbol.for("cygnus.preload.shim.state");
const CYGNUS_LISTEN_PATCH = Symbol.for("cygnus.preload.shim.listen");

function configuredSocket() {
  const value = process.env.CYGNUS_SOCKET;
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(
      "Cygnus preload: CYGNUS_SOCKET is required and must be an absolute Unix socket path",
    );
  }
  if (!path.isAbsolute(value)) {
    throw new Error(
      `Cygnus preload: CYGNUS_SOCKET must be an absolute Unix socket path (received ${JSON.stringify(value)})`,
    );
  }
  if (value.includes("\0")) {
    throw new Error("Cygnus preload: CYGNUS_SOCKET contains a NUL byte");
  }
  return value;
}

const socket = configuredSocket();
const existingState = globalThis[CYGNUS_STATE];
if (existingState) {
  if (existingState.socket !== socket) {
    throw new Error(
      `Cygnus preload: shim is already initialized for ${JSON.stringify(existingState.socket)}`,
    );
  }
} else {
  const state = { socket };

  if (!globalThis.Bun || typeof Bun.serve !== "function") {
    throw new Error("Cygnus preload: Bun.serve is unavailable");
  }

  const originalServe = Bun.serve;
  const redirectedServe = function cygnusServe(options, ...rest) {
    if (options === null || typeof options !== "object") {
      throw new TypeError("Cygnus preload: Bun.serve requires an options object");
    }

    const redirected = { ...options };
    delete redirected.port;
    delete redirected.hostname;
    delete redirected.host;
    redirected.unix = socket;
    return Reflect.apply(originalServe, this, [redirected, ...rest]);
  };

  Object.defineProperty(redirectedServe, CYGNUS_LISTEN_PATCH, {
    configurable: false,
    enumerable: false,
    value: { socket, original: originalServe },
    writable: false,
  });
  try {
    Bun.serve = redirectedServe;
  } catch (error) {
    throw new Error("Cygnus preload: unable to patch Bun.serve", { cause: error });
  }
  if (Bun.serve !== redirectedServe) {
    throw new Error("Cygnus preload: unable to patch Bun.serve");
  }

  const patchedPrototypes = new Set();
  function patchListen(Server, moduleName) {
    if (!Server || !Server.prototype || typeof Server.prototype.listen !== "function") {
      throw new Error(`Cygnus preload: ${moduleName}.Server.listen is unavailable`);
    }

    const prototype = Server.prototype;
    if (patchedPrototypes.has(prototype)) return;
    patchedPrototypes.add(prototype);

    const originalListen = prototype.listen;
    const priorPatch = originalListen[CYGNUS_LISTEN_PATCH];
    if (priorPatch) {
      if (priorPatch.socket !== socket) {
        throw new Error(
          `Cygnus preload: ${moduleName}.Server.listen is already redirected to ${JSON.stringify(priorPatch.socket)}`,
        );
      }
      return;
    }

    const redirectedListen = function cygnusListen(...args) {
      const [first, ...rest] = args;
      let redirectedArgs;

      // Node's options overload uses `path` for Unix sockets. Preserve all
      // non-address options (backlog, exclusive, signal, and permissions).
      if (first !== null && typeof first === "object") {
        const options = { ...first };
        delete options.port;
        delete options.host;
        delete options.hostname;
        options.path = socket;
        redirectedArgs = [options, ...rest];
      } else {
        // Port/path overloads accept host/path strings, an optional backlog,
        // and a callback. Drop host/path strings but retain those meaningful
        // numeric and function arguments in their original order.
        const meaningful = [
          ...(typeof first === "function" ? [first] : []),
          ...rest.filter(
            (value) => typeof value === "number" || typeof value === "function",
          ),
        ];
        redirectedArgs = [socket, ...meaningful];
      }

      return Reflect.apply(originalListen, this, redirectedArgs);
    };

    Object.defineProperty(redirectedListen, CYGNUS_LISTEN_PATCH, {
      configurable: false,
      enumerable: false,
      value: { socket, original: originalListen },
      writable: false,
    });
    try {
      prototype.listen = redirectedListen;
    } catch (error) {
      throw new Error(`Cygnus preload: unable to patch ${moduleName}.Server.listen`, {
        cause: error,
      });
    }
    if (prototype.listen !== redirectedListen) {
      throw new Error(`Cygnus preload: unable to patch ${moduleName}.Server.listen`);
    }
  }

  patchListen(http.Server, "node:http");
  patchListen(net.Server, "node:net");
  state.patchedPrototypes = patchedPrototypes;
  globalThis[CYGNUS_STATE] = state;
}
