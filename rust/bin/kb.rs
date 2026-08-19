// Thin shim: the program lives in the `kanban` library so both binaries
// are one build rather than two compilations of the same file.
fn main() {
    kanban::entrypoint()
}
