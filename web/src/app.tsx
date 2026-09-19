import type { ReactElement } from "react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { Card, Choice, WriteResult } from "./api";
import { fetchNeedsYou, postDecision, postReopen } from "./api";
import {
  type CardState,
  DecisionCard,
  type Draft,
  INCOMPLETE_ANSWER,
  type Sending,
} from "./card";
import { connectLive, type Notice } from "./live";
import { NOTICE_SHOWN, Notices } from "./notices";
import BoardPage from "./pages/board";
import BoardSprintsPage from "./pages/board-sprints";
import BoardsPage from "./pages/boards";
import DecidedPage from "./pages/decided";
import DeploymentPage from "./pages/deployment";
import DeploymentsPage from "./pages/deployments";
import LanesPage from "./pages/lanes";
import PlansPage from "./pages/plans";
import SearchPage from "./pages/search";
import SprintPage from "./pages/sprint";
import SprintsPage from "./pages/sprints";
import SubscriptionsPage from "./pages/subscriptions";
import TaskPage from "./pages/task";
import { bindPreviews, dismissPreviews } from "./previews";
import { type Route, setMountedRoutes, useRoute } from "./router";
import { Chrome, RefreshProvider } from "./shell";

/**
 * The Needs-you deck: one card on screen, the next one a keystroke away.
 *
 * Every guard in here exists because of a specific failure, and each one is
 * named where it is implemented. The four that shape the whole component:
 *
 * 1. **An answer in progress holds the projection.** Words typed into a
 *    note, a verdict picked for the own-words answer, and a decision already
 *    posted are all answers being given; a projection that arrived and was
 *    applied over one used to throw it away. The fetch still happens — the
 *    page must converge — and the result waits in `held` until the answer is
 *    finished (SPA-16, SPA-17, SPA-18).
 * 2. **The card the reader is on is never re-created.** Cards are keyed by
 *    item, so a projection that says nothing new about the current card
 *    updates the same node: same scroll position, same focus, no advance
 *    animation. That was the blink (George, 2026-09-17: "it keeps
 *    blinking") and is SPA-30.
 * 3. **A decision leaves the queue the instant it is posted**, and comes
 *    all the way back if the board refuses: the posted card stays in the
 *    document, out of the queue, so the refusal restores it to the slot it
 *    left with every control usable and the draft still in the field
 *    (SPA-21).
 * 4. **The receipts are the sitting's**, not the projection's: they live
 *    beside the queue and survive every swap, because an undo that vanished
 *    a second after the click would be no undo at all (SPA-15, SPA-33).
 */

/** The one motion: 140ms each way (WEB-20, SPA-52). */
const DECK_SLIDE = 140;

/**
 * How long a decision may be in flight before the button says so a second
 * time. Longer than any answer that is actually going to land: a local board
 * answers in milliseconds, so eight seconds means something is wrong rather
 * than something is slow.
 */
const STILL_SENDING_AFTER = 8000;

/**
 * A redelivered notice must not act twice, so every key the page has
 * rendered is remembered — bounded, because a page left open for days must
 * not grow a set that never forgets.
 */
const NOTICE_MEMORY = 200;

/**
 * What this tab has just done to a board, so the socket does not report the
 * operator to themselves: a decision made here is already on screen as a
 * receipt, and the same change arriving a second later as `Attention
 * resolved` is noise over the card being read next (George, 2026-09-17).
 * Only these words, only on the board just written to, and only for ten
 * seconds — a change somebody else makes is still news.
 */
const ECHO_WINDOW = 10_000;
const OWN_WORDS = ["Attention resolved", "Attention reopened"];

/** One row of this sitting's history: what is in flight, or what landed. */
interface HistoryRow {
  id: string;
  board: string;
  label: string;
  /** `false` while the board is still answering: a pending row is no receipt. */
  recorded: boolean;
  noted: boolean;
  outcome: string;
  undoing: boolean;
  refusal: string | null;
}

type Drafts = Record<string, Draft>;

const EMPTY_DRAFT: Draft = { note: "", outcome: null };

