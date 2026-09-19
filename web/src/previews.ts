/**
 * Hover previews, outside React on purpose.
 *
 * Every `a[data-ref]` shows what it points at on hover, and the fragment it
 * fetches can itself carry `data-ref` anchors, so previews nest: hovering a
 * reference inside a preview opens the next one beside it. That is only true
 * when the listening is delegated to the document — content that did not
 * exist when the page loaded, including the fragments themselves, has to
 * work — and the popups belong to the viewport rather than to the card, so
 * they are appended to `<body>` and are not part of any component's tree.
 *
 * The fragment is the server's own `/preview/...` markup, escaped where it
 * renders operator text (`rust/serve.rs`'s `preview_page`); the client adds
 * no markup of its own to it.
 */

const PREVIEW_DELAY = 120;

const cache = new Map<string, string>();
const open: HTMLElement[] = [];
let openTimer: number | null = null;
let closeTimer: number | null = null;

function previewUrl(anchor: HTMLAnchorElement): string | null {
  try {
    const url = new URL(anchor.href, location.href);
    if (url.origin !== location.origin) {
      return null;
    }
    return `/preview${url.pathname}`;
  } catch {
    return null;
  }
}

/**
 * Pop every popup that is not an ancestor of `keep`: hovering a new anchor in
 * the page closes everything, hovering one inside a popup closes only the
 * popups deeper than that one.
 */
function closeDeeperThan(keep: Node | null): void {
  while (open.length > 0) {
    const top = open[open.length - 1];
    if (top === undefined || (keep !== null && (top.contains(keep) || top === keep))) {
      return;
    }
    open.pop()?.remove();
  }
}

function closeAll(): void {
  while (open.length > 0) {
    open.pop()?.remove();
  }
}

function place(popup: HTMLElement, anchor: HTMLElement): void {
  const rect = anchor.getBoundingClientRect();
  const margin = 8;
  popup.style.visibility = "hidden";
  document.body.append(popup);
  const box = popup.getBoundingClientRect();
  const left = Math.max(
    margin,
    Math.min(rect.right + margin, window.innerWidth - box.width - margin),
  );
  const below = rect.bottom + margin + box.height < window.innerHeight;
  popup.style.top = below
    ? `${rect.bottom + margin}px`
    : `${Math.max(margin, rect.top - box.height - margin)}px`;
  popup.style.left = `${left}px`;
  popup.style.visibility = "visible";
}

async function show(anchor: HTMLAnchorElement): Promise<void> {
  const url = previewUrl(anchor);
  if (url === null) {
    return;
  }
  closeDeeperThan(anchor);
  if (
    open.some((popup) => popup.dataset.previewFor === url && popup.contains(anchor))
  ) {
    return;
  }
  const popup = document.createElement("div");
  popup.className = "preview-pop";
  popup.dataset.previewFor = url;
  popup.dataset.testid = "deck-preview";
  popup.setAttribute("role", "tooltip");
  const loading = document.createElement("p");
  loading.className = "meta";
  loading.textContent = "Loading…";
  popup.append(loading);
  open.push(popup);
  place(popup, anchor);
  try {
    let html = cache.get(url);
    if (html === undefined) {
      const response = await fetch(url, { credentials: "same-origin" });
      if (!response.ok) {
        throw new Error(`preview ${response.status}`);
      }
      html = await response.text();
      cache.set(url, html);
    }
    if (!popup.isConnected) {
      return;
    }
    popup.innerHTML = html;
    place(popup, anchor);
  } catch {
    if (popup.isConnected) {
      popup.innerHTML = "";
      const failed = document.createElement("p");
      failed.className = "meta";
      failed.textContent = "The preview did not load. Click the link to open the item.";
      popup.append(failed);
    }
  }
}

function schedule(anchor: HTMLAnchorElement): void {
  if (closeTimer !== null) {
    window.clearTimeout(closeTimer);
  }
  if (openTimer !== null) {
    window.clearTimeout(openTimer);
  }
  openTimer = window.setTimeout(() => void show(anchor), PREVIEW_DELAY);
}

/**
 * Start listening. Returns the teardown, and closes whatever is open with
 * it: a popup that outlived its page would be a tooltip for nothing.
 */
export function bindPreviews(): () => void {
  const reference = (target: EventTarget | null): HTMLAnchorElement | null => {
    if (!(target instanceof Element)) {
      return null;
    }
    return target.closest("a[data-ref]");
  };
  const onOver = (event: MouseEvent) => {
    const anchor = reference(event.target);
    if (anchor !== null) {
      schedule(anchor);
      return;
    }
    if (
      !(event.target instanceof Element) ||
      event.target.closest(".preview-pop") === null
    ) {
      if (openTimer !== null) {
        window.clearTimeout(openTimer);
      }
      if (closeTimer !== null) {
        window.clearTimeout(closeTimer);
      }
      closeTimer = window.setTimeout(closeAll, PREVIEW_DELAY * 2);
    }
  };
  const onFocus = (event: FocusEvent) => {
    const anchor = reference(event.target);
    if (anchor !== null) {
      schedule(anchor);
    }
  };
  document.addEventListener("mouseover", onOver);
  document.addEventListener("focusin", onFocus);
  return () => {
    document.removeEventListener("mouseover", onOver);
    document.removeEventListener("focusin", onFocus);
    closeAll();
  };
}

/** Whether `Escape` had a preview to close, which is its first job. */
export function dismissPreviews(): boolean {
  const had = open.length > 0;
  closeAll();
  return had;
}
