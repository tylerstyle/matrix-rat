//! The shells around `App`: they carry out what the app asks of the machine it runs on
//! and bring back what happens there. `App` keeps the state, draws and asks (the
//! `take_*` requests, the `Probes`, its `Storage`); the shell owns the screen, the
//! keyboard, the programs, the signals and the desktop. The terminal's is [`terminal`].

#[cfg(not(target_family = "wasm"))]
pub mod terminal;