function Deck(): ReactElement {
  const [cards, setCards] = useState<Card[] | null>(null);
  const [order, setOrder] = useState<string[]>([]);
  const [position, setPosition] = useState(0);
  const [drafts, setDrafts] = useState<Drafts>({});
  const [customOpen, setCustomOpen] = useState<Record<string, boolean>>({});
  const [sending, setSending] = useState<Record<string, Sending>>({});
  const [refusals, setRefusals] = useState<
    Record<string, { incomplete: string | null; board: string | null }>
  >({});
  const [history, setHistory] = useState<HistoryRow[]>([]);
  const [notices, setNotices] = useState<Notice[]>([]);
  const [socketUp, setSocketUp] = useState<boolean | null>(null);
  const [applied, setApplied] = useState(0);
  const [ghost, setGhost] = useState<{ id: string; direction: string } | null>(null);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [historyOpen, setHistoryOpen] = useState(false);

  const held = useRef<Card[] | null>(null);
  const nodes = useRef<Record<string, HTMLElement | null>>({});
  const seen = useRef<Set<string>>(new Set());
  const ownChanges = useRef<Map<string, number>>(new Map());
  const ghostTimer = useRef<number | null>(null);
  const shownRef = useRef<string | null>(null);
  // The drafts and the posts in flight as the ASYNC paths read them. A
  // decision is posted across an await, and a note typed in the meantime has
  // to ride with it; the socket's handler is installed once, so the gate it
  // asks about an answer in progress has to read the same way.
  const draftsRef = useRef<Drafts>({});
  draftsRef.current = drafts;
  const sendingRef = useRef<Record<string, Sending>>({});
  sendingRef.current = sending;

  // The queue is the projection's order with the reader's own skips kept,
  // minus whatever is in flight: a posted card is out of the queue and still
  // in the document.
  const byId = new Map((cards ?? []).map((card) => [card.attention.id, card]));
  const queue = order.filter((id) => byId.has(id) && sending[id] === undefined);
  const index = Math.min(Math.max(position, 0), Math.max(0, queue.length - 1));
  const current = queue[index] ?? null;
  const inFlight = Object.keys(sending).length > 0;

  const draftOf = (id: string): Draft => drafts[id] ?? EMPTY_DRAFT;

  /**
   * An answer in progress anywhere on the page. A picked verdict with
   * nothing typed yet is an answer being written, not an idle page.
   *
   * Read through refs: the socket's handler is installed once, so a gate
   * that closed over the drafts of the first render would hold nothing.
   */
  const answerInProgress = useCallback(
    () =>
      Object.keys(sendingRef.current).length > 0 ||
      Object.values(draftsRef.current).some(
        (draft) => draft.note.trim().length > 0 || draft.outcome !== null,
      ),
    [],
  );
  /** The same question as a value, for the effects that watch for it. */
  const answering =
    inFlight ||
    Object.values(drafts).some(
      (draft) => draft.note.trim().length > 0 || draft.outcome !== null,
    );

  /**
   * What this sitting has decided. The board is the authority on what is
   * open, and it is authoritative a moment LATER than the receipt: the
   * projection fetched while a decision is settling still lists the row,
   * and applying it would put an answered card back in front of the reader
   * under the receipt that says it is done. An id leaves this set when the
   * undo brings the row back.
   */
  const decidedHere = useRef<Set<string>>(new Set());

  /** Apply one projection: the queue moves, the reader does not. */
  const apply = useCallback((projection: Card[]) => {
    const incoming = projection.filter(
      (card) => !decidedHere.current.has(card.attention.id),
    );
    setCards(incoming);
    setOrder((previous) => {
      const arriving = incoming.map((card) => card.attention.id);
      const kept = previous.filter((id) => arriving.includes(id));
      return [...kept, ...arriving.filter((id) => !kept.includes(id))];
    });
    setApplied((count) => count + 1);
  }, []);

  const refresh = useCallback(async () => {
    const incoming = await fetchNeedsYou();
    if (answerInProgress()) {
      // Fetched, not applied: the page has converged on the wire and will
      // converge on screen the moment the answer is finished.
      held.current = incoming;
      return;
    }
    apply(incoming);
  }, [answerInProgress, apply]);

  // The held projection lands as soon as there is no answer to lose, which
  // is the render where `answering` goes false: the last word is deleted,
  // the verdict is unpicked, or the post that was open comes back.
  useEffect(() => {
    if (held.current !== null && !answering) {
      const incoming = held.current;
      held.current = null;
      apply(incoming);
    }
  }, [answering, apply]);

  // The history drawer is flagged on the BODY, as the server-rendered deck
  // flagged it: the stylesheet slides the panel in from
  // `body[data-history-open]`, and the backdrop and the Escape key read the
  // same flag. One place says whether the drawer is open.
  useEffect(() => {
    document.body.toggleAttribute("data-history-open", historyOpen);
    return () => document.body.removeAttribute("data-history-open");
  }, [historyOpen]);

  /**
   * One notice onto the strip. The answer is whether it was RENDERED: a key
   * this sitting has already shown, and an echo of the reader's own write,
   * are both dropped, and the socket's redelivery path is only observable
   * if it says so.
   */
  const addNotice = useCallback((notice: Notice): boolean => {
    if (notice.key.length === 0 || seen.current.has(notice.key)) {
      return false;
    }
    seen.current.add(notice.key);
    // Remembered, then dropped: an echo that is redelivered must not be
    // rendered the second time either.
    const board = notice.board;
    if (
      notice.type === "notice" &&
      board !== undefined &&
      OWN_WORDS.includes(notice.what)
    ) {
      const at = ownChanges.current.get(board);
      if (at !== undefined && Date.now() - at < ECHO_WINDOW) {
        return false;
      }
    }
    while (seen.current.size > NOTICE_MEMORY) {
      const oldest = seen.current.values().next().value;
      if (oldest === undefined) {
        break;
      }
      seen.current.delete(oldest);
    }
    setNotices((rows) => [notice, ...rows].slice(0, NOTICE_SHOWN));
    return true;
  }, []);

  const dismiss = useCallback((key: string) => {
    setNotices((rows) => rows.filter((row) => row.key !== key));
  }, []);

  // The first projection, the socket, and the hover previews: everything
  // this page listens to, started once. The socket outlives every
  // projection, so re-running this would drop the connection on every
  // keystroke.
  // biome-ignore lint/correctness/useExhaustiveDependencies: started once on mount, by design
  useEffect(() => {
    void fetchNeedsYou().then(apply);
    const stopLive = connectLive({
      onStatus: (status) => setSocketUp(status === "live"),
      onNotice: addNotice,
      onRefresh: () => {
        void refresh().catch(() => undefined);
      },
    });
    const stopPreviews = bindPreviews();
    return () => {
      stopLive();
      stopPreviews();
    };
  }, []);

  const focusCurrent = useCallback(() => {
    if (current === null) {
      return;
    }
    const node = nodes.current[current];
    if (node === null || node === undefined) {
      return;
    }
    // Focus is only taken when it is not already somewhere the operator put
    // it: a swap must never pull the cursor out of a note being written, out
    // of the menu, or out of the side history.
    const focus = document.activeElement;
    if (
      focus instanceof HTMLElement &&
      focus.isConnected &&
      focus !== document.body &&
      (node.contains(focus) ||
        focus.closest("[data-drawer], [data-side]") !== null ||
        focus.matches("input, textarea"))
    ) {
      return;
    }
    node.focus();
  }, [current]);

  /**
   * The advance, and only when the card on screen CHANGED: a projection
   * refreshed under an unchanged card must not re-animate it.
   */
  useEffect(() => {
    if (current === null) {
      shownRef.current = null;
      return;
    }
    if (shownRef.current === current) {
      return;
    }
    shownRef.current = current;
    const node = nodes.current[current];
    if (node === null || node === undefined) {
      return;
    }
    node.classList.add("entering");
    const timer = window.setTimeout(
      () => node.classList.remove("entering"),
      DECK_SLIDE,
    );
    focusCurrent();
    return () => window.clearTimeout(timer);
  }, [current, focusCurrent]);

  const leave = useCallback((id: string, direction: string) => {
    if (ghostTimer.current !== null) {
      window.clearTimeout(ghostTimer.current);
    }
    setGhost({ id, direction });
    ghostTimer.current = window.setTimeout(() => setGhost(null), DECK_SLIDE);
  }, []);

  /**
   * `s` puts this card at the back of the queue and shows the next one, with
   * nothing recorded: the answer needs thinking about and there are 132
   * other cards. The position does not move — the same slot now holds the
   * next card — and the skipped card comes round again at the end.
   */
  const skip = useCallback(() => {
    if (current === null || queue.length < 2) {
      return;
    }
    leave(current, "forward");
    setOrder((previous) => [...previous.filter((id) => id !== current), current]);
  }, [current, leave, queue.length]);

  /** `ArrowLeft` walks back through the queue, recording nothing. */
  const back = useCallback(() => {
    if (queue.length < 2 || index === 0 || current === null) {
      return;
    }
    leave(current, "back");
    setPosition(index - 1);
  }, [current, index, leave, queue.length]);

  const setDraft = useCallback((id: string, change: Partial<Draft>) => {
    setDrafts((previous) => ({
      ...previous,
      [id]: { ...(previous[id] ?? EMPTY_DRAFT), ...change },
    }));
  }, []);

  const setRefusal = useCallback(
    (id: string, change: { incomplete?: string | null; board?: string | null }) => {
      setRefusals((previous) => {
        const held = previous[id] ?? { incomplete: null, board: null };
        return { ...previous, [id]: { ...held, ...change } };
      });
    },
    [],
  );

  /**
   * The way back out of an answer that was started and is not wanted. The
   * typed words are NOT cleared — losing them is the thing the hold exists
   * to prevent. The composer's own refusal IS, even though it is still true:
   * it was asking for the two halves of an answer the operator has just said
   * they are not giving.
   */
  const clearVerdict = useCallback(
    (id: string) => {
      setDraft(id, { outcome: null });
      setRefusal(id, { incomplete: null });
    },
    [setDraft, setRefusal],
  );

  const answer = useCallback(
    async (card: Card, decision: string, label: string, outcome: string) => {
      const id = card.attention.id;
      const draft = draftsRef.current[id] ?? EMPTY_DRAFT;
      const body = new URLSearchParams({ decision });
      if (draft.note.length > 0) {
        body.set("reply", draft.note);
      }
      if (decision === "custom" && draft.outcome !== null) {
        body.set("outcome", draft.outcome);
      }
      const noted = decision !== "custom" && draft.note.trim().length > 0;
      const slot = queue.indexOf(id);
      setSending((previous) => ({
        ...previous,
        [id]: { pressed: decision, still: false },
      }));
      setHistory((rows) => [
        {
          id,
          board: card.board,
          label,
          recorded: false,
          noted,
          outcome,
          undoing: false,
          refusal: null,
        },
        ...rows,
      ]);
      // The deck hands over to the next card now rather than when the board
      // answers: `1` sends this one off and shows the next one, and what this
      // one is doing is said in the side history instead.
      leave(id, "forward");
      if (slot >= 0) {
        setPosition(slot);
      }
      const still = window.setTimeout(() => {
        setSending((previous) => {
          const open = previous[id];
          return open === undefined
            ? previous
            : { ...previous, [id]: { ...open, still: true } };
        });
      }, STILL_SENDING_AFTER);
      let result: WriteResult;
      try {
        result = await postDecision(card.board, id, body);
      } catch {
        result = { recorded: false, refusal: null, status: 0 };
      }
      window.clearTimeout(still);
      setSending((previous) => {
        const { [id]: _gone, ...rest } = previous;
        return rest;
      });
      if (result.recorded) {
        decidedHere.current.add(id);
        ownChanges.current.set(card.board, Date.now());
        setHistory((rows) =>
          rows.map((row) =>
            row.id === id && !row.recorded ? { ...row, recorded: true } : row,
          ),
        );
        setCards((previous) =>
          previous === null
            ? previous
            : previous.filter((row) => row.attention.id !== id),
        );
        setDrafts((previous) => {
          const { [id]: _draft, ...rest } = previous;
          return rest;
        });
        setRefusal(id, { incomplete: null, board: null });
        return;
      }
      // Nothing was recorded, so the card comes all the way back before it is
      // told why: a refusal under a control that can no longer be used reads
      // as a card that can no longer be answered at all.
      setHistory((rows) => rows.filter((row) => !(row.id === id && !row.recorded)));
      if (slot >= 0) {
        setPosition(slot);
      }
      setRefusal(id, {
        board:
          result.status === 0
            ? "The decision did not reach the board. Try again."
            : (result.refusal ?? `The board refused this decision (${result.status}).`),
      });
      window.setTimeout(() => nodes.current[id]?.focus(), 0);
    },
    [leave, queue, setRefusal],
  );

  const onChoice = useCallback(
    (card: Card, choice: Choice) => {
      const id = card.attention.id;
      if (sending[id] !== undefined) {
        return;
      }
      // An authored choice carries its own verdict and the route forwards no
      // picker value onto it, so a verdict the operator left picked is not
      // part of THIS decision and the card must not go on showing it.
      clearVerdict(id);
      void answer(card, choice.key, choice.label, choice.outcome);
    },
    [answer, clearVerdict, sending],
  );

  const onCustom = useCallback(
    (card: Card) => {
      const id = card.attention.id;
      if (sending[id] !== undefined) {
        return;
      }
      const draft = draftsRef.current[id] ?? EMPTY_DRAFT;
      if (draft.outcome === null || draft.note.trim().length === 0) {
        // Refused here, in the card's own words, with the focus moved to the
        // half that is missing. Nothing is posted, and the sentence does not
        // say which half is missing — the cursor does.
        setRefusal(id, { incomplete: INCOMPLETE_ANSWER });
        const node = nodes.current[id];
        const missing =
          draft.outcome === null
            ? node?.querySelector<HTMLElement>("input[name=outcome]")
            : node?.querySelector<HTMLElement>("textarea[name=reply]");
        if (draft.outcome !== null) {
          setCustomOpen((previous) => ({ ...previous, [id]: true }));
        }
        window.setTimeout(() => missing?.focus(), 0);
        return;
      }
      void answer(
        card,
        "custom",
        `Custom answer, recorded as ${draft.outcome}`,
        draft.outcome,
      );
    },
    [answer, sending, setRefusal],
  );

  /**
   * Reopen one decided item through the same trusted-edge route a reply
   * uses, then put the deck and the keyboard back on the card it brought
   * back: `1` keeps working.
   */
  const undo = useCallback(
    async (row: HistoryRow) => {
      if (row.undoing) {
        return;
      }
      setHistory((rows) =>
        rows.map((each) => (each.id === row.id ? { ...each, undoing: true } : each)),
      );
      let result: WriteResult;
      try {
        result = await postReopen(row.board, row.id);
      } catch {
        result = { recorded: false, refusal: null, status: 0 };
      }
      if (!result.recorded) {
        const refusal =
          result.status === 0
            ? "The undo did not reach the board. Try again."
            : (result.refusal ??
              `The board refused to bring back ${row.id} (${result.status}).`);
        setHistory((rows) =>
          rows.map((each) =>
            each.id === row.id ? { ...each, undoing: false, refusal } : each,
          ),
        );
        return;
      }
      // Open again, so the projection may hand it back.
      decidedHere.current.delete(row.id);
      ownChanges.current.set(row.board, Date.now());
      setHistory((rows) => rows.filter((each) => each.id !== row.id));
      const incoming = await fetchNeedsYou();
      apply(incoming);
      const at = incoming.findIndex((card) => card.attention.id === row.id);
      if (at >= 0) {
        setPosition(at);
        setOrder((previous) => [
          row.id,
          ...previous.filter((id) => id !== row.id).slice(0, at),
          ...previous.filter((id) => id !== row.id).slice(at),
        ]);
      }
      addNotice({
        type: "notice",
        key: `undone-${row.id}-${Date.now()}`,
        what: `Brought back ${row.id}. It is open again.`,
      });
      window.setTimeout(() => nodes.current[row.id]?.focus(), 0);
    },
    [addNotice, apply],
  );

  const cardRef = useCallback((id: string, node: HTMLElement | null) => {
    nodes.current[id] = node;
  }, []);

  // --- the keyboard ---------------------------------------------------------
  // 1-4 answer the card the deck is showing, in the order it lists them, so
  // 1 is always the recommendation. Every key below calls `preventDefault`,
  // and not only to stop the browser's own default: a keydown this page
  // leaves unhandled arrives again, and again — the driver the browser tests
  // run through delivers one keypress as thousands of keydowns until one of
  // them is prevented.
  const onKey = useCallback(
    (event: KeyboardEvent) => {
      if (event.metaKey || event.ctrlKey || event.altKey || event.isComposing) {
        return;
      }
      const target = event.target instanceof HTMLElement ? event.target : null;
      // Focus INSIDE the composer is composing, whatever kind of element it
      // is on: the note, the picker, a fold's own summary, the submit, the
      // release. A `summary` is a tab stop, so `Tab` then `1` used to reach
      // the choice shortcut and record the recommendation over a note still
      // being written.
      const composing =
        target?.matches("textarea, input, summary, details[data-custom] *") === true;
      const cardNode = target?.closest<HTMLElement>("[data-testid=deck-card]") ?? null;
      if (event.key === "Escape") {
        if (dismissPreviews()) {
          event.preventDefault();
          return;
        }
        if (notices.length > 0) {
          event.preventDefault();
          setNotices([]);
          return;
        }
        if (drawerOpen || historyOpen) {
          event.preventDefault();
          setDrawerOpen(false);
          setHistoryOpen(false);
          return;
        }
        const escaped = cardNode?.dataset.item ?? current;
        if (escaped !== null && escaped !== undefined) {
          event.preventDefault();
          clearVerdict(escaped);
        }
        return;
      }
      if (composing) {
        // Enter from the picker has to be aimed: the browser's own implicit
        // submission would pick the form's first submit button, which is the
        // recommendation.
        if (event.key === "Enter" && target.matches("input[name=outcome]")) {
          const record = target
            .closest("form")
            ?.querySelector<HTMLButtonElement>("button.record");
          if (record !== null && record !== undefined) {
            event.preventDefault();
            record.click();
          }
        }
        return;
      }
      if (event.key === "u") {
        const row =
          target?.closest<HTMLElement>("[data-receipt]")?.dataset.receipt ??
          history.find((each) => each.recorded)?.id;
        const undone = history.find((each) => each.id === row && each.recorded);
        if (undone !== undefined) {
          event.preventDefault();
          void undo(undone);
        }
        return;
      }
      if (event.key === "m") {
        event.preventDefault();
        setDrawerOpen((open) => !open);
        return;
      }
      if (event.key === "s" || event.key === "ArrowRight") {
        event.preventDefault();
        skip();
        return;
      }
      if (event.key === "ArrowLeft") {
        event.preventDefault();
        back();
        return;
      }
      // The deck answers for the card it is showing even when focus has been
      // put somewhere that is not a card at all — a tap on the backdrop, a
      // link followed back — because there is exactly one card a digit could
      // mean.
      const acting = cardNode ?? (current === null ? null : nodes.current[current]);
      if (acting === null || acting === undefined) {
        return;
      }
      const digit = ["1", "2", "3", "4"].indexOf(event.key);
      if (digit >= 0) {
        // A picked verdict is the card composing its own answer, and a digit
        // is then a digit rather than a decision.
        if (acting.querySelector("input[name=outcome]:checked") !== null) {
          return;
        }
        const choices = acting.querySelectorAll<HTMLButtonElement>("button.choice");
        const choice = choices[digit];
        if (choice !== undefined) {
          event.preventDefault();
          choice.click();
        }
        return;
      }
      if (event.key === "c") {
        const id = acting.dataset.item;
        if (id !== undefined) {
          setCustomOpen((previous) => ({ ...previous, [id]: true }));
        }
        const note = acting.querySelector<HTMLTextAreaElement>("textarea[name=reply]");
        if (note !== null) {
          event.preventDefault();
          note.focus();
        }
      }
    },
    [
      back,
      clearVerdict,
      current,
      drawerOpen,
      history,
      historyOpen,
      notices.length,
      skip,
      undo,
    ],
  );

  useEffect(() => {
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onKey]);

  // --- what is on screen ---------------------------------------------------
  const receipts = history.filter((row) => row.recorded).length;
  const rendered = [
    ...queue,
    ...Object.keys(sending),
    ...(ghost !== null && !queue.includes(ghost.id) ? [ghost.id] : []),
  ].filter((id, at, all) => all.indexOf(id) === at && byId.has(id));
  const params = new URLSearchParams(location.search);
  const replied = params.get("replied");
  const undone = params.get("undone");

  const stateOf = (id: string): CardState => {
    const open = sending[id];
    const refusal = refusals[id];
    return {
      current: id === current,
      hidden: id !== current && (ghost === null || ghost.id !== id),
      sending: open ?? null,
      motion:
        ghost !== null && ghost.id === id
          ? ghost.direction === "back"
            ? "leaving-back"
            : "leaving"
          : null,
      draft: draftOf(id),
      customOpen: customOpen[id] === true || draftOf(id).outcome !== null,
      incomplete: refusal?.incomplete ?? null,
      board: refusal?.board ?? null,
    };
  };

  return (
    <>
      <Chrome
        current="needs-you"
        live={
          inFlight
            ? "sending"
            : socketUp === null
              ? "connecting"
              : socketUp
                ? "live"
                : "reconnecting"
        }
        drawerOpen={drawerOpen}
        onDrawer={setDrawerOpen}
        covered={drawerOpen || historyOpen}
        onUncover={() => {
          setDrawerOpen(false);
          setHistoryOpen(false);
        }}
      />
      <main
        id="main"
        data-deck
        data-testid="app-root"
        data-deck-position={index}
        data-projection={applied}
      >
        <div className="heading">
          <h1>Needs you</h1>
          <button
            type="button"
            className="history-toggle"
            data-history-toggle
            data-testid="history-toggle"
            aria-expanded={historyOpen}
            aria-controls="session-history"
            onClick={() => setHistoryOpen((open) => !open)}
          >
            Decided{" "}
            <span data-history-count data-testid="history-count">
              {receipts}
            </span>
          </button>
        </div>
        {replied === null ? null : (
          <p className="success">
            <code>{replied}</code> is decided and the board has it.
          </p>
        )}
        {undone === null ? null : (
          <p className="success">
            Brought back <code>{undone}</code> - it is open again and back at its place
            in this list.
          </p>
        )}
        <p className="progress" data-testid="deck-progress" hidden={queue.length === 0}>
          <span data-open-count>{queue.length}</span> left
        </p>
        <section className="deck" data-deck-cards data-testid="deck-cards">
          {rendered.map((id) => {
            const card = byId.get(id);
            return card === undefined ? null : (
              <DecisionCard
                key={id}
                card={card}
                state={stateOf(id)}
                cardRef={cardRef}
                handlers={{
                  onNote: (item, note) => {
                    setDraft(item, { note });
                    // The refusal named what was missing. Once both halves
                    // are there it has been answered, so it goes as the
                    // words are typed rather than waiting for the next
                    // click to take it away.
                    if (note.trim().length > 0 && draftOf(item).outcome !== null) {
                      setRefusal(item, { incomplete: null });
                    }
                  },
                  onOutcome: (item, outcome) => {
                    setDraft(item, { outcome });
                    setRefusal(item, { incomplete: null });
                  },
                  onClear: clearVerdict,
                  onCustomOpen: (item, open) =>
                    setCustomOpen((previous) => ({ ...previous, [item]: open })),
                  onChoice,
                  onCustom,
                }}
              />
            );
          })}
          {cards !== null && queue.length === 0 ? (
            // The empty queue says so at once rather than after the board
            // answers: deciding the last card used to leave a blank deck for
            // the length of a round trip.
            <div className="empty-queue" data-deck-empty data-testid="deck-empty">
              <p className="empty">
                Nothing is waiting. Every question an agent raised has an answer.
              </p>
              <p>
                <a href="/decided">See what was decided</a>
              </p>
            </div>
          ) : null}
        </section>
        <p className="keys" data-testid="deck-keys">
          1–4 answer · s skip · u undo · c own
        </p>
        <aside className="side" data-side data-testid="deck-side">
          <Notices notices={notices} onDismiss={dismiss} />
          <section
            className="history"
            id="session-history"
            data-history
            data-testid="deck-history"
          >
            <h2>Decided this session</h2>
            {history.map((row) =>
              row.recorded ? (
                <p
                  key={row.id}
                  className={`receipt outcome-${row.outcome}`}
                  data-receipt={row.id}
                  data-testid="deck-receipt"
                  data-item={row.id}
                  data-project={row.board}
                  {...(row.refusal === null
                    ? {}
                    : { "aria-describedby": `refusal-row-${row.id}` })}
                >
                  <span className="decided">
                    {row.noted
                      ? `Decided: ${row.label}. Your reply is recorded.`
                      : `Decided: ${row.label}.`}
                  </span>{" "}
                  <button
                    type="button"
                    className="undo-button"
                    data-undo
                    {...(row.undoing ? { "data-pressed": "" } : {})}
                    onClick={() => void undo(row)}
                  >
                    {row.undoing ? "Undoing…" : "Undo"}
                  </button>
                  {row.refusal === null ? null : (
                    <span
                      className="error"
                      data-refusal="board"
                      id={`refusal-row-${row.id}`}
                    >
                      {row.refusal}
                    </span>
                  )}
                </p>
              ) : (
                // What the history says while the board is still answering.
                // It is NOT a receipt: nothing is recorded yet, so it carries
                // no undo and no receipt tag.
                <p key={row.id} className="pending" data-pending={row.id}>
                  {`Sending… ${row.label}`}
                </p>
              ),
            )}
          </section>
        </aside>
      </main>
    </>
  );
}

