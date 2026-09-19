/**
 * Hover previews, outside React on purpose.
 *
 * Every `a[data-ref]` shows what it points at on hover, and the card it
 * renders can itself carry `data-ref` anchors, so previews nest: hovering a
 * reference inside a preview opens the next one beside it. That is only true
 * when the listening is delegated to the document — content that did not
 * exist when the page loaded, including the cards themselves, has to work —
 * and the popups belong to the viewport rather than to the card, so they are
 * appended to `<body>` and are not part of any component's tree.
 *
 * The card is built here from `/api/v1/preview/{kind}/{project}/{id}`
 * (SPA-40). Every value it renders is set as TEXT; the one exception is the
 * task body, which arrives as `bodyHtml` — the server's own `markdown`,
 * which drops raw HTML and defangs every link that is not http(s), mailto or
 * same-page. The client adds no parser and no markup of its own to agent
 * text (SPA-41).
 */

import { ago, aOrAn, cardQuestion, statusLabel } from "./format";

const PREVIEW_DELAY = 120;

/** What `/api/v1/preview/...` answers, by kind. */
interface Preview {
  kind: string;
  board: string;
  task?: PreviewTask;
  bodyHtml?: string;
  openAttention?: number;
  parent?: Reference;
  attention?: PreviewAttention;
  about?: Reference;
  deployment?: PreviewDeployment;
  boardCounts?: {
    openAttention: number;
    todo: number;
    inProgress: number;
    tasks: number;
  };
}

interface Reference {
  id: string;
  taskType: string;
  title: string;
}

interface PreviewTask {
  id: string;
  type: string;
  title: string;
  status: string;
  priority: number;
  priorityLevel?: string | null;
  lane?: string | null;
  parentID?: string | null;
  updatedAt: number;
}

interface PreviewAttention {
  id: string;
  kind: string;
  body: string;
  question?: string | null;
  context?: string | null;
  status: string;
  choices: { key: string; label: string; consequence: string; recommended?: boolean }[];
  decision?: { choice: string; outcome: string } | null;
  taskID?: string | null;
}

interface PreviewDeployment {
  id: string;
  repo: string;
  tier: string;
  environment: string;
  host: string;
  status: string;
  updatedAt: number;
}

const cache = new Map<string, Preview>();
const open: HTMLElement[] = [];
let openTimer: number | null = null;
let closeTimer: number | null = null;

/**
 * The projection that answers one reference, or `null` for a link that is
 * not a record on this origin.
 *
 * `/board/{project}` previews as `board` with the project repeated as the
 * id, exactly as the served fragment's own arm passes it.
 */
