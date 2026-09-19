/**
 * Subscriptions: what each consumer watches, how far it has actually got,
 * and the one control an operator has over it.
 *
 * The page is the served one moved, not redesigned (ADR-046, SPA-35/36): one
 * borderless table with the row hairline, quiet at rest, and exactly one
 * loud element — a dead-lettered delivery, which is the only thing here that
 * needs a person.
 *
 * Every sentence it writes is ported from `rust/serve.rs`'s own renderer,
 * function for function — `watch_sentence`, `position_sentence`,
 * `dead_letter_attribution`, `or_list` — because these are the strings an
 * operator compares between the page and `kb subscription show`, and a
 * second wording would be a second answer.
 *
 * **The one display preference lives in the URL.** `?show=all` lists paused
 * subscriptions. It is deliberately not stored: a shareable URL already says
 * it, and two operators would otherwise disagree about what "the page"
 * lists. Paused rows are read from the projection whatever the filter says,
 * so a hidden row is counted and offered rather than reading as "nothing
 * exists".
 */

import type { FormEvent, MouseEvent, ReactElement, ReactNode } from "react";
import { Fragment, useState } from "react";
import { postForm } from "../api";
import { ago } from "../format";
import { navigate, type Route } from "../router";
import { useProjection } from "../shell";

/** `rust/model.rs`'s `Subscription`, as the projection serialises it. */
interface Subscription {
  id: string;
  subjectTaskID?: string | null;
  relations: string[];
  kinds: string[];
  priorStatuses: string[];
  currentStatuses: string[];
  tags: string[];
  consumerID: string;
  actionID: string;
  timeoutMs: number;
  maxRetries: number;
  ratePerMinute: number;
  maxConcurrency: number;
  startEventSeq: number;
  secretRef?: string | null;
  status: string;
  createdAt: number;
  pausedAt?: number | null;
  pausedBy?: string | null;
}

/** Where one subscription has got to: derived per request, never stored. */
interface Position {
  ackedThroughSeq: number | null;
  pending: number;
  leased: number;
  retryWait: number;
  deadLetter: number;
}

/** One classification a dead-lettered delivery was refused with. */
interface DeadLetterCode {
  code: string;
  deliveries: number;
}

interface SubscriptionView {
  board: string;
  subscription: Subscription;
  position: Position;
  deadLetterCodes: DeadLetterCode[];
  headEventSeq: number;
}

interface Listing {
  items: SubscriptionView[];
}

/** How many codes a dead-letter line names before it summarises the rest. */
const DEAD_LETTER_CODES_NAMED = 3;

/** `rust/serve.rs`'s `or_list`: "a", "b", or "c". */
function orList(values: string[]): string {
  if (values.length <= 1) {
    return values[0] ?? "";
  }
  if (values.length === 2) {
    return `${values[0]} or ${values[1]}`;
  }
  return `${values.slice(0, -1).join(", ")}, or ${values[values.length - 1]}`;
}

/**
 * `rust/serve.rs`'s `watch_sentence`: what a subscription watches, in a
 * sentence. Six selector fields rendered as six columns is six things to
 * decode; what an operator wants is to read what the thing is for.
 */
function watchSentence(subscription: Subscription): string {
  const opening =
    subscription.kinds.length === 0
      ? "Every event"
      : `Every ${orList(subscription.kinds)} event`;
  const clauses: string[] = [];
  if (subscription.subjectTaskID !== null && subscription.subjectTaskID !== undefined) {
    clauses.push(`about task ${subscription.subjectTaskID}`);
  }
  if (subscription.relations.length > 0) {
    clauses.push(`related through ${orList(subscription.relations)}`);
  }
  if (subscription.priorStatuses.length > 0) {
    clauses.push(`leaving ${orList(subscription.priorStatuses)}`);
  }
  if (subscription.currentStatuses.length > 0) {
    clauses.push(`arriving at ${orList(subscription.currentStatuses)}`);
  }
  if (subscription.tags.length > 0) {
    clauses.push(`tagged ${orList(subscription.tags)}`);
  }
  if (clauses.length === 0) {
    return `${opening} on the board.`;
  }
  return `${opening} ${clauses.join(", ")}.`;
}

