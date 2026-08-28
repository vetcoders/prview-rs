fn main() {
    // Keep real rustc work observable to the process sampler without making the
    // ordinary-runner receipt expensive.
    std::thread::sleep(std::time::Duration::from_millis(750));
}
