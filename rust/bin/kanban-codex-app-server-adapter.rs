// Thin shim: the app-server adapter lives in the `kanban` library so the
// binary stays a one-line entrypoint.
fn main() {
    kanban::codex_app_server_adapter_entrypoint()
}
