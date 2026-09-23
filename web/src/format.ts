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

/* read pages -------------------------------------------------------------
 *
 * The four sentence-shapes the read pages share, each ported from the
 * `rust/serve.rs` function named on it. They are here rather than in a page
 * because three pages read the same sentence and a reader compares them.
 */

/** `rust/serve.rs`'s `status_label`: one board status as a heading reads it. */
export function statusLabel(status: string): string {
  // `todo` is two words in English; every other status is one word or
  // underscore-joined.
  if (status === "todo") {
    return "To do";
  }
  const words = status.replaceAll("_", " ");
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/**
 * `rust/serve.rs`'s `a_or_an`, capitalised as every call site is: the
 * article opens a row's meta sentence, so "An epic" and "A task".
 */
export function aOrAn(word: string): string {
  return "aeiou".includes((word[0] ?? "").toLowerCase()) ? "An" : "A";
}

/**
 * `rust/serve.rs`'s `stamp`: an absolute UTC moment, to the second.
 *
 * `YYYY-MM-DD HH:MM:SSZ` — a space rather than the ISO `T`, because this is
 * a moment a person reads in a row, not a value anything parses back.
 */
export function stamp(ms: number): string {
  return `${new Date(ms).toISOString().slice(0, 19).replace("T", " ")}Z`;
}
