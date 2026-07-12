// SGUI-02, ported for CONST-04: WebSocket hook with reconnect.
// The actual WebSocket construction (and the window.location read it needs) now lives entirely
// inside src/lib/aggregationClient.ts (the one module allowed to touch it) — this hook just
// wires the client's connection lifecycle into React state. No localStorage token either: auth
// is the same-origin session cookie, sent automatically on the WS handshake.
import { useEffect, useRef, useCallback, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { WsConnection } from '../lib/aggregationClient';
import type { WsEvent } from '../types/events';

type WsHandler = (event: WsEvent) => void;

export function useWebSocket(onEvent: WsHandler) {
  const [connected, setConnected] = useState(false);
  const connRef = useRef<WsConnection | null>(null);
  const onEventRef = useRef(onEvent);
  onEventRef.current = onEvent;

  useEffect(() => {
    const client = getAggregationClient();
    const conn = client.ws.connect({
      onEvent: (e) => onEventRef.current(e as WsEvent),
      onOpen: () => setConnected(true),
      onClose: () => setConnected(false),
    });
    connRef.current = conn;
    return () => {
      conn.close();
      connRef.current = null;
    };
  }, []);

  const send = useCallback((data: unknown) => {
    connRef.current?.send(data);
  }, []);

  return { connected, send };
}