/**
 * Every page that is not the deck, under one bar, one drawer and one
 * socket.
 *
 * The deck keeps its own of each because it is the one page with a state
 * worth protecting — an answer half written, a card mid-advance — and
 * mounting it under a component that swaps pages would be one more thing
 * able to throw that away. Everything else is a read page: it fetches its
 * projection, it re-fetches when the boards move, and swapping one for
 * another costs nothing.
 *
 * The socket is here rather than in each page so that walking the drawer
 * does not open and drop a `/live` connection per destination.
 */
function ReadPages({ route }: { route: Route }): ReactElement {
  const [socketUp, setSocketUp] = useState<boolean | null>(null);
  const [refreshes, setRefreshes] = useState(0);
  const [drawerOpen, setDrawerOpen] = useState(false);

  useEffect(() => {
    const stopLive = connectLive({
      onStatus: (status) => setSocketUp(status === "live"),
      // A read page renders no notice strip, exactly as the served read
      // pages rendered none: the strip is the deck's, where a decision is
      // being made. Nothing was shown, so nothing is claimed.
      onNotice: () => false,
      onRefresh: () => setRefreshes((count) => count + 1),
    });
    const stopPreviews = bindPreviews();
    return () => {
      stopLive();
      stopPreviews();
    };
  }, []);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") {
        return;
      }
      if (dismissPreviews()) {
        event.preventDefault();
        return;
      }
      if (drawerOpen) {
        event.preventDefault();
        setDrawerOpen(false);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [drawerOpen]);

  return (
    <RefreshProvider value={refreshes}>
      <Chrome
        current={route.name}
        live={socketUp === null ? "connecting" : socketUp ? "live" : "reconnecting"}
        drawerOpen={drawerOpen}
        onDrawer={setDrawerOpen}
        covered={drawerOpen}
        onUncover={() => setDrawerOpen(false)}
      />
      <Page route={route} />
    </RefreshProvider>
  );
}

