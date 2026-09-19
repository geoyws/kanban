import type { ReactElement } from "react";

/**
 * The application root, and deliberately nothing more.
 *
 * The first wave serves a root that mounts and says so: the Needs-you
 * cutover is `t-1f495a7f` and the JSON projection it will read is
 * `t-88814b7a`. What this root owes today is SPA-04's stable test id on a
 * visible element, the heading the cutover replaces in place, and the live
 * line every page carries.
 */
export function App(): ReactElement {
  return (
    <main data-testid="app-root">
      <output className="live" data-live>
        live
      </output>
      <h1>Needs you</h1>
    </main>
  );
}
