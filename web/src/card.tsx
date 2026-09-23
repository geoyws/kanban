import type { ReactElement } from "react";
import type { Card, Choice } from "./api";
import { ago, cardQuestion } from "./format";
import { TagChips } from "./tags";

/**
 * One decision card, in the order ADR-042 §5 fixes and the deck renders:
 * who asked and what, the raiser's context and long form in one capped
 * scroller, the recommendation, the alternatives, the note, and the
 * own-words answer folded beneath them. Losing the scriptless fallback did
 * not license reordering the card (SPA-51).
 */

/** `rust/serve.rs`'s `MAX_REPLY_BYTES`: what the route will accept. */
const MAX_REPLY = 4096;

/** `rust/model.rs`'s `ATTENTION_OUTCOMES`, in the order the picker shows. */
export const OUTCOMES = ["approve", "reject", "defer", "other"] as const;

/**
 * What the composer says when it refuses to post a half-written answer, in
 * the page's language — the card speaking to somebody holding a phone, never
 * the route's CLI wording (which is quoted verbatim when the ROUTE says it).
 */
export const INCOMPLETE_ANSWER =
  "Your own answer needs both halves: pick a verdict (approve, reject, defer or other) and write your reply.";

/** The draft an operator has started on one card, and nothing else. */
export interface Draft {
  note: string;
  outcome: string | null;
}

/** What is in flight on this card, from the click until the board answers. */
export interface Sending {
  /** The control that was pressed: a choice key, or the custom submit. */
  pressed: string;
  /** Whether the wait has gone on long enough to say so a second time. */
  still: boolean;
}

export interface CardHandlers {
  onNote: (id: string, note: string) => void;
  onOutcome: (id: string, outcome: string) => void;
  onClear: (id: string) => void;
  onCustomOpen: (id: string, open: boolean) => void;
  onChoice: (card: Card, choice: Choice) => void;
  /** Answer the card's comprehension check (ACC-11): one key, one write. */
  onCheck: (card: Card, key: string) => void;
  onCustom: (card: Card) => void;
}

export interface CardState {
  current: boolean;
  hidden: boolean;
  sending: Sending | null;
  /** `leaving`, `leaving-back` or `entering`, while the deck is moving. */
  motion: string | null;
  draft: Draft;
  customOpen: boolean;
  /** The composer's own refusal, true only while a half is missing. */
  incomplete: string | null;
  /** What the board or the network said about an attempt actually made. */
  board: string | null;
}

/**
 * The priority, which is the only thing in the card's orientation line that
 * is not prose (`rust/serve.rs`'s `priority_badge`).
 *
 * Exported because every read page ends a row's sentence on the same badge,
 * and a second rendering of it would be a second idea of what P0 looks like.
 */
export function Priority({
  priority,
  level,
}: {
  priority: number;
  level: string | null | undefined;
}) {
  if (level === null || level === undefined) {
    return (
      <span
        className="priority priority-legacy"
        data-testid="deck-priority"
        title="legacy out-of-band priority"
      >
        {priority}
      </span>
    );
  }
  return (
    <span
      className={`priority priority-${level.toLowerCase()}`}
      data-testid="deck-priority"
      title={`stored priority ${priority}`}
    >
      {level}
    </span>
  );
}

/**
 * One answer, with its consequence under it. The digit is on the button
 * because that is where the keyboard map says it is (WEB-27), and the label
 * is replaced by what the click is doing while it is in flight — on the
 * control's OWN fill, never disabled: a disabled button is the one control
 * that cannot report its own refusal.
 */
function Answer({
  card,
  choice,
  digit,
  sending,
  onChoice,
}: {
  card: Card;
  choice: Choice;
  digit: number;
  sending: Sending | null;
  onChoice: CardHandlers["onChoice"];
}) {
  const pressed = sending !== null && sending.pressed === choice.key;
  return (
    <>
      <button
        type="submit"
        className={`choice outcome-${choice.outcome}`}
        name="decision"
        value={choice.key}
        data-label={choice.label}
        data-testid={`deck-choice-${choice.key}`}
        {...(pressed ? { "data-pressed": "" } : {})}
        onClick={(event) => {
          event.preventDefault();
          onChoice(card, choice);
        }}
      >
        {pressed ? (
          sending.still ? (
            "Still sending…"
          ) : (
            "Sending…"
          )
        ) : (
          <>
            <span className="key" data-testid="deck-digit">
              {digit}
            </span>
            {choice.label}
          </>
        )}
      </button>
      <p className="consequence">{choice.consequence}</p>
    </>
  );
}

/**
 * The row the card is about, as the meta sentence reads it
 * (`rust/serve.rs`'s `task_reference`): every reference previews on hover
 * and opens in its own tab, so an id met mid-sentence answers "what is
 * this?" without leaving the page.
 */
