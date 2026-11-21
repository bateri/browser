//! Chromium's other processes (renderer, GPU, network, ...): the sandbox first, then the engine.

use cef::{args::Args, *};

fn main() {
    let args = Args::new();

    // The sandbox is entered before anything else is loaded, and kept for the process's life.
    let _sandbox = {
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        sandbox
    };

    let _loader = {
        let loader = library_loader::LibraryLoader::new(
            &std::env::current_exe().expect("current exe"),
            true,
        );
        assert!(loader.load());
        loader
    };

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    );
}