function previewUrl(anchor: HTMLAnchorElement): string | null {
  let url: URL;
  try {
    url = new URL(anchor.href, location.href);
  } catch {
    return null;
  }
  if (url.origin !== location.origin) {
    return null;
  }
  const segments = url.pathname.split("/").filter((segment) => segment.length > 0);
  const [kind, project, id] = segments;
  if (kind === "board" && project !== undefined && segments.length === 2) {
    return `/api/v1/preview/board/${project}/${project}`;
  }
  if (
    segments.length === 3 &&
    project !== undefined &&
    id !== undefined &&
    (kind === "task" || kind === "attention" || kind === "deployment")
  ) {
    return `/api/v1/preview/${kind}/${project}/${id}`;
  }
  return null;
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

function meta(text: string): HTMLParagraphElement {
  const line = document.createElement("p");
  line.className = "meta";
  line.textContent = text;
  return line;
}

function heading(text: string): HTMLHeadingElement {
  const head = document.createElement("h3");
  head.textContent = text;
  return head;
}

/** The status pill, as every read surface draws it. */
function pill(status: string): HTMLSpanElement {
  const span = document.createElement("span");
  span.className = `pill status-${status}`;
  span.textContent = statusLabel(status);
  return span;
}

function priority(value: number, level: string | null | undefined): HTMLSpanElement {
  const span = document.createElement("span");
  if (level === null || level === undefined) {
    span.className = "priority priority-legacy";
    span.title = "legacy out-of-band priority";
    span.textContent = String(value);
    return span;
  }
  span.className = `priority priority-${level.toLowerCase()}`;
  span.title = `stored priority ${value}`;
  span.textContent = level;
  return span;
}

/**
 * A reference inside a preview: it carries `data-ref` too, which is what
 * makes previews nest, and it opens its record in its own tab.
 */
function reference(board: string, id: string, text: string): HTMLAnchorElement {
  const link = document.createElement("a");
  link.href = `/task/${encodeURIComponent(board)}/${encodeURIComponent(id)}`;
  link.dataset.taskLink = id;
  link.dataset.ref = "";
  link.target = "_blank";
  link.rel = "noopener";
  link.textContent = text;
  return link;
}

/** The decision a resolved item recorded, in `decision_words`' wording. */
function decisionWords(item: PreviewAttention): string {
  const decision = item.decision;
  if (decision === null || decision === undefined) {
    return "recorded";
  }
  if (decision.choice === "custom") {
    return `Your own answer, recorded as ${decision.outcome}.`;
  }
  const chosen = item.choices.find((choice) => choice.key === decision.choice);
  return chosen === undefined
    ? `recorded as ${decision.outcome}`
    : `${chosen.label}. ${chosen.consequence}`;
}

/** The card one reference answers with, built from its projection. */
function card(preview: Preview): HTMLElement {
  const body = document.createElement("div");
  body.className = "preview-card";
  body.dataset.previewCard = "";
  const task = preview.task;
  const item = preview.attention;
  const deployment = preview.deployment;
  const counts = preview.boardCounts;
  if (task !== undefined) {
    body.append(heading(task.title));
    const line = meta("");
    line.append(`${aOrAn(task.type)} ${task.type} in `, pill(task.status), " at ");
    line.append(
      priority(task.priority, task.priorityLevel),
      `, updated ${ago(task.updatedAt)}`,
    );
    if (task.lane !== null && task.lane !== undefined) {
      line.append(` in lane ${task.lane}`);
    }
    if (preview.parent !== undefined) {
      line.append(
        ", part of the ",
        Object.assign(document.createElement("span"), {
          textContent: preview.parent.taskType,
        }),
        " ",
        reference(preview.board, preview.parent.id, preview.parent.title),
      );
    } else if (task.parentID !== null && task.parentID !== undefined) {
      line.append(", part of ", reference(preview.board, task.parentID, task.parentID));
    }
    body.append(line);
    if (preview.bodyHtml !== undefined) {
      const prose = document.createElement("div");
      prose.className = "body md";
      // The server's own typesetting, sanitised where it was rendered.
      prose.innerHTML = preview.bodyHtml;
      body.append(prose);
    }
    if (preview.openAttention !== undefined && preview.openAttention > 0) {
      body.append(
        meta(`${preview.openAttention} open attention - it is on Needs you.`),
      );
    }
    return body;
  }
  if (item !== undefined) {
    body.append(heading(cardQuestion(item.question, item.body)));
    if (item.context !== null && item.context !== undefined) {
      body.append(meta(item.context));
    }
    const kind = item.kind.replace(/_/g, " ");
    const state =
      item.status === "resolved"
        ? `decided: ${decisionWords(item)}`
        : `open - ${[
            ...item.choices.filter((choice) => choice.recommended === true),
            ...item.choices.filter((choice) => choice.recommended !== true),
          ]
            .map((choice) => `${choice.recommended === true ? "*" : ""}${choice.label}`)
            .join(", ")}`;
    body.append(meta(`${aOrAn(kind)} ${kind} ask, ${state}`));
    if (preview.about !== undefined) {
      const about = meta("about ");
      about.append(reference(preview.board, preview.about.id, preview.about.title));
      body.append(about);
    } else if (item.taskID !== null && item.taskID !== undefined) {
      const about = meta("about ");
      about.append(reference(preview.board, item.taskID, item.taskID));
      body.append(about);
    }
    return body;
  }
  if (deployment !== undefined) {
    const head = document.createElement("h3");
    const id = document.createElement("code");
    id.textContent = deployment.id;
    head.append(id);
    body.append(head);
    body.append(
      meta(
        `${deployment.repo} on the ${deployment.tier} tier, ${deployment.environment} on ` +
          `${deployment.host}, ${deployment.status} ${ago(deployment.updatedAt)}, ` +
          `from board ${preview.board}`,
      ),
    );
    return body;
  }
  if (counts !== undefined) {
    body.append(heading(preview.board));
    body.append(
      meta(
        `${counts.openAttention} open attention, ${counts.todo} to do, ` +
          `${counts.inProgress} in progress, ${counts.tasks} tasks in all`,
      ),
    );
    return body;
  }
  body.append(meta("Nothing to preview here."));
  return body;
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
  popup.append(meta("Loading…"));
  open.push(popup);
  place(popup, anchor);
  try {
    let preview = cache.get(url);
    if (preview === undefined) {
      const response = await fetch(url, {
        credentials: "same-origin",
        headers: { Accept: "application/json" },
      });
      if (!response.ok) {
        throw new Error(`preview ${response.status}`);
      }
      preview = (await response.json()) as Preview;
      cache.set(url, preview);
    }
    if (!popup.isConnected) {
      return;
    }
    popup.replaceChildren(card(preview));
    place(popup, anchor);
  } catch {
    if (popup.isConnected) {
      popup.replaceChildren(
        meta("The preview did not load. Click the link to open the item."),
      );
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
