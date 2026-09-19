import type { ReactElement } from "react";
import { useEffect, useRef } from "react";
import type { Notice } from "./live";

/**
 * How many notices stay on screen. Three, newest on top: the strip is meant
 * to be readable at a glance, not to be a log; `kb ev` is the log.
 */
export const NOTICE_SHOWN = 3;

/**
 * A toast stays twenty seconds (George, 2026-09-17: "the toast is too
 * fast"), and the clock stops while the pointer is on it or the keyboard is
 * in it — a notice that vanished out from under the eye reading it would be
 * worse than one that stayed.
 */
const TOAST_LIFE = 20_000;

/**
 * One notice, with its own clock.
 *
 * The clock belongs to the row rather than to the strip because holding is
 * per-row: hovering the notice being read must not keep the two above it
 * alive. Dismissal is a click anywhere on the row — the button is the
 * keyboard's way in and the label that says so, the row itself is the
 * thumb's.
 */
function NoticeRow({
  notice,
  onDismiss,
}: {
  notice: Notice;
  onDismiss: (key: string) => void;
}): ReactElement {
  const timer = useRef<number | null>(null);
  useEffect(() => {
    timer.current = window.setTimeout(() => onDismiss(notice.key), TOAST_LIFE);
    return () => {
      if (timer.current !== null) {
        window.clearTimeout(timer.current);
      }
    };
  }, [notice.key, onDismiss]);
  const hold = () => {
    if (timer.current !== null) {
      window.clearTimeout(timer.current);
      timer.current = null;
    }
  };
  const release = () => {
    if (timer.current === null) {
      timer.current = window.setTimeout(() => onDismiss(notice.key), TOAST_LIFE);
    }
  };
  return (
    // The row is the pointer's shortcut; the keyboard has the Dismiss button
    // inside it and `Escape` for all of them at once.
    // biome-ignore lint/a11y/useKeyWithClickEvents: pointer shortcut, keyboard has Dismiss and Escape
    <p
      className={notice.type === "notice" ? "notice" : "notice summary"}
      data-key={notice.key}
      data-testid="deck-notice"
      onClick={() => onDismiss(notice.key)}
      onMouseEnter={hold}
      onMouseLeave={release}
      onFocus={hold}
      onBlur={release}
    >
      {notice.board === undefined ? null : (
        <span className="notice-board">{notice.board}</span>
      )}
      <span className="notice-what">{notice.what}</span>
      {notice.task === undefined || notice.board === undefined ? null : (
        <a
          href={`/task/${encodeURIComponent(notice.board)}/${encodeURIComponent(notice.task)}`}
        >
          {notice.title === undefined ? notice.task : `${notice.task} ${notice.title}`}
        </a>
      )}
      <button type="button" className="dismiss">
        Dismiss
      </button>
    </p>
  );
}

/**
 * The page's one `role=log`: what arrived, newest first.
 *
 * It is a log rather than a status because what arrived is a list of
 * entries; the connection line is the page's one status (WEB-52, SPA-34).
 */
export function Notices({
  notices,
  onDismiss,
}: {
  notices: Notice[];
  onDismiss: (key: string) => void;
}): ReactElement {
  return (
    <div
      className="toasts notices"
      data-notices=""
      data-testid="deck-notices"
      role="log"
      aria-live="polite"
    >
      {notices.map((notice) => (
        <NoticeRow key={notice.key} notice={notice} onDismiss={onDismiss} />
      ))}
    </div>
  );
}
