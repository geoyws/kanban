import type { ReactElement } from "react";

/**
 * Tag chips: the `, tagged …` trailing clause every meta sentence ends with,
 * rendered as one chip per tag instead of a joined string.
 *
 * A namespaced tag reads `estate/subsystem` (slash spelling only, SPA-61):
 * the estate prefix is dimmed and the subsystem is bold (SPA-59), so the eye
 * lands on what the row is about rather than on who owns it. A tag with no
 * slash is rendered whole and bold — there is no prefix to dim. The full
 * slash spelling stays in `data-tag`, so a test reads the namespace it sees.
 */

export function TagChip({ tag }: { tag: string }): ReactElement {
  const slash = tag.indexOf("/");
  const estate = slash < 0 ? "" : tag.slice(0, slash);
  const sub = slash < 0 ? tag : tag.slice(slash + 1);
  return (
    <span className="tag-chip" data-tag={tag}>
      {estate === "" ? null : (
        <>
          <span className="tag-estate">{estate}</span>/
        </>
      )}
      <span className="tag-sub">{sub}</span>
    </span>
  );
}

/** `rust/serve.rs`'s `tag_list`: what a row is about, as a trailing clause. */
export function TagChips({ tags }: { tags: readonly string[] }): ReactElement | null {
  if (tags.length === 0) {
    return null;
  }
  return (
    <>
      {", tagged "}
      {tags.map((tag, at) => (
        <span key={`${tag}-${at}`}>
          {at === 0 ? null : ", "}
          <TagChip tag={tag} />
        </span>
      ))}
    </>
  );
}