/**
 * `rust/serve.rs`'s `position_sentence`: how far behind the board head a
 * subscription is, in words. "Caught up" is only honest when nothing sits
 * between the last ack and the head, and the count is board events rather
 * than matching events — which is why the page says so above the table.
 */
function positionSentence(headEventSeq: number, ackedPosition: number): string {
  const behind = headEventSeq - ackedPosition;
  if (behind <= 0) {
    return `Caught up with head seq ${headEventSeq}.`;
  }
  if (behind === 1) {
    return `1 board event behind head seq ${headEventSeq}.`;
  }
  return `${behind} board events behind head seq ${headEventSeq}.`;
}

/**
 * `rust/serve.rs`'s `dead_letter_attribution`: which refusal the dead
 * letters are, in the same sentence as how many.
 *
 * Mixed codes stay apart and are never collapsed into the count: two
 * adapters refusing for two reasons is two pieces of work, and "all" said
 * about a mixed set is a false sentence that reads as a diagnosis. Beyond
 * the third code the tail is summarised with its own numbers, so the named
 * counts plus the tail still add up to the total beside them.
 */
function deadLetterAttribution(deadLetter: number, codes: DeadLetterCode[]): string {
  const only = codes[0];
  if (only === undefined) {
    // A count with no code behind it is a state the table's CHECK forbids,
    // not an adapter that failed anonymously. Say nothing rather than invent
    // an attribution; the count is still true.
    return "";
  }
  if (codes.length === 1 && only.deliveries === deadLetter) {
    return `, all ${only.code}`;
  }
  const named = codes.slice(0, DEAD_LETTER_CODES_NAMED);
  const attribution = named.map((code) => `${code.deliveries} ${code.code}`).join(", ");
  const rest = codes.length - named.length;
  if (rest === 0) {
    return `: ${attribution}`;
  }
  const counted = named.reduce((total, code) => total + code.deliveries, 0);
  const deliveries = deadLetter - counted;
  return `: ${attribution}, and ${rest} more code${rest === 1 ? "" : "s"} across ${deliveries} deliver${deliveries === 1 ? "y" : "ies"}`;
}

/**
 * What is actually waiting, rendered only when something is.
 *
 * These three counts are aspects of position rather than peer facts, and at
 * rest all three are zero — three columns of nothing crowded out the
 * sentence that carries the meaning. Silence here reads correctly: nothing
 * queued.
 */
function QueuedState({ view }: { view: SubscriptionView }): ReactElement | null {
  const { position } = view;
  const parts: ReactNode[] = [];
  if (position.pending > 0) {
    parts.push(`${position.pending} pending`);
  }
  if (position.retryWait > 0) {
    parts.push(
      <span className="retrying" data-testid="subscription-retrying">
        {position.retryWait} retrying
      </span>,
    );
  }
  if (position.deadLetter > 0) {
    parts.push(
      <span className="dead" data-testid="subscription-dead-letters">
        {position.deadLetter} dead-lettered
        {deadLetterAttribution(position.deadLetter, view.deadLetterCodes)}
      </span>,
    );
  }
  if (parts.length === 0) {
    return null;
  }
  return (
    <div className="queued" data-testid="subscription-queued">
      {parts.map((part, at) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: the three parts are positional and never reordered
        <Fragment key={at}>
          {at > 0 ? ", " : null}
          {part}
        </Fragment>
      ))}
    </div>
  );
}

/** Where the write lands, which is wherever the affected row is visible. */
function landing(id: string, showAll: boolean): string {
  return `/subscriptions?${showAll ? "show=all&" : ""}changed=${encodeURIComponent(id)}`;
}

