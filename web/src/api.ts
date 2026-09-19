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

interface Listing {
  items: Card[];
}

/** The projection this page is: every open item, in deck order. */
export async function fetchNeedsYou(): Promise<Card[]> {
  const response = await fetch("/api/v1/needs-you", {
    credentials: "same-origin",
    headers: { Accept: "application/json" },
  });
  if (!response.ok) {
    throw new Error(`needs-you ${response.status}`);
  }
  const listing = (await response.json()) as Listing;
  return listing.items;
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

async function post(path: string, body?: URLSearchParams): Promise<WriteResult> {
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
  return post(
    `/attention/${encodeURIComponent(board)}/${encodeURIComponent(id)}/reply`,
    body,
  );
}

/**
 * Reopen one decided item. The reopen note is fixed prose on the server; an
 * undo that demanded words would be a dialog wearing a button.
 */
export function postReopen(board: string, id: string): Promise<WriteResult> {
  return post(
    `/attention/${encodeURIComponent(board)}/${encodeURIComponent(id)}/reopen`,
  );
}
