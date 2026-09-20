/**
 * The served surfaces this application talks to, and nothing else.
 *
 * Reads go to the JSON projection (`docs/api/kanban-web.openapi.yaml`);
 * writes go to the two form POSTs the server has always accepted, with the
 * same field names and the same encoding, because moving them would be a
 * change to the write surface disguised as a rename (SPA-10).
 */

/** One answer the raiser authored, in the order the card lists them. */
export interface Choice {
  key: string;
  label: string;
  consequence: string;
  outcome: string;
  recommended?: boolean;
}

/** `rust/model.rs`'s `Attention`, as the projection serialises it. */
export interface Attention {
  id: string;
  taskID?: string | null;
  kind: string;
  body: string;
  question?: string | null;
  context?: string | null;
  choices: Choice[];
  raisedBy: string;
  createdAt: number;
  status: string;
  priority: number;
  priorityLevel?: string | null;
  tags: string[];
  resolvedAt?: number | null;
  resolvedBy?: string | null;
  /** The composed sentence a row settled with before decisions were kept. */
  resolution?: string | null;
  decision?: Decision | null;
}

/**
 * What settling one item recorded (`rust/model.rs`'s `AttentionDecision`).
 * `null` while the item is open: a reopen clears it from the row and keeps
 * it in the ledger (ADR-042 §3).
 */
export interface Decision {
  choice: string;
  outcome: string;
  note?: string | null;
  by: string;
  at: number;
}

/** The row a card is about, as the card's meta sentence reads it. */
export interface TaskReference {
  id: string;
  taskType: string;
  title: string;
}

/** One open item paired with the board that raised it. */
export interface Card {
  board: string;
  attention: Attention;
  /** The body, typeset by the server's one markdown renderer. */
  bodyHtml: string;
  task?: TaskReference;
}

/**
 * The capped-listing envelope every JSON listing arrives in (ADR-037 §4,
 * option A): the rows, and whether there are rows this answer does not
 * hold. A page that renders `items` and drops `truncated` is telling the
 * operator a bounded list is the whole list.
 */
export interface Listing<T> {
  items: T[];
  returned: number;
  limit: number | null;
  truncated: boolean;
}

/**
 * A read the server refused, carrying the status it refused with.
 *
 * Typed rather than a bare `Error` because one status is a product state
 * and not a fault: `404` is the projection's single non-enumerating
 * refusal (`Refusal::DeniedOrNotFound` in `rust/projection.rs`), which a
 * page renders as a named state instead of as a broken page. Every other
 * status stays the generic failure it is.
 */
export class Refused extends Error {
  readonly status: number;

  constructor(path: string, status: number) {
    super(`${path} ${status}`);
    this.name = "Refused";
    this.status = status;
  }
}

/**
 * Read one projection.
 *
 * Every read in this application goes through here, so there is one place
 * that says what a read is: same-origin, JSON, and a refusal that carries
 * the route and the status rather than a parse error twenty frames later.
 */
export async function fetchJson<T>(path: string): Promise<T> {
  const response = await fetch(path, {
    credentials: "same-origin",
    headers: { Accept: "application/json" },
  });
  if (!response.ok) {
    throw new Refused(path, response.status);
  }
  return (await response.json()) as T;
}

/** The projection this page is: every open item, in deck order. */
export async function fetchNeedsYou(): Promise<Card[]> {
  return (await fetchJson<Listing<Card>>("/api/v1/needs-you")).items;
}

/**
 * What a write came back as.
 *
 * The routes answer a recorded write with a redirect and every refusal with
 * a page, so an opaque redirect IS the receipt — which is why every POST
 * here is `redirect: "manual"`. A refusal carries the board's own sentence,
 * read out of the page it answered with and never paraphrased.
 */
export type WriteResult =
  | { recorded: true }
  | { recorded: false; refusal: string | null; status: number };

/**
 * Post one of the four allowed form writes and read what came back.
 *
 * Shared by every page that writes, because the refusal-reading is the
 * delicate half: three copies of it would be three different ideas of what
 * a refused write says.
 */
export async function postForm(
  path: string,
  body?: URLSearchParams,
): Promise<WriteResult> {
  const response = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    redirect: "manual",
    ...(body === undefined ? {} : { body }),
  });
  if (response.type === "opaqueredirect" || response.ok) {
    return { recorded: true };
  }
  // The refusal is the route's `<p class=error>`, verbatim. A page that
  // carries none leaves the caller to say what it knows, which is the status.
  const page = new DOMParser().parseFromString(await response.text(), "text/html");
  const refused = page.querySelector(".error");
  return {
    recorded: false,
    refusal: refused === null ? null : refused.textContent,
    status: response.status,
  };
}

/** Record one decision through the trusted edge's own reply route. */
export function postDecision(
  board: string,
  id: string,
  body: URLSearchParams,
): Promise<WriteResult> {
  return postForm(
    `/attention/${encodeURIComponent(board)}/${encodeURIComponent(id)}/reply`,
    body,
  );
}

/**
 * Reopen one decided item. The reopen note is fixed prose on the server; an
 * undo that demanded words would be a dialog wearing a button.
 */
export function postReopen(board: string, id: string): Promise<WriteResult> {
  return postForm(
    `/attention/${encodeURIComponent(board)}/${encodeURIComponent(id)}/reopen`,
  );
}
