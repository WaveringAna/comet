import net from "node:net";
import { randomUUID } from "node:crypto";

const address = process.env.NOVA_PI_COLLAB_ADDR;
const token = process.env.NOVA_PI_COLLAB_TOKEN;
const MAX_LINE = 1024 * 1024;

export default function novaCollaborationBridge(pi) {
  if (!address || !token) return;
  const [host, portText] = address.split(":");
  const socket = net.createConnection({ host, port: Number(portText) });
  let buffer = "";
  const cleanups = [];

  const send = (frame) => {
    if (!socket.destroyed && socket.writable) {
      socket.write(`${JSON.stringify({ token, ...frame })}\n`);
    }
  };

  const request = (method, params = {}) => {
    const requestId = `nova-bridge-${randomUUID()}`;
    const replyEvent = `subagents:rpc:v1:reply:${requestId}`;
    const off = pi.events.on(replyEvent, (reply) => {
      off?.();
      send({ type: "event", event: `bridge:${method}`, data: reply });
    });
    pi.events.emit("subagents:rpc:v1:request", {
      version: 1,
      requestId,
      method,
      params,
      source: { extension: "nova" },
    });
  };

  const relay = (event) => {
    const off = pi.events.on(event, (data) => send({ type: "event", event, data }));
    if (off) cleanups.push(off);
  };
  for (const event of [
    "subagent:async-started",
    "subagent:async-complete",
    "subagent:control-event",
    "subagent:steering-notice",
    "subagent:process-terminal",
  ]) relay(event);

  const readyOff = pi.events.on("subagents:rpc:v1:ready", (data) => {
    send({ type: "event", event: "subagents:rpc:v1:ready", data });
    request("ping");
    request("status");
  });
  if (readyOff) cleanups.push(readyOff);

  socket.on("connect", () => {
    send({ type: "hello", version: 1 });
    // Registration order is not an API. A zero-delay ping discovers an RPC
    // bridge that registered before this extension even if its ready event was
    // already emitted.
    setTimeout(() => request("ping"), 0);
  });
  socket.on("data", (chunk) => {
    buffer += chunk.toString("utf8");
    if (buffer.length > MAX_LINE) {
      socket.destroy();
      return;
    }
    for (;;) {
      const newline = buffer.indexOf("\n");
      if (newline < 0) break;
      const line = buffer.slice(0, newline);
      buffer = buffer.slice(newline + 1);
      let frame;
      try { frame = JSON.parse(line); } catch { continue; }
      if (frame?.token !== token || frame?.type !== "request") continue;
      const requestId = frame.requestId;
      const replyEvent = `subagents:rpc:v1:reply:${requestId}`;
      const off = pi.events.on(replyEvent, (reply) => {
        off?.();
        send({ type: "reply", requestId, data: reply });
      });
      pi.events.emit("subagents:rpc:v1:request", {
        version: 1,
        requestId,
        method: frame.method,
        params: frame.params ?? {},
        source: { extension: "nova" },
      });
    }
  });
  socket.on("error", () => {});
  socket.on("close", () => cleanups.splice(0).forEach((off) => off()));
}