/**
 * Every read page this bundle renders, by the route name that reaches it.
 *
 * One table rather than a switch and a second list of names: this is also
 * what `navigate` is told the application can draw, so a page cannot be
 * added and left unroutable, and a route cannot be routed to before there
 * is a page for it.
 */
const PAGES: Record<string, (route: Route) => ReactElement> = {
  decided: (route) => <DecidedPage route={route} />,
  boards: () => <BoardsPage />,
  board: (route) => <BoardPage route={route} />,
  lanes: () => <LanesPage />,
  subscriptions: (route) => <SubscriptionsPage route={route} />,
  deployments: () => <DeploymentsPage />,
  deployment: (route) => <DeploymentPage route={route} />,
  sprints: (route) => <SprintsPage route={route} />,
  "board-sprints": (route) => <BoardSprintsPage route={route} />,
  sprint: (route) => <SprintPage route={route} />,
  plans: (route) => <PlansPage route={route} />,
  search: (route) => <SearchPage route={route} />,
  task: (route) => <TaskPage route={route} />,
};

setMountedRoutes(["needs-you", ...Object.keys(PAGES)]);

/** One route to one page. A name with no page is a page that says so. */
function Page({ route }: { route: Route }): ReactElement {
  const page = PAGES[route.name];
  if (page !== undefined) {
    return page(route);
  }
  return (
    <main id="main" data-page data-route="not-found" data-testid="app-root">
      <h1>Not found</h1>
      <p>
        No page at that address. <a href="/">Start over</a>.
      </p>
    </main>
  );
}

/**
 * The application: one document, and the address bar says which page it is
 * showing (ADR-048 §1).
 */
export function App(): ReactElement {
  const route = useRoute();
  return route.name === "needs-you" ? <Deck /> : <ReadPages route={route} />;
}
