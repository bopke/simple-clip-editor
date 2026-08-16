//! Prints the window title this build would use. Handy for confirming that a
//! downloaded build really is the commit you think it is.
fn main() {
    println!("{}", clip_editor::version::window_title());
}
