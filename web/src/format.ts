/**
 * The words the card reads, in the same shapes the server writes them.
 *
 * Each function here mirrors one in `rust/serve.rs` and says which: these
 * are the strings a reader compares between the deck and a read page, so
 * they are ported rather than reinvented.
 */

/** `rust/serve.rs`'s `age`: how long ago, in the coarsest unit still true. */
export function age(created: number, now: number = Date.now()): string {
  const minutes = Math.floor(Math.max(now - created, 0) / 60_000);
  if (minutes === 0) {
    return "just now";
  }
  if (minutes === 1) {
    return "1 min";
  }
  if (minutes < 60) {
    return `${minutes} min`;
  }
  if (minutes < 1440) {
    const hours = Math.floor(minutes / 60);
    return `${hours}h${String(minutes % 60).padStart(2, "0")}m`;
  }
  if (minutes < 2880) {
    return "1 day";
  }
  return `${Math.floor(minutes / 1440)} days`;
}

/** `rust/serve.rs`'s `ago`: the same, as a phrase that reads in a sentence. */
export function ago(created: number, now: number = Date.now()): string {
  const text = age(created, now);
  return text === "just now" ? text : `${text} ago`;
}

/** The longest a question may be before the card elides it. */
const QUESTION_BOUND = 160;

/**
 * `rust/serve.rs`'s `card_question`: what the card asks, which for a row
 * that authored no question is the first line of its body, bounded to the
 * same 160 characters.
 */
export function cardQuestion(
  question: string | null | undefined,
  body: string,
): string {
  if (question !== null && question !== undefined && question.length > 0) {
    return question;
  }
  const first = (body.split("\n")[0] ?? "").trim();
  const glyphs = [...first];
  if (glyphs.length <= QUESTION_BOUND) {
    return first;
  }
  return `${glyphs.slice(0, QUESTION_BOUND - 1).join("")}…`;
}

/* read pages */

/**
 * `rust/serve.rs`'s `status_label`: one board status as a heading and a
 * pill read it. `todo` is two words in English; every other status is one
 * word or underscore-joined.
 */
export function statusLabel(status: string): string {
  if (status === "todo") {
    return "To do";
  }
  const words = status.replace(/_/g, " ");
  return words.length === 0 ? words : words[0]?.toUpperCase() + words.slice(1);
}

/**
 * `rust/serve.rs`'s `a_or_an`: capitalised, because every sentence it opens
 * is a meta sentence and it is the first word of it.
 */
export function aOrAn(word: string): "A" | "An" {
  return /^[aeiou]/i.test(word) ? "An" : "A";
}

/**
 * `rust/serve.rs`'s `stamp`: a millisecond stamp as a readable UTC instant,
 * deliberately not localised — the ledger stores UTC and a page that
 * quietly shifted stamps would disagree with every `--json` read.
 */
export function stamp(ms: number): string {
  const at = new Date(ms);
  const pad = (value: number, width = 2) => String(value).padStart(width, "0");
  return (
    `${pad(at.getUTCFullYear(), 4)}-${pad(at.getUTCMonth() + 1)}-${pad(at.getUTCDate())} ` +
    `${pad(at.getUTCHours())}:${pad(at.getUTCMinutes())}:${pad(at.getUTCSeconds())}Z`
  );
}

/**
 * `rust/serve.rs`'s `tag_list`: the words a row was filed under, as a
 * fragment of the row's own sentence. A tag is neither a state nor an
 * outcome, so it gets no pill (WEB-41).
 */
export function tagSentence(tags: readonly string[]): string {
  return tags.length === 0 ? "" : `, tagged ${tags.join(", ")}`;
}