function SubscriptionRow({
  view,
  showAll,
  onWrote,
  onRefused,
}: {
  view: SubscriptionView;
  showAll: boolean;
  onWrote: (href: string) => void;
  onRefused: (refusal: string) => void;
}): ReactElement {
  const { subscription, position } = view;
  const paused = subscription.status !== "active";
  // Nothing acked yet means the subscription is still sitting on its start
  // anchor, which is where it began — not seq 0, and not "caught up".
  const ackedPosition = position.ackedThroughSeq ?? subscription.startEventSeq;
  // Neither control is destructive: pausing is reversible and resuming
  // restores the default, so neither gets the approve/decline weight the
  // attention surface uses for a decision.
  const verb = paused ? "resume" : "pause";
  const verbLabel = paused ? "Resume delivery" : "Pause delivery";
  // The form's action is the server's own write route, carried unchanged
  // (SPA-10): same path, same method, same absence of a body. The query it
  // carries is the filter the row was seen under, so resuming lands back on
  // the list the operator was reading.
  const action = `/subscription/${encodeURIComponent(view.board)}/${encodeURIComponent(subscription.id)}/${verb}${showAll && paused ? "?show=all" : ""}`;
  // Pausing always lands on the unfiltered list: an action whose result
  // vanishes from the page reads as an action that failed. This is the rule
  // the route's own redirect applies, and the address it produces.
  const lands = landing(subscription.id, verb === "pause" || showAll);

  const submit = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault();
    const result = await postForm(action);
    if (result.recorded) {
      onWrote(lands);
      return;
    }
    onRefused(result.refusal ?? `The board refused the change (${result.status}).`);
  };

  return (
    <tr data-subscription={subscription.id} data-testid="subscription-row">
      <td>
        <code>{subscription.id}</code>
        <div className="meta">
          <a
            href={`/board/${encodeURIComponent(view.board)}`}
            data-ref
            target="_blank"
            rel="noopener"
          >
            {view.board}
          </a>
        </div>
      </td>
      <td>{watchSentence(subscription)}</td>
      <td>
        <code>{subscription.consumerID}</code>
        <div className="meta">
          action <code>{subscription.actionID}</code> and{" "}
          {/* Whether a secret is configured is operational; which secret it
              is stays a host-local lookup name the page has no business
              repeating. */}
          {subscription.secretRef === null || subscription.secretRef === undefined
            ? "no secret configured"
            : "a secret is configured"}
        </div>
      </td>
      <td>
        <span
          className={`pill status-${subscription.status}`}
          data-subscription-state
          data-testid="subscription-state"
        >
          {subscription.status}
        </span>
        {subscription.pausedBy !== null &&
        subscription.pausedBy !== undefined &&
        subscription.pausedAt !== null &&
        subscription.pausedAt !== undefined ? (
          <div className="meta">
            paused by {subscription.pausedBy} {ago(subscription.pausedAt)}
          </div>
        ) : null}
        <form method="post" action={action} onSubmit={submit}>
          <button
            type="submit"
            data-subscription-action={verb}
            data-testid={`subscription-${verb}`}
          >
            {verbLabel}
          </button>
        </form>
      </td>
      <td>
        <span data-testid="subscription-position">
          {positionSentence(view.headEventSeq, ackedPosition)}
        </span>
        <div className="meta" data-testid="subscription-anchor">
          started at seq {subscription.startEventSeq}
          {position.ackedThroughSeq === null
            ? ", nothing acked yet"
            : `, acked through seq ${position.ackedThroughSeq}`}
          {position.leased === 0 ? "" : `, ${position.leased} in flight`}
        </div>
        <QueuedState view={view} />
      </td>
      <td>
        <div className="meta">
          {subscription.timeoutMs} ms timeout, {subscription.maxRetries} retries,{" "}
          {subscription.ratePerMinute}/min, {subscription.maxConcurrency} at a time
        </div>
      </td>
    </tr>
  );
}

