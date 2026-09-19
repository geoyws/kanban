/**
 * The `/live` socket, which is unchanged in kind by the cutover (ADR-048 §5).
 *
 * Four frame kinds and no others (`rust/serve.rs`'s `websocket`): `ready`
 * with the revision fingerprint, `notice` and `behind` — the two the strip
 * renders — and `refresh`, which says only that the boards moved. Nothing on
 * this socket is state to render from: the page reads the projection for
 * that, which is what keeps one read model (SPA-49).
 */

/** One thing that happened, in the words a person reads. */
export interface Notice {
  type: string;
  key: string;
  what: string;
  board?: string;
  task?: string;
  title?: string;
}

export interface LiveHandlers {
  /** The socket's own state, which is all the connection line may say. */
  onStatus: (status: "live" | "reconnecting") => void;
  /** `true` when the notice was rendered, `false` when it was already seen. */
  onNotice: (notice: Notice) => boolean;
  /** The boards moved: read the projection again. */
  onRefresh: () => void;
}

/** One frame off the wire: a notice, with the `ready` frame's extra word. */
type Frame = Notice & { noticesFrom?: string };

declare global {
  interface Window {
    /**
     * THE TEST SEAM, and the only one: the socket's frame handler, so a
     * case can redeliver a frame the server already sent and observe what
     * the page does with the second copy. A duplicate cannot be provoked
     * from outside the browser -- the server never sends one, and two
     * writes produce two different keys -- so the only honest way to drive
     * the redelivery path is to hand the page the same frame twice.
     *
     * It is one function, it is installed by `connectLive` and removed when
     * the socket is stopped, and it does exactly what `onmessage` does.
     */
    __kbLive?: { deliver: (frame: Frame) => boolean };
  }
}

/** How long a closed socket waits before trying again. */
const RECONNECT_DELAY = 1500;

/**
 * Connect, and keep connecting. The returned function stops both the socket
 * and the retry, so a page that unmounts leaves nothing running.
 */
export function connectLive(handlers: LiveHandlers): () => void {
  let socket: WebSocket | null = null;
  let retry: number | null = null;
  let stopped = false;
  let connects = 0;

  /**
   * What one frame does to the page. `true` when it changed something: a
   * notice that rendered, a refresh that was asked for. `false` when the
   * frame was a duplicate or had nothing to say.
   */
  const deliver = (frame: Frame): boolean => {
    if (frame.type === "notice" || frame.type === "behind") {
      return handlers.onNotice(frame);
    }
    if (frame.type === "ready") {
      // The server starts every connection at the head and says so. Only a
      // RECONNECT needs saying out loud: the operator who just lost a
      // socket is the one who would otherwise wonder what they missed.
      connects += 1;
      if (connects > 1 && frame.noticesFrom === "now") {
        return handlers.onNotice({
          type: "reconnected",
          key: `reconnected-${connects}`,
          what: "Reconnected. Showing changes from now.",
        });
      }
      return false;
    }
    if (frame.type === "refresh") {
      handlers.onRefresh();
      return true;
    }
    return false;
  };

  const open = () => {
    if (stopped) {
      return;
    }
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    socket = new WebSocket(`${scheme}://${location.host}/live`);
    socket.onopen = () => handlers.onStatus("live");
    socket.onmessage = (event: MessageEvent<string>) => {
      let frame: Frame;
      try {
        frame = JSON.parse(event.data);
      } catch {
        return;
      }
      deliver(frame);
    };
    socket.onclose = () => {
      handlers.onStatus("reconnecting");
      retry = window.setTimeout(open, RECONNECT_DELAY);
    };
    socket.onerror = () => socket?.close();
  };

  window.__kbLive = { deliver };

  open();
  return () => {
    stopped = true;
    if (window.__kbLive?.deliver === deliver) {
      delete window.__kbLive;
    }
    if (retry !== null) {
      window.clearTimeout(retry);
    }
    if (socket !== null) {
      socket.onclose = null;
      socket.close();
    }
  };
}
