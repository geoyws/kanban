import { createRoot } from "react-dom/client";
import { App } from "./app";

/**
 * The entry point: find the shell's mount point and put the application in
 * it.
 *
 * The shell (`rust/serve.rs`'s `app_shell`) serves `#root` and nothing else,
 * so a missing mount point is a served-bytes bug and must be loud rather
 * than a page that silently stays blank.
 */
const mount = document.getElementById("root");
if (mount === null) {
  throw new Error("the app shell served no #root to mount into");
}
createRoot(mount).render(<App />);