export default function SubscriptionsPage({ route }: { route: Route }): ReactElement {
  const { data, error, reload } = useProjection<Listing>("/api/v1/subscriptions");
  const [refusal, setRefusal] = useState<string | null>(null);
  const showAll = route.query.get("show") === "all";
  const changed = route.query.get("changed");
  const views = data?.items ?? [];
  const shown = views.filter(
    (view) => showAll || view.subscription.status === "active",
  );
  const hidden = views.length - shown.length;

  const wrote = (href: string): void => {
    setRefusal(null);
    navigate(href);
    reload();
  };

  return (
    <main id="main" data-page data-route="subscriptions" data-testid="app-root">
      <div className="heading">
        <h1>Subscriptions</h1>
      </div>
      {changed === null ? null : (
        <p className="success" data-testid="subscription-changed">
          Recorded the change to <code>{changed}</code> and the dispatcher reads its
          state on the next pass.
        </p>
      )}
      {refusal === null ? null : (
        <p className="error" data-testid="subscription-refusal">
          {refusal}
        </p>
      )}
      {error === null ? null : (
        <p className="error" data-testid="subscriptions-error">
          The subscriptions could not be read. Reload the page to try again.
        </p>
      )}
      {data === null ? null : shown.length === 0 ? (
        // An empty list has two very different causes, and saying the wrong
        // one is how absence reads as a finding.
        <p className="empty" data-testid="subscriptions-empty">
          {hidden === 0 ? (
            <>
              Nothing is subscribed yet.{" "}
              <code>
                kb subscription add --consumer NAME --action NAME --timeout-ms 30000
                --max-retries 3 --rate-per-minute 60 --max-concurrency 1 --as geoyws
              </code>{" "}
              registers one, and it starts watching from the event that created it — add{" "}
              <code>--kind</code> or <code>--subject</code> or{" "}
              <code>--current-status</code> or <code>--tag</code> to narrow what it
              sees.
            </>
          ) : (
            <>
              Every subscription here is paused right now.{" "}
              <a href="/subscriptions?show=all" onClick={follow}>
                Show the {hidden} paused one{hidden === 1 ? "" : "s"}
              </a>{" "}
              to see where each of them stopped.
            </>
          )}
        </p>
      ) : (
        <>
          <p className="meta">
            Position is derived per request: the start anchor, the highest acked seq,
            and the distance to that board's event head. The distance counts board
            events, and a subscription only receives the ones its filter selects — the
            queued counts are what is actually waiting for it.
          </p>
          <table data-testid="subscriptions-table">
            <thead>
              <tr>
                <th>Subscription</th>
                <th>Watches</th>
                <th>Delivers to</th>
                <th>State</th>
                <th>Position</th>
                <th>Limits</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((view) => (
                <SubscriptionRow
                  key={`${view.board}/${view.subscription.id}`}
                  view={view}
                  showAll={showAll}
                  onWrote={wrote}
                  onRefused={setRefusal}
                />
              ))}
            </tbody>
          </table>
          {hidden > 0 ? (
            <p className="meta" data-testid="subscriptions-hidden">
              {hidden} paused subscription{hidden === 1 ? " is" : "s are"} hidden.{" "}
              <a href="/subscriptions?show=all" onClick={follow}>
                Show paused subscriptions
              </a>
              .
            </p>
          ) : showAll ? (
            <p className="meta" data-testid="subscriptions-showing-all">
              Listing paused subscriptions too.{" "}
              <a href="/subscriptions" onClick={follow}>
                Show active only
              </a>
              .
            </p>
          ) : null}
        </>
      )}
    </main>
  );
}

/**
 * The filter links are addresses, so they stay anchors — copyable, openable
 * in a new tab, and readable by anything that reads links — and the click
 * that a pointer makes is routed rather than reloaded.
 */
function follow(event: MouseEvent<HTMLAnchorElement>): void {
  if (event.metaKey || event.ctrlKey || event.shiftKey || event.button !== 0) {
    return;
  }
  event.preventDefault();
  navigate(event.currentTarget.getAttribute("href") ?? "/subscriptions");
}