function TaskSentence({ card }: { card: Card }) {
  const id = card.attention.taskID;
  if (id === null || id === undefined) {
    return null;
  }
  const href = `/task/${encodeURIComponent(card.board)}/${encodeURIComponent(id)}`;
  if (card.task === undefined) {
    return (
      <>
        {", about "}
        <a href={href} data-task-link={id} data-ref target="_blank" rel="noopener">
          {id}
        </a>
      </>
    );
  }
  return (
    <>
      {", about the "}
      <span data-task-type>{card.task.taskType}</span>{" "}
      <a href={href} data-task-link={id} data-ref target="_blank" rel="noopener">
        {card.task.title}
      </a>
    </>
  );
}

export function DecisionCard({
  card,
  state,
  handlers,
  cardRef,
}: {
  card: Card;
  state: CardState;
  handlers: CardHandlers;
  cardRef: (id: string, node: HTMLElement | null) => void;
}): ReactElement {
  const item = card.attention;
  const ordered = [
    ...item.choices.filter((choice) => choice.recommended === true),
    ...item.choices.filter((choice) => choice.recommended !== true),
  ];
  const recommended = ordered[0]?.recommended === true ? ordered[0] : null;
  const alternatives = recommended === null ? ordered : ordered.slice(1);
  const ready = state.draft.outcome !== null && state.draft.note.trim().length > 0;
  const refusalId = (kind: string) => `refusal-${kind}-${item.id}`;
  const describe =
    state.incomplete !== null && state.draft.outcome === null
      ? refusalId("incomplete")
      : undefined;
  const locked = item.check !== undefined && item.check.answered === undefined;
  // decide without a click first, which is the whole keyboard path.
  // biome-ignore-start lint/a11y/noNoninteractiveTabindex: the deck focuses the card so the digits decide
  return (
    <article
      className={`item${state.motion === null ? "" : ` ${state.motion}`}`}
      tabIndex={0}
      data-testid="deck-card"
      data-item={item.id}
      data-project={card.board}
      aria-labelledby={`q-${item.id}`}
      hidden={state.hidden}
      {...(state.current ? { "data-current": "" } : {})}
      {...(state.sending === null
        ? {}
        : { "data-sent": "1", "data-state": "sending", "aria-busy": true })}
      {...(state.board === null ? {} : { "aria-describedby": refusalId("board") })}
      ref={(node) => cardRef(item.id, node)}
    >
      <p className="eyebrow">
        {`${item.raisedBy} asked on `}
        <a
          href={`/board/${encodeURIComponent(card.board)}`}
          data-ref
          target="_blank"
          rel="noopener"
        >
          {card.board}
        </a>
        {`, ${ago(item.createdAt)} · `}
        <Priority priority={item.priority} level={item.priorityLevel} />
        <TaskSentence card={card} />
        <TagChips tags={item.tags} />
      </p>
      <h2 id={`q-${item.id}`}>{cardQuestion(item.question, item.body)}</h2>
      {/* The context and the long form come before the answers in DOM and
          reader order, as they do in `decision_card`: what the question is
          about is read before it is answered. Where they SIT on the deck is
          the column's `order`, not the source order, so this is a change to
          what a screen reader and a scriptless client get and to nothing
          that is painted. */}
      <details className="full" data-testid="deck-full" open>
        <summary>show the full item</summary>
        <div className="body md" data-testid="deck-body">
          {item.context === null || item.context === undefined ? null : (
            <p className="context">{item.context}</p>
          )}
          {/* The server's own typesetting: `bodyHtml` is `rust/serve.rs`'s
              `markdown`, which drops every raw HTML event and defangs any
              link that is not http(s), mailto or same-page. The client adds
              no parser of its own. */}
          <div
            className="typeset"
            // biome-ignore lint/security/noDangerouslySetInnerHtml: server-typeset, server-sanitised
            dangerouslySetInnerHTML={{ __html: card.bodyHtml }}
          />
        </div>
      </details>
      {/* The comprehension check sits between the facts and the decision it
          gates (ACC-09): the reader holds the body, meets the one question
          that names the reusable fact, and only then reaches choices that
          can submit. Pre-answer the projection carries no `answer` and no
          `explanation` at all, so nothing here can leak them (ACC-10). */}
      {item.check === undefined ? null : item.check.answered === undefined ? (
        <form
          className="check"
          data-testid="deck-check"
          method="post"
          action={`/attention/${encodeURIComponent(card.board)}/${encodeURIComponent(item.id)}/check`}
          onSubmit={(event) => {
            event.preventDefault();
          }}
          aria-labelledby={`check-q-${item.id}`}
        >
          <p className="subject" data-testid="deck-check-about">
            {item.check.about}
          </p>
          <h3 id={`check-q-${item.id}`}>{item.check.question}</h3>
          {item.check.choices.map((choice, index) => (
            <button
              type="submit"
              className="check-choice"
              name="key"
              value={choice.key}
              data-label={choice.label}
              data-testid={`deck-check-choice-${choice.key}`}
              key={choice.key}
              onClick={(event) => {
                event.preventDefault();
                handlers.onCheck(card, choice.key);
              }}
            >
              <span className="key" data-testid="deck-digit">
                {index + 1}
              </span>
              {choice.label}
            </button>
          ))}
        </form>
      ) : (
        <section
          className="check answered"
          data-testid="deck-check-result"
          aria-live="polite"
        >
          <p
            className="check-result"
            data-check-result={item.check.correct === true ? "pass" : "miss"}
          >
            {item.check.correct === true
              ? "Check answered — pass."
              : `Check answered — miss on ${item.check.answered}.`}
          </p>
          {item.check.explanation === undefined ? null : (
            <p className="check-explain" data-testid="deck-check-explain">
              {item.check.explanation}
            </p>
          )}
        </section>
      )}
      <form
        className="decide"
        data-testid="deck-panel"
        method="post"
        action={`/attention/${encodeURIComponent(card.board)}/${encodeURIComponent(item.id)}/reply`}
        onSubmit={(event) => {
          event.preventDefault();
          handlers.onCustom(card);
        }}
      >
        {/* ACC-09: the decision is present but inert until the server has
            the check answer. `disabled` on the fieldset is the whole law —
            it cannot submit by keyboard, pointer or form association — and
            the hint says why in words rather than leaving a grey control to
            explain itself (ACC-12). */}
        <fieldset className="decide-controls" disabled={locked}>
          {recommended === null ? null : (
            <fieldset className="recommended" data-testid="deck-recommended">
              <legend>Recommended</legend>
              <Answer
                card={card}
                choice={recommended}
                digit={1}
                sending={state.sending}
                onChoice={handlers.onChoice}
              />
            </fieldset>
          )}
          {alternatives.length === 0 ? null : (
            <div className="alternatives" data-testid="deck-answers">
              {alternatives.map((choice, index) => (
                <div className="alternative" key={choice.key}>
                  <Answer
                    card={card}
                    choice={choice}
                    digit={index + (recommended === null ? 1 : 2)}
                    sending={state.sending}
                    onChoice={handlers.onChoice}
                  />
                </div>
              ))}
            </div>
          )}
          <div className="reply">
            <label htmlFor={`answer-${item.id}`}>Add a note</label>
            <textarea
              id={`answer-${item.id}`}
              name="reply"
              maxLength={MAX_REPLY}
              data-testid="deck-note"
              value={state.draft.note}
              onChange={(event) => handlers.onNote(item.id, event.target.value)}
              {...(describe !== undefined && state.draft.outcome !== null
                ? { "aria-describedby": describe }
                : {})}
            />
          </div>
          <details
            className="custom"
            data-custom
            data-testid="deck-custom"
            open={state.customOpen}
            onToggle={(event) =>
              handlers.onCustomOpen(item.id, (event.target as HTMLDetailsElement).open)
            }
          >
            <summary>Answer in my own words</summary>
            <fieldset className="outcomes">
              <legend>recorded as</legend>
              <div className="picks">
                {OUTCOMES.map((outcome) => (
                  <label key={outcome} htmlFor={`outcome-${item.id}-${outcome}`}>
                    <input
                      type="radio"
                      id={`outcome-${item.id}-${outcome}`}
                      name="outcome"
                      data-testid={`deck-outcome-${outcome}`}
                      value={outcome}
                      checked={state.draft.outcome === outcome}
                      onChange={() => handlers.onOutcome(item.id, outcome)}
                      {...(describe === undefined
                        ? {}
                        : { "aria-describedby": describe })}
                    />
                    {outcome}
                  </label>
                ))}
              </div>
            </fieldset>
            <div className="actions">
              <button
                type="submit"
                className="record"
                name="decision"
                value="custom"
                data-testid="deck-record"
                {...(state.sending !== null && state.sending.pressed === "custom"
                  ? { "data-pressed": "" }
                  : {})}
              >
                {state.sending !== null && state.sending.pressed === "custom"
                  ? state.sending.still
                    ? "Still sending…"
                    : "Sending…"
                  : "Record my answer"}
              </button>
              <button
                type="button"
                className="clear"
                data-clear
                data-testid="deck-clear"
                hidden={state.draft.outcome === null}
                onClick={() => handlers.onClear(item.id)}
              >
                Clear verdict
              </button>
              <p className="hint" data-hint data-testid="deck-hint" hidden={ready}>
                Pick a verdict and write your reply above.
              </p>
            </div>
          </details>
          {locked ? (
            <p className="hint" data-testid="deck-locked">
              These choices unlock once the check above is answered.
            </p>
          ) : null}
        </fieldset>
        {state.incomplete === null ? null : (
          <p className="error" data-refusal="incomplete" id={refusalId("incomplete")}>
            {state.incomplete}
          </p>
        )}
        {state.board === null ? null : (
          <p className="error" data-refusal="board" id={refusalId("board")}>
            {state.board}
          </p>
        )}
      </form>
    </article>
  );
}
// biome-ignore-end lint/a11y/noNoninteractiveTabindex: the deck focuses the card so the digits decide
